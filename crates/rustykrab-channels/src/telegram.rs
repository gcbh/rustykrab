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
        let resp = self.client.post(&url).json(&body).send().await.map_err(|e| Error::Channel(format!("sendChatAction: {e}")))?;
        if !resp.status().is_success() { tracing::debug!("sendChatAction failed: {}", resp.text().await.unwrap_or_default()); }
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
        let file_data = tokio::fs::read(file_path).await.map_err(|e| Error::Channel(format!("read {}: {e}", file_path.display())))?;
        let file_name = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
        let mime = match file_path.extension().and_then(|e| e.to_str()).unwrap_or("") { "mp4"=>"video/mp4","webm"=>"video/webm","wav"=>"audio/wav","mp3"=>"audio/mpeg","png"=>"image/png","jpg"|"jpeg"=>"image/jpeg",_=>"application/octet-stream" };
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
        Err(last_err.unwrap_or_else(|| Error::Channel(format!("{api_method} failed"))))
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
        Err(last_err.unwrap_or_else(|| Error::Channel("send failed".into())))
    }

    pub async fn start_polling(&self) -> Result<()> {
        let _ = self.delete_webhook().await;
        let mut offset: Option<i64> = None; let mut errs: u32 = 0;
        tracing::info!("Telegram long-polling started");
        loop {
            if self.shutdown_flag.load(Ordering::Relaxed) { return Ok(()); }
            let url = format!("{}/getUpdates", self.api_base);
            let mut params = vec![("timeout","30".into())];
            if let Some(o) = offset { params.push(("offset", o.to_string())); }
            let resp = match self.client.get(&url).query(&params).send().await { Ok(r)=>r, Err(e)=>{ errs+=1; tokio::time::sleep(backoff_delay(errs)).await; tracing::error!(errs,"getUpdates: {e}"); continue; } };
            if !resp.status().is_success() { errs+=1; tokio::time::sleep(backoff_delay(errs)).await; continue; }
            let raw = match resp.text().await { Ok(t)=>t, Err(_)=>{ errs+=1; tokio::time::sleep(backoff_delay(errs)).await; continue; } };
            let body: GetUpdatesResponse = match serde_json::from_str(&raw) { Ok(b)=>b, Err(e)=>{ errs+=1; tokio::time::sleep(backoff_delay(errs)).await; tracing::error!(errs,"parse: {e}"); continue; } };
            errs = 0;
            for u in body.result { if let Some(n) = Some(u.update_id+1) { offset = Some(n); } let _ = self.handle_update(u).await; }
        }
    }

    pub async fn parse_webhook_update(&self, payload: &[u8], secret_header: Option<&str>) -> Result<()> {
        let secret = self.webhook_secret.as_ref().ok_or_else(|| Error::Auth("no webhook secret".into()))?;
        let header = secret_header.ok_or_else(|| Error::Auth("missing secret header".into()))?;
        if !constant_time_eq(header, secret) { return Err(Error::Auth("invalid secret".into())); }
        let update: Update = serde_json::from_slice(payload)?;
        self.handle_update(update).await
    }

    pub async fn set_webhook(&self, url: &str) -> Result<()> {
        let api_url = format!("{}/setWebhook", self.api_base);
        let mut body = serde_json::json!({"url": url});
        if let Some(ref s) = self.webhook_secret { body["secret_token"] = serde_json::json!(s); }
        let resp = self.client.post(&api_url).json(&body).send().await.map_err(|e| Error::Channel(format!("webhook: {e}")))?;
        if !resp.status().is_success() { return Err(Error::Channel(format!("setWebhook: {}", resp.text().await.unwrap_or_default()))); }
        tracing::info!(%url, "webhook registered"); Ok(())
    }

    pub async fn delete_webhook(&self) -> Result<()> {
        self.client.post(&format!("{}/deleteWebhook", self.api_base)).send().await.map_err(|e| Error::Channel(format!("deleteWebhook: {e}")))?; Ok(())
    }

    include!("telegram_ext.rs");
