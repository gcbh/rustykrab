use hmac::{Hmac, Mac};
use rustykrab_core::crypto::constant_time_eq;
use rustykrab_core::types::{Message, MessageContent, Role};
use rustykrab_core::{Error, Result};
use serde::Deserialize;
use sha2::Sha256;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

type HmacSha256 = Hmac<Sha256>;

/// Maximum text length Telegram allows in a single message.
const TELEGRAM_MAX_LENGTH: usize = 4096;

/// Maximum retries for sending a message before giving up.
const SEND_MAX_RETRIES: u32 = 3;

/// Commands the host answers before a message reaches the agent: the work
/// surface's `/work`, `/approve`, `/reject`, `/cancel` and `/answer` (plan
/// `docs/plans/control-layer-and-worker-fleet.md`, section 14.2). A command
/// handled here never enters a conversation, so an answer resumes the work
/// item that asked, not the chat.
#[async_trait::async_trait]
pub trait CommandHook: Send + Sync {
    /// The reply to `text` from `chat_id`, or `None` when the hook does not
    /// handle it and the message goes on to the agent.
    async fn handle(&self, text: &str, chat_id: i64, thread_id: i64) -> Option<String>;

    /// The commands it adds to `/help`, one line each.
    fn help(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A row of inline buttons: `(label, callback data)` per button.
pub type ButtonRow = Vec<(String, String)>;

/// The command a button press stands for: `approve:<id>` is `/approve <id>`,
/// `reject:<id>` is `/reject <id>`, `answer:<question>:<option>` is
/// `/answer <question> <option>`. Anything else is ignored.
pub fn command_for_button(data: &str) -> Option<String> {
    let (verb, rest) = data.split_once(':')?;
    match verb {
        "approve" | "reject" | "cancel" if !rest.is_empty() => Some(format!("/{verb} {rest}")),
        "answer" => {
            let (question, option) = rest.split_once(':')?;
            (!question.is_empty() && !option.is_empty())
                .then(|| format!("/answer {question} {option}"))
        }
        _ => None,
    }
}

/// An inbound message with channel-specific routing metadata.
pub struct ChannelMessage {
    pub chat_id: i64,
    /// Telegram forum topic thread ID. `0` means no thread (non-forum chat
    /// or the implicit "General" topic).
    pub thread_id: i64,
    pub message: Message,
    /// If true, the conversation for this chat should be reset.
    pub reset: bool,
}

/// Telegram Bot API channel.
///
/// Supports two modes:
/// - **Long-polling** (`start_polling`) — no public IP required, ideal for local dev
/// - **Webhook** (`parse_webhook_update`) — for production behind a reverse proxy
///
/// Security features (addressing original RustyKrab Telegram CVEs):
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
    /// Graceful shutdown flag.
    shutdown_flag: Arc<AtomicBool>,
    /// The host's commands (the work surface), consulted before a message
    /// reaches the agent.
    commands: Option<Arc<dyn CommandHook>>,
    /// Serialize message requests and preserve flood-control waits across callers
    /// and exhausted retry batches. A new outbox pass cannot bypass the wait.
    send_cooldown: Mutex<Option<tokio::time::Instant>>,
}

impl TelegramChannel {
    /// Create a new Telegram channel.
    ///
    /// `bot_token` is the token from @BotFather.
    /// `allowed_chats` restricts which Telegram chats can use the bot.
    pub fn new(bot_token: String, allowed_chats: HashSet<i64>) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            // `TELEGRAM_API_BASE` redirects the Bot API at a local stand-in so
            // harnesses can observe what the bot would have sent without
            // talking to Telegram. Unset in every real deployment, where this
            // is the documented api.telegram.org endpoint.
            api_base: match std::env::var("TELEGRAM_API_BASE") {
                Ok(base) if !base.trim().is_empty() => {
                    format!("{}/bot{bot_token}", base.trim_end_matches('/'))
                }
                _ => format!("https://api.telegram.org/bot{bot_token}"),
            },
            bot_token,
            allowed_chats,
            webhook_secret: None,
            inbound_tx: tx,
            inbound_rx: Some(rx),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            commands: None,
            send_cooldown: Mutex::new(None),
        }
    }

    /// Answer the host's commands before the agent sees a message.
    pub fn with_command_hook(mut self, hook: Arc<dyn CommandHook>) -> Self {
        self.commands = Some(hook);
        self
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

    /// Request graceful shutdown of the polling loop.
    pub fn shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
    }

    /// Send a "typing" chat action so the user sees the bot is working.
    ///
    /// When `thread_id > 0`, the typing indicator is scoped to that forum
    /// topic so it appears in the correct thread.
    pub async fn send_typing(&self, chat_id: i64, thread_id: i64) -> Result<()> {
        let url = format!("{}/sendChatAction", self.api_base);
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "action": "typing",
        });
        if thread_id > 0 {
            body["message_thread_id"] = serde_json::json!(thread_id);
        }

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
    /// When `thread_id > 0`, the message is posted inside that forum topic.
    /// Uses Markdown parse mode with automatic plain-text fallback if
    /// Telegram rejects the formatting.
    pub async fn send_text(&self, chat_id: i64, text: &str, thread_id: i64) -> Result<()> {
        self.send_text_with_receipts(chat_id, text, thread_id)
            .await
            .map(|_| ())
    }

    /// Bot-acknowledged message IDs for all chunks. An incomplete or malformed
    /// acknowledgement is an error; callers must not interpret HTTP 200 as delivery.
    pub async fn send_text_with_receipts(
        &self,
        chat_id: i64,
        text: &str,
        thread_id: i64,
    ) -> Result<Vec<i64>> {
        let mut ids = Vec::new();
        for chunk in split_message(text, TELEGRAM_MAX_LENGTH) {
            ids.push(self.send_single_message(chat_id, &chunk, thread_id).await?);
        }
        Ok(ids)
    }

    /// Send `text` with rows of inline buttons under it (plan previews'
    /// approve and reject, a question's options). Plain text, so ids and
    /// underscores in it are never read as Markdown. A text too long for one
    /// message goes out without buttons, with the commands it already
    /// carries.
    pub async fn send_text_with_buttons(
        &self,
        chat_id: i64,
        text: &str,
        thread_id: i64,
        buttons: &[ButtonRow],
    ) -> Result<()> {
        if buttons.is_empty() || text.len() > TELEGRAM_MAX_LENGTH {
            return self.send_text(chat_id, text, thread_id).await;
        }
        let keyboard: Vec<Vec<serde_json::Value>> = buttons
            .iter()
            .map(|row| {
                row.iter()
                    .map(
                        |(label, data)| serde_json::json!({ "text": label, "callback_data": data }),
                    )
                    .collect()
            })
            .collect();
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "reply_markup": { "inline_keyboard": keyboard },
        });
        if thread_id > 0 {
            body["message_thread_id"] = serde_json::json!(thread_id);
        }
        match self
            .try_send_body(&body, SEND_MAX_RETRIES, chat_id, thread_id)
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if error.to_string().contains("400") => {
                tracing::debug!("buttons refused; sending the text alone");
                self.send_text(chat_id, text, thread_id).await
            }
            Err(error) => Err(error),
        }
    }

    /// Acknowledge a button press, so the client stops its spinner.
    async fn answer_callback(&self, callback_id: &str, text: &str) {
        let url = format!("{}/answerCallbackQuery", self.api_base);
        let body = serde_json::json!({
            "callback_query_id": callback_id,
            "text": text.chars().take(190).collect::<String>(),
        });
        if let Err(e) = self.client.post(&url).json(&body).send().await {
            tracing::debug!("answerCallbackQuery failed (non-critical): {e}");
        }
    }

    /// Send a single message chunk with retry and Markdown fallback.
    async fn send_single_message(&self, chat_id: i64, text: &str, thread_id: i64) -> Result<i64> {
        // First attempt: with Markdown.
        match self
            .try_send(chat_id, text, Some("Markdown"), SEND_MAX_RETRIES, thread_id)
            .await
        {
            Ok(id) => Ok(id),
            Err(e) => {
                // If Markdown parsing failed (400 Bad Request), retry as plain text.
                let err_str = format!("{e}");
                if err_str.contains("400") || err_str.contains("parse") || err_str.contains("can't")
                {
                    tracing::debug!("Markdown rejected by Telegram, retrying as plain text");
                    self.try_send(chat_id, text, None, SEND_MAX_RETRIES, thread_id)
                        .await
                } else {
                    Err(e)
                }
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
        thread_id: i64,
    ) -> Result<i64> {
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
        });
        if let Some(mode) = parse_mode {
            body["parse_mode"] = serde_json::json!(mode);
        }
        if thread_id > 0 {
            body["message_thread_id"] = serde_json::json!(thread_id);
        }

        self.try_send_body(&body, max_retries, chat_id, thread_id)
            .await
    }

    /// The shared send path also protects inline-button sends from flooding.
    async fn try_send_body(
        &self,
        body: &serde_json::Value,
        max_retries: u32,
        chat_id: i64,
        thread_id: i64,
    ) -> Result<i64> {
        let mut last_err = None;
        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay = std::time::Duration::from_millis(500 * 2u64.pow(attempt - 1));
                tokio::time::sleep(delay).await;
            }

            match self.send_once(body).await {
                Ok((status, err_text)) => {
                    if status.is_success() {
                        let value: serde_json::Value =
                            serde_json::from_str(&err_text).map_err(|_| {
                                Error::Channel(
                                    "Telegram acknowledgement unreadable; delivery uncertain"
                                        .into(),
                                )
                            })?;
                        return acknowledged_message(&value, chat_id, thread_id);
                    }

                    // Don't retry client errors (except 429 rate limit).
                    if status.is_client_error() && status.as_u16() != 429 {
                        return Err(Error::Channel(format!(
                            "Telegram sendMessage failed ({status}): {err_text}"
                        )));
                    }

                    last_err = Some(Error::Channel(format!(
                        "Telegram sendMessage failed ({status}): {err_text}"
                    )));
                }
                Err(e) => last_err = Some(e),
            }

            if attempt < max_retries {
                tracing::debug!(attempt, "retrying Telegram sendMessage");
            }
        }

        Err(last_err.unwrap_or_else(|| Error::Channel("send failed after retries".into())))
    }

    /// Hold the send gate through the response so parallel callers see a 429
    /// before sending. Keep the deadline even when the final retry fails.
    async fn send_once(&self, body: &serde_json::Value) -> Result<(reqwest::StatusCode, String)> {
        let mut cooldown = self.send_cooldown.lock().await;
        if let Some(until) = *cooldown {
            tokio::time::sleep_until(until).await;
        }
        let url = format!("{}/sendMessage", self.api_base);
        let response = self
            .client
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|e| Error::Channel(format!("Telegram API error: {}", e.without_url())))?;
        let status = response.status();
        let retry_header = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let text = response.text().await.map_err(|e| {
            Error::Channel(format!("Telegram response unreadable: {}", e.without_url()))
        })?;
        if status.as_u16() == 429 {
            let delay = retry_delay(retry_header.as_deref(), &text);
            *cooldown = Some(
                tokio::time::Instant::now()
                    .checked_add(delay)
                    .ok_or_else(|| {
                        Error::Channel("Telegram retry delay exceeds the clock range".into())
                    })?,
            );
            tracing::warn!(
                delay_secs = delay.as_secs(),
                "Telegram rate limited, backing off"
            );
        }
        Ok((status, text))
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
            if self.shutdown_flag.load(Ordering::Relaxed) {
                tracing::info!("Telegram polling shutdown requested");
                return Ok(());
            }

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

            let raw = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    consecutive_errors += 1;
                    let delay = backoff_delay(consecutive_errors);
                    tracing::error!(
                        consecutive_errors,
                        delay_secs = delay.as_secs(),
                        "failed to read Telegram response body: {e}"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
            };

            // Only render the raw body when debug logging is actually on;
            // it is a per-poll allocation otherwise.
            if tracing::enabled!(tracing::Level::DEBUG) {
                tracing::debug!(
                    raw_json = %String::from_utf8_lossy(&raw),
                    "raw getUpdates response"
                );
            }

            let body: GetUpdatesResponse = match serde_json::from_slice(&raw) {
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
        // Validate webhook secret (required — reject if none configured).
        let secret = self.webhook_secret.as_ref().ok_or_else(|| {
            Error::Auth("no webhook secret configured — refusing unauthenticated payload".into())
        })?;
        let header = secret_header
            .ok_or_else(|| Error::Auth("missing X-Telegram-Bot-Api-Secret-Token header".into()))?;
        if !constant_time_eq(header, secret) {
            return Err(Error::Auth("invalid Telegram webhook secret".into()));
        }

        tracing::debug!(
            raw_json = %String::from_utf8_lossy(payload),
            "raw Telegram webhook payload"
        );

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
        if let Some(press) = update.callback_query {
            return self.handle_button(press).await;
        }
        let msg = match update.message {
            Some(m) => m,
            None => return Ok(()), // Ignore non-message updates (edits, etc.)
        };

        let chat_id = msg.chat.id;
        let thread_id = msg.message_thread_id.unwrap_or(0);

        tracing::debug!(
            update_id = update.update_id,
            chat_id,
            chat_type = %msg.chat.chat_type,
            chat_title = ?msg.chat.title,
            thread_id,
            user_id = msg.from.as_ref().map(|u| u.id),
            "parsed Telegram update IDs"
        );

        // Chat allowlist check.
        if !self.allowed_chats.is_empty() && !self.allowed_chats.contains(&chat_id) {
            tracing::warn!(
                chat_id,
                username = msg
                    .from
                    .as_ref()
                    .map(|u| u.username.as_deref().unwrap_or("unknown")),
                "message from disallowed chat, ignoring"
            );
            return Ok(());
        }

        // Extract text from the message. Telegram sends bare @mentions with
        // text: null and the mention in the entities array. Fall back to
        // caption for media messages.
        let has_mention = msg
            .entities
            .iter()
            .any(|e| e.entity_type == "mention" || e.entity_type == "text_mention");
        let text = match msg.text.or(msg.caption) {
            Some(t) => t,
            None if has_mention => {
                // Bare @mention with no other text — acknowledge and return.
                tracing::info!(
                    chat_id,
                    thread_id,
                    entities_count = msg.entities.len(),
                    "received bare @mention with no text body"
                );
                let _ = self
                    .send_text(
                        chat_id,
                        "Hi! You mentioned me — send a message and I'll help.",
                        thread_id,
                    )
                    .await;
                return Ok(());
            }
            None => {
                tracing::debug!(
                    chat_id,
                    thread_id,
                    entities_count = msg.entities.len(),
                    caption_entities_count = msg.caption_entities.len(),
                    "ignoring non-text message (no text or caption)"
                );
                return Ok(());
            }
        };

        // Handle bot commands before forwarding to the agent.
        if let Some(reply) = self.handle_command(&text, chat_id, thread_id).await {
            let _ = self.send_text(chat_id, &reply, thread_id).await;
            return Ok(());
        }

        let from = msg
            .from
            .as_ref()
            .and_then(|u| u.username.as_deref())
            .unwrap_or("unknown");

        tracing::info!(chat_id, thread_id, %from, "received Telegram message");

        let message = Message::stamped(Role::User, MessageContent::Text(text));

        self.inbound_tx
            .send(ChannelMessage {
                chat_id,
                thread_id,
                message,
                reset: false,
            })
            .await
            .map_err(|e| Error::Channel(format!("inbound queue full: {e}")))?;

        Ok(())
    }

    /// A button press: its command, from an allowed chat, answered like the
    /// command typed.
    async fn handle_button(&self, press: CallbackQuery) -> Result<()> {
        let Some(msg) = press.message else {
            return Ok(());
        };
        let chat_id = msg.chat.id;
        if !self.allowed_chats.is_empty() && !self.allowed_chats.contains(&chat_id) {
            tracing::warn!(chat_id, "button press from disallowed chat, ignoring");
            return Ok(());
        }
        let thread_id = msg.message_thread_id.unwrap_or(0);
        let Some(command) = press.data.as_deref().and_then(command_for_button) else {
            self.answer_callback(&press.id, "Unknown button.").await;
            return Ok(());
        };
        let reply = match &self.commands {
            Some(hook) => hook.handle(&command, chat_id, thread_id).await,
            None => None,
        };
        let reply = reply.unwrap_or_else(|| "That button is not available here.".to_string());
        self.answer_callback(&press.id, reply.lines().next().unwrap_or(""))
            .await;
        let _ = self.send_text(chat_id, &reply, thread_id).await;
        Ok(())
    }

    /// Handle built-in bot commands, then the host's. Returns Some(reply) if
    /// the command was handled, None if the message should be forwarded to
    /// the agent.
    async fn handle_command(&self, text: &str, chat_id: i64, thread_id: i64) -> Option<String> {
        let cmd = text.split_whitespace().next()?;
        match cmd {
            "/start" => Some(
                "Hello! I'm your RustyKrab AI assistant. Send me a message and I'll do my best to help.\n\n\
                 Use /help to see available commands."
                    .to_string(),
            ),
            "/help" => {
                let mut help = "Available commands:\n\
                 /start — Introduction\n\
                 /help — Show this help\n\
                 /ping — Check if the bot is alive\n\
                 /reset — Start a new conversation\n"
                    .to_string();
                if let Some(hook) = &self.commands {
                    for line in hook.help() {
                        help.push_str(&line);
                        help.push('\n');
                    }
                }
                help.push_str("\nAny other message will be processed by the AI agent.");
                Some(help)
            }
            "/ping" => Some("Pong! Bot is running.".to_string()),
            "/reset" => Some(
                "Conversation reset. Send a new message to start fresh.".to_string(),
            ),
            _ if cmd.starts_with('/') => match &self.commands {
                // The host's commands; anything it does not know passes
                // through to the agent.
                Some(hook) => hook.handle(text, chat_id, thread_id).await,
                None => None,
            },
            _ => None,
        }
    }

    /// Validate an HMAC-SHA256 signature for webhook payloads.
    /// This provides an additional layer of verification beyond the
    /// secret_token header that Telegram sends.
    pub fn verify_hmac(&self, payload: &[u8], signature_hex: &str) -> Result<()> {
        let secret = self.webhook_secret.as_ref().ok_or_else(|| {
            Error::Config("no webhook secret configured for HMAC verification".into())
        })?;

        let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
            .map_err(|e| Error::Config(format!("invalid HMAC key: {e}")))?;
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

        // Find the nearest char boundary at or before max_len to avoid
        // panicking on multi-byte UTF-8 characters.
        let safe_end = remaining.floor_char_boundary(max_len);
        if safe_end == 0 {
            // Single character larger than max_len (shouldn't happen with
            // reasonable limits, but handle gracefully).
            chunks.push(remaining.to_string());
            break;
        }

        // Find the best split point within the limit.
        let window = &remaining[..safe_end];
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

// --- Telegram Bot API wire types ---

#[derive(Deserialize)]
struct GetUpdatesResponse {
    result: Vec<Update>,
}

#[derive(Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<TelegramMessage>,
    /// A press of an inline button the bot sent.
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    #[serde(default)]
    pub data: Option<String>,
    /// The message the button was on, which names the chat.
    #[serde(default)]
    pub message: Option<TelegramMessage>,
}

