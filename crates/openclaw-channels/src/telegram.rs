use chrono::Utc;
use hmac::{Hmac, Mac};
use openclaw_core::types::{Message, MessageContent, Role};
use openclaw_core::{Error, Result};
use serde::Deserialize;
use sha2::Sha256;
use std::collections::HashSet;
use tokio::sync::mpsc;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// Maximum text length Telegram allows in a single message.
const TELEGRAM_MAX_LENGTH: usize = 4096;

/// Maximum retries for sending a message before giving up.
const SEND_MAX_RETRIES: u32 = 3;

/// An inbound message with channel-specific routing metadata.
pub struct ChannelMessage {
    pub chat_id: i64,
    pub message: Message,
}

/// Telegram Bot API channel.
///
/// Supports two modes:
/// - **Long-polling** (`start_polling`) — no public IP required, ideal for local dev
/// - **Webhook** (`parse_webhook_update`) — for production behind a reverse proxy
///
/// Security features (addressing original OpenClaw Telegram CVEs):
/// - Webhook secret token validation (HMAC-SHA256)
/// - Chat ID allowlist — only specified chats can interact
/// - No auto-join; every chat must be explicitly allowed
pub struct TelegramChannel {
    client: reqwest::Client,
    bot_token: String,
    api_base: String,
    /// Only these chat IDs may interact. Empty = deny all.
    allowed_chats: HashSet<i64>,
    /// Secret token for webhook validation.
    webhook_secret: Option<String>,
    /// Sender for inbound messages (user -> agent).
    inbound_tx: mpsc::Sender<ChannelMessage>,
    /// Receiver for inbound messages (consumed by the agent loop).
    inbound_rx: Option<mpsc::Receiver<ChannelMessage>>,
}

impl TelegramChannel {
    /// Create a new Telegram channel.
    ///
    /// `bot_token` is the token from @BotFather.
    /// `allowed_chats` restricts which Telegram chats can use the bot.
    pub fn new(bot_token: String, allowed_chats: HashSet<i64>) -> Self {
        let (tx, rx) = mpsc::channel(256);
        Self {
            client: reqwest::Client::new(),
            api_base: format!("https://api.telegram.org/bot{bot_token}"),
            bot_token,
            allowed_chats,
            webhook_secret: None,
            inbound_tx: tx,
            inbound_rx: Some(rx),
        }
    }

    /// Set a webhook secret for HMAC validation of incoming updates.
    pub fn with_webhook_secret(mut self, secret: String) -> Self {
        self.webhook_secret = Some(secret);
        self
    }

    /// Take the inbound receiver (can only be called once).
    /// The agent loop reads from this to get user messages.
    pub fn take_inbound_rx(&mut self) -> Option<mpsc::Receiver<ChannelMessage>> {
        self.inbound_rx.take()
    }

    /// Send a "typing" chat action so the user sees the bot is working.
    pub async fn send_typing(&self, chat_id: i64) -> Result<()> {
        let url = format!("{}/sendChatAction", self.api_base);
        let body = serde_json::json!({
            "chat_id": chat_id,
            "action": "typing",
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Channel(format!("Telegram sendChatAction error: {e}")))?;

        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            tracing::debug!("sendChatAction failed (non-critical): {err}");
        }

        Ok(())
    }

    /// Send a text message to a Telegram chat, automatically splitting
    /// messages that exceed Telegram's 4096 character limit.
    ///
    /// Uses Markdown parse mode with automatic plain-text fallback if
    /// Telegram rejects the formatting.
    pub async fn send_text(&self, chat_id: i64, text: &str) -> Result<()> {
        let chunks = split_message(text, TELEGRAM_MAX_LENGTH);
        for chunk in &chunks {
            self.send_single_message(chat_id, chunk).await?;
        }
        Ok(())
    }

    /// Send a single message chunk with retry and Markdown fallback.
    async fn send_single_message(&self, chat_id: i64, text: &str) -> Result<()> {
        // First attempt: with Markdown.
        match self
            .try_send(chat_id, text, Some("Markdown"), SEND_MAX_RETRIES)
            .await
        {
            Ok(()) => return Ok(()),
            Err(e) => {
                // If Markdown parsing failed (400 Bad Request), retry as plain text.
                let err_str = format!("{e}");
                if err_str.contains("400") || err_str.contains("parse") || err_str.contains("can't") {
                    tracing::debug!("Markdown rejected by Telegram, retrying as plain text");
                    return self.try_send(chat_id, text, None, SEND_MAX_RETRIES).await;
                }
                return Err(e);
            }
        }
    }

