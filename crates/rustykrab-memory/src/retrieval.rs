use std::collections::HashSet;
use std::sync::Arc;

use chrono::{Duration, Utc};
use tracing::debug;
use uuid::Uuid;

use crate::bm25::Bm25Index;
use crate::config::MemoryConfig;
use crate::embedding::{self, Embedder};
use crate::scoring::rrf_fuse_with_sources;
use crate::types::RetrievalSource;
use crate::storage::MemoryStorage;
use crate::types::RetrievalResult;

/// Four-way parallel retrieval pipeline with RRF fusion.
///
/// Executes semantic (vector), keyword (BM25), graph (link expansion),
/// and temporal (recency) retrieval arms in parallel via `tokio::join!`,
/// then fuses results with weighted Reciprocal Rank Fusion (k=60).
pub struct MemoryRetriever {
    storage: Arc<dyn MemoryStorage>,
    embedder: Arc<dyn Embedder>,
    config: MemoryConfig,
    bm25_index: Arc<tokio::sync::Mutex<Bm25Index>>,
}

impl MemoryRetriever {
    pub fn new(
        storage: Arc<dyn MemoryStorage>,
        embedder: Arc<dyn Embedder>,
        config: MemoryConfig,
        bm25_index: Arc<tokio::sync::Mutex<Bm25Index>>,
    ) -> Self {
        Self {
            storage,
            embedder,
            config,
            bm25_index,
        }
    }

