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
