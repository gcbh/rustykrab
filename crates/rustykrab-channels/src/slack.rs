//! Slack channel using the Socket Mode transport.
//!
//! The bot opens an outbound WebSocket to Slack via `apps.connections.open`
//! and receives `events_api` envelopes there — no public webhook URL is
//! required. Outbound messages still use the standard Web API
//! (`chat.postMessage`) authenticated with the bot token.
//!
//! Threading model:
//! - When the agent is `@`-mentioned at the top level of a channel, the
//!   reply auto-threads off the user's message (`thread_ts =
//!   inbound.message_ts`). This is the standard Slack-bot convention and
//!   keeps channels uncluttered.
//! - When the mention happens inside an existing thread, the reply uses
//!   the same `thread_ts`.
//! - Cron jobs and other background sends carry their own `thread_ts`
//!   (stored on the [`ScheduledJob`] row) so they land in the right thread.
//!
//! Security:
//! - `SLACK_ALLOWED_CHANNELS` is an explicit allowlist of channel IDs.
//!   Empty means deny all.
//! - Optional `SLACK_ALLOWED_TEAMS` adds a workspace-level allowlist.
//! - Bot's own `app_mention` echoes are filtered using the user id learned
//!   from `auth.test` at startup.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::{SinkExt, StreamExt};
use rustykrab_core::types::{Message, MessageContent, Role};
use rustykrab_core::{Error, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use uuid::Uuid;

/// Maximum length of a single Slack message chunk. Slack's nominal limit
/// is ~40,000 chars but mrkdwn formatting and clients render long messages
/// poorly; 3000 keeps replies readable.
const SLACK_MAX_LENGTH: usize = 3000;

/// Number of recent `event_id`s to remember for retry suppression.
const EVENT_ID_CACHE_SIZE: usize = 1024;

/// Maximum reconnect backoff for the Socket Mode loop.
const MAX_RECONNECT_DELAY_SECS: u64 = 30;

/// An inbound Slack message with channel-specific routing metadata.
///
/// `thread_ts` is `Some` when the user posted inside an existing thread;
/// `None` when the mention is at the top level of a channel. The
/// agent-loop converts a `None` into an auto-thread off `message_ts` so
/// the reply opens a new thread.
#[derive(Debug, Clone)]
pub struct SlackInboundMessage {
    pub team_id: String,
    pub channel_id: String,
    pub thread_ts: Option<String>,
    pub user_id: String,
    pub user_name: Option<String>,
    pub message_ts: String,
    pub message: Message,
    pub reset: bool,
}

/// Slack channel handle (Socket Mode + Web API outbound).
pub struct SlackChannel {
    client: reqwest::Client,
    bot_token: String,
    app_token: String,
    /// Only these channel IDs may interact. Empty = deny all.
    allowed_channels: HashSet<String>,
    /// Optional workspace-level allowlist. Empty = allow any workspace.
    allowed_teams: HashSet<String>,
    /// Bot's own user id, learned via `auth.test` on first connect. Used
    /// to drop the bot's own `app_mention` echoes.
    bot_user_id: Mutex<Option<String>>,
    /// Recent `event_id`s for retry suppression.
    seen_event_ids: Mutex<VecDeque<String>>,
    inbound_tx: mpsc::Sender<SlackInboundMessage>,
    inbound_rx: Option<mpsc::Receiver<SlackInboundMessage>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl SlackChannel {
    /// Create a new Slack channel.
    ///
    /// `bot_token` is the workspace-installed bot token (`xoxb-...`).
    /// `app_token` is the app-level token with `connections:write`
    /// (`xapp-...`) used to open the Socket Mode WebSocket.
    pub fn new(bot_token: String, app_token: String, allowed_channels: HashSet<String>) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            bot_token,
            app_token,
            allowed_channels,
            allowed_teams: HashSet::new(),
            bot_user_id: Mutex::new(None),
            seen_event_ids: Mutex::new(VecDeque::with_capacity(EVENT_ID_CACHE_SIZE)),
            inbound_tx: tx,
            inbound_rx: Some(rx),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Set an additional workspace-level allowlist. When empty (default)
    /// any workspace passes the team check; the channel allowlist still
    /// applies.
    pub fn with_allowed_teams(mut self, teams: HashSet<String>) -> Self {
        self.allowed_teams = teams;
        self
    }

    /// Take the inbound receiver (one-shot). The agent loop reads from
    /// this to get user messages.
    pub fn take_inbound_rx(&mut self) -> Option<mpsc::Receiver<SlackInboundMessage>> {
        self.inbound_rx.take()
    }

    /// Request graceful shutdown of the Socket Mode loop.
    pub fn shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
    }

    /// Send a text message to a Slack channel.
    ///
    /// `thread_ts` controls threading: `Some(ts)` posts inside that
    /// thread; `None` posts at the channel's top level. Long messages
    /// are split into multiple posts so each chunk fits cleanly.
    /// Returns the `ts` of the first chunk, which can be used as a thread
    /// anchor for follow-up messages.
    pub async fn send_text(
        &self,
        channel_id: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String> {
        let chunks = split_message(text, SLACK_MAX_LENGTH);
        let mut first_ts: Option<String> = None;
        for chunk in &chunks {
            let ts = self.post_message(channel_id, chunk, thread_ts).await?;
            if first_ts.is_none() {
                first_ts = Some(ts);
            }
        }
        first_ts.ok_or_else(|| Error::Channel("send_text produced no chunks".into()))
    }

    /// Low-level `chat.postMessage` call. Returns the new message's `ts`.
    async fn post_message(
        &self,
        channel_id: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String> {
        let mut body = json!({
            "channel": channel_id,
            "text": text,
        });
        if let Some(ts) = thread_ts {
            body["thread_ts"] = json!(ts);
        }

        let resp = self
            .client
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Channel(format!("Slack chat.postMessage error: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            return Err(Error::Channel(format!(
                "Slack chat.postMessage HTTP {status}: {err}"
            )));
        }

        let value: Value = resp
            .json()
            .await
            .map_err(|e| Error::Channel(format!("Slack chat.postMessage decode: {e}")))?;
        if value["ok"].as_bool() != Some(true) {
            let err = value["error"].as_str().unwrap_or("unknown_error");
            return Err(Error::Channel(format!("Slack chat.postMessage: {err}")));
        }
        let ts = value["ts"]
            .as_str()
            .ok_or_else(|| Error::Channel("chat.postMessage response missing ts".into()))?
            .to_string();
        Ok(ts)
    }

    /// Discover this bot's own user id via `auth.test`. Cached on the
    /// first successful call so the subsequent app_mention filter is
    /// cheap. Errors are non-fatal — without the id we just can't filter
    /// self-mentions.
    async fn ensure_bot_user_id(&self) -> Option<String> {
        {
            let guard = self.bot_user_id.lock().await;
            if let Some(id) = guard.as_ref() {
                return Some(id.clone());
            }
        }
        let resp = self
            .client
            .post("https://slack.com/api/auth.test")
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .ok()?;
        let v: Value = resp.json().await.ok()?;
        if v["ok"].as_bool() != Some(true) {
            tracing::warn!(
                error = %v["error"].as_str().unwrap_or("unknown"),
                "Slack auth.test failed"
            );
            return None;
        }
        let id = v["user_id"].as_str()?.to_string();
        *self.bot_user_id.lock().await = Some(id.clone());
        Some(id)
    }

    /// Idempotency check: returns `true` if this `event_id` was already
    /// processed recently and we should drop it.
    async fn is_duplicate_event(&self, event_id: &str) -> bool {
        let mut seen = self.seen_event_ids.lock().await;
        if seen.iter().any(|e| e == event_id) {
            return true;
        }
        if seen.len() >= EVENT_ID_CACHE_SIZE {
            seen.pop_front();
        }
        seen.push_back(event_id.to_string());
        false
    }

    /// Run the Socket Mode loop indefinitely. Spawn this as a background
    /// task. Reconnects with capped exponential backoff on disconnects.
    pub async fn start_socket_mode(&self) -> Result<()> {
        // Best-effort cache bot user id at startup so the self-mention
        // filter is ready before the first event.
        let _ = self.ensure_bot_user_id().await;

        let mut consecutive_errors: u32 = 0;
        tracing::info!("Slack Socket Mode loop starting");

        loop {
            if self.shutdown_flag.load(Ordering::Relaxed) {
                tracing::info!("Slack Socket Mode shutdown requested");
                return Ok(());
            }

            match self.run_one_connection().await {
                Ok(()) => {
                    // Clean disconnect (e.g. Slack asked us to reconnect);
                    // loop and dial again immediately.
                    consecutive_errors = 0;
                }
                Err(e) => {
                    consecutive_errors += 1;
                    let delay = reconnect_delay(consecutive_errors);
                    tracing::error!(
                        consecutive_errors,
                        delay_secs = delay.as_secs(),
                        "Slack Socket Mode connection error: {e}"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Open one Socket Mode WebSocket and process envelopes until it
    /// closes or asks us to reconnect.
    async fn run_one_connection(&self) -> Result<()> {
        // 1. Get a wss URL.
        let resp = self
            .client
            .post("https://slack.com/api/apps.connections.open")
            .bearer_auth(&self.app_token)
            .send()
            .await
            .map_err(|e| Error::Channel(format!("apps.connections.open: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            return Err(Error::Channel(format!(
                "apps.connections.open HTTP {status}: {err}"
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Channel(format!("apps.connections.open decode: {e}")))?;
        if v["ok"].as_bool() != Some(true) {
            return Err(Error::Channel(format!(
                "apps.connections.open: {}",
                v["error"].as_str().unwrap_or("unknown")
            )));
        }
        let url = v["url"]
            .as_str()
            .ok_or_else(|| Error::Channel("apps.connections.open missing url".into()))?
            .to_string();

        // 2. Connect.
        let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| Error::Channel(format!("Slack ws connect: {e}")))?;
        tracing::info!("Slack Socket Mode connected");

        let (mut sink, mut stream) = ws_stream.split();

        // 3. Loop reading envelopes.
        while let Some(msg) = stream.next().await {
            if self.shutdown_flag.load(Ordering::Relaxed) {
                let _ = sink.close().await;
                return Ok(());
            }
            let msg = msg.map_err(|e| Error::Channel(format!("Slack ws read: {e}")))?;
            let text = match msg {
                WsMessage::Text(t) => t,
                WsMessage::Ping(p) => {
                    if let Err(e) = sink.send(WsMessage::Pong(p)).await {
                        return Err(Error::Channel(format!("Slack ws pong: {e}")));
                    }
                    continue;
                }
                WsMessage::Close(_) => {
                    tracing::info!("Slack Socket Mode close frame");
                    return Ok(());
                }
                _ => continue,
            };

            let envelope: Envelope = match serde_json::from_str(&text) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("Slack envelope decode failed: {e}");
                    continue;
                }
            };

            match envelope.envelope_type.as_deref() {
                Some("hello") => {
                    tracing::debug!("Slack Socket Mode hello received");
                }
                Some("disconnect") => {
                    tracing::info!("Slack asked us to reconnect");
                    return Ok(());
                }
                Some("events_api") => {
                    // Ack first (Slack's 3-second window) before doing
                    // anything that could take time.
                    if let Some(eid) = envelope.envelope_id.as_deref() {
                        let ack = json!({ "envelope_id": eid });
                        if let Err(e) = sink.send(WsMessage::Text(ack.to_string())).await {
                            return Err(Error::Channel(format!("Slack ws ack: {e}")));
                        }
                    }
                    if let Some(payload) = envelope.payload {
                        if let Err(e) = self.handle_events_api_payload(payload).await {
                            tracing::warn!("Slack event handling failed: {e}");
                        }
                    }
                }
                other => {
                    tracing::debug!(?other, "Slack envelope type ignored");
                }
            }
        }

        Ok(())
    }

    /// Handle one `event_callback` payload from an `events_api` envelope.
    async fn handle_events_api_payload(&self, payload: Value) -> Result<()> {
        // Idempotency: same event_id may be redelivered.
        if let Some(eid) = payload["event_id"].as_str() {
            if self.is_duplicate_event(eid).await {
                tracing::debug!(event_id = eid, "dropping duplicate Slack event");
                return Ok(());
            }
        }

        let team_id = payload["team_id"].as_str().unwrap_or("").to_string();
        if !self.allowed_teams.is_empty() && !self.allowed_teams.contains(&team_id) {
            tracing::warn!(%team_id, "Slack event from disallowed team, ignoring");
            return Ok(());
        }

        let event = match payload.get("event") {
            Some(e) => e,
            None => return Ok(()),
        };

        // v1: only respond to app_mention.
        if event["type"].as_str() != Some("app_mention") {
            tracing::debug!(
                event_type = ?event["type"].as_str(),
                "ignoring non app_mention event"
            );
            return Ok(());
        }

        let channel_id = match event["channel"].as_str() {
            Some(c) => c.to_string(),
            None => return Ok(()),
        };
        if !self.allowed_channels.is_empty() && !self.allowed_channels.contains(&channel_id) {
            tracing::warn!(%channel_id, "Slack event from disallowed channel, ignoring");
            return Ok(());
        }

        let user_id = match event["user"].as_str() {
            Some(u) => u.to_string(),
            None => return Ok(()),
        };

        // Drop the bot's own self-mentions to avoid loops.
        if let Some(bot_id) = self.ensure_bot_user_id().await {
            if user_id == bot_id {
                tracing::debug!("ignoring self app_mention");
                return Ok(());
            }
        }

        let message_ts = match event["ts"].as_str() {
            Some(t) => t.to_string(),
            None => return Ok(()),
        };
        let thread_ts = event["thread_ts"].as_str().map(|s| s.to_string());

        let raw_text = event["text"].as_str().unwrap_or("");
        let text = strip_mention_prefix(raw_text);
        if text.is_empty() {
            // Bare mention with no other text — ack with a friendly nudge
            // rather than launching the agent.
            let reply_thread = thread_ts.clone().unwrap_or_else(|| message_ts.clone());
            let _ = self
                .send_text(
                    &channel_id,
                    "Hi! You mentioned me — send a message and I'll help.",
                    Some(&reply_thread),
                )
                .await;
            return Ok(());
        }

        tracing::info!(
            %team_id, %channel_id, %user_id, %message_ts,
            thread_ts = ?thread_ts,
            "received Slack app_mention"
        );

        let message = Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(text),
            created_at: Utc::now(),
        };

        self.inbound_tx
            .send(SlackInboundMessage {
                team_id,
                channel_id,
                thread_ts,
                user_id,
                user_name: None,
                message_ts,
                message,
                reset: false,
            })
            .await
            .map_err(|e| Error::Channel(format!("Slack inbound queue full: {e}")))?;

        Ok(())
    }
}

/// One Socket Mode envelope.
#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    envelope_type: Option<String>,
    envelope_id: Option<String>,
    payload: Option<Value>,
}

/// Strip a leading `<@U…>` mention (and surrounding whitespace) so the
/// agent receives just the user's actual prompt.
fn strip_mention_prefix(text: &str) -> String {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("<@") {
        if let Some(end) = rest.find('>') {
            return rest[end + 1..].trim().to_string();
        }
    }
    trimmed.to_string()
}

/// Capped exponential backoff for reconnect attempts.
fn reconnect_delay(consecutive_errors: u32) -> Duration {
    let secs = 2u64
        .saturating_pow(consecutive_errors.min(6))
        .min(MAX_RECONNECT_DELAY_SECS);
    Duration::from_secs(secs)
}

/// Split a message into chunks that fit within Slack's per-post budget.
/// Tries paragraph → sentence → word boundaries to avoid mid-word breaks.
fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }
        // Find a safe char boundary at or before max_len.
        let mut end = max_len.min(remaining.len());
        while end > 0 && !remaining.is_char_boundary(end) {
            end -= 1;
        }
        // Prefer paragraph, then sentence, then word boundaries within
        // the first `end` bytes.
        let split_at = remaining[..end]
            .rfind("\n\n")
            .map(|i| i + 2)
            .or_else(|| remaining[..end].rfind(". ").map(|i| i + 2))
            .or_else(|| remaining[..end].rfind(' ').map(|i| i + 1))
            .unwrap_or(end);
        let (head, tail) = remaining.split_at(split_at);
        chunks.push(head.trim_end().to_string());
        remaining = tail.trim_start();
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_mention_prefix_removes_user_mention() {
        assert_eq!(strip_mention_prefix("<@U012345> hello"), "hello");
        assert_eq!(strip_mention_prefix("  <@U012345>   hi  "), "hi");
        assert_eq!(strip_mention_prefix("no mention here"), "no mention here");
    }

    #[test]
    fn strip_mention_prefix_handles_empty() {
        assert_eq!(strip_mention_prefix("<@U012345>"), "");
        assert_eq!(strip_mention_prefix(""), "");
    }

    #[test]
    fn split_message_short_returns_single_chunk() {
        let chunks = split_message("hello", 100);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn split_message_splits_on_paragraph() {
        let text = "first paragraph\n\nsecond paragraph that is much longer than the limit";
        let chunks = split_message(text, 30);
        assert!(chunks.len() >= 2);
        assert!(chunks[0].contains("first"));
    }

    #[test]
    fn reconnect_delay_caps_at_max() {
        assert!(reconnect_delay(100).as_secs() <= MAX_RECONNECT_DELAY_SECS);
        assert!(reconnect_delay(1).as_secs() >= 1);
    }

    #[tokio::test]
    async fn duplicate_event_returns_true_on_second_call() {
        let ch = SlackChannel::new(
            "xoxb-test".to_string(),
            "xapp-test".to_string(),
            HashSet::new(),
        );
        assert!(!ch.is_duplicate_event("E1").await);
        assert!(ch.is_duplicate_event("E1").await);
        assert!(!ch.is_duplicate_event("E2").await);
    }

    #[tokio::test]
    async fn duplicate_event_evicts_oldest_when_full() {
        let ch = SlackChannel::new(
            "xoxb-test".to_string(),
            "xapp-test".to_string(),
            HashSet::new(),
        );
        // Fill the cache.
        for i in 0..EVENT_ID_CACHE_SIZE {
            assert!(!ch.is_duplicate_event(&format!("E{i}")).await);
        }
        // First entry should be evicted by the next insert.
        assert!(!ch.is_duplicate_event("Enew").await);
        assert!(!ch.is_duplicate_event("E0").await);
    }
}
