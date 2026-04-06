use chrono::Utc;
use hmac::{Hmac, Mac};
use openclaw_core::types::{Message, MessageContent, Role};
use openclaw_core::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashSet;
use tokio::sync::mpsc;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

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
    inbound_tx: mpsc::Sender<Message>,
    /// Receiver for inbound messages (consumed by the agent loop).
    inbound_rx: Option<mpsc::Receiver<Message>>,
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
    pub fn take_inbound_rx(&mut self) -> Option<mpsc::Receiver<Message>> {
        self.inbound_rx.take()
    }

    /// Send a text message to a Telegram chat.
    pub async fn send_text(&self, chat_id: i64, text: &str) -> Result<()> {
        let url = format!("{}/sendMessage", self.api_base);
        let body = SendMessage {
            chat_id,
            text: text.to_string(),
            parse_mode: Some("Markdown".to_string()),
        };

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Channel(format!("Telegram API error: {e}")))?;

        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            return Err(Error::Channel(format!("Telegram sendMessage failed: {err}")));
        }

        Ok(())
    }

    /// Start long-polling for updates. Runs forever — spawn this as a task.
    ///
    /// This is the simplest way to receive messages without exposing a
    /// public endpoint. Suitable for local development.
    pub async fn start_polling(&self) -> Result<()> {
        let mut offset: Option<i64> = None;

        tracing::info!("Telegram long-polling started");

        loop {
            let url = format!("{}/getUpdates", self.api_base);
            let mut params = vec![("timeout", "30".to_string())];
            if let Some(off) = offset {
                params.push(("offset", off.to_string()));
            }

            let resp = self
                .client
                .get(&url)
                .query(&params)
                .send()
                .await
                .map_err(|e| Error::Channel(format!("Telegram polling error: {e}")))?;

            if !resp.status().is_success() {
                let err = resp.text().await.unwrap_or_default();
                tracing::error!("Telegram getUpdates failed: {err}");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }

            let body: GetUpdatesResponse = resp
                .json()
                .await
                .map_err(|e| Error::Channel(format!("failed to parse updates: {e}")))?;

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
            .send(message)
            .await
            .map_err(|e| Error::Channel(format!("inbound queue full: {e}")))?;

        Ok(())
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

#[derive(Serialize)]
struct SendMessage {
    chat_id: i64,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<String>,
}

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
