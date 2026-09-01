use std::sync::Arc;

use rustykrab_core::active_tools::with_session_context;
use rustykrab_core::retrieval_log::RetrievalLog;
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
    /// Scope for writes made outside any conversation — a background task,
    /// or a caller that constructed the backend directly. Minted once per
    /// process by the CLI.
    ///
    /// It is **not** the scope of a write made while serving a turn. That is
    /// the conversation, read from the ambient tool context by
    /// [`Self::write_scope`]. The two were the same field once, and the
    /// consequences are described there.
    fallback_scope: Uuid,
    user_id: Option<Uuid>,
    /// Records which memories were handed to the model, so a later outcome
    /// can be attributed to them. See `DREAMING.md`.
    retrieval_log: Option<RetrievalLog>,
}

impl HybridMemoryBackend {
    /// `fallback_scope` is used only for writes made outside a conversation.
    /// A write made while serving a turn is scoped to that conversation —
    /// see [`Self::write_scope`].
    pub fn new(system: Arc<MemorySystem>, agent_id: Uuid, fallback_scope: Uuid) -> Self {
        Self {
            system,
            agent_id,
            fallback_scope,
            user_id: None,
            retrieval_log: None,
        }
    }

    /// The session a write belongs to.
    ///
    /// `memories.session_id` is read back as a conversation id: the gateway
    /// persists every turn under `conversation.id`, and `search` filters on
    /// a conversation id supplied by the caller. `memory_save` was the one
    /// writer that used something else — the id minted once per process when
    /// the backend was constructed — so a fact the agent deliberately saved
    /// was stamped with a scope no session-scoped search could ever match,
    /// and only ever surfaced through unscoped search.
    ///
    /// The conversation is read from the ambient tool context, which is the
    /// same source `search` already uses to record retrievals. Outside a
    /// runner scope there is no conversation, and the construction-time id
    /// stands in.
    fn write_scope(&self) -> Uuid {
        with_session_context(|ctx| ctx.conversation_id).unwrap_or(self.fallback_scope)
    }

    /// Set the user ID for scoped retrieval.
    pub fn with_user_id(mut self, user_id: Uuid) -> Self {
        self.user_id = Some(user_id);
        self
    }

