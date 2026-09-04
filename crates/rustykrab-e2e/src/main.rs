//! End-to-end evaluation harness (Workstream F3).
//!
//! Boots the daemon on a throwaway data dir and an ephemeral port, drives
//! the phase exit-criteria scenarios over HTTP with a scripted agent, and
//! asserts on responses **and** on store state. Scenarios that encode
//! not-yet-implemented behaviour (the Phase 2 credential guard) are marked
//! `XFail`: the suite is green while they fail, and a phase ships by
//! flipping its scenarios to must-pass. An unexpected pass (XPASS) fails
//! the suite — it means a scenario must be promoted.
//!
//! Output is a JSON report on stdout plus a matching exit code (0 = green),
//! so agents and CI can assert mechanically. Run via `scripts/e2e.sh`, or
//! directly:
//!
//! ```sh
//! cargo build -p rustykrab-cli --no-default-features
//! RUSTYKRAB_BIN=target/debug/rustykrab-cli cargo run -p rustykrab-e2e
//! ```

mod ablation;
mod assertion;
mod classify;
mod credential_suite;
mod fixture_repo;
mod judge;
mod login_suite;
mod model_suite;
mod planning_suite;
mod surface;
mod transcript;

use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

/// Master key for the throwaway store (hex, 32 bytes). Test-only.
const MASTER_KEY_HEX: &str = "e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0";
const AUTH_TOKEN: &str = "e2e-master-token";
/// A non-loopback origin the harness allows, standing in for the tailnet
/// hostname the phone uses.
const ALLOWED_ORIGIN: &str = "https://harness.example.ts.net";

/// Script replayed by the daemon's `RUSTYKRAB_PROVIDER=scripted` provider.
/// Triggers here must match the messages the scenarios send. Each scenario
/// ends with `task_complete` — the runner's explicit completion signal,
/// whose `summary` becomes the final assistant message.
const AGENT_SCRIPT: &str = r#"{
  "defaultText": "Done.",
  "scenarios": [
    {
      "trigger": "e2e: create credential",
      "steps": [
        { "toolCalls": [ { "name": "credential_write",
                           "arguments": { "action": "set",
                                          "name": "e2e_scripted_token",
                                          "value": "scripted-v1" } } ] },
        { "toolCalls": [ { "name": "task_complete",
                           "arguments": { "summary": "Created e2e_scripted_token." } } ] }
      ]
    },
    {
      "trigger": "e2e: streamed credential",
      "steps": [
        { "toolCalls": [ { "name": "credential_write",
                           "arguments": { "action": "set",
                                          "name": "e2e_streamed_token",
                                          "value": "streamed-v1" } } ] },
        { "toolCalls": [ { "name": "task_complete",
                           "arguments": { "summary": "Created e2e_streamed_token." } } ] }
      ]
    },
    {
      "trigger": "e2e: overwrite credential",
      "steps": [
        { "toolCalls": [ { "name": "credential_write",
                           "arguments": { "action": "set",
                                          "name": "e2e_guard_token",
                                          "value": "hijacked" } } ] },
        { "toolCalls": [ { "name": "task_complete",
                           "arguments": { "summary": "Attempted overwrite of e2e_guard_token." } } ] }
      ]
    },
    {
      "trigger": "e2e: delete credential",
      "steps": [
        { "toolCalls": [ { "name": "credential_write",
                           "arguments": { "action": "delete",
                                          "name": "e2e_delete_token" } } ] },
        { "toolCalls": [ { "name": "task_complete",
                           "arguments": { "summary": "Attempted delete of e2e_delete_token." } } ] }
      ]
    }
  ]
}"#;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Expected {
    /// Implemented behaviour — must pass.
    Pass,
    /// A behavioural measurement rather than a test. Reports an outcome
    /// distribution and never turns the suite red: there is no single
    /// right answer to "what does the model do when it lacks a
    /// credential", only a rate, and a rate that moved is news rather
    /// than a failure. Gate one of these by giving it a threshold.
    Measure,
    /// Target behaviour that is not built yet: the suite stays green while
    /// it fails, and an unexpected pass turns the suite red so the
    /// scenario gets promoted.
    ///
    /// Currently unconstructed, because every scenario has been promoted —
    /// which is exactly what "the server is done" looks like. Kept because
    /// the next phase's targets are written as xfail first; deleting it
    /// would throw away the convention the moment it had proved itself.
    #[allow(dead_code)]
    XFail,
}

#[derive(Debug, Serialize)]
struct ScenarioReport {
    id: String,
    /// "scripted" or "model". The two are not comparable — one says the
    /// plumbing works, the other says a real model could use it — and the
    /// report must never let them be read as a single number.
    mode: &'static str,
    expected: Expected,
    passed: bool,
    /// "pass" | "fail" | "xfail" | "xpass"
    outcome: &'static str,
    runs: usize,
    passes: usize,
    mean_ms: u128,
    /// Distinct failure reasons across repetitions.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    details: Vec<String>,
    /// Outcome-class counts, for cells scored by a classifier rather than
    /// by boolean assertions. A distribution is the product of a
    /// measurement; collapsing it to pass/fail would throw it away.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    classes: Vec<(String, usize)>,
    /// The rate this cell measures, when it measures one.
    #[serde(skip_serializing_if = "Option::is_none")]
    rate: Option<f64>,
}

/// A model scenario passes when it passes strictly more than this fraction
/// of its repetitions. Scripted scenarios are deterministic and must pass
/// every run instead: one failure in three is a broken scenario, not an
/// acceptable rate.
const MODEL_MAJORITY: f64 = 0.5;

impl ScenarioReport {
    fn new(
        id: impl Into<String>,
        mode: &'static str,
        expected: Expected,
        runs: usize,
        passes: usize,
        details: Vec<String>,
        mean_ms: u128,
    ) -> Self {
        let passed = match mode {
            "model" => runs > 0 && (passes as f64 / runs as f64) > MODEL_MAJORITY,
            _ => runs > 0 && passes == runs,
        };
        let outcome = match (expected, passed) {
            (Expected::Pass, true) => "pass",
            (Expected::Pass, false) => "fail",
            (Expected::XFail, false) => "xfail",
            (Expected::XFail, true) => "xpass",
            // A measurement reports; it does not judge. Its rate is in the
            // report either way, and the suite's colour does not depend on
            // which way the model happened to go this run.
            (Expected::Measure, _) => "measure",
        };
        Self {
            id: id.into(),
            mode,
            expected,
            passed,
            outcome,
            runs,
            passes,
            mean_ms,
            details,
            classes: Vec::new(),
            rate: None,
        }
    }

    /// A measured cell: an outcome distribution and a rate, with no
    /// verdict. `passed` is true so the suite's colour never depends on
    /// which way the model happened to go.
    #[allow(clippy::too_many_arguments)]
    fn measured(
        id: impl Into<String>,
        expected: Expected,
        runs: usize,
        passes: usize,
        classes: Vec<(String, usize)>,
        rate: f64,
        mean_ms: u128,
        details: Vec<String>,
    ) -> Self {
        Self {
            id: id.into(),
            mode: "credential",
            expected,
            passed: true,
            outcome: "measure",
            runs,
            passes,
            mean_ms,
            details,
            classes,
            rate: Some(rate),
        }
    }

    /// Passed some repetitions but not all — the interesting middle, where
    /// the framework is neither reliably right nor reliably wrong.
    fn flaky(&self) -> bool {
        self.passes > 0 && self.passes < self.runs
    }

    fn line(&self) -> String {
        let reps = if self.runs > 1 {
            format!(" {}/{}", self.passes, self.runs)
        } else {
            String::new()
        };
        let flaky = if self.flaky() { " ~flaky" } else { "" };
        let detail = match self.details.first() {
            Some(d) if self.expected == Expected::Pass && !self.passed => format!(" — {d}"),
            _ => String::new(),
        };
        format!(
            "[{:>5}] {}{reps} ({}ms){flaky}{detail}",
            self.outcome, self.id, self.mean_ms
        )
    }
}

struct Ctx {
    base: String,
    client: reqwest::Client,
    secrets: rustykrab_store::SecretStore,
    /// Path to the daemon's SQLite database, for asserting on the tables
    /// the plan specifies but no endpoint exposes.
    db_path: std::path::PathBuf,
    /// The daemon binary, used to drive CLI subcommands (`pair`).
    bin: String,
    /// The daemon's data dir — CLI subcommands must see the same store.
    data_dir: std::path::PathBuf,
    /// Owns the disposable daemon so scenarios can prove restart recovery
    /// against the same port and data directory.
    daemon: Arc<tokio::sync::Mutex<Option<Child>>>,
}