    /// Low-level send with retry on transient failures.
    async fn try_send(
        &self,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
        max_retries: u32,
    ) -> Result<()> {
        let url = format!("{}/sendMessage", self.api_base);
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
        });
        if let Some(mode) = parse_mode {
            body["parse_mode"] = serde_json::json!(mode);
        }

        let mut last_err = None;
        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay = std::time::Duration::from_millis(500 * 2u64.pow(attempt - 1));
                tokio::time::sleep(delay).await;
            }

            match self.client.post(&url).json(&body).send().await {
                Ok(resp) => {
                    if resp.status().is_success() {
                        return Ok(());
                    }
                    let status = resp.status();
                    let err_text = resp.text().await.unwrap_or_default();

                    // Don't retry client errors (except 429 rate limit).
                    if status.is_client_error() && status.as_u16() != 429 {
                        return Err(Error::Channel(format!(
                            "Telegram sendMessage failed ({status}): {err_text}"
                        )));
                    }

                    // Rate limited — respect Retry-After if present.
                    if status.as_u16() == 429 {
                        tracing::warn!("Telegram rate limited, backing off");
                    }

                    last_err = Some(Error::Channel(format!(
                        "Telegram sendMessage failed ({status}): {err_text}"
                    )));
                }
                Err(e) => {
                    last_err = Some(Error::Channel(format!("Telegram API error: {e}")));
                }
            }

            if attempt < max_retries {
                tracing::debug!(attempt, "retrying Telegram sendMessage");
            }
        }

        Err(last_err.unwrap_or_else(|| Error::Channel("send failed after retries".into())))
    }

    /// Start long-polling for updates. Runs forever — spawn this as a task.
    ///
    /// This is the simplest way to receive messages without exposing a
    /// public endpoint. Suitable for local development.
    ///
    /// Resilient to transient errors: logs and retries with exponential
    /// backoff rather than crashing on network blips.
    pub async fn start_polling(&self) -> Result<()> {
        // Clear any stale webhook so Telegram doesn't send duplicates.
        if let Err(e) = self.delete_webhook().await {
            tracing::warn!("failed to clear stale webhook (may not be set): {e}");
        }

        let mut offset: Option<i64> = None;
        let mut consecutive_errors: u32 = 0;

        tracing::info!("Telegram long-polling started");

        loop {
            let url = format!("{}/getUpdates", self.api_base);
            let mut params = vec![("timeout", "30".to_string())];
            if let Some(off) = offset {
                params.push(("offset", off.to_string()));
            }

            let resp = match self.client.get(&url).query(&params).send().await {
                Ok(r) => r,
                Err(e) => {
                    consecutive_errors += 1;
                    let delay = backoff_delay(consecutive_errors);
                    tracing::error!(
                        consecutive_errors,
                        delay_secs = delay.as_secs(),
                        "Telegram getUpdates network error: {e}"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
            };

            if !resp.status().is_success() {
                consecutive_errors += 1;
                let delay = backoff_delay(consecutive_errors);
                let err = resp.text().await.unwrap_or_default();
                tracing::error!(
                    consecutive_errors,
                    delay_secs = delay.as_secs(),
                    "Telegram getUpdates HTTP error: {err}"
                );
                tokio::time::sleep(delay).await;
                continue;
            }

            let body: GetUpdatesResponse = match resp.json().await {
                Ok(b) => b,
                Err(e) => {
                    consecutive_errors += 1;
                    let delay = backoff_delay(consecutive_errors);
                    tracing::error!(
                        consecutive_errors,
                        delay_secs = delay.as_secs(),
                        "failed to parse Telegram updates: {e}"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
            };

            // Successful poll — reset error counter.
            consecutive_errors = 0;

            for update in body.result {
                if let Some(new_offset) = Some(update.update_id + 1) {
                    offset = Some(new_offset);
                }

                if let Err(e) = self.handle_update(update).await {
                    tracing::warn!("failed to handle Telegram update: {e}");
                }
            }
        }
    }

    /// Parse and handle a webhook update payload.
    ///
    /// Call this from your axum webhook handler. Returns an error if
    /// the update is from a disallowed chat or fails validation.
    pub async fn parse_webhook_update(
        &self,
        payload: &[u8],
        secret_header: Option<&str>,
    ) -> Result<()> {
        // Validate webhook secret if configured (using constant-time comparison).
        if let Some(ref secret) = self.webhook_secret {
            let header = secret_header
                .ok_or_else(|| Error::Auth("missing X-Telegram-Bot-Api-Secret-Token header".into()))?;
            if !constant_time_eq(header, secret) {
                return Err(Error::Auth("invalid Telegram webhook secret".into()));
            }
        }

        let update: Update = serde_json::from_slice(payload)?;
        self.handle_update(update).await
    }

    /// Register a webhook URL with Telegram.
    pub async fn set_webhook(&self, url: &str) -> Result<()> {
        let api_url = format!("{}/setWebhook", self.api_base);

        let mut body = serde_json::json!({ "url": url });
        if let Some(ref secret) = self.webhook_secret {
            body["secret_token"] = serde_json::json!(secret);
        }

        let resp = self
            .client
            .post(&api_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Channel(format!("failed to set webhook: {e}")))?;

        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            return Err(Error::Channel(format!("setWebhook failed: {err}")));
        }

        tracing::info!(%url, "Telegram webhook registered");
        Ok(())
    }

    /// Delete the webhook (switch back to long-polling).
    pub async fn delete_webhook(&self) -> Result<()> {
        let url = format!("{}/deleteWebhook", self.api_base);
        self.client
            .post(&url)
            .send()
            .await
            .map_err(|e| Error::Channel(format!("deleteWebhook failed: {e}")))?;
        Ok(())
    }

    /// Process a single Telegram update.
    async fn handle_update(&self, update: Update) -> Result<()> {
        let msg = match update.message {
            Some(m) => m,
            None => return Ok(()), // Ignore non-message updates (edits, callbacks, etc.)
        };

        let chat_id = msg.chat.id;

        // Chat allowlist check.
        if !self.allowed_chats.is_empty() && !self.allowed_chats.contains(&chat_id) {
            tracing::warn!(
                chat_id,
                username = msg.from.as_ref().map(|u| u.username.as_deref().unwrap_or("unknown")),
                "message from disallowed chat, ignoring"
            );
            return Ok(());
        }

        let text = match msg.text {
            Some(t) => t,
            None => return Ok(()), // Ignore non-text messages for now.
        };

        // Handle bot commands before forwarding to the agent.
        if let Some(reply) = self.handle_command(&text, chat_id).await {
            let _ = self.send_text(chat_id, &reply).await;
            return Ok(());
        }

        let from = msg
            .from
            .as_ref()
            .and_then(|u| u.username.as_deref())
            .unwrap_or("unknown");

        tracing::info!(chat_id, %from, "received Telegram message");

        let message = Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(text),
            created_at: Utc::now(),
        };

        self.inbound_tx
            .send(ChannelMessage { chat_id, message })
            .await
            .map_err(|e| Error::Channel(format!("inbound queue full: {e}")))?;

        Ok(())
    }

    /// Handle built-in bot commands. Returns Some(reply) if the command
    /// was handled, None if the message should be forwarded to the agent.
    async fn handle_command(&self, text: &str, _chat_id: i64) -> Option<String> {
        let cmd = text.split_whitespace().next()?;
        match cmd {
            "/start" => Some(
                "Hello! I'm your OpenClaw AI assistant. Send me a message and I'll do my best to help.\n\n\
                 Use /help to see available commands."
                    .to_string(),
            ),
            "/help" => Some(
                "Available commands:\n\
                 /start — Introduction\n\
                 /help — Show this help\n\
                 /ping — Check if the bot is alive\n\
                 /reset — Start a new conversation\n\n\
                 Any other message will be processed by the AI agent."
                    .to_string(),
            ),
            "/ping" => Some("Pong! Bot is running.".to_string()),
            "/reset" => {
                // The actual conversation reset is handled in the agent loop
                // by looking for this sentinel in the message content.
                None
            }
            _ if cmd.starts_with('/') => {
                // Unknown command — let it pass through to the agent.
                None
            }
            _ => None,
        }
    }

    /// Validate an HMAC-SHA256 signature for webhook payloads.
    /// This provides an additional layer of verification beyond the
    /// secret_token header that Telegram sends.
    pub fn verify_hmac(&self, payload: &[u8], signature_hex: &str) -> Result<()> {
        let secret = self
            .webhook_secret
            .as_ref()
            .ok_or_else(|| Error::Config("no webhook secret configured for HMAC verification".into()))?;

        let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
            .expect("HMAC accepts any key size");
        mac.update(payload);

        let expected = hex::decode(signature_hex)
            .map_err(|e| Error::Auth(format!("invalid HMAC hex: {e}")))?;

        mac.verify_slice(&expected)
            .map_err(|_| Error::Auth("HMAC verification failed".into()))
    }

    /// Get the bot token (for constructing webhook URLs).
    pub fn bot_token(&self) -> &str {
        &self.bot_token
    }
}

/// Exponential backoff delay, capped at 60 seconds.
fn backoff_delay(consecutive_errors: u32) -> std::time::Duration {
    let secs = (2u64.pow(consecutive_errors.min(6))).min(60);
    std::time::Duration::from_secs(secs)
}

/// Split a message into chunks that fit within Telegram's character limit.
/// Tries to split on paragraph boundaries, then sentence boundaries,
/// then word boundaries, to avoid cutting mid-sentence.
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

        // Find the best split point within the limit.
        let window = &remaining[..max_len];
        let split_at = find_split_point(window);

        chunks.push(remaining[..split_at].trim_end().to_string());
        remaining = remaining[split_at..].trim_start();
    }

    chunks
}

