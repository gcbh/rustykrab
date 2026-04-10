use std::sync::Arc;

use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};
use uuid::Uuid;

use crate::config::MemoryConfig;
use crate::embedding::{cosine_similarity, Embedder};
use crate::storage::MemoryStorage;
use crate::types::{LifecycleStage, Memory, MemoryLink, LinkType};

/// Statistics from a lifecycle sweep operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LifecycleSweepStats {
    pub promoted_to_semantic: u32,
    pub demoted_to_archival: u32,
    pub tombstoned: u32,
    pub near_duplicates_found: u32,
}

/// Lifecycle manager: handles promotion, demotion, consolidation,
/// and deduplication of memories.
///
/// Runs as a background job (typically after session end or on a
/// periodic schedule), never on the hot read/write path.
pub struct LifecycleManager {
    storage: Arc<dyn MemoryStorage>,
    embedder: Arc<dyn Embedder>,
    config: MemoryConfig,
}

impl LifecycleManager {
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

    /// Run a full lifecycle sweep for an agent's memories.
    ///
    /// 1. Promote: episodic → semantic (accessed ≥3×, older than 7 days).
    /// 2. Demote: episodic → archival (effective score < threshold, idle > 30 days).
    /// 3. Tombstone: archival → tombstone (idle > 180 days, low importance).
    pub async fn sweep(&self, agent_id: Uuid) -> rustykrab_core::Result<LifecycleSweepStats> {
        let now = Utc::now();
        let mut stats = LifecycleSweepStats::default();

        // ── Promote: episodic → semantic ────────────────────────
        let episodic = self
            .storage
            .list_by_stage(agent_id, LifecycleStage::Episodic)
            .await?;

        let promote_age = Duration::days(self.config.promote_min_age_days as i64);
        let mut promotions = Vec::new();
        let mut demotions = Vec::new();

        for mem in &episodic {
            let age = now - mem.created_at;

            // Promotion criteria: accessed enough times AND old enough.
            if mem.access_count >= self.config.promote_min_access_count
                && age >= promote_age
            {
                promotions.push((mem.id, LifecycleStage::Semantic));
                continue;
            }

            // Demotion criteria: low effective score AND idle > 30 days.
            // Use max(0) instead of unsigned_abs() to surface clock skew (#119).
            let idle_hours = (now - mem.last_accessed_at.unwrap_or(mem.created_at))
                .num_hours()
                .max(0) as f64;
            let effective_decay = mem.decay_rate * (1.0 - mem.importance * 0.8);
            let score = mem.importance * (-effective_decay * idle_hours / 168.0).exp();

            let idle_days = idle_hours / 24.0;
            if score < self.config.archive_score_threshold && idle_days > 30.0 {
                demotions.push((mem.id, LifecycleStage::Archival));
            }
        }

        if !promotions.is_empty() {
            stats.promoted_to_semantic = self
                .storage
                .batch_update_stages(&promotions)
                .await?;
        }
        if !demotions.is_empty() {
            stats.demoted_to_archival = self
                .storage
                .batch_update_stages(&demotions)
                .await?;
        }

        // ── Tombstone: archival → tombstone ─────────────────────
        let archival = self
            .storage
            .list_by_stage(agent_id, LifecycleStage::Archival)
            .await?;

        let tombstone_threshold = Duration::days(self.config.tombstone_idle_days as i64);
        let mut tombstones = Vec::new();

        for mem in &archival {
            let idle = now - mem.last_accessed_at.unwrap_or(mem.created_at);
            if idle >= tombstone_threshold
                && mem.importance < self.config.tombstone_importance_threshold
            {
                tombstones.push((mem.id, LifecycleStage::Tombstone));
            }
        }

        if !tombstones.is_empty() {
            stats.tombstoned = self
                .storage
                .batch_update_stages(&tombstones)
                .await?;
        }

        info!(
            agent_id = %agent_id,
            promoted = stats.promoted_to_semantic,
            demoted = stats.demoted_to_archival,
            tombstoned = stats.tombstoned,
            "lifecycle sweep complete"
        );

        Ok(stats)
    }

