use std::sync::Arc;

use chrono::Utc;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::admission;
use crate::chunking::chunk_text;
use crate::config::MemoryConfig;
use crate::embedding::{cosine_similarity, Embedder};
use crate::extraction::RegexExtractor;
use crate::scoring::compute_importance;
use crate::storage::MemoryStorage;
use crate::types::{
    ConversationTurn, ImportanceSource, LifecycleStage, Memory, MemoryChunk, MemoryScope,
};

/// Dual-track memory writer.
///
/// Track 1 (synchronous): Store verbatim content as Working memory, chunk,
/// embed, and index.
/// Track 2 (asynchronous): Extract facts/entities and detect near-duplicates
/// in the background.
///
/// New memories start in the `Working` lifecycle stage and are promoted to
/// `Episodic` when the session is finalized via [`LifecycleManager::finalize_session`].
///
/// The write path never blocks on extraction or dedup — raw verbatim storage
/// is always the synchronous, reliable path.
pub struct MemoryWriter {
    storage: Arc<dyn MemoryStorage>,
    embedder: Arc<dyn Embedder>,
    config: MemoryConfig,
    /// Bounds concurrent background dedup/extraction tasks so heavy
    /// ingestion cannot pile up unbounded embedding scans.
    dedup_limiter: Arc<tokio::sync::Semaphore>,
}

/// Maximum background dedup tasks doing work at once.
const DEDUP_MAX_CONCURRENCY: usize = 2;

impl MemoryWriter {
    pub fn new(
        storage: Arc<dyn MemoryStorage>,
        embedder: Arc<dyn Embedder>,
        config: MemoryConfig,
    ) -> Self {
        Self {
            storage,
            embedder,
            config,
            dedup_limiter: Arc::new(tokio::sync::Semaphore::new(DEDUP_MAX_CONCURRENCY)),
        }
    }

    /// Retain a conversation turn in memory.
    ///
    /// 1. Dedup check via SHA-256 content hash.
    /// 2. Store verbatim memory record as `Working` (sync).
    /// 3. Chunk, embed, and store chunk embeddings (sync).
    /// 4. Index in FTS5 (sync).
    /// 5. Compute heuristic importance (sync).
    /// 6. Spawn background extraction + near-duplicate detection (async, never blocks).
    ///
    /// Returns the memory ID.
    pub async fn retain(
        &self,
        turn: ConversationTurn,
        agent_id: Uuid,
    ) -> rustykrab_core::Result<Option<Uuid>> {
        self.retain_with_stage(turn, agent_id, LifecycleStage::Working)
            .await
    }