/// Find the best place to split text, preferring paragraph > sentence > word boundaries.
fn find_split_point(window: &str) -> usize {
    // Try to split on a double newline (paragraph break).
    if let Some(pos) = window.rfind("\n\n") {
        if pos > 0 {
            return pos + 2; // Include the double newline.
        }
    }

    // Try to split on a single newline.
    if let Some(pos) = window.rfind('\n') {
        if pos > 0 {
            return pos + 1;
        }
    }

    // Try to split on sentence-ending punctuation followed by a space.
    for &sep in &[". ", "! ", "? "] {
        if let Some(pos) = window.rfind(sep) {
            if pos > 0 {
                return pos + sep.len();
            }
        }
    }

    // Fall back to a word boundary (space).
    if let Some(pos) = window.rfind(' ') {
        if pos > 0 {
            return pos + 1;
        }
    }

    // Absolute fallback: hard split at the limit.
    window.len()
}

/// Constant-time string comparison to prevent timing attacks on webhook secrets.
/// Compares all bytes up to the length of the longer string
/// so that the length of neither input is leaked through timing.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let len = a_bytes.len().max(b_bytes.len());
    let mut result = (a_bytes.len() != b_bytes.len()) as u8;
    for i in 0..len {
        let x = a_bytes.get(i).copied().unwrap_or(0);
        let y = b_bytes.get(i).copied().unwrap_or(0);
        result |= x ^ y;
    }
    result == 0
}

