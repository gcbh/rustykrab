
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
            "/help" => Some("Commands:\n/start — Intro\n/help — Help\n/ping — Status\n/reset — New conversation\n\nAnything else goes to the AI.".into()),
            "/ping" => Some("Pong!".into()),
            "/reset" => Some("Conversation reset.".into()),
            c if c.starts_with('/') => None,
            _ => None,
        }
    }

    pub fn verify_hmac(&self, payload: &[u8], signature_hex: &str) -> Result<()> {
        let secret = self.webhook_secret.as_ref().ok_or_else(|| Error::Config("no webhook secret".into()))?;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|e| Error::Config(format!("bad key: {e}")))?;
        mac.update(payload);
        let expected = hex::decode(signature_hex).map_err(|e| Error::Auth(format!("bad hex: {e}")))?;
        mac.verify_slice(&expected).map_err(|_| Error::Auth("HMAC failed".into()))
    }

    pub fn bot_token(&self) -> &str { &self.bot_token }
}

fn backoff_delay(n: u32) -> std::time::Duration { std::time::Duration::from_secs((2u64.pow(n.min(6))).min(60)) }

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
