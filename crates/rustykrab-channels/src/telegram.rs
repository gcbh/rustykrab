use chrono::Utc;
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
use tokio::sync::mpsc;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

const TELEGRAM_MAX_LENGTH: usize = 4096;
const SEND_MAX_RETRIES: u32 = 3;

pub struct ChannelMessage {
    pub chat_id: i64,
    pub thread_id: i64,
    pub message: Message,
    pub reset: bool,
}

pub struct TelegramChannel {
    client: reqwest::Client,
    bot_token: String,
    api_base: String,
    allowed_chats: HashSet<i64>,
    webhook_secret: Option<String>,
    inbound_tx: mpsc::Sender<ChannelMessage>,
    inbound_rx: Option<mpsc::Receiver<ChannelMessage>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl TelegramChannel {
    pub fn new(bot_token: String, allowed_chats: HashSet<i64>) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            api_base: format!("https://api.telegram.org/bot{bot_token}"),
            bot_token,
            allowed_chats,
            webhook_secret: None,
            inbound_tx: tx,
            inbound_rx: Some(rx),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_webhook_secret(mut self, secret: String) -> Self {
        self.webhook_secret = Some(secret);
        self
    }

    pub fn take_inbound_rx(&mut self) -> Option<mpsc::Receiver<ChannelMessage>> {
        self.inbound_rx.take()
    }

    pub fn shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
    }

