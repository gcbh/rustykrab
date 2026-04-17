
    async fn handle_update(&self, update: Update) -> Result<()> {
        let msg = match update.message {
            Some(m) => m,
            None => return Ok(()), // Ignore non-message updates (edits, callbacks, etc.)
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
        if let Some(reply) = self.handle_command(&text, chat_id).await {
            let _ = self.send_text(chat_id, &reply, thread_id).await;
            return Ok(());
        }

        let from = msg
            .from
            .as_ref()
            .and_then(|u| u.username.as_deref())
            .unwrap_or("unknown");

        tracing::info!(chat_id, thread_id, %from, "received Telegram message");

        let message = Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(text),
            created_at: Utc::now(),
        };

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

    async fn handle_command(&self, text: &str, _chat_id: i64) -> Option<String> {
        let cmd = text.split_whitespace().next()?;
        match cmd {
            "/start" => Some(
                "Hello! I'm your RustyKrab AI assistant. Send me a message and I'll do my best to help.\n\n\
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
            "/reset" => Some(
                "Conversation reset. Send a new message to start fresh.".to_string(),
            ),
            _ if cmd.starts_with('/') => {
                // Unknown command — let it pass through to the agent.
                None
            }
            _ => None,
        }
    }

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

    pub fn bot_token(&self) -> &str {
        &self.bot_token
    }
}

fn backoff_delay(consecutive_errors: u32) -> std::time::Duration {
    let secs = (2u64.pow(consecutive_errors.min(6))).min(60);
    std::time::Duration::from_secs(secs)
}

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
}

#[derive(Deserialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub chat: Chat,
    pub from: Option<User>,
    pub text: Option<String>,
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub entities: Vec<MessageEntity>,
    #[serde(default)]
    pub caption_entities: Vec<MessageEntity>,
    pub date: i64,
    #[serde(default)]
    pub message_thread_id: Option<i64>,
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
