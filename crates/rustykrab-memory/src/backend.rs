use std::sync::Arc;

use serde_json::{json, Value};
use uuid::Uuid;

use crate::types::ExtractedFact;
use crate::MemorySystem;

/// Render an [`ExtractedFact`] as a compact JSON object for tool output.
///
/// Surfaces the structured triple plus type and confidence so the agent can
/// reason over stated preferences/decisions directly rather than re-parsing
/// the verbatim source text.
fn fact_to_json(fact: &ExtractedFact) -> Value {
    json!({
        "type": fact.fact_type,
        "subject": fact.subject,
        "predicate": fact.predicate,
        "object": fact.object,
        "confidence": fact.confidence,
    })
}

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
    ///
    /// When `tags` is non-empty, results are filtered to memories carrying at
    /// least one of the requested tags. Because the tag filter runs after
    /// retrieval, we over-fetch candidates so a tag-scoped search can still
    /// return up to `limit` rows.
    ///
    /// Each result also carries any extracted facts (e.g. stated preferences,
    /// decisions) attached to the source memory, so a caller searching for
    /// "what does the user prefer" surfaces the structured fact, not just the
    /// raw conversational text it was lifted from.
    pub async fn search(
        &self,
        query: &str,
        tags: &[String],
        limit: usize,
    ) -> rustykrab_core::Result<Value> {
        // Over-fetch when tag-filtering so the post-filter can still fill `limit`.
        let fetch = if tags.is_empty() {
            limit
        } else {
            (limit * 4).min(100)
        };
        let results = self.system.recall(query, self.agent_id, fetch).await?;

        let mut items: Vec<Value> = Vec::with_capacity(limit);
        for r in &results {
            // Tag filter: keep only memories carrying at least one requested tag.
            if !tags.is_empty() && !r.memory.tags.iter().any(|t| tags.contains(t)) {
                continue;
            }

            let mut item = json!({
                "id": r.memory_id.to_string(),
                "content": r.content,
                "score": r.effective_score,
                "rrf_score": r.rrf_score,
                "sources": r.sources.iter().map(|s| format!("{:?}", s)).collect::<Vec<_>>(),
                "lifecycle_stage": format!("{:?}", r.memory.lifecycle_stage),
                "scope": format!("{:?}", r.memory.scope),
                "importance": r.memory.importance,
                "access_count": r.memory.access_count,
                "tags": r.memory.tags,
                "created_at": r.memory.created_at.to_rfc3339(),
            });

            // Attach extracted facts when present. Best-effort: a facts lookup
            // failure must not sink an otherwise-good search result.
            let facts = self
                .system
                .storage()
                .get_facts_for_memory(r.memory_id)
                .await
                .unwrap_or_default();
            if !facts.is_empty() {
                item["facts"] = json!(facts.iter().map(fact_to_json).collect::<Vec<_>>());
            }

            items.push(item);
            if items.len() >= limit {
                break;
            }
        }

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
            Some(mem) => {
                let facts = self
                    .system
                    .storage()
                    .get_facts_for_memory(mem.id)
                    .await
                    .unwrap_or_default();
                Ok(json!({
                    "id": mem.id.to_string(),
                    "content": mem.content,
                    "importance": mem.importance,
                    "lifecycle_stage": format!("{:?}", mem.lifecycle_stage),
                    "scope": format!("{:?}", mem.scope),
                    "access_count": mem.access_count,
                    "tags": mem.tags,
                    "created_at": mem.created_at.to_rfc3339(),
                    "last_accessed_at": mem.last_accessed_at.map(|t| t.to_rfc3339()),
                    "facts": facts.iter().map(fact_to_json).collect::<Vec<_>>(),
                }))
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::MemoryConfig;
    use crate::embedding::HashEmbedder;
    use crate::storage::SqliteMemoryStorage;

    fn backend() -> HybridMemoryBackend {
        let storage = Arc::new(SqliteMemoryStorage::open_in_memory().unwrap());
        let embedder = Arc::new(HashEmbedder::new(768));
        let system = Arc::new(MemorySystem::new(
            MemoryConfig::default(),
            storage,
            embedder,
        ));
        HybridMemoryBackend::new(system, Uuid::new_v4(), Uuid::new_v4())
    }

    /// The `tags` argument is honored: a tag-scoped search must exclude
    /// memories that don't carry the requested tag, while an untagged search
    /// returns everything. This guards against the regression where `tags`
    /// was silently ignored.
    #[tokio::test]
    async fn search_filters_by_tag() {
        let backend = backend();
        backend
            .save("I prefer dark mode", &["ui".to_string()])
            .await
            .unwrap();
        backend
            .save("Deploy on Fridays", &["ops".to_string()])
            .await
            .unwrap();

        // Untagged search returns both memories.
        let all = backend.search("preferences", &[], 10).await.unwrap();
        assert_eq!(all["count"], 2, "untagged search should return both");

        // Tag-scoped search returns only the matching memory.
        let ui = backend
            .search("preferences", &["ui".to_string()], 10)
            .await
            .unwrap();
        assert_eq!(ui["count"], 1, "tag filter should drop the ops memory");
        let results = ui["results"].as_array().unwrap();
        assert_eq!(results[0]["content"], "I prefer dark mode");
        let tags = results[0]["tags"].as_array().unwrap();
        assert!(tags.iter().any(|t| t == "ui"));
    }
}
