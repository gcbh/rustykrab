use std::sync::Arc;

use serde_json::{json, Value};
use uuid::Uuid;

use crate::MemorySystem;

/// Adapter that bridges [MemorySystem] to the `MemoryBackend` trait
/// used by the existing tool system in `rustykrab-tools`.
///
/// This allows the hybrid memory system to be used as a drop-in
/// replacement for the old tag-based `MemoryStore`, providing the same
/// tool interface (memory_save, memory_search, memory_get, memory_delete)
/// while transparently using vector search, FTS5, and lifecycle scoring.
///
/// The trait itself is defined in `rustykrab-tools::memory_backend`.
/// We re-implement it here structurally to avoid a circular dependency;
/// the gateway wires this into the tool system via a thin wrapper.
pub struct HybridMemoryBackend {
    system: Arc<MemorySystem>,
    agent_id: Uuid,
    session_id: Uuid,
    user_id: Option<Uuid>,
}

impl HybridMemoryBackend {
    pub fn new(system: Arc<MemorySystem>, agent_id: Uuid, session_id: Uuid) -> Self {
        Self {
            system,
            agent_id,
            session_id,
            user_id: None,
        }
    }

    /// Set the user ID for scoped retrieval.
    pub fn with_user_id(mut self, user_id: Uuid) -> Self {
        self.user_id = Some(user_id);
        self
    }

    /// Get the memory system reference (for auto-persist wiring).
    pub fn system(&self) -> &Arc<MemorySystem> {
        &self.system
    }

    /// Get the agent ID.
    pub fn agent_id(&self) -> Uuid {
        self.agent_id
    }

    /// Get the session ID.
    pub fn session_id(&self) -> Uuid {
        self.session_id
    }

    /// Search memories using hybrid retrieval (vector + FTS5 + graph + temporal).
    pub async fn search(
        &self,
        query: &str,
        _tags: &[String],
        limit: usize,
    ) -> rustykrab_core::Result<Value> {
        let results = self.system.recall(query, self.agent_id, limit).await?;

        let items: Vec<Value> = results
            .iter()
            .map(|r| {
                json!({
                    "id": r.memory_id.to_string(),
                    "content": r.content,
                    "score": r.effective_score,
                    "rrf_score": r.rrf_score,
                    "sources": r.sources.iter().map(|s| format!("{:?}", s)).collect::<Vec<_>>(),
                    "lifecycle_stage": format!("{:?}", r.memory.lifecycle_stage),
                    "scope": format!("{:?}", r.memory.scope),
                    "importance": r.memory.importance,
                    "access_count": r.memory.access_count,
                    "created_at": r.memory.created_at.to_rfc3339(),
                })
            })
            .collect();

        Ok(json!({
            "results": items,
            "count": items.len(),
        }))
    }

    /// Get a specific memory by ID.
    pub async fn get(&self, memory_id: &str) -> rustykrab_core::Result<Value> {
        let id = Uuid::parse_str(memory_id)
            .map_err(|e| rustykrab_core::Error::Internal(format!("invalid memory ID: {e}")))?;

        match self.system.get_memory(id).await? {
            Some(mem) => Ok(json!({
                "id": mem.id.to_string(),
                "content": mem.content,
                "importance": mem.importance,
                "lifecycle_stage": format!("{:?}", mem.lifecycle_stage),
                "scope": format!("{:?}", mem.scope),
                "access_count": mem.access_count,
                "tags": mem.tags,
                "created_at": mem.created_at.to_rfc3339(),
                "last_accessed_at": mem.last_accessed_at.map(|t| t.to_rfc3339()),
            })),
            None => Err(rustykrab_core::Error::NotFound(format!(
                "memory {memory_id}"
            ))),
        }
    }

    /// Save a fact with association tags, creating a new memory.
    pub async fn save(&self, fact: &str, tags: &[String]) -> rustykrab_core::Result<Value> {
        let memory_id = self
            .system
            .writer()
            .save_fact(self.agent_id, self.session_id, fact, tags)
            .await?;

        Ok(json!({
            "id": memory_id.to_string(),
            "status": "saved",
        }))
    }

    /// Delete (invalidate) a memory by ID.
    pub async fn delete(&self, memory_id: &str) -> rustykrab_core::Result<Value> {
        let id = Uuid::parse_str(memory_id)
            .map_err(|e| rustykrab_core::Error::Internal(format!("invalid memory ID: {e}")))?;

        self.system.invalidate_memory(id, None).await?;

        Ok(json!({
            "id": memory_id,
            "status": "deleted",
        }))
    }

    /// Finalize the current session, promoting all Working memories to Episodic.
    pub async fn finalize_session(&self) -> rustykrab_core::Result<Value> {
        let count = self
            .system
            .finalize_session(self.agent_id, self.session_id)
            .await?;

        Ok(json!({
            "session_id": self.session_id.to_string(),
            "promoted_to_episodic": count,
            "status": "finalized",
        }))
    }

    /// List all valid memories for the current agent.
    pub async fn list(&self) -> rustykrab_core::Result<Value> {
        let memories = self
            .system
            .storage()
            .list_retrievable(self.agent_id)
            .await?;

        let items: Vec<Value> = memories
            .iter()
            .map(|m| {
                json!({
                    "id": m.id.to_string(),
                    "content": m.content,
                    "importance": m.importance,
                    "lifecycle_stage": format!("{:?}", m.lifecycle_stage),
                    "scope": format!("{:?}", m.scope),
                    "access_count": m.access_count,
                    "tags": m.tags,
                    "created_at": m.created_at.to_rfc3339(),
                })
            })
            .collect();

        Ok(json!({
            "memories": items,
            "count": items.len(),
        }))
    }
}