    /// Detect near-duplicate memories and create links between them.
    ///
    /// Thresholds (from production systems):
    /// - ≥0.95 cosine: auto-link as near-duplicate
    /// - 0.85–0.95: link as semantically similar
    /// - <0.85: distinct, no link
    ///
    /// Bounded to 50 links per memory to prevent graph explosion.
    pub async fn detect_near_duplicates(
        &self,
        agent_id: Uuid,
    ) -> rustykrab_core::Result<u32> {
        let memories = self.storage.list_retrievable(agent_id).await?;
        if memories.len() < 2 {
            return Ok(0);
        }

        // Cap the number of memories to prevent O(n^2) blowup (#114).
        const MAX_DEDUP_MEMORIES: usize = 500;

        // Get embeddings for all memories (use first chunk of each).
        let mut mem_embeddings: Vec<(Uuid, Vec<f32>)> = Vec::new();
        for mem in memories.iter().take(MAX_DEDUP_MEMORIES) {
            let chunks = self.storage.get_chunks_for_memory(mem.id).await?;
            if let Some(first) = chunks.first() {
                if !first.embedding.is_empty() {
                    mem_embeddings.push((mem.id, first.embedding.clone()));
                }
            }
        }

        if mem_embeddings.len() > MAX_DEDUP_MEMORIES {
            tracing::warn!(
                agent_id = %agent_id,
                total = memories.len(),
                capped_to = MAX_DEDUP_MEMORIES,
                "near-duplicate detection capped to prevent O(n^2) blowup"
            );
        }

        let mut link_count = 0u32;
        let now = Utc::now();

        // Pairwise comparison (O(n²) — acceptable for reasonable memory counts).
        for i in 0..mem_embeddings.len() {
            let mut links_for_i = 0u32;
            for j in (i + 1)..mem_embeddings.len() {
                if links_for_i >= 50 {
                    break; // Bound links per memory.
                }

                let sim = cosine_similarity(&mem_embeddings[i].1, &mem_embeddings[j].1);

                if sim >= self.config.dedup_auto_merge_threshold as f32 {
                    // Near-duplicate: create bidirectional links.
                    let link = MemoryLink {
                        source_id: mem_embeddings[i].0,
                        target_id: mem_embeddings[j].0,
                        link_type: LinkType::SemanticSimilar,
                        weight: sim as f64,
                        created_at: now,
                    };
                    self.storage.upsert_link(&link).await?;

                    let reverse = MemoryLink {
                        source_id: mem_embeddings[j].0,
                        target_id: mem_embeddings[i].0,
                        link_type: LinkType::SemanticSimilar,
                        weight: sim as f64,
                        created_at: now,
                    };
                    self.storage.upsert_link(&reverse).await?;

                    link_count += 2;
                    links_for_i += 1;
                } else if sim >= self.config.dedup_distinct_threshold as f32 {
                    // Semantically similar but distinct — create bidirectional links (#125).
                    let link = MemoryLink {
                        source_id: mem_embeddings[i].0,
                        target_id: mem_embeddings[j].0,
                        link_type: LinkType::SemanticSimilar,
                        weight: sim as f64,
                        created_at: now,
                    };
                    self.storage.upsert_link(&link).await?;

                    let reverse = MemoryLink {
                        source_id: mem_embeddings[j].0,
                        target_id: mem_embeddings[i].0,
                        link_type: LinkType::SemanticSimilar,
                        weight: sim as f64,
                        created_at: now,
                    };
                    self.storage.upsert_link(&reverse).await?;
                    link_count += 2;
                    links_for_i += 1;
                }
            }
        }

        debug!(
            agent_id = %agent_id,
            links_created = link_count,
            "near-duplicate detection complete"
        );

        Ok(link_count)
    }

    /// Invalidate a memory (soft delete), optionally recording which
    /// memory supersedes it.
    pub async fn invalidate_memory(
        &self,
        id: Uuid,
        superseded_by: Option<Uuid>,
    ) -> rustykrab_core::Result<()> {
        self.storage.invalidate(id, superseded_by).await
    }

    /// Check for embedding drift by re-embedding a sample of memories
    /// and comparing to stored embeddings.
    ///
    /// Returns the average cosine distance between old and new embeddings.
    /// Alert if this exceeds ~0.05 (indicates model or preprocessing change).
    pub async fn check_embedding_drift(
        &self,
        agent_id: Uuid,
        sample_size: usize,
    ) -> rustykrab_core::Result<f64> {
        let memories = self.storage.list_retrievable(agent_id).await?;
        let sample: Vec<&Memory> = memories.iter().take(sample_size).collect();

        if sample.is_empty() {
            return Ok(0.0);
        }

        let mut total_distance = 0.0;
        let mut compared = 0usize;

        for mem in &sample {
            let chunks = self.storage.get_chunks_for_memory(mem.id).await?;
            if let Some(old_chunk) = chunks.first() {
                if old_chunk.embedding.is_empty() {
                    continue;
                }

                // Re-embed the same content.
                let new_embeddings = self
                    .embedder
                    .embed(vec![old_chunk.content.clone()])
                    .await?;

                if let Some(new_emb) = new_embeddings.first() {
                    let sim = cosine_similarity(&old_chunk.embedding, new_emb);
                    let distance = 1.0 - sim;
                    total_distance += distance as f64;
                    compared += 1;
                }
            }
        }

        let avg_distance = if compared > 0 {
            total_distance / compared as f64
        } else {
            0.0
        };

        if avg_distance > 0.05 {
            tracing::warn!(
                agent_id = %agent_id,
                avg_distance = avg_distance,
                samples = compared,
                "embedding drift detected! Consider re-indexing."
            );
        }

        Ok(avg_distance)
    }
}
