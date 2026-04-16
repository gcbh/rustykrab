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

pub struct ChannelMessage { pub chat_id: i64, pub thread_id: i64, pub message: Message, pub reset: bool }

pub struct TelegramChannel {
    client: reqwest::Client, bot_token: String, api_base: String,
    allowed_chats: HashSet<i64>, webhook_secret: Option<String>,
    inbound_tx: mpsc::Sender<ChannelMessage>, inbound_rx: Option<mpsc::Receiver<ChannelMessage>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl TelegramChannel {
    pub fn new(bot_token: String, allowed_chats: HashSet<i64>) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let client = reqwest::Client::builder().timeout(Duration::from_secs(60)).connect_timeout(Duration::from_secs(10)).build().expect("failed to build HTTP client");
        Self { client, api_base: format!("https://api.telegram.org/bot{bot_token}"), bot_token, allowed_chats, webhook_secret: None, inbound_tx: tx, inbound_rx: Some(rx), shutdown_flag: Arc::new(AtomicBool::new(false)) }
    }
    pub fn with_webhook_secret(mut self, secret: String) -> Self { self.webhook_secret = Some(secret); self }
    pub fn take_inbound_rx(&mut self) -> Option<mpsc::Receiver<ChannelMessage>> { self.inbound_rx.take() }
    pub fn shutdown(&self) { self.shutdown_flag.store(true, Ordering::Relaxed); }

    pub async fn send_typing(&self, chat_id: i64, thread_id: i64) -> Result<()> {
        let url = format!("{}/sendChatAction", self.api_base);
        let mut body = serde_json::json!({"chat_id": chat_id, "action": "typing"});
        if thread_id > 0 { body["message_thread_id"] = serde_json::json!(thread_id); }
        let resp = self.client.post(&url).json(&body).send().await.map_err(|e| Error::Channel(format!("sendChatAction error: {e}")))?;
        if !resp.status().is_success() { let err = resp.text().await.unwrap_or_default(); tracing::debug!("sendChatAction failed: {err}"); }
        Ok(())
    }

