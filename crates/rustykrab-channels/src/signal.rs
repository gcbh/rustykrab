use chrono::Utc;
use rustykrab_core::types::{Message, MessageContent, Role};
use rustykrab_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Signal channel via signal-cli-rest-api.
///
/// Connects to a local signal-cli-rest-api instance (typically running
/// in Docker) which bridges the Signal protocol to a REST interface.
///
/// Supports two modes:
/// - **Polling** (`start_polling`) — periodically fetches messages, no public URL needed
/// - **Webhook** (`parse_webhook_payload`) — signal-cli-rest-api pushes to our endpoint
///
/// Security:
/// - Phone number allowlist — only specified numbers can interact
/// - All traffic between RustyKrab and signal-cli stays on localhost
/// - Signal protocol provides E2E encryption on the wire
/// - Webhook payloads validated via shared secret header
pub struct SignalChannel {
    client: reqwest::Client,
    /// Base URL of the signal-cli-rest-api instance.
    base_url: String,
    /// The phone number this bot is registered as (E.164 format, e.g. +1234567890).
    account_number: String,
    /// Only these phone numbers may interact. Empty = deny all.
    allowed_numbers: HashSet<String>,
    /// Shared secret for webhook validation.
    webhook_secret: Option<String>,
    /// Sender for inbound messages (user -> agent).
    inbound_tx: mpsc::Sender<Message>,
    /// Receiver for inbound messages (consumed by the agent loop).
    inbound_rx: Option<mpsc::Receiver<Message>>,
}

impl SignalChannel {
    /// Create a new Signal channel.
    ///
    /// - `base_url`: URL of signal-cli-rest-api (e.g. `http://localhost:8080`)
    /// - `account_number`: your registered Signal number in E.164 format
    /// - `allowed_numbers`: set of phone numbers allowed to message the bot
    pub fn new(
        base_url: String,
        account_number: String,
        allowed_numbers: HashSet<String>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(256);
        Self {
            client: reqwest::Client::new(),
            base_url,
            account_number,
            allowed_numbers,
            webhook_secret: None,
            inbound_tx: tx,
            inbound_rx: Some(rx),
        }
    }

    /// Set a shared secret for webhook validation.
    pub fn with_webhook_secret(mut self, secret: String) -> Self {
        self.webhook_secret = Some(secret);
        self
    }

    /// Take the inbound receiver (can only be called once).
    pub fn take_inbound_rx(&mut self) -> Option<mpsc::Receiver<Message>> {
        self.inbound_rx.take()
    }

