use async_trait::async_trait;
use rustykrab_core::Result;
use serde_json::Value;

#[async_trait]
pub trait MessageBackend: Send + Sync {
    async fn send_message(
        &self,
        channel: &str,
        text: &str,
        chat_id: Option<&str>,
        thread_id: Option<&str>,
    ) -> Result<Value>;
}