    pub async fn send_typing(&self, chat_id: i64, thread_id: i64) -> Result<()> {
        let url = format!("{}/sendChatAction", self.api_base);
        let mut body = serde_json::json!({"chat_id": chat_id, "action": "typing"});
        if thread_id > 0 { body["message_thread_id"] = serde_json::json!(thread_id); }
        let resp = self.client.post(&url).json(&body).send().await
            .map_err(|e| Error::Channel(format!("Telegram sendChatAction error: {e}")))?;
        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            tracing::debug!("sendChatAction failed (non-critical): {err}");
        }
        Ok(())
    }

    pub async fn send_text(&self, chat_id: i64, text: &str, thread_id: i64) -> Result<()> {
        let chunks = split_message(text, TELEGRAM_MAX_LENGTH);
        for chunk in &chunks { self.send_single_message(chat_id, chunk, thread_id).await?; }
        Ok(())
    }

    pub async fn send_video(&self, chat_id: i64, file_path: &std::path::Path, caption: Option<&str>, thread_id: i64) -> Result<()> {
        self.send_file(chat_id, file_path, "sendVideo", "video", caption, thread_id).await
    }

    pub async fn send_document(&self, chat_id: i64, file_path: &std::path::Path, caption: Option<&str>, thread_id: i64) -> Result<()> {
        self.send_file(chat_id, file_path, "sendDocument", "document", caption, thread_id).await
    }

    async fn send_file(&self, chat_id: i64, file_path: &std::path::Path, api_method: &str, field_name: &str, caption: Option<&str>, thread_id: i64) -> Result<()> {
        let url = format!("{}/{api_method}", self.api_base);
        let file_data = tokio::fs::read(file_path).await.map_err(|e| Error::Channel(format!("failed to read file {}: {e}", file_path.display())))?;
        let file_name = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
        let mime = match file_path.extension().and_then(|e| e.to_str()).unwrap_or("") {
            "mp4" => "video/mp4", "webm" => "video/webm", "wav" => "audio/wav",
            "mp3" => "audio/mpeg", "png" => "image/png", "jpg" | "jpeg" => "image/jpeg",
            _ => "application/octet-stream",
        };
        let mut last_err = None;
        for attempt in 0..=SEND_MAX_RETRIES {
            if attempt > 0 { tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt - 1))).await; }
            let file_part = reqwest::multipart::Part::bytes(file_data.clone()).file_name(file_name.clone()).mime_str(mime)
                .unwrap_or_else(|_| reqwest::multipart::Part::bytes(file_data.clone()).file_name(file_name.clone()));
            let mut form = reqwest::multipart::Form::new().text("chat_id", chat_id.to_string()).part(field_name.to_string(), file_part);
            if let Some(cap) = caption { form = form.text("caption", cap.to_string()); }
            if thread_id > 0 { form = form.text("message_thread_id", thread_id.to_string()); }
            match self.client.post(&url).multipart(form).send().await {
                Ok(resp) => {
                    if resp.status().is_success() { return Ok(()); }
                    let status = resp.status();
                    let err_text = resp.text().await.unwrap_or_default();
                    if status.is_client_error() && status.as_u16() != 429 {
                        return Err(Error::Channel(format!("Telegram {api_method} failed ({status}): {err_text}")));
                    }
                    if status.as_u16() == 429 { tracing::warn!("Telegram rate limited on {api_method}, backing off"); }
                    last_err = Some(Error::Channel(format!("Telegram {api_method} failed ({status}): {err_text}")));
                }
                Err(e) => { last_err = Some(Error::Channel(format!("Telegram API error: {e}"))); }
            }
            if attempt < SEND_MAX_RETRIES { tracing::debug!(attempt, "retrying Telegram {}", api_method); }
        }
        Err(last_err.unwrap_or_else(|| Error::Channel(format!("{api_method} failed after retries"))))
    }

    async fn send_single_message(&self, chat_id: i64, text: &str, thread_id: i64) -> Result<()> {
        match self.try_send(chat_id, text, Some("Markdown"), SEND_MAX_RETRIES, thread_id).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let err_str = format!("{e}");
                if err_str.contains("400") || err_str.contains("parse") || err_str.contains("can't") {
                    tracing::debug!("Markdown rejected by Telegram, retrying as plain text");
                    self.try_send(chat_id, text, None, SEND_MAX_RETRIES, thread_id).await
                } else { Err(e) }
            }
        }
    }

    async fn try_send(&self, chat_id: i64, text: &str, parse_mode: Option<&str>, max_retries: u32, thread_id: i64) -> Result<()> {
        let url = format!("{}/sendMessage", self.api_base);
        let mut body = serde_json::json!({"chat_id": chat_id, "text": text});
        if let Some(mode) = parse_mode { body["parse_mode"] = serde_json::json!(mode); }
        if thread_id > 0 { body["message_thread_id"] = serde_json::json!(thread_id); }
        let mut last_err = None;
        for attempt in 0..=max_retries {
            if attempt > 0 { tokio::time::sleep(std::time::Duration::from_millis(500 * 2u64.pow(attempt - 1))).await; }
            match self.client.post(&url).json(&body).send().await {
                Ok(resp) => {
                    if resp.status().is_success() { return Ok(()); }
                    let status = resp.status();
                    let err_text = resp.text().await.unwrap_or_default();
                    if status.is_client_error() && status.as_u16() != 429 {
                        return Err(Error::Channel(format!("Telegram sendMessage failed ({status}): {err_text}")));
                    }
                    if status.as_u16() == 429 { tracing::warn!("Telegram rate limited, backing off"); }
                    last_err = Some(Error::Channel(format!("Telegram sendMessage failed ({status}): {err_text}")));
                }
                Err(e) => { last_err = Some(Error::Channel(format!("Telegram API error: {e}"))); }
            }
            if attempt < max_retries { tracing::debug!(attempt, "retrying Telegram sendMessage"); }
        }
        Err(last_err.unwrap_or_else(|| Error::Channel("send failed after retries".into())))
    }

    pub async fn start_polling(&self) -> Result<()> {
        if let Err(e) = self.delete_webhook().await { tracing::warn!("failed to clear stale webhook (may not be set): {e}"); }
        let mut offset: Option<i64> = None;
        let mut consecutive_errors: u32 = 0;
        tracing::info!("Telegram long-polling started");
        loop {
            if self.shutdown_flag.load(Ordering::Relaxed) { tracing::info!("Telegram polling shutdown requested"); return Ok(()); }
            let url = format!("{}/getUpdates", self.api_base);
            let mut params = vec![("timeout", "30".to_string())];
            if let Some(off) = offset { params.push(("offset", off.to_string())); }
            let resp = match self.client.get(&url).query(&params).send().await {
                Ok(r) => r,
                Err(e) => { consecutive_errors += 1; let delay = backoff_delay(consecutive_errors); tracing::error!(consecutive_errors, delay_secs = delay.as_secs(), "Telegram getUpdates network error: {e}"); tokio::time::sleep(delay).await; continue; }
            };
            if !resp.status().is_success() { consecutive_errors += 1; let delay = backoff_delay(consecutive_errors); let err = resp.text().await.unwrap_or_default(); tracing::error!(consecutive_errors, delay_secs = delay.as_secs(), "Telegram getUpdates HTTP error: {err}"); tokio::time::sleep(delay).await; continue; }
            let raw_text = match resp.text().await {
                Ok(t) => t,
                Err(e) => { consecutive_errors += 1; let delay = backoff_delay(consecutive_errors); tracing::error!(consecutive_errors, delay_secs = delay.as_secs(), "failed to read Telegram response body: {e}"); tokio::time::sleep(delay).await; continue; }
            };
            tracing::debug!(raw_json = %raw_text, "raw getUpdates response");
            let body: GetUpdatesResponse = match serde_json::from_str(&raw_text) {
                Ok(b) => b,
                Err(e) => { consecutive_errors += 1; let delay = backoff_delay(consecutive_errors); tracing::error!(consecutive_errors, delay_secs = delay.as_secs(), "failed to parse Telegram updates: {e}"); tokio::time::sleep(delay).await; continue; }
            };
            consecutive_errors = 0;
            for update in body.result {
                if let Some(new_offset) = Some(update.update_id + 1) { offset = Some(new_offset); }
                if let Err(e) = self.handle_update(update).await { tracing::warn!("failed to handle Telegram update: {e}"); }
            }
        }
    }

    pub async fn parse_webhook_update(&self, payload: &[u8], secret_header: Option<&str>) -> Result<()> {
        let secret = self.webhook_secret.as_ref().ok_or_else(|| Error::Auth("no webhook secret configured — refusing unauthenticated payload".into()))?;
        let header = secret_header.ok_or_else(|| Error::Auth("missing X-Telegram-Bot-Api-Secret-Token header".into()))?;
        if !constant_time_eq(header, secret) { return Err(Error::Auth("invalid Telegram webhook secret".into())); }
        tracing::debug!(raw_json = %String::from_utf8_lossy(payload), "raw Telegram webhook payload");
        let update: Update = serde_json::from_slice(payload)?;
        self.handle_update(update).await
    }

    pub async fn set_webhook(&self, url: &str) -> Result<()> {
        let api_url = format!("{}/setWebhook", self.api_base);
        let mut body = serde_json::json!({ "url": url });
        if let Some(ref secret) = self.webhook_secret { body["secret_token"] = serde_json::json!(secret); }
        let resp = self.client.post(&api_url).json(&body).send().await.map_err(|e| Error::Channel(format!("failed to set webhook: {e}")))?;
        if !resp.status().is_success() { let err = resp.text().await.unwrap_or_default(); return Err(Error::Channel(format!("setWebhook failed: {err}"))); }
        tracing::info!(%url, "Telegram webhook registered");
        Ok(())
    }

    pub async fn delete_webhook(&self) -> Result<()> {
        let url = format!("{}/deleteWebhook", self.api_base);
        self.client.post(&url).send().await.map_err(|e| Error::Channel(format!("deleteWebhook failed: {e}")))?;
        Ok(())
    }

    async fn handle_update(&self, update: Update) -> Result<()> {
        let msg = match update.message { Some(m) => m, None => return Ok(()) };
        let chat_id = msg.chat.id;
        let thread_id = msg.message_thread_id.unwrap_or(0);
        tracing::debug!(update_id = update.update_id, chat_id, chat_type = %msg.chat.chat_type, chat_title = ?msg.chat.title, thread_id, user_id = msg.from.as_ref().map(|u| u.id), "parsed Telegram update IDs");
        if !self.allowed_chats.is_empty() && !self.allowed_chats.contains(&chat_id) {
            tracing::warn!(chat_id, username = msg.from.as_ref().map(|u| u.username.as_deref().unwrap_or("unknown")), "message from disallowed chat, ignoring");
            return Ok(());
        }
        let has_mention = msg.entities.iter().any(|e| e.entity_type == "mention" || e.entity_type == "text_mention");
        let text = match msg.text.or(msg.caption) {
            Some(t) => t,
            None if has_mention => {
                tracing::info!(chat_id, thread_id, entities_count = msg.entities.len(), "received bare @mention with no text body");
                let _ = self.send_text(chat_id, "Hi! You mentioned me — send a message and I'll help.", thread_id).await;
                return Ok(());
            }
            None => {
                tracing::debug!(chat_id, thread_id, entities_count = msg.entities.len(), caption_entities_count = msg.caption_entities.len(), "ignoring non-text message (no text or caption)");
                return Ok(());
            }
        };
        if let Some(reply) = self.handle_command(&text, chat_id).await { let _ = self.send_text(chat_id, &reply, thread_id).await; return Ok(()); }
        let from = msg.from.as_ref().and_then(|u| u.username.as_deref()).unwrap_or("unknown");
        tracing::info!(chat_id, thread_id, %from, "received Telegram message");
        let message = Message { id: Uuid::new_v4(), role: Role::User, content: MessageContent::Text(text), created_at: Utc::now() };
        self.inbound_tx.send(ChannelMessage { chat_id, thread_id, message, reset: false }).await.map_err(|e| Error::Channel(format!("inbound queue full: {e}")))?;
        Ok(())
    }

    async fn handle_command(&self, text: &str, _chat_id: i64) -> Option<String> {
        let cmd = text.split_whitespace().next()?;
        match cmd {
            "/start" => Some("Hello! I'm your RustyKrab AI assistant. Send me a message and I'll do my best to help.\n\nUse /help to see available commands.".to_string()),
            "/help" => Some("Available commands:\n/start — Introduction\n/help — Show this help\n/ping — Check if the bot is alive\n/reset — Start a new conversation\n\nAny other message will be processed by the AI agent.".to_string()),
            "/ping" => Some("Pong! Bot is running.".to_string()),
            "/reset" => Some("Conversation reset. Send a new message to start fresh.".to_string()),
            _ if cmd.starts_with('/') => None,
            _ => None,
        }
    }

    pub fn verify_hmac(&self, payload: &[u8], signature_hex: &str) -> Result<()> {
        let secret = self.webhook_secret.as_ref().ok_or_else(|| Error::Config("no webhook secret configured for HMAC verification".into()))?;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|e| Error::Config(format!("invalid HMAC key: {e}")))?;
        mac.update(payload);
        let expected = hex::decode(signature_hex).map_err(|e| Error::Auth(format!("invalid HMAC hex: {e}")))?;
        mac.verify_slice(&expected).map_err(|_| Error::Auth("HMAC verification failed".into()))
    }

    pub fn bot_token(&self) -> &str { &self.bot_token }
}