    /// Recall memories relevant to a query.
    ///
    /// Pipeline:
    /// 1. Embed query + tokenize for BM25 (parallel with dispatch setup).
    /// 2. Dispatch four retrieval arms in parallel via `tokio::join!`.
    /// 3. Fuse with weighted RRF.
    /// 4. Multiply by lifecycle effective_score.
    /// 5. Update access metadata on returned memories.
    /// 6. Return top-K with provenance.
    pub async fn recall(
        &self,
        query: &str,
        agent_id: Uuid,
        limit: usize,
    ) -> rustykrab_core::Result<Vec<RetrievalResult>> {
        let candidates = self.config.retrieval_candidates_per_arm;

        // ── Stage 1: Query preprocessing ────────────────────────
        // Embed query in parallel with the rest of setup.
        let query_embedding = {
            let vecs = self.embedder.embed(vec![query.to_string()]).await?;
            vecs.into_iter().next().unwrap_or_default()
        };

        // ── Stage 2: Fetch embeddings once, then dispatch ──────
        // Shared embedding fetch avoids duplicate full-table scans (#106).
        let chunk_embeddings = self.storage.get_all_chunk_embeddings(agent_id).await?;

        let (semantic_results, keyword_results, graph_results, temporal_results) = tokio::join!(
            self.retrieve_semantic(&query_embedding, &chunk_embeddings, candidates),
            self.retrieve_bm25(query, agent_id, candidates),
            self.retrieve_graph(&query_embedding, &chunk_embeddings, candidates),
            self.retrieve_temporal(agent_id, candidates),
        );

        let semantic = semantic_results.unwrap_or_default();
        let keyword = keyword_results.unwrap_or_default();
        let graph = graph_results.unwrap_or_default();
        let temporal = temporal_results.unwrap_or_default();

        debug!(
            semantic = semantic.len(),
            keyword = keyword.len(),
            graph = graph.len(),
            temporal = temporal.len(),
            "retrieval arms completed"
        );

        // ── Stage 3: RRF fusion with sources ────────────────────
        let ranked_lists = vec![
            (semantic, self.config.rrf_weight_semantic, RetrievalSource::Semantic),
            (keyword, self.config.rrf_weight_keyword, RetrievalSource::Keyword),
            (graph, self.config.rrf_weight_graph, RetrievalSource::Graph),
            (temporal, self.config.rrf_weight_temporal, RetrievalSource::Temporal),
        ];

        let fused = rrf_fuse_with_sources(&ranked_lists, self.config.rrf_k);

        // ── Stage 4: Fetch full memories and apply lifecycle boost ──
        let now = Utc::now();
        let candidate_ids: Vec<Uuid> = fused.iter().map(|(id, _, _)| *id).collect();
        let memories = self.storage.get_memories(&candidate_ids).await?;

        let mut results: Vec<RetrievalResult> = Vec::new();

        for (memory_id, rrf_score, sources) in &fused {
            if let Some(mem) = memories.iter().find(|m| m.id == *memory_id) {
                if !mem.is_valid || !mem.lifecycle_stage.is_retrievable() {
                    continue;
                }

                // Normalize RRF score to [0, 1] using the theoretical max (#117).
                // max_rrf = sum_of_all_weights / rrf_k (when doc is rank 0 in all arms).
                let max_rrf = (self.config.rrf_weight_semantic
                    + self.config.rrf_weight_keyword
                    + self.config.rrf_weight_graph
                    + self.config.rrf_weight_temporal)
                    / self.config.rrf_k;
                let normalized_rrf = if max_rrf > 0.0 {
                    (*rrf_score / max_rrf).min(1.0)
                } else {
                    0.0
                };
                let eff_score = mem.effective_score(normalized_rrf, now);

                results.push(RetrievalResult {
                    memory_id: *memory_id,
                    content: mem.content.clone(),
                    rrf_score: *rrf_score,
                    effective_score: eff_score,
                    sources: sources.clone(),
                    memory: mem.clone(),
                });
            }
        }

        // Sort by effective score (lifecycle-adjusted).
        results.sort_by(|a, b| {
            b.effective_score
                .partial_cmp(&a.effective_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);

        // ── Stage 5: Record access on returned results ──────────
        // Batch access updates instead of spawning unbounded tasks (#110).
        for result in &results {
            if let Err(e) = self.storage.record_access(result.memory_id).await {
                tracing::warn!(
                    memory_id = %result.memory_id,
                    error = %e,
                    "failed to record access"
                );
            }
        }

        debug!(
            returned = results.len(),
            total_candidates = fused.len(),
            "recall complete"
        );

        Ok(results)
    }

    /// Semantic retrieval: find nearest neighbors via brute-force cosine
    /// similarity over pre-fetched chunk embeddings.
    async fn retrieve_semantic(
        &self,
        query_vec: &[f32],
        chunk_embeddings: &[(Uuid, Vec<f32>)],
        limit: usize,
    ) -> rustykrab_core::Result<Vec<(Uuid, usize)>> {
        if query_vec.is_empty() || query_vec.iter().all(|v| *v == 0.0) {
            return Ok(Vec::new());
        }

        let top = embedding::top_k_similar(query_vec, chunk_embeddings, limit * 2);

        // Deduplicate to memory level (multiple chunks may belong to same memory).
        let mut seen = HashSet::new();
        let mut results = Vec::new();
        for (memory_id, _sim) in top {
            if seen.insert(memory_id) {
                results.push((memory_id, results.len())); // rank = position
            }
            if results.len() >= limit {
                break;
            }
        }

        Ok(results)
    }

    /// Keyword retrieval: BM25 search over in-memory inverted index,
    /// scoped to the querying agent.
    async fn retrieve_bm25(
        &self,
        query: &str,
        agent_id: Uuid,
        limit: usize,
    ) -> rustykrab_core::Result<Vec<(Uuid, usize)>> {
        let index = self.bm25_index.lock().await;
        let results = index.search(query, agent_id, limit);
        Ok(results
            .into_iter()
            .enumerate()
            .map(|(rank, (id, _score))| (id, rank))
            .collect())
    }

    /// Graph retrieval: find semantically similar memories, then expand
    /// via precomputed links (1-hop). Uses pre-fetched embeddings.
    async fn retrieve_graph(
        &self,
        query_vec: &[f32],
        chunk_embeddings: &[(Uuid, Vec<f32>)],
        limit: usize,
    ) -> rustykrab_core::Result<Vec<(Uuid, usize)>> {
        if query_vec.is_empty() || query_vec.iter().all(|v| *v == 0.0) {
            return Ok(Vec::new());
        }

        // Seed: top-5 from semantic search.
        let seeds = embedding::top_k_similar(query_vec, chunk_embeddings, 5);

        let mut seen = HashSet::new();
        let mut linked = Vec::new();

        for (seed_id, _sim) in &seeds {
            seen.insert(*seed_id);
            let links = self.storage.get_links_from(*seed_id).await?;
            for link in links {
                if seen.insert(link.target_id) {
                    linked.push((link.target_id, link.weight));
                }
            }
        }

        // Sort by link weight descending, assign ranks.
        linked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        linked.truncate(limit);

        Ok(linked
            .into_iter()
            .enumerate()
            .map(|(rank, (id, _))| (id, rank))
            .collect())
    }

    /// Temporal retrieval: most recent memories within a sliding window.
    async fn retrieve_temporal(
        &self,
        agent_id: Uuid,
        limit: usize,
    ) -> rustykrab_core::Result<Vec<(Uuid, usize)>> {
        let now = Utc::now();
        let from = now - Duration::days(30); // 30-day window

        let memories = self
            .storage
            .list_by_time_range(agent_id, from, now, limit)
            .await?;

        Ok(memories
            .into_iter()
            .enumerate()
            .map(|(rank, mem)| (mem.id, rank))
            .collect())
    }
}