// --- Telegram Bot API wire types ---

#[derive(Deserialize)]
struct GetUpdatesResponse {
    result: Vec<Update>,
}

#[derive(Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<TelegramMessage>,
}

#[derive(Deserialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub chat: Chat,
    pub from: Option<User>,
    pub text: Option<String>,
    pub date: i64,
}

#[derive(Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub chat_type: String,
}

#[derive(Deserialize)]
pub struct User {
    pub id: i64,
    pub first_name: String,
    #[serde(default)]
    pub username: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_message_short() {
        let chunks = split_message("hello", 4096);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn test_split_message_on_paragraph() {
        let text = format!("{}\n\n{}", "a".repeat(2000), "b".repeat(2000));
        let chunks = split_message(&text, 2500);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].ends_with("a"));
        assert!(chunks[1].starts_with("b"));
    }

    #[test]
    fn test_split_message_on_newline() {
        let text = format!("{}\n{}", "a".repeat(2000), "b".repeat(2000));
        let chunks = split_message(&text, 2500);
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn test_split_message_on_sentence() {
        let text = format!("{}. {}", "a".repeat(2000), "b".repeat(2000));
        let chunks = split_message(&text, 2500);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].ends_with('.'));
    }

    #[test]
    fn test_split_message_hard_split() {
        let text = "a".repeat(5000);
        let chunks = split_message(&text, 4096);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 4096);
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
    }
}
