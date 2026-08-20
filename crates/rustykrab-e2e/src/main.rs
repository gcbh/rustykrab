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

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

/// Master key for the throwaway store (hex, 32 bytes). Test-only.
const MASTER_KEY_HEX: &str = "e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0";
const AUTH_TOKEN: &str = "e2e-master-token";

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
    id: &'static str,
    expected: Expected,
    passed: bool,
    /// "pass" | "fail" | "xfail" | "xpass"
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
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

    /// Run a daemon CLI subcommand with the harness environment.
    fn cli(&self, args: &[&str]) -> Result<std::process::Output> {
        Ok(Command::new(&self.bin)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("RUSTYKRAB_DATA_DIR", &self.data_dir)
            .env("RUSTYKRAB_MASTER_KEY", MASTER_KEY_HEX)
            .env("RUSTYKRAB_AUTH_TOKEN", AUTH_TOKEN)
            .env("RUSTYKRAB_DISABLE_KEYCHAIN", "1")
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

fn spawn_daemon(bin: &str, data_dir: &std::path::Path, port: u16) -> Result<Child> {
    let script_path = data_dir.join("e2e-script.json");
    std::fs::write(&script_path, AGENT_SCRIPT)?;
    let log = std::fs::File::create(data_dir.join("daemon.log"))?;
    let child = Command::new(bin)
        // Start from an empty environment: the developer's shell may hold
        // real credentials (which would be written into this throwaway
        // store) and RUSTYKRAB_* settings that would change behaviour and
        // break determinism. Only PATH and HOME are carried over.
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("RUSTYKRAB_DATA_DIR", data_dir)
        .env("RUSTYKRAB_PORT", port.to_string())
        .env("RUSTYKRAB_PROVIDER", "scripted")
        .env("RUSTYKRAB_SCRIPT_PATH", &script_path)
        .env("RUSTYKRAB_MASTER_KEY", MASTER_KEY_HEX)
        .env("RUSTYKRAB_AUTH_TOKEN", AUTH_TOKEN)
        // Never touch the host's real Keychain from a throwaway boot.
        .env("RUSTYKRAB_DISABLE_KEYCHAIN", "1")
        // Dummy values for the registry's startup-required secrets.
        .env("NOTION_API_TOKEN", "e2e-dummy-notion")
        .env("OBSIDIAN_API_KEY", "e2e-dummy-obsidian")
        .env("RUSTYKRAB_LOG_STDOUT", "1")
        // The suite drives far more than the shipping 20 req/min from one
        // IP; raise the limit for this throwaway boot only.
        .env("RUSTYKRAB_RATE_LIMIT_MAX", "100000")
        .env("RUSTYKRAB_RATE_LIMIT_LOCKOUT_SECS", "1")
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .with_context(|| format!("failed to spawn daemon binary {bin}"))?;
    Ok(child)
}

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

type ScenarioFn =
    for<'a> fn(&'a Ctx) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>>;

macro_rules! scenario {
    ($f:ident) => {{
        let f: ScenarioFn = |ctx| Box::pin($f(ctx));
        (stringify!($f), f)
    }};
}

#[tokio::main]
async fn main() -> Result<()> {
    let bin =
        std::env::var("RUSTYKRAB_BIN").unwrap_or_else(|_| "target/debug/rustykrab-cli".to_string());
    if !std::path::Path::new(&bin).exists() {
        bail!("daemon binary not found at {bin} — build it first or set RUSTYKRAB_BIN");
    }

    let tmp = tempfile::Builder::new()
        .prefix("rustykrab-e2e-")
        .tempdir()?;
    let keep_tmp = std::env::var("E2E_KEEP_TMP").is_ok();
    let data_dir = tmp.path().to_path_buf();
    let port = pick_free_port()?;

    let mut child = spawn_daemon(&bin, &data_dir, port)?;

    let result = run_suite(&bin, &data_dir, port, &mut child).await;

    // Graceful shutdown first (SIGTERM) so the store flushes; hard-kill on
    // timeout or if TERM couldn't be sent.
    let _ = Command::new("kill").arg(child.id().to_string()).status();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let (report, ok) = result?;
    if keep_tmp {
        let path = tmp.keep();
        eprintln!("E2E_KEEP_TMP set — data dir kept at {}", path.display());
    } else {
        // Delete the throwaway data dir explicitly rather than relying on
        // TempDir's Drop: this process exits via `std::process::exit` when
        // the suite is red, which skips destructors. Leaving it implicit
        // meant every red run — and going red is how an xpass gets
        // noticed — leaked a daemon data dir and its logs.
        let path = tmp.path().to_path_buf();
        drop(tmp);
        if path.exists() {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

async fn run_suite(
    bin: &str,
    data_dir: &std::path::Path,
    port: u16,
    child: &mut Child,
) -> Result<(Value, bool)> {
    let base = format!("http://127.0.0.1:{port}");
    // The origin-check middleware requires an Origin header on every
    // /api request (loopback origins are always allowed).
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::ORIGIN, base.parse()?);
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(120))
        .build()?;
    wait_for_health(&base, &client, child).await?;

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
    };

    let scenarios: Vec<(Expected, (&'static str, ScenarioFn))> = vec![
        // Baseline — implemented today, must pass.
        (Expected::Pass, scenario!(health)),
        (Expected::Pass, scenario!(auth_required)),
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

    let mut reports = Vec::new();
    for (expected, (id, f)) in scenarios {
        let outcome = tokio::time::timeout(Duration::from_secs(120), f(ctx)).await;
        let (passed, detail) = match outcome {
            Ok(Ok(())) => (true, None),
            Ok(Err(e)) => (false, Some(format!("{e:#}"))),
            Err(_) => (false, Some("scenario timed out after 120s".to_string())),
        };
        let outcome = match (expected, passed) {
            (Expected::Pass, true) => "pass",
            (Expected::Pass, false) => "fail",
            (Expected::XFail, false) => "xfail",
            (Expected::XFail, true) => "xpass",
        };
        eprintln!(
            "[{outcome:>5}] {id}{}",
            match &detail {
                Some(d) if expected == Expected::Pass => format!(" — {d}"),
                _ => String::new(),
            }
        );
        reports.push(ScenarioReport {
            id,
            expected,
            passed,
            outcome,
            detail,
        });
    }

    let count = |o: &str| reports.iter().filter(|r| r.outcome == o).count();
    let (pass, fail, xfail, xpass) = (count("pass"), count("fail"), count("xfail"), count("xpass"));
    // Green means: every implemented scenario passes and no target
    // scenario passes unexpectedly (an xpass must be promoted to Pass).
    let ok = fail == 0 && xpass == 0;
    let report = json!({
        "scenarios": reports,
        "summary": {
            "pass": pass,
            "fail": fail,
            "xfail": xfail,
            "xpass": xpass,
            "ok": ok,
        },
    });
    Ok((report, ok))
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(Into::into))
        .collect()
}