impl Ctx {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Count rows matching a query against the daemon's store. Returns 0
    /// when the table doesn't exist yet (Phase 2 tables), so a scenario
    /// fails on the assertion rather than on a SQL error.
    fn count(&self, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Result<i64> {
        let conn = rusqlite::Connection::open_with_flags(
            &self.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        match conn.query_row(sql, params, |row| row.get::<_, i64>(0)) {
            Ok(n) => Ok(n),
            Err(rusqlite::Error::SqliteFailure(_, Some(msg))) if msg.contains("no such table") => {
                Ok(0)
            }
            Err(e) if e.to_string().contains("no such table") => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    /// Persist machine-readable proof outside the throwaway daemon directory.
    fn write_evidence(&self, relative_path: &str, evidence: &Value) -> Result<PathBuf> {
        write_json_artifact(&artifact_dir(), relative_path, evidence)
    }

    /// Stop the live daemon, prove the port went dark, and boot a distinct
    /// process against the same durable state.
    async fn restart_daemon(&self) -> Result<(u32, u32)> {
        let mut daemon = self.daemon.lock().await;
        let previous = daemon
            .take()
            .ok_or_else(|| anyhow!("daemon is not running"))?;
        let previous_pid = previous.id();
        shutdown_daemon(previous).await;

        if self
            .client
            .get(self.url("/api/health"))
            .send()
            .await
            .is_ok()
        {
            bail!("daemon still answered after shutdown");
        }

        let mut replacement = spawn_daemon(&self.bin, &self.data_dir, self.port()?)?;
        let replacement_pid = replacement.id();
        if replacement_pid == previous_pid {
            bail!("replacement daemon reused pid {previous_pid}");
        }
        wait_for_health(&self.base, &self.client, &mut replacement).await?;
        *daemon = Some(replacement);
        Ok((previous_pid, replacement_pid))
    }

    fn port(&self) -> Result<u16> {
        self.base
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse().ok())
            .ok_or_else(|| anyhow!("invalid harness base URL: {}", self.base))
    }

    /// Run a daemon CLI subcommand with the harness environment.
    fn cli(&self, args: &[&str]) -> Result<std::process::Output> {
        Ok(Command::new(&self.bin)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env(
                "RUSTYKRAB_BROWSER_ISOLATED_ROOT",
                isolated_browser_root(&self.data_dir),
            )
            .env("RUSTYKRAB_DATA_DIR", &self.data_dir)
            .env("RUSTYKRAB_MASTER_KEY", MASTER_KEY_HEX)
            .env("RUSTYKRAB_AUTH_TOKEN", AUTH_TOKEN)
            .env("RUSTYKRAB_DISABLE_KEYCHAIN", "1")
            // Credentials must go to a secure store, and there isn't one here —
            // the line above saw to that. Without this the harness cannot
            // exercise fulfil at all; with it, nothing touches the developer's
            // real keychain.
            .env("RUSTYKRAB_CREDENTIAL_BACKEND", "memory")
            .env("NOTION_API_TOKEN", "e2e-dummy-notion")
            .env("OBSIDIAN_API_KEY", "e2e-dummy-obsidian")
            .output()?)
    }

    /// A request authenticated with an arbitrary token (device tokens).
    async fn get_with_token(&self, path: &str, token: &str) -> Result<reqwest::Response> {
        Ok(self
            .client
            .get(self.url(path))
            .bearer_auth(token)
            .send()
            .await?)
    }

    /// A request that overrides the default loopback `Origin`, for
    /// exercising the allowlist.
    async fn get_with_origin(&self, path: &str, origin: &str) -> Result<reqwest::Response> {
        Ok(self
            .client
            .get(self.url(path))
            .bearer_auth(AUTH_TOKEN)
            .header(reqwest::header::ORIGIN, origin)
            .send()
            .await?)
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response> {
        Ok(self
            .client
            .get(self.url(path))
            .bearer_auth(AUTH_TOKEN)
            .send()
            .await?)
    }

    async fn post(&self, path: &str, body: Value) -> Result<reqwest::Response> {
        Ok(self
            .client
            .post(self.url(path))
            .bearer_auth(AUTH_TOKEN)
            .json(&body)
            .send()
            .await?)
    }

    async fn delete(&self, path: &str) -> Result<reqwest::Response> {
        Ok(self
            .client
            .delete(self.url(path))
            .bearer_auth(AUTH_TOKEN)
            .send()
            .await?)
    }

    /// Create a user-side secret via the REST API and verify it landed.
    async fn create_secret(&self, name: &str, value: &str) -> Result<()> {
        let resp = self
            .post("/api/secrets", json!({"name": name, "value": value}))
            .await?;
        if resp.status() != 204 {
            bail!("POST /api/secrets {name} returned {}", resp.status());
        }
        let stored = self.secrets.get(name).await?;
        if stored != value {
            bail!("store value for {name} is not what was just written");
        }
        Ok(())
    }

    async fn create_conversation(&self) -> Result<String> {
        let conv: Value = self
            .post("/api/conversations", json!({}))
            .await?
            .json()
            .await?;
        Ok(conv["id"]
            .as_str()
            .ok_or_else(|| anyhow!("conversation create returned no id: {conv}"))?
            .to_string())
    }

    /// Send one message to an existing conversation, returning the reply.
    async fn send(&self, conv_id: &str, content: &str) -> Result<Value> {
        let resp = self
            .post(
                &format!("/api/conversations/{conv_id}/messages"),
                json!({ "content": content }),
            )
            .await?;
        if resp.status() != 200 {
            bail!("send_message returned {}", resp.status());
        }
        Ok(resp.json().await?)
    }

    /// Create a conversation, send one message, return the assistant reply.
    async fn chat(&self, content: &str) -> Result<Value> {
        let conv: Value = self
            .post("/api/conversations", json!({}))
            .await?
            .json()
            .await?;
        let conv_id = conv["id"]
            .as_str()
            .ok_or_else(|| anyhow!("conversation create returned no id: {conv}"))?
            .to_string();
        let resp = self
            .post(
                &format!("/api/conversations/{conv_id}/messages"),
                json!({"content": content}),
            )
            .await?;
        if resp.status() != 200 {
            bail!("send_message returned {}", resp.status());
        }
        Ok(resp.json().await?)
    }
}

// ── scenarios ────────────────────────────────────────────────────────

async fn health(ctx: &Ctx) -> Result<()> {
    let resp = ctx.client.get(ctx.url("/api/health")).send().await?;
    if resp.status() != 200 {
        bail!("health returned {}", resp.status());
    }
    Ok(())
}

/// The gateway demands an `Origin` on every `/api` request and allows
/// only loopback plus a configured list. Apollo reaches the daemon by its
/// tailnet name, so without this it was rejected on everything except
/// `/api/health` — the gap this scenario now pins shut.
async fn origin_allowlist(ctx: &Ctx) -> Result<()> {
    // The harness boots the daemon with this origin allowed.
    let allowed = ctx
        .get_with_origin("/api/conversations", ALLOWED_ORIGIN)
        .await?;
    if allowed.status() != 200 {
        bail!(
            "configured origin {ALLOWED_ORIGIN} was rejected with {}",
            allowed.status()
        );
    }

    // Anything else still is not.
    let refused = ctx
        .get_with_origin("/api/conversations", "https://evil.example.com")
        .await?;
    if refused.status() != 403 {
        bail!("an unlisted origin returned {}, want 403", refused.status());
    }

    // A request with no Origin at all remains refused: that is what stops
    // a non-browser client from skipping the check entirely.
    let bare = ctx
        .client
        .get(ctx.url("/api/conversations"))
        .bearer_auth(AUTH_TOKEN)
        .header(reqwest::header::ORIGIN, "")
        .send()
        .await?;
    if bare.status() != 403 {
        bail!("an empty Origin returned {}, want 403", bare.status());
    }
    Ok(())
}

async fn auth_required(ctx: &Ctx) -> Result<()> {
    let resp = ctx.client.get(ctx.url("/api/conversations")).send().await?;
    if resp.status() != 401 {
        bail!(
            "unauthenticated request returned {}, want 401",
            resp.status()
        );
    }
    Ok(())
}

async fn conversations_crud(ctx: &Ctx) -> Result<()> {
    let conv: Value = ctx
        .post("/api/conversations", json!({"title": "e2e"}))
        .await?
        .json()
        .await?;
    let id = conv["id"].as_str().ok_or_else(|| anyhow!("no id"))?;
    if conv["title"] != "e2e" || conv["createdAt"].as_i64().is_none() {
        bail!("conversation shape mismatch: {conv}");
    }
    let list: Value = ctx.get("/api/conversations").await?.json().await?;
    let listed = list
        .as_array()
        .map(|a| a.iter().any(|c| c["id"] == conv["id"]))
        .unwrap_or(false);
    if !listed {
        bail!("created conversation missing from list");
    }
    let got = ctx.get(&format!("/api/conversations/{id}")).await?;
    if got.status() != 200 {
        bail!("get conversation returned {}", got.status());
    }
    let del = ctx.delete(&format!("/api/conversations/{id}")).await?;
    if del.status() != 204 {
        bail!("delete conversation returned {}", del.status());
    }
    let gone = ctx.get(&format!("/api/conversations/{id}")).await?;
    if gone.status() != 404 {
        bail!("deleted conversation still returns {}", gone.status());
    }
    Ok(())
}

async fn chat_scripted_default(ctx: &Ctx) -> Result<()> {
    let msg = ctx.chat("hello from the e2e harness").await?;
    if msg["role"] != "assistant" {
        bail!("reply role: {msg}");
    }
    if msg["content"] != "Done." {
        bail!("expected scripted default text, got: {}", msg["content"]);
    }
    Ok(())
}

/// What a consumed SSE stream contained.
struct StreamOutcome {
    saw_text: bool,
    /// Names from `tool_start` frames, in order.
    tools: Vec<String>,
    /// The message from the terminal `done` frame.
    done: Option<Value>,
}

/// Drive `POST …/messages/stream` and consume the SSE response, applying
/// the frame rules from docs/integrations/apollo.md: `text` deltas,
/// a terminal `done` carrying the full message, and tool/heartbeat frames
/// that clients may ignore.
async fn stream_message(ctx: &Ctx, content: &str) -> Result<StreamOutcome> {
    use tokio_stream::StreamExt;

    let conv: Value = ctx
        .post("/api/conversations", json!({}))
        .await?
        .json()
        .await?;
    let conv_id = conv["id"].as_str().ok_or_else(|| anyhow!("no id"))?;
    let resp = ctx
        .post(
            &format!("/api/conversations/{conv_id}/messages/stream"),
            json!({"content": content}),
        )
        .await?;
    if resp.status() != 200 {
        bail!("stream returned {}", resp.status());
    }

    let mut outcome = StreamOutcome {
        saw_text: false,
        tools: Vec::new(),
        done: None,
    };
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk?));
        // SSE frames are separated by a blank line.
        while let Some(pos) = buf.find("\n\n") {
            let frame: String = buf.drain(..pos + 2).collect();
            let mut event = "";
            let mut data = "";
            for line in frame.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    event = v.trim();
                } else if let Some(v) = line.strip_prefix("data:") {
                    data = v.trim();
                }
            }
            match event {
                "text" => outcome.saw_text = true,
                "tool_start" => {
                    if let Ok(p) = serde_json::from_str::<Value>(data) {
                        if let Some(name) = p["delta"].as_str() {
                            outcome.tools.push(name.to_string());
                        }
                    }
                }
                "error" => bail!("stream reported an error frame: {data}"),
                "done" => {
                    let payload: Value = serde_json::from_str(data)
                        .with_context(|| format!("bad done payload: {data}"))?;
                    // Progress frames are a bare {"type":"done"}; the
                    // terminal frame carries the full message.
                    if payload.get("message").is_some() {
                        outcome.done = Some(payload["message"].clone());
                    }
                }
                _ => {} // thinking, heartbeats — safe no-ops
            }
        }
        if outcome.done.is_some() {
            break;
        }
    }
    Ok(outcome)
}

