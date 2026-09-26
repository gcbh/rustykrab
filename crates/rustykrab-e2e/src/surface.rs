//! The channels a user can actually reach the agent on, and a stand-in
//! for the APIs they talk to.
//!
//! An eval that only ever drives the gateway measures the gateway. The
//! agent behaves differently on Telegram and Signal — different prompts,
//! different message plumbing, different failure modes — so a behavioural
//! result that does not name its surface is not a result.

use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    /// What Apollo speaks: POST a message, consume the SSE stream.
    Gateway,
    Telegram,
    Signal,
}

impl Surface {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_lowercase().as_str() {
            "gateway" | "ios" | "apollo" => Ok(Surface::Gateway),
            "telegram" => Ok(Surface::Telegram),
            "signal" => Ok(Surface::Signal),
            other => bail!("unknown surface '{other}' (gateway|telegram|signal)"),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Surface::Gateway => "gateway",
            Surface::Telegram => "telegram",
            Surface::Signal => "signal",
        }
    }
}

/// Stands in for the Telegram Bot API and signal-cli-rest-api so a trial
/// can read what the bot *would* have sent without any network egress.
#[derive(Clone, Default)]
pub struct Captured(Arc<Mutex<Vec<String>>>);

impl Captured {
    pub fn push(&self, s: String) {
        self.0.lock().unwrap().push(s);
    }
    pub fn drain(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
    /// Every message so far, oldest first, without draining them.
    pub fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
    pub fn joined(&self) -> String {
        self.0.lock().unwrap().join("\n")
    }
    pub fn is_empty(&self) -> bool {
        self.0.lock().unwrap().is_empty()
    }
}

/// Boots the stand-in API on an ephemeral port and returns its base URL.
pub async fn start_capture_server(captured: Captured) -> Result<String> {
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::Uri;
    use axum::response::IntoResponse;
    use axum::Json;

    async fn handle(State(cap): State<Captured>, uri: Uri, body: Bytes) -> impl IntoResponse {
        // Telegram calls it `text`, signal-cli calls it `message`.
        if let Ok(v) = serde_json::from_slice::<Value>(&body) {
            for key in ["text", "message"] {
                if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
                    if !s.is_empty() {
                        cap.push(s.to_string());
                    }
                }
            }
        }
        // Both channels long-poll for inbound messages as well as sending,
        // and each parses a different shape: signal-cli's /v1/receive
        // returns a bare array, Telegram's getUpdates an {ok, result: []}.
        // Answering either with the wrong shape makes the poll loop log a
        // decode error every second for the length of the trial.
        let path = uri.path();
        if path.starts_with("/v1/receive") {
            Json(json!([]))
        } else if path.contains("getUpdates") {
            Json(json!({"ok": true, "result": []}))
        } else {
            Json(json!({"ok": true, "result": {}, "versions": ["v0.0-credeval"]}))
        }
    }

    let app = axum::Router::new()
        .fallback(axum::routing::any(handle))
        .with_state(captured);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(format!("http://127.0.0.1:{}", addr.port()))
}

/// Credentials the capture server accepts. None of them reach anything
/// real: the capture server answers on localhost and the daemon's
/// environment is otherwise empty.
pub const WEBHOOK_SECRET: &str = "e2e-webhook-secret";
pub const TG_CHAT_ID: i64 = 4242;
pub const SIGNAL_ACCOUNT: &str = "+15550000000";
pub const SIGNAL_USER: &str = "+15551234567";