    /// Retain a conversation turn with an explicit lifecycle stage.
    ///
    /// Used by auto-persist to write `Working` memories, and by the
    /// `memory_save` tool path which writes `Episodic` memories.
    /// Returns `Ok(None)` when the content is refused by admission control
    /// (machine output, agent-loop chatter — see [`crate::admission`]);
    /// losing a memory write must never fail the caller, so a rejection is
    /// not an error.
    pub async fn retain_with_stage(
        &self,
        turn: ConversationTurn,
        agent_id: Uuid,
        stage: LifecycleStage,
    ) -> rustykrab_core::Result<Option<Uuid>> {
        // ── Admission control ───────────────────────────────────
        if let Err(rejection) = admission::admit(&turn.content) {
            debug!(?rejection, "memory write refused by admission control");
            return Ok(None);
        }

        // ── SHA-256 dedup ───────────────────────────────────────
        let content_hash = crate::hash_content(&turn.content);

        if let Some(existing) = self
            .storage
            .find_by_content_hash(agent_id, &content_hash)
            .await?
        {
            if existing.session_id == Some(turn.session_id) {
                debug!(memory_id = %existing.id, "exact duplicate, skipping write");
                // A re-save is corroboration, not a recall: bump proof_count
                // and leave access_count (retrieval ranking) and
                // last_accessed_at (the decay clock) alone — otherwise
                // repeated identical writes make a memory immortal and rank
                // it above genuinely useful ones.
                self.storage.record_duplicate(existing.id).await?;
                return Ok(Some(existing.id));
            }
            // Identical content from a DIFFERENT conversation: corroborate
            // the original, but still store a fresh row stamped with this
            // conversation's session id — otherwise session-scoped recall
            // ("earlier in this conversation…") silently loses the turn.
            debug!(
                original = %existing.id,
                "duplicate content from another conversation; storing per-session copy"
            );
            self.storage.record_duplicate(existing.id).await?;
        }

        // ── Track 1: Synchronous verbatim storage ───────────────
        let importance = compute_importance(&turn.content, &turn.metadata);
        let memory_id = Uuid::new_v4();
        let now = Utc::now();

        let memory = Memory {
            id: memory_id,
            agent_id,
            content: turn.content.clone(),
            content_hash,
            scope: MemoryScope::User,
            session_id: Some(turn.session_id),
            user_id: None, // Set by the caller via HybridMemoryBackend
            lifecycle_stage: stage,
            importance,
            importance_source: ImportanceSource::Heuristic,
            decay_rate: self.config.default_decay_rate,
            confidence: 1.0,
            access_count: 0,
            last_accessed_at: None,
            last_relevant_at: None,
            created_at: now,
            parent_memory_ids: Vec::new(),
            consolidation_generation: 0,
            proof_count: 1,
            occurred_start: None,
            occurred_end: None,
            is_valid: true,
            invalidated_by: None,
            invalidated_at: None,
            tags: turn.metadata.tags.clone(),
            // session_id lives in its own indexed column; don't duplicate it here.
            metadata: serde_json::json!({
                "turn_number": turn.turn_number,
                "speaker": turn.speaker,
            }),
        };

        self.storage.upsert_memory(&memory).await?;

        // ── Chunk + embed ───────────────────────────────────────
        let chunk_texts = chunk_text(
            &turn.content,
            self.config.chunk_max_tokens,
            self.config.chunk_overlap_ratio,
        );
        let chunk_count = chunk_texts.len();

        if !chunk_texts.is_empty() {
            let embeddings = self.embedder.embed(chunk_texts.clone()).await?;
            let model_version = self.embedder.model_version().to_string();

            // Consume the texts and embeddings instead of cloning each.
            let chunks: Vec<MemoryChunk> = chunk_texts
                .into_iter()
                .zip(embeddings)
                .enumerate()
                .map(|(i, (text, emb))| MemoryChunk {
                    id: Uuid::new_v4(),
                    memory_id,
                    chunk_index: i as u32,
                    content: text,
                    embedding: emb,
                    embedding_model_version: model_version.clone(),
                    created_at: now,
                })
                .collect();

            self.storage.store_chunks(&chunks).await?;
        }

        // ── FTS5 index ─────────────────────────────────────────
        self.storage
            .fts_index(memory_id, agent_id, &turn.content)
            .await?;

        // ── Track 2: Async background extraction + near-duplicate check ──
        let storage = Arc::clone(&self.storage);
        let content = turn.content;
        let dedup_threshold = self.config.dedup_auto_merge_threshold as f32;
        let session_id = Some(turn.session_id);
        let limiter = Arc::clone(&self.dedup_limiter);
        tokio::spawn(async move {
            // Bound concurrency: heavy ingestion queues here instead of
            // running an embedding scan per write all at once.
            let _permit = match limiter.acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => return, // semaphore closed (shutdown)
            };

            // Step 1: Extract facts.
            let facts = RegexExtractor::extract(&content, memory_id);
            if !facts.is_empty() {
                if let Err(e) = storage.store_facts(&facts).await {
                    warn!(memory_id = %memory_id, error = %e, "background extraction failed");
                }
            }

            // Step 2: Near-duplicate detection against existing memories.
            // Fetch this memory's first chunk embedding (stored by the sync path above).
            let new_emb = match storage.get_chunks_for_memory(memory_id).await {
                Ok(chunks) => match chunks.into_iter().next() {
                    Some(c) if !c.embedding.is_empty() => c.embedding,
                    _ => return,
                },
                Err(_) => return,
            };

            // Served from the shared per-agent embedding cache.
            let all_embeddings = match storage.get_all_chunk_embeddings(agent_id).await {
                Ok(e) => e,
                Err(_) => return,
            };

            for (existing_id, existing_emb) in all_embeddings.iter() {
                if *existing_id == memory_id {
                    continue;
                }
                let sim = cosine_similarity(&new_emb, existing_emb);
                if sim >= dedup_threshold {
                    // Only collapse near-duplicates within the SAME
                    // conversation: invalidating a row in favour of another
                    // session's copy would make the turn invisible to
                    // session-scoped recall.
                    let same_session = match storage.get_memory(*existing_id).await {
                        Ok(Some(existing)) => existing.session_id == session_id,
                        _ => false,
                    };
                    if !same_session {
                        let _ = storage.record_duplicate(*existing_id).await;
                        continue;
                    }
                    debug!(
                        new_id = %memory_id,
                        existing_id = %existing_id,
                        similarity = %sim,
                        "near-duplicate detected, invalidating new memory"
                    );
                    let _ = storage.invalidate(memory_id, Some(*existing_id)).await;
                    let _ = storage.record_duplicate(*existing_id).await;
                    return;
                }
            }
        });

        debug!(
            memory_id = %memory_id,
            importance = importance,
            chunks = chunk_count,
            ?stage,
            "memory retained"
        );