    /// Record surfaced memories into a shared log for outcome attribution.
    pub fn with_retrieval_log(mut self, log: RetrievalLog) -> Self {
        self.retrieval_log = Some(log);
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

    /// The scope used for writes made outside any conversation.
    pub fn fallback_scope(&self) -> Uuid {
        self.fallback_scope
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
    /// When `session_id` is given, results are restricted to memories written
    /// during that conversation. The filter runs inside retrieval, before
    /// access recording. An empty scoped result falls back to a labeled
    /// global search: facts saved via `memory_save` carry the daemon's
    /// session id, not the conversation's, and an unlabeled empty result
    /// would teach the model that a fact it saved earlier does not exist.
    pub async fn search(
        &self,
        query: &str,
        tags: &[String],
        limit: usize,
        session_id: Option<Uuid>,
    ) -> rustykrab_core::Result<Value> {
        // Over-fetch when tag-filtering so the post-filter can still fill `limit`.
        let fetch = if tags.is_empty() {
            limit
        } else {
            (limit * 4).min(100)
        };
        let mut scope_note: Option<&'static str> = None;
        let results = match session_id {
            Some(sid) => {
                let scoped = self
                    .system
                    .recall_in_session(query, self.agent_id, fetch, sid)
                    .await?;
                if scoped.is_empty() {
                    // Widening is still worth doing — memories from earlier
                    // conversations are often what the agent wants — but say
                    // so, or the model will attribute them to this one.
                    //
                    // This note used to add "(explicitly saved facts are
                    // stored globally)". That was true, and it was the bug:
                    // `memory_save` stamped facts with a process-wide id
                    // rather than the conversation, so a scoped search could
                    // never match one and always landed here. See
                    // `write_scope`.
                    scope_note =
                        Some("no matches within this conversation; showing global results");
                    self.system.recall(query, self.agent_id, fetch).await?
                } else {
                    scoped
                }
            }
            None => self.system.recall(query, self.agent_id, fetch).await?,
        };

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

        // Attribution: log only what survived filtering and the limit —
        // a memory that was recalled but never handed to the model did not
        // contribute to whatever happens next. The conversation id comes
        // from the enclosing runner scope; outside one there is nothing to
        // attribute to, so recording is skipped.
        if let Some(log) = self.retrieval_log.as_ref() {
            let surfaced: Vec<Uuid> = items
                .iter()
                .filter_map(|i| i.get("id").and_then(|v| v.as_str()))
                .filter_map(|s| Uuid::parse_str(s).ok())
                .collect();
            if !surfaced.is_empty() {
                if let Some(conversation_id) = with_session_context(|ctx| ctx.conversation_id) {
                    log.record(conversation_id, surfaced);
                }
            }
        }

        let mut response = json!({
            "results": items,
            "count": items.len(),
        });
        if let Some(note) = scope_note {
            response["session_scope"] = json!(note);
        }
        Ok(response)
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
        // Pre-check admission here so the tool response can carry the
        // per-cause reason (the writer's gate only reports "refused").
        if let Err(rejection) = crate::admission::admit(fact) {
            return Ok(json!({
                "status": "rejected",
                "reason": rejection.reason(),
            }));
        }
        match self
            .system
            .writer()
            .save_fact(self.agent_id, self.write_scope(), fact, tags)
            .await?
        {
            Some(memory_id) => Ok(json!({
                "id": memory_id.to_string(),
                "status": "saved",
            })),
            None => Ok(json!({
                "status": "rejected",
                "reason": "content was refused by memory admission control",
            })),
        }
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

    /// Finalize the current scope, promoting its Working memories to Episodic.
    pub async fn finalize_session(&self) -> rustykrab_core::Result<Value> {
        let scope = self.write_scope();
        let count = self.system.finalize_session(self.agent_id, scope).await?;

        Ok(json!({
            "session_id": scope.to_string(),
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

    /// Build the ambient tool context a runner installs around a tool call,
    /// so a test can exercise the in-a-conversation path.
    fn session_context(conversation_id: Uuid) -> rustykrab_core::SessionToolContext {
        rustykrab_core::SessionToolContext {
            conversation_id,
            capabilities: Arc::new(rustykrab_core::CapabilitySet::none()),
            all_tools: Arc::new(Vec::new()),
            active_tools: Arc::new(rustykrab_core::ActiveToolsRegistry::new()),
            recall: Arc::new(rustykrab_core::RecallStore::new()),
            todos: Arc::new(rustykrab_core::TodoStore::new()),
        }
    }

    /// The bug: `memory_save` stamped every fact with the id minted when the
    /// backend was constructed — once per process — while `search` filters on
    /// a conversation id. No scoped search could ever match one.
    ///
    /// It never surfaced as "the fact is missing", because `search` falls
    /// back to a global sweep when the scoped one comes back empty. It
    /// surfaced as every scoped search silently widening — and the note
    /// explaining the widening had been updated to describe the bug as
    /// intended behaviour.
    ///
    /// So the assertion is on the note, not the count: the fact must be found
    /// *within* the conversation, not by giving up on the scope.
    #[tokio::test]
    async fn a_saved_fact_is_found_within_its_own_conversation() {
        let backend = backend();
        let conversation = Uuid::new_v4();

        rustykrab_core::SESSION_TOOL_CONTEXT
            .scope(session_context(conversation), async {
                backend.save("I prefer dark mode", &[]).await.unwrap();
            })
            .await;

        let scoped = backend
            .search("dark mode", &[], 10, Some(conversation))
            .await
            .unwrap();

        assert_eq!(scoped["count"], 1);
        assert!(
            scoped.get("session_scope").is_none(),
            "the fact must be found inside the conversation, not by widening \
             to a global search: {scoped}"
        );
    }

    /// The complement: from another conversation the fact is still reachable,
    /// but only by widening, and the response says so. Unchanged behaviour —
    /// pinned here because it is what makes the assertion above meaningful.
    #[tokio::test]
    async fn another_conversation_reaches_it_only_by_widening() {
        let backend = backend();
        let mine = Uuid::new_v4();

        rustykrab_core::SESSION_TOOL_CONTEXT
            .scope(session_context(mine), async {
                backend.save("I prefer dark mode", &[]).await.unwrap();
            })
            .await;

        let other = backend
            .search("dark mode", &[], 10, Some(Uuid::new_v4()))
            .await
            .unwrap();

        assert_eq!(other["count"], 1);
        assert!(
            other["session_scope"]
                .as_str()
                .is_some_and(|n| n.contains("global")),
            "a cross-conversation hit must be labelled as one: {other}"
        );
    }

    /// Outside a runner there is no conversation, and the write still has to
    /// land somewhere — the construction-time scope stands in.
    #[tokio::test]
    async fn a_save_outside_a_conversation_falls_back_to_the_construction_scope() {
        let backend = backend();
        backend.save("Deploy on Fridays", &[]).await.unwrap();

        let scoped = backend
            .search("deploy", &[], 10, Some(backend.fallback_scope()))
            .await
            .unwrap();
        assert_eq!(scoped["count"], 1);
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
        let all = backend.search("preferences", &[], 10, None).await.unwrap();
        assert_eq!(all["count"], 2, "untagged search should return both");

        // Tag-scoped search returns only the matching memory.
        let ui = backend
            .search("preferences", &["ui".to_string()], 10, None)
            .await
            .unwrap();
        assert_eq!(ui["count"], 1, "tag filter should drop the ops memory");
        let results = ui["results"].as_array().unwrap();
        assert_eq!(results[0]["content"], "I prefer dark mode");
        let tags = results[0]["tags"].as_array().unwrap();
        assert!(tags.iter().any(|t| t == "ui"));
    }
}

#[cfg(test)]
mod session_scope_tests {
    use super::*;
    use crate::embedding::ZeroEmbedder;
    use crate::storage::SqliteMemoryStorage;
    use crate::types::{ConversationTurn, TurnMetadata};
    use crate::{MemoryConfig, MemorySystem};

    fn system() -> Arc<MemorySystem> {
        Arc::new(MemorySystem::new(
            MemoryConfig::default(),
            Arc::new(SqliteMemoryStorage::open_in_memory().unwrap()),
            Arc::new(ZeroEmbedder::new(8)),
        ))
    }

    fn turn(session_id: Uuid, content: &str) -> ConversationTurn {
        ConversationTurn {
            id: Uuid::new_v4(),
            session_id,
            turn_number: 0,
            speaker: "user".to_string(),
            content: content.to_string(),
            token_count: None,
            metadata: TurnMetadata::default(),
        }
    }

    /// A session-scoped search must not bump access_count or reset the decay
    /// clock on memories belonging to other conversations — the filter runs
    /// inside retrieval, before access recording.
    #[tokio::test]
    async fn scoped_search_does_not_touch_other_sessions_access_counts() {
        let sys = system();
        let agent = Uuid::new_v4();
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();

        let a = sys
            .retain(turn(mine, "Palermo trip departs on October 3rd."), agent)
            .await
            .unwrap()
            .unwrap();
        let b = sys
            .retain(turn(theirs, "Maui trip departs on May 24th."), agent)
            .await
            .unwrap()
            .unwrap();

        let _ = sys
            .recall_in_session("trip departure", agent, 10, mine)
            .await
            .unwrap();

        let other = sys.get_memory(b).await.unwrap().unwrap();
        assert_eq!(
            other.access_count, 0,
            "out-of-session memory must get no phantom access bump"
        );
        assert!(
            other.last_accessed_at.is_none(),
            "out-of-session memory's decay clock must not be reset"
        );
        // Sanity: the in-session memory is still reachable.
        assert!(sys.get_memory(a).await.unwrap().is_some());
    }
}