/// SSE contract smoke: the stream carries incremental `text` events and a
/// terminal `done` event with the full message (docs/integrations/apollo.md).
async fn chat_sse_stream(ctx: &Ctx) -> Result<()> {
    let outcome = stream_message(ctx, "hello stream").await?;
    let done = outcome
        .done
        .ok_or_else(|| anyhow!("stream ended without a done event"))?;
    if !outcome.saw_text {
        bail!("no text events before done");
    }
    if done["content"] != "Done." || done["role"] != "assistant" {
        bail!("done message mismatch: {done}");
    }
    Ok(())
}

/// Tool calls must work over the **streaming** route too — that is the
/// path Apollo actually uses for chat, and it goes through
/// `chat_stream`, not `chat`. Asserts the tool lifecycle frames appear
/// and that the tool's side effect really landed in the store.
async fn chat_sse_stream_with_tools(ctx: &Ctx) -> Result<()> {
    let outcome = stream_message(ctx, "e2e: streamed credential").await?;
    let done = outcome
        .done
        .ok_or_else(|| anyhow!("stream ended without a done event"))?;
    if !outcome.tools.iter().any(|t| t == "credential_write") {
        bail!("no credential_write tool_start frame: {:?}", outcome.tools);
    }
    let text = done["content"].as_str().unwrap_or_default();
    if !text.contains("e2e_streamed_token") {
        bail!("unexpected final streamed message: {done}");
    }
    let stored = ctx.secrets.get("e2e_streamed_token").await?;
    if stored != "streamed-v1" {
        bail!("streamed tool call did not persist the credential");
    }
    Ok(())
}

async fn secrets_create_and_delete(ctx: &Ctx) -> Result<()> {
    ctx.create_secret("e2e_lifecycle_token", "v1").await?;
    let list: Value = ctx.get("/api/secrets").await?.json().await?;
    let names = list["names"].as_array().cloned().unwrap_or_default();
    if !names.iter().any(|n| n == "e2e_lifecycle_token") {
        bail!("created secret missing from list: {list}");
    }
    let del = ctx.delete("/api/secrets/e2e_lifecycle_token").await?;
    if del.status() != 204 {
        bail!("delete secret returned {}", del.status());
    }
    if ctx.secrets.get("e2e_lifecycle_token").await.is_ok() {
        bail!("secret still present in store after delete");
    }
    Ok(())
}

/// Plan §F3 scenario 3: the scripted agent creates a **new** credential —
/// allowed silently under the create-only policy, and already true today.
async fn agent_creates_new_credential(ctx: &Ctx) -> Result<()> {
    let msg = ctx.chat("e2e: create credential").await?;
    let text = msg["content"].as_str().unwrap_or_default();
    if !text.contains("e2e_scripted_token") {
        bail!("unexpected final reply: {msg}");
    }
    let stored = ctx.secrets.get("e2e_scripted_token").await?;
    if stored != "scripted-v1" {
        bail!("scripted credential has wrong value");
    }
    Ok(())
}