    pub async fn send_text(&self, chat_id: i64, text: &str, thread_id: i64) -> Result<()> {
        for chunk in &split_message(text, TELEGRAM_MAX_LENGTH) { self.send_single_message(chat_id, chunk, thread_id).await?; }
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
        let mime = match file_path.extension().and_then(|e| e.to_str()).unwrap_or("") { "mp4"=>"video/mp4", "webm"=>"video/webm", "wav"=>"audio/wav", "mp3"=>"audio/mpeg", "png"=>"image/png", "jpg"|"jpeg"=>"image/jpeg", _=>"application/octet-stream" };
        let mut last_err = None;
        for attempt in 0..=SEND_MAX_RETRIES {
            if attempt > 0 { tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt - 1))).await; }
            let part = reqwest::multipart::Part::bytes(file_data.clone()).file_name(file_name.clone()).mime_str(mime).unwrap_or_else(|_| reqwest::multipart::Part::bytes(file_data.clone()).file_name(file_name.clone()));
            let mut form = reqwest::multipart::Form::new().text("chat_id", chat_id.to_string()).part(field_name.to_string(), part);
            if let Some(c) = caption { form = form.text("caption", c.to_string()); }
            if thread_id > 0 { form = form.text("message_thread_id", thread_id.to_string()); }
            match self.client.post(&url).multipart(form).send().await {
                Ok(resp) => { if resp.status().is_success() { return Ok(()); } let s = resp.status(); let e = resp.text().await.unwrap_or_default(); if s.is_client_error() && s.as_u16() != 429 { return Err(Error::Channel(format!("{api_method} failed ({s}): {e}"))); } last_err = Some(Error::Channel(format!("{api_method} failed ({s}): {e}"))); }
                Err(e) => { last_err = Some(Error::Channel(format!("API error: {e}"))); }
            }
        }
        Err(last_err.unwrap_or_else(|| Error::Channel(format!("{api_method} failed after retries"))))
    }

    async fn send_single_message(&self, chat_id: i64, text: &str, thread_id: i64) -> Result<()> {
        match self.try_send(chat_id, text, Some("Markdown"), SEND_MAX_RETRIES, thread_id).await {
            Ok(()) => Ok(()),
            Err(e) => { let s = format!("{e}"); if s.contains("400") || s.contains("parse") || s.contains("can't") { self.try_send(chat_id, text, None, SEND_MAX_RETRIES, thread_id).await } else { Err(e) } }
        }
    }

    async fn try_send(&self, chat_id: i64, text: &str, parse_mode: Option<&str>, max_retries: u32, thread_id: i64) -> Result<()> {
        let url = format!("{}/sendMessage", self.api_base);
        let mut body = serde_json::json!({"chat_id": chat_id, "text": text});
        if let Some(m) = parse_mode { body["parse_mode"] = serde_json::json!(m); }
        if thread_id > 0 { body["message_thread_id"] = serde_json::json!(thread_id); }
        let mut last_err = None;
        for attempt in 0..=max_retries {
            if attempt > 0 { tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt - 1))).await; }
            match self.client.post(&url).json(&body).send().await {
                Ok(resp) => { if resp.status().is_success() { return Ok(()); } let s = resp.status(); let e = resp.text().await.unwrap_or_default(); if s.is_client_error() && s.as_u16() != 429 { return Err(Error::Channel(format!("sendMessage failed ({s}): {e}"))); } if s.as_u16() == 429 { tracing::warn!("rate limited"); } last_err = Some(Error::Channel(format!("sendMessage failed ({s}): {e}"))); }
                Err(e) => { last_err = Some(Error::Channel(format!("API error: {e}"))); }
            }
        }
        Err(last_err.unwrap_or_else(|| Error::Channel("send failed after retries".into())))
    }

    pub async fn start_polling(&self) -> Result<()> {
        let _ = self.delete_webhook().await;
        let mut offset: Option<i64> = None;
        let mut errs: u32 = 0;
        tracing::info!("Telegram long-polling started");
        loop {
            if self.shutdown_flag.load(Ordering::Relaxed) { return Ok(()); }
            let url = format!("{}/getUpdates", self.api_base);
            let mut params = vec![("timeout", "30".to_string())];
            if let Some(o) = offset { params.push(("offset", o.to_string())); }
            let resp = match self.client.get(&url).query(&params).send().await { Ok(r) => r, Err(e) => { errs += 1; tokio::time::sleep(backoff_delay(errs)).await; tracing::error!(errs, "getUpdates error: {e}"); continue; } };
            if !resp.status().is_success() { errs += 1; let e = resp.text().await.unwrap_or_default(); tokio::time::sleep(backoff_delay(errs)).await; tracing::error!(errs, "getUpdates HTTP error: {e}"); continue; }
            let raw = match resp.text().await { Ok(t) => t, Err(e) => { errs += 1; tokio::time::sleep(backoff_delay(errs)).await; tracing::error!(errs, "body read error: {e}"); continue; } };
            let body: GetUpdatesResponse = match serde_json::from_str(&raw) { Ok(b) => b, Err(e) => { errs += 1; tokio::time::sleep(backoff_delay(errs)).await; tracing::error!(errs, "parse error: {e}"); continue; } };
            errs = 0;
            for update in body.result { if let Some(n) = Some(update.update_id + 1) { offset = Some(n); } let _ = self.handle_update(update).await; }
        }
    }

    pub async fn parse_webhook_update(&self, payload: &[u8], secret_header: Option<&str>) -> Result<()> {
        let secret = self.webhook_secret.as_ref().ok_or_else(|| Error::Auth("no webhook secret".into()))?;
        let header = secret_header.ok_or_else(|| Error::Auth("missing secret header".into()))?;
        if !constant_time_eq(header, secret) { return Err(Error::Auth("invalid webhook secret".into())); }
        let update: Update = serde_json::from_slice(payload)?;
        self.handle_update(update).await
    }

    pub async fn set_webhook(&self, url: &str) -> Result<()> {
        let api_url = format!("{}/setWebhook", self.api_base);
        let mut body = serde_json::json!({"url": url});
        if let Some(ref s) = self.webhook_secret { body["secret_token"] = serde_json::json!(s); }
        let resp = self.client.post(&api_url).json(&body).send().await.map_err(|e| Error::Channel(format!("set webhook: {e}")))?;
        if !resp.status().is_success() { let e = resp.text().await.unwrap_or_default(); return Err(Error::Channel(format!("setWebhook failed: {e}"))); }
        tracing::info!(%url, "webhook registered");
        Ok(())
    }

    pub async fn delete_webhook(&self) -> Result<()> {
        self.client.post(&format!("{}/deleteWebhook", self.api_base)).send().await.map_err(|e| Error::Channel(format!("deleteWebhook: {e}")))?;
        Ok(())
    }

    async fn handle_update(&self, update: Update) -> Result<()> {
        let msg = match update.message { Some(m) => m, None => return Ok(()) };
        let chat_id = msg.chat.id;
        let thread_id = msg.message_thread_id.unwrap_or(0);
        if !self.allowed_chats.is_empty() && !self.allowed_chats.contains(&chat_id) { tracing::warn!(chat_id, "disallowed chat"); return Ok(()); }
        let has_mention = msg.entities.iter().any(|e| e.entity_type == "mention" || e.entity_type == "text_mention");
        let text = match msg.text.or(msg.caption) {
            Some(t) => t,
            None if has_mention => { let _ = self.send_text(chat_id, "Hi! Send a message and I'll help.", thread_id).await; return Ok(()); }
            None => return Ok(()),
        };
        if let Some(reply) = self.handle_command(&text, chat_id).await { let _ = self.send_text(chat_id, &reply, thread_id).await; return Ok(()); }
        let from = msg.from.as_ref().and_then(|u| u.username.as_deref()).unwrap_or("unknown");
        tracing::info!(chat_id, thread_id, %from, "received message");
        let message = Message { id: Uuid::new_v4(), role: Role::User, content: MessageContent::Text(text), created_at: Utc::now() };
        self.inbound_tx.send(ChannelMessage { chat_id, thread_id, message, reset: false }).await.map_err(|e| Error::Channel(format!("queue full: {e}")))?;
        Ok(())
    }

    async fn handle_command(&self, text: &str, _chat_id: i64) -> Option<String> {
        match text.split_whitespace().next()? {
            "/start" => Some("Hello! I'm your RustyKrab AI assistant.\n\nUse /help for commands.".into()),
            "/help" => Some("Commands:\n/start — Intro\n/help — Help\n/ping — Status\n/reset — New conversation\n\nAnything else goes to the AI agent.".into()),
            "/ping" => Some("Pong!".into()),
            "/reset" => Some("Conversation reset.".into()),
            c if c.starts_with('/') => None,
            _ => None,
        }
    }

    pub fn verify_hmac(&self, payload: &[u8], signature_hex: &str) -> Result<()> {
        let secret = self.webhook_secret.as_ref().ok_or_else(|| Error::Config("no webhook secret".into()))?;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|e| Error::Config(format!("bad HMAC key: {e}")))?;
        mac.update(payload);
        let expected = hex::decode(signature_hex).map_err(|e| Error::Auth(format!("bad hex: {e}")))?;
        mac.verify_slice(&expected).map_err(|_| Error::Auth("HMAC verification failed".into()))
    }

    pub fn bot_token(&self) -> &str { &self.bot_token }
}