/// Point a daemon's outbound channel calls at the capture server.
///
/// The gateway needs nothing: it is the daemon's own HTTP API. The other
/// two are bot integrations, and each has to be told both who it is and
/// where its API lives, or the daemon will not start the channel loop at
/// all.
pub fn configure_channel(
    command: &mut std::process::Command,
    surface: Surface,
    capture_base: &str,
) {
    match surface {
        Surface::Gateway => {}
        Surface::Telegram => {
            command
                .env("TELEGRAM_BOT_TOKEN", "e2e-bot-token")
                .env("TELEGRAM_ALLOWED_CHATS", TG_CHAT_ID.to_string())
                .env("TELEGRAM_WEBHOOK_SECRET", WEBHOOK_SECRET)
                .env("TELEGRAM_API_BASE", capture_base);
        }
        Surface::Signal => {
            command
                .env("SIGNAL_ACCOUNT", SIGNAL_ACCOUNT)
                .env("SIGNAL_CLI_URL", capture_base)
                .env("SIGNAL_ALLOWED_NUMBERS", SIGNAL_USER)
                .env("SIGNAL_WEBHOOK_SECRET", WEBHOOK_SECRET);
        }
    }
}

/// The repository the GitHub stand-in pretends to be.
pub const GITHUB_REPO: &str = "e2e-owner/e2e-repo";

/// Where the scripted daemon's Telegram webhook is registered. Only the
/// stand-in ever sees it: webhook mode means inbound messages arrive solely
/// through `POST /webhook/telegram`, so the long-poll loop never spins
/// against a stand-in that answers `getUpdates` at once.
const TELEGRAM_WEBHOOK_URL: &str = "https://harness.example.ts.net/webhook/telegram";

/// The external APIs the scripted daemon talks to, stood in for locally:
/// the Telegram Bot API, so a scenario can read every message the bot sent,
/// and GitHub's issue API, so a scenario can read every call the projection
/// adapter made. Started once per scripted suite and kept across daemon
/// restarts, so what reached the user can be counted over a restart.
#[derive(Clone)]
pub struct StandIns {
    pub telegram: Captured,
    telegram_base: String,
    pub github: GithubStandIn,
    github_base: String,
}

impl StandIns {
    pub async fn start() -> Result<Self> {
        let telegram = Captured::default();
        let telegram_base = start_capture_server(telegram.clone()).await?;
        let github = GithubStandIn::default();
        let github_base = github.start().await?;
        Ok(Self {
            telegram,
            telegram_base,
            github,
            github_base,
        })
    }

    /// Point a scripted daemon at the stand-ins. The GitHub variables are
    /// read by the projection adapter of the control plan's Phase 6; until
    /// it exists nothing reads them.
    pub fn configure(&self, command: &mut std::process::Command) {
        configure_channel(command, Surface::Telegram, &self.telegram_base);
        command
            .env("TELEGRAM_WEBHOOK_URL", TELEGRAM_WEBHOOK_URL)
            .env("RUSTYKRAB_GITHUB_API_BASE", &self.github_base)
            .env("RUSTYKRAB_GITHUB_REPO", GITHUB_REPO)
            .env("RUSTYKRAB_GITHUB_TOKEN", "e2e-github-token");
    }
}

/// One call the daemon made to the GitHub stand-in.
#[derive(Debug, Clone)]
pub struct GithubCall {
    pub method: String,
    pub path: String,
    pub body: Value,
}

#[derive(Default)]
struct GithubState {
    calls: Vec<GithubCall>,
    /// Issues by number, as the stand-in currently holds them.
    issues: std::collections::BTreeMap<u64, Value>,
}

/// A minimal GitHub issue API: create, read, list, edit, label and comment.
/// Every call is logged, and a scenario can edit an issue the way a person
/// on github.com would, to prove the next projection overwrites it.
#[derive(Clone, Default)]
pub struct GithubStandIn(Arc<Mutex<GithubState>>);

impl GithubStandIn {
    pub fn calls(&self) -> Vec<GithubCall> {
        self.0.lock().unwrap().calls.clone()
    }

    pub fn issues(&self) -> Vec<Value> {
        self.0.lock().unwrap().issues.values().cloned().collect()
    }