/// Mint a pairing code on the Mac side and exchange it for a device
/// identity — the first half of plan §F3 scenario 1. Returns the device
/// id and its one-time token.
async fn pair_a_device(ctx: &Ctx, name: &str) -> Result<(String, String)> {
    // `rustykrab pair` prints the code (and a QR payload) — the plan's
    // Mac-side mint step.
    let out = ctx.cli(&["pair"])?;
    if !out.status.success() {
        bail!(
            "`rustykrab-cli pair` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Accept either the bare 8-char code or the QR JSON payload.
    let code = serde_json::from_str::<Value>(stdout.trim())
        .ok()
        .and_then(|v| v["code"].as_str().map(str::to_string))
        .or_else(|| {
            stdout
                .split_whitespace()
                .find(|t| t.len() == 8 && t.chars().all(|c| c.is_ascii_alphanumeric()))
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow!("no pairing code in `pair` output: {}", stdout.trim()))?;

    let resp = ctx
        .client
        .post(ctx.url("/api/pair"))
        .json(&json!({"code": code, "deviceName": name}))
        .send()
        .await?;
    if resp.status() != 200 {
        bail!("POST /api/pair returned {}", resp.status());
    }
    let body: Value = resp.json().await?;
    let device_id = body["deviceId"]
        .as_str()
        .ok_or_else(|| anyhow!("pair response has no deviceId: {body}"))?
        .to_string();
    let token = body["deviceToken"]
        .as_str()
        .ok_or_else(|| anyhow!("pair response has no deviceToken: {body}"))?
        .to_string();
    Ok((device_id, token))
}

/// Plan §F3 scenario 1 (Phase 2): mint a code, exchange it, and call the
/// API with the resulting **device token** — the token must be accepted
/// anywhere the master token is. Also checks the code is single-use and
/// that the device shows up in the management listing.
async fn pair_device_target(ctx: &Ctx) -> Result<()> {
    let (device_id, token) = pair_a_device(ctx, "e2e-phone").await?;

    // The device token authenticates like the master token.
    let resp = ctx.get_with_token("/api/conversations", &token).await?;
    if resp.status() != 200 {
        bail!(
            "device token rejected on /api/conversations: {}",
            resp.status()
        );
    }

    // The paired device is listed and attributable.
    let devices: Vec<Value> = ctx.get("/api/devices").await?.json().await?;
    let listed = devices
        .iter()
        .find(|d| d["id"].as_str() == Some(device_id.as_str()))
        .ok_or_else(|| anyhow!("paired device missing from /api/devices"))?;
    if listed["name"] != "e2e-phone" {
        bail!("device name not recorded: {listed}");
    }
    // Tokens are stored hashed — never echoed back by the listing.
    if listed.get("token").is_some() || listed.get("deviceToken").is_some() {
        bail!("device listing leaks the token");
    }
    Ok(())
}

/// Plan §F3 scenario 2 (Phase 2): POST /api/secrets is create-only — an
/// existing name without `overwrite: true` is refused with 409 and the
/// stored value is untouched.
async fn secrets_create_only_409(ctx: &Ctx) -> Result<()> {
    ctx.create_secret("e2e_createonly_token", "original")
        .await?;
    let resp = ctx
        .post(
            "/api/secrets",
            json!({"name": "e2e_createonly_token", "value": "clobbered"}),
        )
        .await?;
    if resp.status() != 409 {
        bail!("repeat POST returned {}, want 409", resp.status());
    }
    let stored = ctx.secrets.get("e2e_createonly_token").await?;
    if stored != "original" {
        bail!("value changed despite create-only default");
    }
    Ok(())
}

/// Plan §F3 scenario 2 (Phase 2): `overwrite: true` applies the change and
/// the secrets listing exposes per-entry metadata with a bumped version
/// (the superseded value is archived server-side).
async fn secrets_overwrite_archives(ctx: &Ctx) -> Result<()> {
    ctx.create_secret("e2e_overwrite_token", "v1").await?;
    let resp = ctx
        .post(
            "/api/secrets",
            json!({"name": "e2e_overwrite_token", "value": "v2", "overwrite": true}),
        )
        .await?;
    if resp.status() != 204 {
        bail!("overwrite POST returned {}", resp.status());
    }
    let stored = ctx.secrets.get("e2e_overwrite_token").await?;
    if stored != "v2" {
        bail!("overwrite did not apply");
    }
    let list: Value = ctx.get("/api/secrets").await?.json().await?;
    let entry = list["secrets"]
        .as_array()
        .and_then(|a| a.iter().find(|e| e["name"] == "e2e_overwrite_token"))
        .cloned()
        .ok_or_else(|| anyhow!("metadata listing missing entry: {list}"))?;
    if entry["version"].as_i64() != Some(2) {
        bail!("expected version 2 after overwrite, got: {entry}");
    }
    // The superseded value is archived, not dropped (plan §A1).
    let archived = ctx.count(
        "SELECT COUNT(*) FROM secret_versions WHERE name = ?1",
        &[&"e2e_overwrite_token"],
    )?;
    if archived < 1 {
        bail!("no secret_versions row archiving the superseded value");
    }
    // …and the write is audited.
    let audited = ctx.count(
        "SELECT COUNT(*) FROM secret_audit WHERE name = ?1 AND op = 'overwrite'",
        &[&"e2e_overwrite_token"],
    )?;
    if audited < 1 {
        bail!("no secret_audit row for the overwrite");
    }
    Ok(())
}

/// Plan §F3 scenario 4 (Phase 2): an agent `set` on an existing name
/// leaves the value unchanged and files exactly one pending request.
async fn agent_overwrite_files_request(ctx: &Ctx) -> Result<()> {
    ctx.create_secret("e2e_guard_token", "original").await?;
    ctx.chat("e2e: overwrite credential").await?;
    let stored = ctx.secrets.get("e2e_guard_token").await?;
    if stored != "original" {
        bail!("agent overwrote an existing credential (value is now the proposed one)");
    }
    let pending = pending_requests(ctx).await?;
    let matching: Vec<&Value> = pending
        .iter()
        .filter(|r| r["name"] == "e2e_guard_token" && r["action"] == "update")
        .collect();
    if matching.len() != 1 {
        bail!(
            "want exactly one pending update request, got {}",
            matching.len()
        );
    }
    if matching[0].get("proposedValue").is_some() || matching[0].get("value").is_some() {
        bail!("pending request leaks the proposed value");
    }
    // Plan §A1: the proposal is encrypted at rest — a pending request is
    // never a plaintext copy of the credential.
    let plaintext = ctx.count(
        "SELECT COUNT(*) FROM credential_requests \
         WHERE name = ?1 AND CAST(proposed_data AS TEXT) LIKE '%hijacked%'",
        &[&"e2e_guard_token"],
    )?;
    if plaintext != 0 {
        bail!("proposed_data holds the proposed value in plaintext");
    }
    Ok(())
}

/// Plan §F3 scenario 5 (Phase 2): approving a pending request applies the
/// stored change with User authority.
async fn approve_applies_change(ctx: &Ctx) -> Result<()> {
    // Reuses the request filed by the overwrite scenario; refile if absent.
    let mut pending = pending_for(ctx, "e2e_guard_token").await?;
    if pending.is_none() {
        ctx.chat("e2e: overwrite credential").await?;
        pending = pending_for(ctx, "e2e_guard_token").await?;
    }
    let req = pending.ok_or_else(|| anyhow!("no pending request to approve"))?;
    let id = req["id"]
        .as_str()
        .ok_or_else(|| anyhow!("request has no id"))?;
    let resp = ctx
        .post(&format!("/api/credential-requests/{id}/approve"), json!({}))
        .await?;
    if resp.status() != 204 {
        bail!("approve returned {}", resp.status());
    }
    let stored = ctx.secrets.get("e2e_guard_token").await?;
    if stored != "hijacked" {
        bail!("approved change was not applied");
    }
    // Plan criterion 5 in full: the superseded value is archived and the
    // approval is audited against the request.
    let archived = ctx.count(
        "SELECT COUNT(*) FROM secret_versions WHERE name = ?1",
        &[&"e2e_guard_token"],
    )?;
    if archived < 1 {
        bail!("approved overwrite archived no previous version");
    }
    let audited = ctx.count(
        "SELECT COUNT(*) FROM secret_audit WHERE request_id = ?1 AND op = 'approve'",
        &[&id],
    )?;
    if audited < 1 {
        bail!("no secret_audit row attributing the approval to request {id}");
    }
    // The decided request is no longer pending.
    if pending_for(ctx, "e2e_guard_token").await?.is_some() {
        bail!("approved request is still listed as pending");
    }
    Ok(())
}

/// Plan §F3 scenario 6 (Phase 2): denying a pending request leaves the
/// value unchanged.
async fn deny_preserves_value(ctx: &Ctx) -> Result<()> {
    // The scripted overwrite scenario targets e2e_guard_token, so reset
    // that name to a known value and refile a request against it.
    let before = "deny-keeps-this";
    ctx.delete("/api/secrets/e2e_guard_token").await?;
    ctx.create_secret("e2e_guard_token", before).await?;
    ctx.chat("e2e: overwrite credential").await?;
    let req = pending_for(ctx, "e2e_guard_token")
        .await?
        .ok_or_else(|| anyhow!("no pending request to deny"))?;
    let id = req["id"]
        .as_str()
        .ok_or_else(|| anyhow!("request has no id"))?;
    let resp = ctx
        .post(&format!("/api/credential-requests/{id}/deny"), json!({}))
        .await?;
    if resp.status() != 204 {
        bail!("deny returned {}", resp.status());
    }
    if pending_for(ctx, "e2e_guard_token").await?.is_some() {
        bail!("denied request still listed as pending");
    }
    // The point of the scenario: denying leaves the value untouched.
    let stored = ctx.secrets.get("e2e_guard_token").await?;
    if stored != before {
        bail!("value changed despite the request being denied");
    }
    // Plan criterion 6: the proposal is wiped on deny, not merely marked.
    let leftover = ctx.count(
        "SELECT COUNT(*) FROM credential_requests \
         WHERE id = ?1 AND proposed_data IS NOT NULL",
        &[&id],
    )?;
    if leftover != 0 {
        bail!("denied request still holds its proposed_data");
    }
    Ok(())
}

/// Plan §F3 scenario 7 (Phase 2): an agent `delete` always files a request
/// and the credential survives until approval. (Expiry sweep and the
/// stale-approval 409 are asserted in store/gateway tests where the clock
/// can be manipulated.)
async fn agent_delete_files_request(ctx: &Ctx) -> Result<()> {
    ctx.create_secret("e2e_delete_token", "still-here").await?;
    ctx.chat("e2e: delete credential").await?;
    let stored = ctx.secrets.get("e2e_delete_token").await;
    if stored.is_err() {
        bail!("agent deleted an existing credential outright");
    }
    let req = pending_for(ctx, "e2e_delete_token").await?;
    match req {
        Some(r) if r["action"] == "delete" => Ok(()),
        Some(r) => bail!("pending request has wrong action: {r}"),
        None => bail!("no pending delete request filed"),
    }
}

/// Plan §F3 scenario 8 (Phase 2): the lost-phone story — pair a device,
/// confirm its token works, revoke it, and confirm the same token is
/// then rejected with 401.
async fn revoked_device_401(ctx: &Ctx) -> Result<()> {
    let (device_id, token) = pair_a_device(ctx, "e2e-lost-phone").await?;

    let before = ctx.get_with_token("/api/conversations", &token).await?;
    if before.status() != 200 {
        bail!("fresh device token rejected: {}", before.status());
    }

    let revoke = ctx.delete(&format!("/api/devices/{device_id}")).await?;
    if revoke.status() != 204 {
        bail!("revoking the device returned {}", revoke.status());
    }

    let after = ctx.get_with_token("/api/conversations", &token).await?;
    if after.status() != 401 {
        bail!("revoked device token returned {}, want 401", after.status());
    }
    Ok(())
}

async fn pending_requests(ctx: &Ctx) -> Result<Vec<Value>> {
    let resp = ctx.get("/api/credential-requests?status=pending").await?;
    if resp.status() != 200 {
        bail!("GET /api/credential-requests returned {}", resp.status());
    }
    Ok(resp.json().await?)
}

async fn pending_for(ctx: &Ctx, name: &str) -> Result<Option<Value>> {
    Ok(pending_requests(ctx)
        .await?
        .into_iter()
        .find(|r| r["name"] == name))
}

// ── daemon lifecycle ─────────────────────────────────────────────────

fn pick_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// Which provider the daemon under test should run.
pub enum Backend<'a> {
    /// Replay a fixed script — no model, no network, deterministic.
    Scripted,
    /// A real model via Ollama, with the tool registry replaced by stubs
    /// the scenario controls.
    Model {
        model: &'a str,
        ollama_url: &'a str,
        /// Context window for the provider. `None` leaves the daemon's
        /// default; the compaction scenarios lower it, because that is
        /// what `effective_context_limit()` actually reads.
        num_ctx: Option<u32>,
        /// Extra environment for this daemon, applied last so a case can
        /// override anything the harness sets by default.
        extra_env: &'a [(String, String)],
        /// Tools to seed into the active set. Registering a tool is not
        /// enough for the model to see it — schemas are only sent for
        /// active tools — so a scenario has to name what it is testing or
        /// it measures tool discovery instead.
        active_tools: &'a [&'a str],
        tool_stubs: &'a str,
        /// Which channel to wire up, and the base URL of the capture
        /// server standing in for its API. `None` drives the gateway
        /// directly, which needs no channel configuration.
        channel: Option<(crate::surface::Surface, &'a str)>,
    },
}

fn spawn_daemon(bin: &str, data_dir: &std::path::Path, port: u16) -> Result<Child> {
    spawn_daemon_with(bin, data_dir, port, &Backend::Scripted)
}

/// The budget the daemon picks for Ollama when nothing overrides it
/// (`resolve_max_context_tokens`). A scenario asking for less than this is
/// shrinking the window on purpose.
const OLLAMA_DEFAULT_CONTEXT_BUDGET: usize = 32_000;

/// Read `max_context_tokens` back out of the `harness.toml` a scenario
/// wrote, if it set one.
fn harness_context_budget(data_dir: &std::path::Path) -> Option<usize> {
    let toml = std::fs::read_to_string(data_dir.join("harness.toml")).ok()?;
    toml.lines()
        .find_map(|line| line.trim().strip_prefix("max_context_tokens"))
        .and_then(|rest| {
            rest.trim()
                .strip_prefix('=')
                .map(str::trim)
                .map(str::to_string)
        })
        .and_then(|v| v.parse().ok())
}

fn spawn_daemon_with(
    bin: &str,
    data_dir: &std::path::Path,
    port: u16,
    backend: &Backend<'_>,
) -> Result<Child> {
    let log = std::fs::File::create(data_dir.join("daemon.log"))?;
    let mut command = Command::new(bin);
    let command = &mut command;
    let command = command
        // Start from an empty environment: the developer's shell may hold
        // real credentials (which would be written into this throwaway
        // store) and RUSTYKRAB_* settings that would change behaviour and
        // break determinism. PATH and HOME are carried over: Chrome
        // needs a real HOME (see `isolated_browser_root`), so the browser
        // is isolated by its own root instead.
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env(
            "RUSTYKRAB_BROWSER_ISOLATED_ROOT",
            isolated_browser_root(data_dir),
        )
        .env("RUSTYKRAB_DATA_DIR", data_dir)
        .env("RUSTYKRAB_PORT", port.to_string())
        .env("RUSTYKRAB_MASTER_KEY", MASTER_KEY_HEX)
        .env("RUSTYKRAB_AUTH_TOKEN", AUTH_TOKEN)
        // Never touch the host's real Keychain from a throwaway boot.
        .env("RUSTYKRAB_DISABLE_KEYCHAIN", "1")
        // Credentials must go to a secure store, and there isn't one here —
        // the line above saw to that. Without this the harness cannot
        // exercise fulfil at all; with it, nothing touches the developer's
        // real keychain.
        .env("RUSTYKRAB_CREDENTIAL_BACKEND", "memory")
        // Dummy values for the registry's startup-required secrets.
        .env("NOTION_API_TOKEN", "e2e-dummy-notion")
        .env("OBSIDIAN_API_KEY", "e2e-dummy-obsidian")
        .env("RUSTYKRAB_LOG_STDOUT", "1")
        // The spawn clears the environment, which also clears RUST_LOG, so
        // a scenario that misbehaves leaves a log with nothing in it to
        // explain why. E2E_DAEMON_LOG turns the detail back on.
        .env(
            "RUST_LOG",
            // The browser tool's diagnostics -- fill digests, tab timings --
            // are `debug`, deliberately: a four-byte hash prefix of a secret
            // is a partial fingerprint and does not belong in a production
            // log by default. The harness wants them, so it asks for that
            // one target rather than raising the level everywhere.
            std::env::var("E2E_DAEMON_LOG")
                .unwrap_or_else(|_| "info,rustykrab_tools::browser=debug".to_string()),
        )
        // The suite drives far more than the shipping 20 req/min from one
        // IP; raise the limit for this throwaway boot only.
        .env("RUSTYKRAB_RATE_LIMIT_MAX", "100000")
        .env("RUSTYKRAB_RATE_LIMIT_LOCKOUT_SECS", "1")
        .env("RUSTYKRAB_ALLOWED_ORIGINS", ALLOWED_ORIGIN)
        // Share one embedding-model download across every throwaway boot.
        // Without this the model suite, which boots a daemon per
        // repetition, re-fetches hundreds of megabytes into each new
        // temporary data dir.
        .env("RUSTYKRAB_MODEL_CACHE_DIR", shared_model_cache());

    match backend {
        Backend::Scripted => {
            let script_path = data_dir.join("e2e-script.json");
            std::fs::write(&script_path, AGENT_SCRIPT)?;
            command
                .env("RUSTYKRAB_PROVIDER", "scripted")
                .env("RUSTYKRAB_SCRIPT_PATH", &script_path);
        }
        Backend::Model {
            model,
            ollama_url,
            num_ctx,
            active_tools,
            tool_stubs,
            channel,
            extra_env,
        } => {
            command
                .env("RUSTYKRAB_PROVIDER", "ollama")
                // Scenarios set their own iteration caps, retry budget and
                // context window; the auto-router would replace the whole
                // profile with a preset and discard them.
                .env("RUSTYKRAB_HARNESS_ROUTER", "off")
                .env("OLLAMA_MODEL", model)
                .env("OLLAMA_BASE_URL", ollama_url)
                // Compaction summarisation on a local model is
                // prefill-heavy; the default would cut it off.
                .env("OLLAMA_TIMEOUT_SECS", "900")
                // One configured profile for every message. Routing
                // classifies per message and swaps presets, which makes a
                // scenario's iteration cap and retry budget depend on how
                // its prompt happens to read. (From #532.)
                .env("RUSTYKRAB_HARNESS_ROUTER", "off");
            if let Some(n) = num_ctx {
                command.env("RUSTYKRAB_NUM_CTX", n.to_string());
            }

            // `max_context_tokens` from `harness.toml` is overwritten at
            // startup by a provider-derived default (32k for Ollama), so a
            // scenario that shrinks the window to force compaction never
            // gets it. `RUSTYKRAB_MAX_CONTEXT_TOKENS` is the documented
            // override that survives; carry the profile's value through it
            // so the toml stays the single source of truth.
            // Only when the scenario is deliberately shrinking the window.
            // The ordinary profiles set a budget far above Ollama's own
            // default, and pinning `num_ctx` that high would allocate a KV
            // cache to match for every scenario.
            if let Some(budget) =
                harness_context_budget(data_dir).filter(|&b| b < OLLAMA_DEFAULT_CONTEXT_BUDGET)
            {
                command.env("RUSTYKRAB_MAX_CONTEXT_TOKENS", budget.to_string());
                // The runner sizes the compaction threshold off the
                // *provider's* reported window in preference to the
                // profile's budget, so leaving Ollama at its default 64k
                // puts the trigger ~55k out of reach and compaction never
                // fires however small the profile says the window is.
                // Shrink the provider window to match.
                command.env("RUSTYKRAB_NUM_CTX", budget.to_string());
            }

            // An empty spec means "leave the registry alone" — the
            // credential suite needs the real tools, because its premise
            // is that they cannot run without a credential. Writing an
            // empty file and pointing the daemon at it would instead fail
            // its JSON parse and stop the daemon from booting at all.
            if !tool_stubs.trim().is_empty() {
                let stub_path = data_dir.join("tool-stubs.json");
                std::fs::write(&stub_path, tool_stubs)?;
                command.env("RUSTYKRAB_TOOL_STUBS", &stub_path);
            }

            // Registering a tool is not enough to make the model aware of
            // it: schemas are only sent for tools in the active set, and a
            // scenario cannot rely on the model calling `tools_load` to
            // discover the one thing it is being tested on. Name the stubs
            // this scenario declares, plus anything it asked for outright.
            let mut active: Vec<String> = active_tools.iter().map(|n| (*n).to_string()).collect();
            if let Some(names) = stub_tool_names(tool_stubs) {
                active.push(names);
            }
            if !active.is_empty() {
                command.env("RUSTYKRAB_ACTIVE_TOOLS", active.join(","));
            }

            // Channel wiring points the daemon's outbound calls at the
            // capture server, so a trial can read what the bot would have
            // sent without any network egress.
            if let Some((surface, capture_base)) = channel {
                crate::surface::configure_channel(command, *surface, capture_base);
            }

            // Last, so a case can override any default above.
            for (key, value) in *extra_env {
                command.env(key, value);
            }
        }
    }

    let child = command
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .with_context(|| format!("failed to spawn daemon binary {bin}"))?;
    Ok(child)
}

/// The tool names a stub file declares, comma separated.
fn stub_tool_names(tool_stubs: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(tool_stubs).ok()?;
    let names: Vec<&str> = parsed["tools"]
        .as_array()?
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    if names.is_empty() {
        return None;
    }
    Some(names.join(","))
}

/// A stable cache for the embedding model, outside the throwaway data
/// dirs. Without it every scenario re-downloads the ONNX weights.
fn shared_model_cache() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("rustykrab-e2e-models");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// A `HOME` for a spawned daemon: the trial's own data dir, never the
/// operator's.
///
/// Two things follow from `HOME`, and both must be per-trial.
///
/// The browser tool symlinks the *real* Chrome profile
/// (`$HOME/Library/Application Support/Google/Chrome/<Profile>`) into the
/// user-data dir it launches with, so the agent inherits the operator's
/// cookies and sessions. That is deliberate and correct in production --
/// it is what gets an agent past bot protection -- and it is exactly
/// wrong here. A suite that inherits it can sign into a live provider as
/// the operator, and it cannot measure anything: the first trial that
/// logs in leaves a cookie every later trial rides, which scores as
/// `SucceededWithoutAsking` and looks like a finding rather than
/// leakage.
///
/// It also decides where `resolve_user_data_dir` puts the profile
/// (`$HOME/.rustykrab/browser/...`), so pointing `HOME` at the trial dir
/// isolates trials from each other as well.
///
/// Deliberately not a production switch. The linking behaviour is wanted
/// everywhere except a test, and a test's needs do not belong in the
/// product's configuration.
/// Kill any browser launched with a user-data-dir under `data_dir`.
///
/// The daemon spawns Chrome and never reaps it, so `shutdown_daemon`
/// leaves it running. Scoped to the trial's own path on purpose: a
/// pattern any broader would kill the operator's browser, and this runs
/// on a developer's machine.
/// Refuse to start when the model cannot answer.
///
/// The README has described this check for some time; it was never
/// implemented, and its absence costs exactly what the README says it
/// would. A wedged Ollama -- loaded, holding VRAM, answering `/api/tags`,
/// but serving nothing -- produced five consecutive `NeverAsked` trials
/// at 321s each. Every one looked like an agent that declined to ask for
/// a credential. None of them ever received a token.
///
/// One trivial generation distinguishes "the model is there" from "the
/// model is working", which `/api/tags` cannot. The budget is generous
/// because a cold load of a 26B model is tens of seconds; it is bounded
/// because the failure being caught is an unbounded hang.
async fn preflight_model(ollama_url: &str, model: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(PREFLIGHT_BUDGET_SECS))
        .build()?;

    let tags = format!("{}/api/tags", ollama_url.trim_end_matches('/'));
    client.get(&tags).send().await.with_context(|| {
        format!("Ollama is not reachable at {ollama_url}. Start it, or pass --ollama-url.")
    })?;

    let started = Instant::now();
    let body = serde_json::json!({ "model": model, "prompt": "say ok", "stream": false });
    let resp = client
        .post(format!("{}/api/generate", ollama_url.trim_end_matches('/')))
        .json(&body)
        .send()
        .await
        .with_context(|| {
            format!(
                "'{model}' did not answer a one-word prompt within {PREFLIGHT_BUDGET_SECS}s. \
                 Ollama serves one request at a time per model, so another client holding \
                 the slot -- a running RustyKrab daemon is the usual culprit -- stops this \
                 suite rather than slowing it. A wedged server behaves the same way and \
                 needs a restart. Pull the model with `ollama pull {model}` if it is absent."
            )
        })?;
    if !resp.status().is_success() {
        bail!(
            "Ollama answered {} for a trivial generation with '{model}'",
            resp.status()
        );
    }
    let elapsed = started.elapsed();
    eprintln!(
        "preflight: {model} answered in {:.1}s",
        elapsed.as_secs_f64()
    );

    // A one-word prompt that takes this long is not a fast model having a
    // slow day. Ollama serves one request at a time per model, so the
    // usual cause is something else holding the slot -- a running
    // RustyKrab daemon is the usual culprit -- and the run that follows
    // will produce trials that look like agent failures and are not.
    //
    // This is a warning rather than an error because a cold load of a
    // large model legitimately takes tens of seconds. It is loud because
    // the timing was already printed, and was read past.
    if elapsed > Duration::from_secs(PREFLIGHT_SLOW_SECS) {
        eprintln!(
            "preflight: WARNING — {:.0}s for a one-word prompt suggests something \
             else is holding {model}'s slot (a running RustyKrab daemon is the \
             usual culprit). Trials will time out and look like agent failures. \
             Stop it, or expect to throw this run away.",
            elapsed.as_secs_f64()
        );
    }
    Ok(())
}

fn kill_browser_for(data_dir: &std::path::Path) {
    #[cfg(unix)]
    {
        let pattern = format!("user-data-dir={}", data_dir.display());
        let _ = std::process::Command::new("pkill")
            .args(["-f", &pattern])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// A browser root of this trial's own, so no trial inherits another's
/// cookies or borrows the developer's signed-in Chrome profile.
///
/// This used to be done by pointing HOME at the trial directory, which
/// isolated the browser and broke it: Chrome wedges its renderer when
/// HOME is an empty directory -- the page commits its URL and the
/// document never arrives. Every browser call then burns its full
/// timeout, which reads as a slow site or a hung agent and is neither.
/// Isolate the browser root; leave HOME alone.
fn isolated_browser_root(data_dir: &std::path::Path) -> std::ffi::OsString {
    let root = data_dir.join("browser");
    let _ = std::fs::create_dir_all(&root);
    root.into_os_string()
}

const PREFLIGHT_BUDGET_SECS: u64 = 120;

/// Above this, a trivial generation means contention, not model speed.
const PREFLIGHT_SLOW_SECS: u64 = 20;

async fn wait_for_health(base: &str, client: &reqwest::Client, child: &mut Child) -> Result<()> {
    for _ in 0..240 {
        if let Some(status) = child.try_wait()? {
            bail!("daemon exited during startup: {status}");
        }
        if let Ok(resp) = client.get(format!("{base}/api/health")).send().await {
            if resp.status() == 200 {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("daemon did not become healthy within 120s");
}

// ── main ─────────────────────────────────────────────────────────────

pub(crate) type ScenarioFn =
    for<'a> fn(&'a Ctx) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>>;

macro_rules! scenario {
    ($f:ident) => {{
        let f: ScenarioFn = |ctx| Box::pin($f(ctx));
        (stringify!($f), f)
    }};
}

const USAGE: &str = "\
rustykrab-e2e — end-to-end evaluation harness

USAGE:
    cargo run -p rustykrab-e2e -- [FLAGS]

FLAGS:
    --mode SUITE                scripted | model | credential | login |
                                ablation | all (default: scripted). `login`
                                reaches the real internet with real
                                credentials and is never included in `all`;
                                it skips unless RK_LOGIN_URL/USER/PASS are set.
    --ctx-list A,B,C            Windows for --mode ablation (default:
                                4096,8192,16384,32768,65536,131072,262144)
    --surfaces LIST             Surfaces for --mode credential
                                (default: gateway,telegram; signal has no
                                agent loop reading it and will error)
    --trials N                  Trials per credential cell (default: 5)
    --resume                    Reuse trials already in the sidecar rather
                                than paying for them twice
    --reps N                    Repetitions per model scenario (default: 3)
    --case SUBSTRING            Only scenarios whose id contains SUBSTRING
    --quick                     Skip scenarios tagged slow
    --model TAG                 Ollama model for --mode model (default: gemma4:26b)
    --ollama-url URL            Ollama base URL (default: http://localhost:11434)
    --list                      List scenarios and exit
    -h, --help                  Show this message

ENVIRONMENT:
    RUSTYKRAB_BIN       Daemon binary (default: target/debug/rustykrab-cli)
    ANTHROPIC_API_KEY   Grades model-mode rubrics with claude-sonnet-5.
                        Without it the model under test grades itself, and
                        the report says so.
    E2E_KEEP_TMP        Keep the throwaway data dir for post-mortems (logs
                        only; the Chrome profile is shed to save disk).
";

struct Args {
    mode: String,
    reps: usize,
    trials: usize,
    /// Ceiling on one trial. See `login_suite::DEFAULT_TRIAL_TIMEOUT`.
    trial_timeout: Duration,
    surfaces: Vec<surface::Surface>,
    resume: bool,
    case_filter: Option<String>,
    quick: bool,
    model: String,
    ollama_url: String,
    ctx_list: Vec<u32>,
    list: bool,
}

fn parse_args(argv: &[String]) -> std::result::Result<Args, String> {
    let mut args = Args {
        mode: "scripted".to_string(),
        reps: 3,
        trials: 5,
        trial_timeout: login_suite::DEFAULT_TRIAL_TIMEOUT,
        resume: false,
        // Signal is omitted on purpose: nothing in the daemon reads its
        // inbound queue, so every trial would time out. Requesting it
        // explicitly gets a clear error rather than a silent hour.
        surfaces: vec![surface::Surface::Gateway, surface::Surface::Telegram],
        case_filter: None,
        quick: false,
        model: std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "gemma4:26b".to_string()),
        ollama_url: std::env::var("OLLAMA_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:11434".to_string()),
        ctx_list: vec![4_096, 8_192, 16_384, 32_768, 65_536, 131_072, 262_144],
        list: false,
    };
    let value = |i: usize, name: &str| -> std::result::Result<String, String> {
        argv.get(i + 1)
            .cloned()
            .ok_or_else(|| format!("{name} requires a value"))
    };
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--list" => args.list = true,
            "--quick" => args.quick = true,
            "--resume" => args.resume = true,
            "-h" | "--help" => return Err(String::new()),
            "--mode" => {
                args.mode = value(i, "--mode")?.to_lowercase();
                if !matches!(
                    args.mode.as_str(),
                    "scripted" | "model" | "credential" | "login" | "ablation" | "all"
                ) {
                    return Err(format!(
                        "--mode: expected scripted|model|credential|login|ablation|all, got {}",
                        args.mode
                    ));
                }
                i += 1;
            }
            "--reps" => {
                let v = value(i, "--reps")?;
                args.reps = v
                    .parse()
                    .map_err(|_| format!("--reps: not a number: {v}"))?;
                if args.reps == 0 {
                    return Err("--reps must be at least 1".to_string());
                }
                i += 1;
            }
            "--case" => {
                args.case_filter = Some(value(i, "--case")?);
                i += 1;
            }
            "--trial-timeout" => {
                let v = value(i, "--trial-timeout")?;
                let secs: u64 = v
                    .parse()
                    .map_err(|_| format!("--trial-timeout: not a number of seconds: {v}"))?;
                if secs == 0 {
                    return Err("--trial-timeout must be at least 1 second".to_string());
                }
                args.trial_timeout = Duration::from_secs(secs);
                i += 2;
            }
            "--trials" => {
                let v = value(i, "--trials")?;
                args.trials = v
                    .parse()
                    .map_err(|_| format!("--trials: not a number: {v}"))?;
                if args.trials == 0 {
                    return Err("--trials must be at least 1".to_string());
                }
                i += 1;
            }
            "--surfaces" => {
                let v = value(i, "--surfaces")?;
                args.surfaces = v
                    .split(',')
                    .map(surface::Surface::parse)
                    .collect::<Result<Vec<_>>>()
                    .map_err(|e| format!("--surfaces: {e}"))?;
                i += 1;
            }
            "--model" => {
                args.model = value(i, "--model")?;
                i += 1;
            }
            "--ollama-url" => {
                args.ollama_url = value(i, "--ollama-url")?;
                i += 1;
            }
            "--ctx-list" => {
                let v = value(i, "--ctx-list")?;
                args.ctx_list = v
                    .split(',')
                    .map(|n| n.trim().parse::<u32>())
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|_| format!("--ctx-list: not a number list: {v}"))?;
                if args.ctx_list.is_empty() {
                    return Err("--ctx-list needs at least one window".to_string());
                }
                i += 1;
            }
            other => return Err(format!("unknown flag: {other}")),
        }
        i += 1;
    }
    Ok(args)
}

#[tokio::main]
async fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("error: {message}\n");
            }
            eprint!("{USAGE}");
            std::process::exit(if message.is_empty() { 0 } else { 2 });
        }
    };

    if args.list {
        eprintln!("── scripted ──");
        for (_, (id, _)) in scripted_scenarios() {
            eprintln!("  {id}");
        }
        eprintln!("\n── credential (measured, per surface) ──");
        for sc in credential_suite::SCENARIOS {
            eprintln!("  {:<42}{}", sc.id, sc.service);
        }
        eprintln!("\n── model ──");
        for case in model_suite::cases() {
            let slow = if case.slow { "  (slow)" } else { "" };
            eprintln!("  {:<42}{slow}\n      {}", case.id, case.description);
        }
        eprintln!("\n── login (live network, opt-in, xfail) ──");
        for sc in login_suite::SCENARIOS {
            eprintln!("  {:<42}\n      {}", sc.id, sc.description);
        }
        return Ok(());
    }

    let bin =
        std::env::var("RUSTYKRAB_BIN").unwrap_or_else(|_| "target/debug/rustykrab-cli".to_string());
    if !std::path::Path::new(&bin).exists() {
        bail!("daemon binary not found at {bin} — build it first or set RUSTYKRAB_BIN");
    }

    if args.mode == "ablation" {
        let report = ablation::run(
            &bin,
            &args.model,
            &args.ollama_url,
            &args.ctx_list,
            args.reps,
            args.case_filter.as_deref(),
            args.quick,
        )
        .await?;
        let md = ablation::render_markdown(&report);
        std::fs::write("e2e-ablation.json", serde_json::to_string_pretty(&report)?)?;
        std::fs::write("e2e-ablation.md", &md)?;
        eprintln!("{md}");
        eprintln!("written: e2e-ablation.json, e2e-ablation.md");
        // A measurement, not a gate.
        return Ok(());
    }

    let mut reports: Vec<ScenarioReport> = Vec::new();
    let mut judge_name: Option<String> = None;
    let mut trials: Vec<credential_suite::TrialResult> = Vec::new();
    let mut login_trials: Vec<login_suite::LoginTrial> = Vec::new();

    if args.mode == "scripted" || args.mode == "all" {
        reports.extend(run_scripted(&bin, args.case_filter.as_deref()).await?);
    }
    if args.mode == "model" || args.mode == "all" {
        let (model_reports, name) = model_suite::run(
            &bin,
            &args.model,
            &args.ollama_url,
            args.reps,
            args.case_filter.as_deref(),
            args.quick,
        )
        .await?;
        reports.extend(model_reports);
        judge_name = Some(name);
    }

    // Nothing model-backed is worth starting if the model cannot answer.
    if matches!(args.mode.as_str(), "model" | "credential" | "login" | "all") {
        preflight_model(&args.ollama_url, &args.model).await?;
    }

    if args.mode == "credential" || args.mode == "all" {
        let (cells, trial_results) = credential_suite::run(
            &bin,
            &args.model,
            &args.ollama_url,
            args.trials,
            &args.surfaces,
            args.case_filter.as_deref(),
            args.trial_timeout,
            args.resume,
        )
        .await?;
        reports.extend(cells);
        trials = trial_results;
    }
    // Not in `all`: this is the only mode that reaches the public internet
    // with live credentials, so it is never swept up by a broad run. Ask
    // for it by name.
    if args.mode == "login" {
        let (cells, login_results) = login_suite::run(
            &bin,
            &args.model,
            &args.ollama_url,
            args.trials,
            args.case_filter.as_deref(),
            args.trial_timeout,
        )
        .await?;
        reports.extend(cells);
        login_trials = login_results;
    }

    let count = |o: &str| reports.iter().filter(|r| r.outcome == o).count();
    let (pass, fail, xfail, xpass) = (count("pass"), count("fail"), count("xfail"), count("xpass"));
    // Green means every implemented scenario passed and no target scenario
    // passed unexpectedly — an xpass must be promoted before the suite can
    // go green again.
    let ok = fail == 0 && xpass == 0;
    let report = json!({
        "scenarios": reports,
        // Every trial, verbatim, so any rate in the summary can be audited
        // back to the reply that produced it.
        "credential_trials": trials,
        "login_trials": login_trials,
        "summary": {
            "pass": pass,
            "fail": fail,
            "xfail": xfail,
            "xpass": xpass,
            "flaky": reports.iter().filter(|r| r.flaky()).count(),
            "measured": count("measure"),
            "judge": judge_name,
            "ok": ok,
        },
    });

    let report_text = serde_json::to_string_pretty(&report)?;
    let report_path = write_json_artifact(&artifact_dir(), "e2e-report.json", &report)?;
    println!("{report_text}");
    eprintln!("evidence report: {}", report_path.display());
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