        Ok(Some(memory_id))
    }

    /// Store a simple fact with tags (backward-compatible with the old
    /// MemoryStore interface). Creates a memory record from the fact string.
    pub async fn save_fact(
        &self,
        agent_id: Uuid,
        session_id: Uuid,
        fact: &str,
        tags: &[String],
    ) -> rustykrab_core::Result<Option<Uuid>> {
        let turn = ConversationTurn {
            id: Uuid::new_v4(),
            session_id,
            turn_number: 0,
            speaker: "agent".to_string(),
            content: fact.to_string(),
            token_count: None,
            metadata: crate::types::TurnMetadata {
                tags: tags.to_vec(),
                ..Default::default()
            },
        };
        self.retain_with_stage(turn, agent_id, LifecycleStage::Episodic)
            .await
    }

    /// Rebuild the FTS5 index from all retrievable memories in storage.
    /// Call this on startup to ensure the FTS index is in sync.
    pub async fn rebuild_fts_index(&self, agent_id: Uuid) -> rustykrab_core::Result<usize> {
        let memories = self.storage.list_retrievable(agent_id).await?;
        let entries: Vec<(Uuid, String)> =
            memories.into_iter().map(|m| (m.id, m.content)).collect();
        let count = entries.len();
        // One transaction instead of two statements per memory.
        self.storage.fts_index_batch(agent_id, entries).await?;
        debug!(agent_id = %agent_id, indexed = count, "FTS5 index rebuilt");
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::ZeroEmbedder;
    use crate::storage::SqliteMemoryStorage;
    use crate::types::TurnMetadata;

    fn test_writer() -> (MemoryWriter, Arc<dyn MemoryStorage>) {
        let storage: Arc<dyn MemoryStorage> =
            Arc::new(SqliteMemoryStorage::open_in_memory().unwrap());
        let embedder = Arc::new(ZeroEmbedder::new(8));
        let writer = MemoryWriter::new(Arc::clone(&storage), embedder, MemoryConfig::default());
        (writer, storage)
    }

    fn turn_in(session_id: Uuid, content: &str) -> ConversationTurn {
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

    fn turn(content: &str) -> ConversationTurn {
        turn_in(Uuid::new_v4(), content)
    }

    /// A duplicate write must corroborate (proof_count) without inflating
    /// the retrieval signal (access_count) or resetting the decay clock
    /// (last_accessed_at) — otherwise repeatedly re-saved content becomes
    /// immortal and outranks genuinely useful memories.
    #[tokio::test]
    async fn duplicate_write_bumps_proof_count_not_access_count() {
        let (writer, storage) = test_writer();
        let agent = Uuid::new_v4();
        let session = Uuid::new_v4();
        let content = "The Maui trip is May 24-31, two travelers, mid-range budget.";

        let first = writer
            .retain(turn_in(session, content), agent)
            .await
            .unwrap()
            .unwrap();
        let second = writer
            .retain(turn_in(session, content), agent)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, second, "dedup should return the existing memory");

        let mem = storage.get_memory(first).await.unwrap().unwrap();
        assert_eq!(mem.proof_count, 2, "duplicate should corroborate");
        assert_eq!(mem.access_count, 0, "duplicate must not count as an access");
        assert!(
            mem.last_accessed_at.is_none(),
            "duplicate must not reset the decay clock"
        );
        assert!(
            mem.last_relevant_at.is_some(),
            "duplicate should refresh relevance"
        );
    }

    /// Machine output must be refused at the door, not stored and ranked.
    #[tokio::test]
    async fn machine_output_is_refused_admission() {
        let (writer, storage) = test_writer();
        let agent = Uuid::new_v4();

        let rejected = writer
            .retain(turn("tool_result:{\"ok\":true,\"count\":17}"), agent)
            .await
            .unwrap();
        assert!(rejected.is_none(), "tool dump should be rejected");

        let accepted = writer
            .retain(turn("User prefers morning briefings at 7am."), agent)
            .await
            .unwrap();
        assert!(accepted.is_some(), "prose should be admitted");

        let all = storage.list_retrievable(agent).await.unwrap();
        assert_eq!(all.len(), 1, "only the prose row should exist");
    }

    /// Explicit `memory_save` goes through the same gate.
    #[tokio::test]
    async fn save_fact_is_gated_too() {
        let (writer, _storage) = test_writer();
        let agent = Uuid::new_v4();
        let session = Uuid::new_v4();

        let rejected = writer
            .save_fact(agent, session, "{\"a\": [1,2,3]}", &["tag".into()])
            .await
            .unwrap();
        assert!(rejected.is_none());

        let accepted = writer
            .save_fact(
                agent,
                session,
                "Geoff's dog is named Rusty.",
                &["pets".into()],
            )
            .await
            .unwrap();
        assert!(accepted.is_some());
    }

    /// Identical content from a DIFFERENT conversation gets its own row (with
    /// the correct session id) so session-scoped recall can find it — while
    /// still corroborating the original.
    #[tokio::test]
    async fn cross_session_duplicate_gets_its_own_row() {
        let (writer, storage) = test_writer();
        let agent = Uuid::new_v4();
        let session_a = Uuid::new_v4();
        let session_b = Uuid::new_v4();
        let content = "What is our Palermo budget for the October trip?";

        let a = writer
            .retain(turn_in(session_a, content), agent)
            .await
            .unwrap()
            .unwrap();
        let b = writer
            .retain(turn_in(session_b, content), agent)
            .await
            .unwrap()
            .unwrap();

        assert_ne!(a, b, "each conversation should own a copy");
        let mem_a = storage.get_memory(a).await.unwrap().unwrap();
        let mem_b = storage.get_memory(b).await.unwrap().unwrap();
        assert_eq!(mem_a.session_id, Some(session_a));
        assert_eq!(mem_b.session_id, Some(session_b));
        assert_eq!(mem_a.proof_count, 2, "original should be corroborated");
    }
}