#[derive(Deserialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub chat: Chat,
    pub from: Option<User>,
    pub text: Option<String>,
    /// Caption for media messages (photos, documents, etc.).
    #[serde(default)]
    pub caption: Option<String>,
    /// Entities in the text (mentions, commands, URLs, etc.).
    #[serde(default)]
    pub entities: Vec<MessageEntity>,
    /// Entities in the caption.
    #[serde(default)]
    pub caption_entities: Vec<MessageEntity>,
    pub date: i64,
    /// Forum topic thread ID. Present when the message belongs to a forum topic.
    #[serde(default)]
    pub message_thread_id: Option<i64>,
    /// Whether this message was sent inside a forum topic.
    #[serde(default)]
    pub is_topic_message: Option<bool>,
}

#[derive(Deserialize)]
pub struct MessageEntity {
    #[serde(rename = "type")]
    pub entity_type: String,
    pub offset: i64,
    pub length: i64,
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
    fn buttons_stand_for_the_commands_they_show() {
        assert_eq!(
            command_for_button("approve:abc-123").as_deref(),
            Some("/approve abc-123")
        );
        assert_eq!(
            command_for_button("reject:abc").as_deref(),
            Some("/reject abc")
        );
        assert_eq!(
            command_for_button("answer:q1:2").as_deref(),
            Some("/answer q1 2")
        );
        assert_eq!(command_for_button("answer:q1:"), None);
        assert_eq!(command_for_button("approve:"), None);
        assert_eq!(command_for_button("delete:everything"), None);
    }

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
}