/// The scripted suite shares one daemon across every scenario — they are
/// deterministic and independent, so a boot each would only add minutes.
async fn run_scripted(bin: &str, case_filter: Option<&str>) -> Result<Vec<ScenarioReport>> {
    let tmp = tempfile::Builder::new()
        .prefix("rustykrab-e2e-")
        .tempdir()?;
    let data_dir = tmp.path().to_path_buf();
    let port = pick_free_port()?;

    let daemon = Arc::new(tokio::sync::Mutex::new(Some(spawn_daemon(
        bin, &data_dir, port,
    )?)));
    let result = run_suite(bin, &data_dir, port, Arc::clone(&daemon), case_filter).await;
    if let Some(child) = daemon.lock().await.take() {
        shutdown_daemon(child).await;
    }
    keep_or_drop(tmp);
    result
}

/// SIGTERM first so the store flushes; hard-kill only if it will not go.
async fn shutdown_daemon(mut child: Child) {
    let _ = Command::new("kill").arg(child.id().to_string()).status();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => break,
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The throwaway data dir must be removed explicitly rather than by
/// TempDir's Drop: this process exits via `std::process::exit` when the
/// suite is red, which skips destructors, and going red is exactly when a
/// run would otherwise leak its dir and logs.
/// Keep the evidence, not the browser profile.
///
/// A retained trial is worth keeping for its logs, which are kilobytes.
/// The Chrome user-data dir beside them is hundreds of megabytes, and
/// nothing in a post-mortem reads it except `chrome-stderr.log`. Fifteen
/// retained trials filled the disk and stopped the machine mid-run --
/// including the suite that was still going -- so retention now keeps
/// what gets read and sheds what does not.
fn shed_browser_profile(root: &std::path::Path) -> String {
    let mut freed = 0u64;
    for user_data in find_user_data_dirs(root) {
        // The one file worth rescuing before the profile goes.
        let stderr_log = user_data.join("chrome-stderr.log");
        let rescued = std::fs::read(&stderr_log).ok();
        freed += dir_size(&user_data);
        if std::fs::remove_dir_all(&user_data).is_ok() {
            if let Some(bytes) = rescued {
                let _ = std::fs::create_dir_all(&user_data);
                let _ = std::fs::write(&stderr_log, bytes);
            }
        }
    }
    if freed == 0 {
        String::new()
    } else {
        format!(
            " (browser profile shed, {} MiB freed; chrome-stderr.log kept)",
            freed / (1024 * 1024)
        )
    }
}

fn find_user_data_dirs(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path.file_name().is_some_and(|n| n == "user-data") {
                found.push(path);
            } else {
                stack.push(path);
            }
        }
    }
    found
}

