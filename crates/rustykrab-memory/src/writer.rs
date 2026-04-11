use std::sync::Arc;

use chrono::Utc;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};
use uuid::Uuid;

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
}

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
    ) -> rustykrab_core::Result<Uuid> {
        self.retain_with_stage(turn, agent_id, LifecycleStage::Episodic)
            .await
    }

    /// Retain a conversation turn with an explicit lifecycle stage.
    ///
    /// Used by auto-persist to write `Working` memories, and by the
    /// `memory_save` tool path which writes `Episodic` memories.
    pub async fn retain_with_stage(
        &self,
        turn: ConversationTurn,
        agent_id: Uuid,
        stage: LifecycleStage,
    ) -> rustykrab_core::Result<Uuid> {
        // ── SHA-256 dedup ───────────────────────────────────────
        let content_hash = {
            let mut hasher = Sha256::new();
            hasher.update(turn.content.as_bytes());
            hex::encode(hasher.finalize())
        };

        if let Some(existing) = self
            .storage
            .find_by_content_hash(agent_id, &content_hash)
            .await?
        {
            debug!(memory_id = %existing.id, "exact duplicate, skipping write");
            // Still record an access on the existing memory.
            self.storage.record_access(existing.id).await?;
            return Ok(existing.id);
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
            metadata: serde_json::json!({
                "session_id": turn.session_id.to_string(),
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

        if !chunk_texts.is_empty() {
            let embeddings = self.embedder.embed(chunk_texts.clone()).await?;
            let model_version = self.embedder.model_version().to_string();

            let chunks: Vec<MemoryChunk> = chunk_texts
                .iter()
                .zip(embeddings.iter())
                .enumerate()
                .map(|(i, (text, emb))| MemoryChunk {
                    id: Uuid::new_v4(),
                    memory_id,
                    chunk_index: i as u32,
                    content: text.clone(),
                    embedding: emb.clone(),
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
        let content = turn.content.clone();
        let dedup_threshold = self.config.dedup_auto_merge_threshold as f32;
        tokio::spawn(async move {
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

            let all_embeddings = match storage.get_all_chunk_embeddings(agent_id).await {
                Ok(e) => e,
                Err(_) => return,
            };

            for (existing_id, existing_emb) in &all_embeddings {
                if *existing_id == memory_id {
                    continue;
                }
                let sim = cosine_similarity(&new_emb, existing_emb);
                if sim >= dedup_threshold {
                    debug!(
                        new_id = %memory_id,
                        existing_id = %existing_id,
                        similarity = %sim,
                        "near-duplicate detected, invalidating new memory"
                    );
                    let _ = storage.invalidate(memory_id, Some(*existing_id)).await;
                    let _ = storage.record_access(*existing_id).await;
                    return;
                }
            }
        });

        debug!(
            memory_id = %memory_id,
            importance = importance,
            chunks = chunk_texts.len(),
            ?stage,
            "memory retained"
        );

        Ok(memory_id)
    }

    /// Store a simple fact with tags (backward-compatible with the old
    /// MemoryStore interface). Creates a memory record from the fact string.
    pub async fn save_fact(
        &self,
        agent_id: Uuid,
        session_id: Uuid,
        fact: &str,
        tags: &[String],
    ) -> rustykrab_core::Result<Uuid> {
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
        self.retain(turn, agent_id).await
    }

    /// Rebuild the FTS5 index from all retrievable memories in storage.
    /// Call this on startup to ensure the FTS index is in sync.
    pub async fn rebuild_fts_index(&self, agent_id: Uuid) -> rustykrab_core::Result<usize> {
        let memories = self.storage.list_retrievable(agent_id).await?;
        let count = memories.len();
        for mem in memories {
            self.storage
                .fts_index(mem.id, agent_id, &mem.content)
                .await?;
        }
        debug!(agent_id = %agent_id, indexed = count, "FTS5 index rebuilt");
        Ok(count)
    }
}