    /// Send a text message to a Signal recipient.
    pub async fn send_text(&self, recipient: &str, text: &str) -> Result<()> {
        let url = format!("{}/v2/send", self.base_url);
        let body = SendRequest {
            message: text.to_string(),
            number: self.account_number.clone(),
            recipients: vec![recipient.to_string()],
        };

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                Error::Channel(format!(
                    "Signal API error (is signal-cli-rest-api running at {}?): {e}",
                    self.base_url
                ))
            })?;

        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            return Err(Error::Channel(format!("Signal send failed: {err}")));
        }

        tracing::debug!(%recipient, "sent Signal message");
        Ok(())
    }

    /// Send a message to a Signal group.
    pub async fn send_to_group(&self, group_id: &str, text: &str) -> Result<()> {
        let url = format!("{}/v2/send", self.base_url);
        let body = serde_json::json!({
            "message": text,
            "number": self.account_number,
            "group_id": group_id,
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Channel(format!("Signal group send error: {e}")))?;

        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            return Err(Error::Channel(format!("Signal group send failed: {err}")));
        }

        Ok(())
    }

    /// Start polling for incoming messages. Runs forever — spawn as a task.
    ///
    /// Uses the signal-cli-rest-api `/v1/receive` endpoint which returns
    /// pending messages and marks them as read.
    pub async fn start_polling(&self) -> Result<()> {
        tracing::info!(
            account = %self.account_number,
            base_url = %self.base_url,
            "Signal polling started"
        );

        loop {
            match self.poll_once().await {
                Ok(count) => {
                    if count > 0 {
                        tracing::debug!(count, "processed Signal messages");
                    }
                }
                Err(e) => {
                    tracing::error!("Signal polling error: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }

            // signal-cli-rest-api doesn't support long-polling, so we poll on an interval.
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    /// Single poll iteration — fetch and process pending messages.
    async fn poll_once(&self) -> Result<usize> {
        let url = format!(
            "{}/v1/receive/{}",
            self.base_url,
            urlencoded(&self.account_number)
        );

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Channel(format!("Signal receive error: {e}")))?;

        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            return Err(Error::Channel(format!("Signal receive failed: {err}")));
        }

        let envelopes: Vec<Envelope> = resp
            .json()
            .await
            .map_err(|e| Error::Channel(format!("failed to parse Signal messages: {e}")))?;

        let mut count = 0;
        for envelope in envelopes {
            if let Err(e) = self.handle_envelope(envelope).await {
                tracing::warn!("failed to handle Signal message: {e}");
            } else {
                count += 1;
            }
        }

        Ok(count)
    }

    /// Parse and handle a webhook payload from signal-cli-rest-api.
    ///
    /// signal-cli-rest-api can be configured to POST incoming messages
    /// to a webhook URL. Call this from your axum handler.
    pub async fn parse_webhook_payload(
        &self,
        payload: &[u8],
        secret_header: Option<&str>,
    ) -> Result<()> {
        // Validate webhook secret (required — reject if none configured).
        let secret = self.webhook_secret.as_ref().ok_or_else(|| {
            Error::Auth("no webhook secret configured — refusing unauthenticated payload".into())
        })?;
        let header = secret_header
            .ok_or_else(|| Error::Auth("missing X-Signal-Webhook-Secret header".into()))?;
        if !constant_time_eq(header, secret) {
            return Err(Error::Auth("invalid Signal webhook secret".into()));
        }

        let envelope: Envelope = serde_json::from_slice(payload)?;
        self.handle_envelope(envelope).await
    }

    /// Process a single Signal envelope.
    async fn handle_envelope(&self, envelope: Envelope) -> Result<()> {
        let data_message = match envelope.data_message {
            Some(dm) => dm,
            None => return Ok(()), // Ignore non-data messages (receipts, typing, etc.)
        };

        let source = match &envelope.source {
            Some(s) => s.clone(),
            None => match &envelope.source_number {
                Some(n) => n.clone(),
                None => return Ok(()),
            },
        };

        // Phone number allowlist check.
        if !self.allowed_numbers.is_empty() && !self.allowed_numbers.contains(&source) {
            tracing::warn!(
                source = %source,
                "Signal message from disallowed number, ignoring"
            );
            return Ok(());
        }

        let text = match data_message.message {
            Some(t) if !t.is_empty() => t,
            _ => return Ok(()), // Ignore non-text messages (attachments, etc.)
        };

        tracing::info!(source = %source, "received Signal message");

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

    /// Check that signal-cli-rest-api is reachable and the account is registered.
    pub async fn health_check(&self) -> Result<()> {
        let url = format!("{}/v1/about", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| {
                Error::Channel(format!(
                    "cannot reach signal-cli-rest-api at {}: {e}",
                    self.base_url
                ))
            })?;

        if !resp.status().is_success() {
            return Err(Error::Channel(
                "signal-cli-rest-api health check failed".into(),
            ));
        }

        tracing::info!(base_url = %self.base_url, "signal-cli-rest-api is reachable");
        Ok(())
    }

    /// Register a webhook URL with signal-cli-rest-api.
    ///
    /// This configures signal-cli to POST incoming messages to the given URL
    /// instead of requiring polling.
    pub async fn register_webhook(&self, webhook_url: &str) -> Result<()> {
        let url = format!(
            "{}/v1/configuration/{}/settings",
            self.base_url,
            urlencoded(&self.account_number)
        );

        let body = serde_json::json!({
            "webhook": {
                "url": webhook_url
            }
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Channel(format!("failed to register Signal webhook: {e}")))?;

        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            return Err(Error::Channel(format!(
                "Signal webhook registration failed: {err}"
            )));
        }

        tracing::info!(%webhook_url, "Signal webhook registered");
        Ok(())
    }

    pub fn account_number(&self) -> &str {
        &self.account_number
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

/// Percent-encode a phone number for URL path segments.
fn urlencoded(s: &str) -> String {
    s.replace('+', "%2B")
}

// --- signal-cli-rest-api wire types ---

#[derive(Serialize)]
struct SendRequest {
    message: String,
    number: String,
    recipients: Vec<String>,
}

#[derive(Deserialize)]
pub struct Envelope {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default, rename = "sourceNumber")]
    pub source_number: Option<String>,
    #[serde(default, rename = "sourceName")]
    pub source_name: Option<String>,
    #[serde(default, rename = "dataMessage")]
    pub data_message: Option<DataMessage>,
}

#[derive(Deserialize)]
pub struct DataMessage {
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub timestamp: Option<i64>,
    #[serde(default, rename = "groupInfo")]
    pub group_info: Option<GroupInfo>,
}

#[derive(Deserialize)]
pub struct GroupInfo {
    #[serde(default, rename = "groupId")]
    pub group_id: Option<String>,
    #[serde(default, rename = "groupName")]
    pub group_name: Option<String>,
}