fn dir_size(dir: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.metadata() {
                Ok(m) if m.is_dir() => stack.push(path),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

fn keep_or_drop(tmp: tempfile::TempDir) {
    if std::env::var("E2E_KEEP_TMP").is_ok() {
        let path = tmp.keep();
        let freed = shed_browser_profile(&path);
        eprintln!(
            "E2E_KEEP_TMP set — data dir kept at {}{}",
            path.display(),
            freed
        );
        return;
    }
    let path = tmp.path().to_path_buf();
    drop(tmp);
    if path.exists() {
        let _ = std::fs::remove_dir_all(&path);
    }
}

fn artifact_dir() -> PathBuf {
    std::env::var_os("E2E_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/e2e-artifacts"))
}

fn write_json_artifact(root: &Path, relative_path: &str, value: &Value) -> Result<PathBuf> {
    let relative = Path::new(relative_path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("artifact path must contain only normal relative components: {relative_path}");
    }
    let path = root.join(relative);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("artifact path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create artifact directory {}", parent.display()))?;
    std::fs::write(&path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write evidence artifact {}", path.display()))?;
    Ok(path)
}

/// The last few lines of the daemon log — a startup failure is otherwise
/// reported as a bare exit status, which says nothing about the cause.
fn log_tail(data_dir: &std::path::Path) -> String {
    let Ok(text) = std::fs::read_to_string(data_dir.join("daemon.log")) else {
        return "(no daemon.log)".to_string();
    };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(25)..].join("\n")
}