fn backoff_delay(consecutive_errors: u32) -> std::time::Duration {
    let secs = (2u64.pow(consecutive_errors.min(6))).min(60);
    std::time::Duration::from_secs(secs)
}

fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len { return vec![text.to_string()]; }
    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.len() <= max_len { chunks.push(remaining.to_string()); break; }
        let safe_end = remaining.floor_char_boundary(max_len);
        if safe_end == 0 { chunks.push(remaining.to_string()); break; }
        let window = &remaining[..safe_end];
        let split_at = find_split_point(window);
        chunks.push(remaining[..split_at].trim_end().to_string());
        remaining = remaining[split_at..].trim_start();
    }
    chunks
}

fn find_split_point(window: &str) -> usize {
    if let Some(pos) = window.rfind("\n\n") { if pos > 0 { return pos + 2; } }
    if let Some(pos) = window.rfind('\n') { if pos > 0 { return pos + 1; } }
    for &sep in &[". ", "! ", "? "] { if let Some(pos) = window.rfind(sep) { if pos > 0 { return pos + sep.len(); } } }
    if let Some(pos) = window.rfind(' ') { if pos > 0 { return pos + 1; } }
    window.len()
}

#[derive(Deserialize)] struct GetUpdatesResponse { result: Vec<Update> }
#[derive(Deserialize)] pub struct Update { pub update_id: i64, pub message: Option<TelegramMessage> }
#[derive(Deserialize)] pub struct TelegramMessage { pub message_id: i64, pub chat: Chat, pub from: Option<User>, pub text: Option<String>, #[serde(default)] pub caption: Option<String>, #[serde(default)] pub entities: Vec<MessageEntity>, #[serde(default)] pub caption_entities: Vec<MessageEntity>, pub date: i64, #[serde(default)] pub message_thread_id: Option<i64>, #[serde(default)] pub is_topic_message: Option<bool> }
#[derive(Deserialize)] pub struct MessageEntity { #[serde(rename = "type")] pub entity_type: String, pub offset: i64, pub length: i64 }
#[derive(Deserialize)] pub struct Chat { pub id: i64, #[serde(default)] pub title: Option<String>, #[serde(rename = "type")] pub chat_type: String }
#[derive(Deserialize)] pub struct User { pub id: i64, pub first_name: String, #[serde(default)] pub username: Option<String> }

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn test_split_message_short() { let chunks = split_message("hello", 4096); assert_eq!(chunks, vec!["hello"]); }
    #[test] fn test_split_message_on_paragraph() { let text = format!("{}\n\n{}", "a".repeat(2000), "b".repeat(2000)); let chunks = split_message(&text, 2500); assert_eq!(chunks.len(), 2); }
    #[test] fn test_split_message_hard_split() { let text = "a".repeat(5000); let chunks = split_message(&text, 4096); assert_eq!(chunks.len(), 2); assert_eq!(chunks[0].len(), 4096); }
}