/// Telegram publishes seconds in parameters.retry_after. Honor the greater
/// of that value and the HTTP Retry-After header; malformed/missing waits get
/// a conservative fallback rather than a tight loop.
fn retry_delay(header: Option<&str>, body: &str) -> Duration {
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let body_seconds = json["parameters"]["retry_after"]
        .as_u64()
        .filter(|n| *n > 0);
    let header_delay = header.and_then(|value| {
        value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .map(Duration::from_secs)
            .or_else(|| {
                chrono::DateTime::parse_from_rfc2822(value)
                    .ok()
                    .and_then(|date| {
                        (date.with_timezone(&chrono::Utc) - chrono::Utc::now())
                            .to_std()
                            .ok()
                    })
                    .filter(|delay| !delay.is_zero())
            })
    });
    body_seconds
        .map(Duration::from_secs)
        .into_iter()
        .chain(header_delay)
        .max()
        .unwrap_or(Duration::from_secs(5))
}

/// Validate the Bot API envelope and intended address without logging content.
fn acknowledged_message(value: &serde_json::Value, chat: i64, thread: i64) -> Result<i64> {
    if value["ok"] != true {
        let code = value["error_code"].as_i64().unwrap_or(0);
        let parse = value["description"]
            .as_str()
            .is_some_and(|s| s.contains("parse"));
        return Err(Error::Channel(format!(
            "Telegram sendMessage rejected ({code}){}",
            if parse { ": parse formatting" } else { "" }
        )));
    }
    let result = &value["result"];
    let id = result["message_id"].as_i64().filter(|id| *id > 0);
    if result["chat"]["id"].as_i64() != Some(chat)
        || (thread > 0 && result["message_thread_id"].as_i64() != Some(thread))
    {
        return Err(Error::Channel(
            "Telegram acknowledgement address mismatch; delivery uncertain".into(),
        ));
    }
    id.ok_or_else(|| {
        Error::Channel("Telegram acknowledgement has no message ID; delivery uncertain".into())
    })
}
#[cfg(test)]
mod acknowledgement_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn http_success_is_not_delivery_without_a_valid_bot_receipt() {
        for v in [
            json!({"ok":false,"error_code":400}),
            json!({"ok":true}),
            json!({"ok":true,"result":{"message_id":1,"chat":{"id":2}}}),
            json!({"ok":true,"result":{"message_id":0,"chat":{"id":1}}}),
        ] {
            assert!(acknowledged_message(&v, 1, 0).is_err());
        }
        let v = json!({"ok":true,"result":{"message_id":42,"chat":{"id":1},"message_thread_id":7}});
        assert_eq!(acknowledged_message(&v, 1, 7).unwrap(), 42);
        assert!(acknowledged_message(&v, 1, 8).is_err());
    }
}