fn backoff_delay(n: u32) -> Duration { Duration::from_secs((2u64.pow(n.min(6))).min(60)) }

fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len { return vec![text.to_string()]; }
    let mut chunks = Vec::new();
    let mut rem = text;
    while !rem.is_empty() {
        if rem.len() <= max_len { chunks.push(rem.to_string()); break; }
        let end = rem.floor_char_boundary(max_len);
        if end == 0 { chunks.push(rem.to_string()); break; }
        let w = &rem[..end];
        let sp = w.rfind("\n\n").map(|p| p+2).or_else(|| w.rfind('\n').map(|p| p+1)).or_else(|| [". ","! ","? "].iter().filter_map(|s| w.rfind(s).map(|p| p+s.len())).next()).or_else(|| w.rfind(' ').map(|p| p+1)).unwrap_or(w.len());
        chunks.push(rem[..sp].trim_end().to_string());
        rem = rem[sp..].trim_start();
    }
    chunks
}

#[derive(Deserialize)] struct GetUpdatesResponse { result: Vec<Update> }
#[derive(Deserialize)] pub struct Update { pub update_id: i64, pub message: Option<TelegramMessage> }
#[derive(Deserialize)] pub struct TelegramMessage { pub message_id: i64, pub chat: Chat, pub from: Option<User>, pub text: Option<String>, #[serde(default)] pub caption: Option<String>, #[serde(default)] pub entities: Vec<MessageEntity>, #[serde(default)] pub caption_entities: Vec<MessageEntity>, pub date: i64, #[serde(default)] pub message_thread_id: Option<i64>, #[serde(default)] pub is_topic_message: Option<bool> }
#[derive(Deserialize)] pub struct MessageEntity { #[serde(rename="type")] pub entity_type: String, pub offset: i64, pub length: i64 }
#[derive(Deserialize)] pub struct Chat { pub id: i64, #[serde(default)] pub title: Option<String>, #[serde(rename="type")] pub chat_type: String }
#[derive(Deserialize)] pub struct User { pub id: i64, pub first_name: String, #[serde(default)] pub username: Option<String> }

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn test_split_short() { assert_eq!(split_message("hi", 4096), vec!["hi"]); }
    #[test] fn test_split_hard() { let t = "a".repeat(5000); let c = split_message(&t, 4096); assert_eq!(c.len(), 2); assert_eq!(c[0].len(), 4096); }
}
