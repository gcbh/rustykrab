use async_trait::async_trait;
use chrono::Utc;
use rustykrab_core::types::{Message, MessageContent, Role};
use rustykrab_core::Result;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::Channel;

/// In-process channel backed by tokio mpsc queues (used by the WebChat UI).
pub struct WebChatChannel {
    rx: mpsc::Receiver<String>,
    tx: mpsc::Sender<Message>,
}

/// Handle given to the HTTP/WebSocket layer to push user text and pull responses.
pub struct WebChatHandle {
    pub tx: mpsc::Sender<String>,
    pub rx: mpsc::Receiver<Message>,
}

/// Create a paired (channel, handle).
pub fn web_chat_pair(buffer: usize) -> (WebChatChannel, WebChatHandle) {
    let (user_tx, user_rx) = mpsc::channel(buffer);
    let (bot_tx, bot_rx) = mpsc::channel(buffer);
    (
        WebChatChannel {
            rx: user_rx,
            tx: bot_tx,
        },
        WebChatHandle {
            tx: user_tx,
            rx: bot_rx,
        },
    )
}

#[async_trait]
impl Channel for WebChatChannel {
    fn name(&self) -> &str {
        "webchat"
    }

    async fn receive(&self) -> Result<Message> {
        // We need &mut self for recv, but the trait takes &self.
        // In practice this will be wrapped in a Mutex or redesigned with streams.
        // For now, provide the shape of the API.
        unimplemented!("use WebChatHandle with a stream adapter in production")
    }

    async fn send(&self, message: &Message) -> Result<()> {
        self.tx
            .send(message.clone())
            .await
            .map_err(|e| rustykrab_core::Error::Channel(e.to_string()))
    }
}

impl WebChatChannel {
    /// Blocking receive (consumes &mut self).
    pub async fn recv(&mut self) -> Result<Message> {
        let text = self
            .rx
            .recv()
            .await
            .ok_or_else(|| rustykrab_core::Error::Channel("channel closed".into()))?;
        Ok(Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(text),
            created_at: Utc::now(),
        })
    }
}
