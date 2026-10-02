//! The memory surface the `memory_*` tools call.
//!
//! The trait lives here rather than beside the tools that consume it so the
//! crate that *owns* agent memory can implement it. `rustykrab-memory` sits
//! below `rustykrab-tools` in the graph, so while this trait was declared in
//! the tool crate it was unimplementable there: the memory crate carried a
//! structurally identical but unrelated method set, and the binary bridged
//! the two with a pass-through adapter.
//!
//! Arguments arrive as they came off the wire — ids are `&str`, not `Uuid` —
//! because parsing them is a judgement the implementation makes. An id that
//! does not parse is a model mistake, and how to answer it (fail, or widen
//! the search and say so) belongs with the thing that knows what widening
//! costs.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::Result;

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
