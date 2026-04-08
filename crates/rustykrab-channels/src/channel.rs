use async_trait::async_trait;
use rustykrab_core::types::Message;
use rustykrab_core::Result;

/// A channel is an I/O boundary: it receives user messages and delivers
/// assistant responses (WebChat, Slack, Discord, etc.).
#[async_trait]
pub trait Channel: Send + Sync {
    /// Human-readable name of the channel.
    fn name(&self) -> &str;

    /// Wait for the next inbound message from this channel.
    async fn receive(&self) -> Result<Message>;

    /// Send an outbound message through this channel.
    async fn send(&self, message: &Message) -> Result<()>;
}