/// The deterministic plumbing scenarios, in run order.
fn scripted_scenarios() -> Vec<(Expected, (&'static str, ScenarioFn))> {
    let mut scenarios = vec![
        // Baseline — implemented today, must pass.
        (Expected::Pass, scenario!(health)),
        (Expected::Pass, scenario!(auth_required)),
        (Expected::Pass, scenario!(origin_allowlist)),
        (Expected::Pass, scenario!(conversations_crud)),
        (Expected::Pass, scenario!(chat_scripted_default)),
        (Expected::Pass, scenario!(chat_sse_stream)),
        (Expected::Pass, scenario!(chat_sse_stream_with_tools)),
        (Expected::Pass, scenario!(secrets_create_and_delete)),
        (Expected::Pass, scenario!(agent_creates_new_credential)),
        // Workstream A — the credential guard. Shipped: these assert the
        // real behaviour now.
        // Workstream B — device pairing.
        (Expected::Pass, scenario!(pair_device_target)),
        (Expected::Pass, scenario!(secrets_create_only_409)),
        (Expected::Pass, scenario!(secrets_overwrite_archives)),
        (Expected::Pass, scenario!(agent_overwrite_files_request)),
        (Expected::Pass, scenario!(approve_applies_change)),
        (Expected::Pass, scenario!(deny_preserves_value)),
        (Expected::Pass, scenario!(agent_delete_files_request)),
        (Expected::Pass, scenario!(revoked_device_401)),
    ];
    scenarios.extend(planning_suite::scenarios());
    scenarios
}

async fn run_suite(
    bin: &str,
    data_dir: &std::path::Path,
    port: u16,
    daemon: Arc<tokio::sync::Mutex<Option<Child>>>,
    case_filter: Option<&str>,
) -> Result<Vec<ScenarioReport>> {
    let base = format!("http://127.0.0.1:{port}");
    // The origin-check middleware requires an Origin header on every
    // /api request (loopback origins are always allowed).
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::ORIGIN, base.parse()?);
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(120))
        .build()?;
    {
        let mut child = daemon.lock().await;
        wait_for_health(
            &base,
            &client,
            child
                .as_mut()
                .ok_or_else(|| anyhow!("daemon is not running"))?,
        )
        .await?;
    }

    // Open the store only after the daemon is healthy — it owns the
    // database and its migrations; this is a read handle for assertions.
    let store = rustykrab_store::Store::open(data_dir.join("db"), hex_decode(MASTER_KEY_HEX)?)?;
    let ctx = &Ctx {
        base,
        client,
        secrets: store.secrets(),
        db_path: data_dir.join("db").join("store.db"),
        bin: bin.to_string(),
        data_dir: data_dir.to_path_buf(),
        daemon,
    };

    let scenarios: Vec<_> = scripted_scenarios()
        .into_iter()
        .filter(|(_, (id, _))| case_filter.is_none_or(|filter| id.contains(filter)))
        .collect();
    if scenarios.is_empty() {
        bail!(
            "no scripted scenarios matched {}",
            case_filter.unwrap_or("the requested filter")
        );
    }

    let mut reports = Vec::new();
    for (expected, (id, f)) in scenarios {
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(120), f(ctx)).await;
        let (passes, details) = match outcome {
            Ok(Ok(())) => (1, vec![]),
            Ok(Err(e)) => (0, vec![format!("{e:#}")]),
            Err(_) => (0, vec!["scenario timed out after 120s".to_string()]),
        };
        let report = ScenarioReport::new(
            id,
            "scripted",
            expected,
            1,
            passes,
            details,
            started.elapsed().as_millis(),
        );
        eprintln!("{}", report.line());
        reports.push(report);
    }

    Ok(reports)
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(Into::into))
        .collect()
}
