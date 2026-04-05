use async_trait::async_trait;
use openclaw_core::Result;
use serde_json::Value;

#[async_trait]
pub trait MemoryBackend: Send + Sync {
    async fn search(&self, query: &str, tags: &[String], limit: usize) -> Result<Value>;
    async fn get(&self, memory_id: &str) -> Result<Value>;
}
