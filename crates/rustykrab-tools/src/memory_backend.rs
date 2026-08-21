use async_trait::async_trait;
use rustykrab_core::Result;
use serde_json::Value;

#[async_trait]
pub trait MemoryBackend: Send + Sync {
    /// Search memories. `session_id` (a conversation id) restricts results to
    /// memories recorded during that conversation.
    async fn search(
        &self,
        query: &str,
        tags: &[String],
        limit: usize,
        session_id: Option<&str>,
    ) -> Result<Value>;
    async fn get(&self, memory_id: &str) -> Result<Value>;
    /// Save a fact with association tags. Returns the new memory ID.
    async fn save(&self, fact: &str, tags: &[String]) -> Result<Value>;
    /// Delete a memory by ID.
    async fn delete(&self, memory_id: &str) -> Result<Value>;
    /// List all memories for the current conversation.
    async fn list(&self) -> Result<Value>;
}
