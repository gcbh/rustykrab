mod channel;
pub mod mcp;
pub mod signal;
pub mod telegram;
pub mod video;
mod webchat;

pub use channel::Channel;
pub use mcp::McpClient;
pub use signal::{SignalChannel, SignalInboundMessage};
pub use telegram::TelegramChannel;
pub use video::{VideoChannel, VideoConfig};
pub use webchat::{web_chat_pair, WebChatChannel, WebChatHandle};