    /// Change one field of an issue as a person on github.com would.
    pub fn hand_edit(&self, number: u64, field: &str, value: Value) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        let Some(issue) = state.issues.get_mut(&number) else {
            bail!("the GitHub stand-in has no issue #{number}");
        };
        issue[field] = value;
        Ok(())
    }

    fn handle(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let mut state = self.0.lock().unwrap();
        state.calls.push(GithubCall {
            method: method.to_string(),
            path: path.to_string(),
            body: body.clone(),
        });
        let prefix = format!("/repos/{GITHUB_REPO}/issues");
        let Some(rest) = path.strip_prefix(&prefix) else {
            return (200, json!({}));
        };
        let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        match (method, segments.as_slice()) {
            ("POST", []) => {
                let number = state.issues.len() as u64 + 1;
                let mut issue = body;
                issue["number"] = json!(number);
                issue["state"] = json!("open");
                issue["html_url"] =
                    json!(format!("https://github.com/{GITHUB_REPO}/issues/{number}"));
                state.issues.insert(number, issue.clone());
                (201, issue)
            }
            ("GET", []) => (200, Value::Array(state.issues.values().cloned().collect())),
            (_, [number, tail @ ..]) => {
                let Some(issue) = number
                    .parse::<u64>()
                    .ok()
                    .and_then(|n| state.issues.get_mut(&n))
                else {
                    return (404, json!({"message": "Not Found"}));
                };
                match (method, tail) {
                    ("GET", []) => (200, issue.clone()),
                    ("PATCH", []) => {
                        if let (Some(target), Some(changes)) =
                            (issue.as_object_mut(), body.as_object())
                        {
                            for (key, value) in changes {
                                target.insert(key.clone(), value.clone());
                            }
                        }
                        (200, issue.clone())
                    }
                    ("POST", ["labels"]) => {
                        let mut labels = issue["labels"].as_array().cloned().unwrap_or_default();
                        labels.extend(body["labels"].as_array().cloned().unwrap_or_default());
                        issue["labels"] = Value::Array(labels);
                        (200, issue["labels"].clone())
                    }
                    ("POST", ["comments"]) => (201, json!({"id": 1, "body": body["body"]})),
                    ("GET", ["comments"]) => (200, json!([])),
                    _ => (200, json!({})),
                }
            }
            _ => (200, json!({})),
        }
    }

    async fn start(&self) -> Result<String> {
        use axum::body::Bytes;
        use axum::extract::State;
        use axum::http::{Method, StatusCode, Uri};
        use axum::response::IntoResponse;
        use axum::Json;

        async fn serve(
            State(stand_in): State<GithubStandIn>,
            method: Method,
            uri: Uri,
            body: Bytes,
        ) -> impl IntoResponse {
            let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let (status, reply) = stand_in.handle(method.as_str(), uri.path(), body);
            (
                StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
                Json(reply),
            )
        }

        let app = axum::Router::new()
            .fallback(axum::routing::any(serve))
            .with_state(self.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(format!("http://127.0.0.1:{}", addr.port()))
    }
}

#[cfg(test)]
mod github_stand_in_tests {
    use super::*;

    #[test]
    fn issues_are_created_edited_by_hand_and_patched_back() {
        let github = GithubStandIn::default();
        let issues = format!("/repos/{GITHUB_REPO}/issues");
        let (status, created) =
            github.handle("POST", &issues, json!({"title": "T", "labels": ["x"]}));
        assert_eq!(status, 201);
        assert_eq!(created["number"], 1);
        github.hand_edit(1, "title", json!("edited")).unwrap();
        assert_eq!(github.issues()[0]["title"], "edited");
        let (status, patched) =
            github.handle("PATCH", &format!("{issues}/1"), json!({"title": "T"}));
        assert_eq!(status, 200);
        assert_eq!(patched["title"], "T");
        assert_eq!(
            github.handle("GET", &format!("{issues}/9"), Value::Null).0,
            404
        );
        assert_eq!(github.calls().len(), 3);
    }
}