#[cfg(test)]
mod rate_limit_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn retry_delays_use_the_server_wait_and_handle_bad_responses() {
        assert_eq!(
            retry_delay(None, r#"{"parameters":{"retry_after":60}}"#),
            Duration::from_secs(60)
        );
        assert_eq!(
            retry_delay(Some("120"), r#"{"parameters":{"retry_after":60}}"#),
            Duration::from_secs(120)
        );
        assert_eq!(
            retry_delay(Some("10"), r#"{"parameters":{"retry_after":60}}"#),
            Duration::from_secs(60)
        );
        for body in [
            "not json",
            "{}",
            r#"{"parameters":{"retry_after":-1}}"#,
            r#"{"parameters":{"retry_after":0}}"#,
        ] {
            assert_eq!(
                retry_delay(Some("bad header"), body),
                Duration::from_secs(5)
            );
        }
        let date = (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc2822();
        let wait = retry_delay(Some(&date), "{}");
        assert!(wait >= Duration::from_secs(28) && wait <= Duration::from_secs(30));
    }

    // A local HTTP stand-in records actual arrival times and bodies. No bot
    // token, live Telegram calls, model or environment mutation is involved.
    async fn server(
        replies: Vec<(u16, &'static str, &'static str)>,
    ) -> (
        TelegramChannel,
        tokio::task::JoinHandle<Vec<(std::time::Instant, serde_json::Value)>>,
    ) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut arrivals = Vec::new();
            for (status, header, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let (end, length) = loop {
                    let mut buffer = [0; 4096];
                    let n = socket.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buffer[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        if request.len() >= end + 4 + length {
                            break (end + 4, length);
                        }
                    }
                };
                arrivals.push((
                    std::time::Instant::now(),
                    serde_json::from_slice(&request[end..end + length]).unwrap(),
                ));
                let reply = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{header}\r\n{body}", body.len());
                socket.write_all(reply.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
            arrivals
        });
        let mut channel = TelegramChannel::new("fixture-token".into(), [1].into_iter().collect());
        channel.api_base = format!("http://{address}/botfixture-token");
        (channel, handle)
    }

    const OK: &str = r#"{"ok":true,"result":{"message_id":42,"chat":{"id":1}}}"#;
    const LIMITED: &str = r#"{"ok":false,"error_code":429,"parameters":{"retry_after":1}}"#;

    #[tokio::test]
    async fn text_retry_waits_for_telegram_before_sending_again() {
        let (channel, server) = server(vec![(429, "", LIMITED), (200, "", OK)]).await;
        channel
            .send_text(1, "A brief notification", 0)
            .await
            .unwrap();
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].0.duration_since(requests[0].0) >= Duration::from_secs(1));
        assert_eq!(requests[0].1["text"], requests[1].1["text"]);
    }

    #[tokio::test]
    async fn button_retry_preserves_buttons_and_honors_the_header() {
        let (channel, server) =
            server(vec![(429, "Retry-After: 2\r\n", LIMITED), (200, "", OK)]).await;
        let buttons = vec![vec![("Approve".into(), "approve:item".into())]];
        channel
            .send_text_with_buttons(1, "Approve this work", 0, &buttons)
            .await
            .unwrap();
        let requests = server.await.unwrap();
        assert!(requests[1].0.duration_since(requests[0].0) >= Duration::from_secs(2));
        assert_eq!(requests[0].1["reply_markup"], requests[1].1["reply_markup"]);
        assert!(requests[1].1["reply_markup"].is_object());
    }

    #[tokio::test]
    async fn exhausted_retry_cooldown_applies_to_other_concurrent_senders() {
        let (channel, server) =
            server(vec![(429, "", LIMITED), (200, "", OK), (200, "", OK)]).await;
        assert!(channel
            .try_send(1, "Failed batch", None, 0, 0)
            .await
            .is_err());
        let buttons = vec![vec![("Approve".into(), "approve:item".into())]];
        let (first, second) = tokio::join!(
            channel.send_text(1, "Next outbox pass", 0),
            channel.send_text_with_buttons(1, "Another sender", 0, &buttons),
        );
        first.unwrap();
        second.unwrap();
        let requests = server.await.unwrap();
        assert!(requests[1].0.duration_since(requests[0].0) >= Duration::from_secs(1));
        assert!(requests[2].0.duration_since(requests[0].0) >= Duration::from_secs(1));
    }
}
