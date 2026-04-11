//! # rustykrab-memory
//!
//! Production agent memory: verbatim storage, hybrid retrieval, value-driven lifecycle.
//!
//! This crate implements a three-pillar memory architecture:
//!
//! 1. **Verbatim storage** — raw conversation text is always stored synchronously
//!    as the source of truth, with extracted facts computed asynchronously in the
//!    background. This beats extraction-heavy approaches on retrieval benchmarks.
//!
//! 2. **Four-way parallel retrieval** — semantic (vector), keyword (FTS5), graph
//!    (link expansion), and temporal (recency) strategies run in parallel via
//!    `tokio::join!`, then fuse with weighted Reciprocal Rank Fusion (k=60).
//!
//! 3. **Value-driven lifecycle** — importance-modulated exponential decay bounds
//!    the retrieval working set, with promotion (episodic→semantic) and demotion
//!    (episodic→archival→tombstone) keeping tail latency low as memory grows.
//!
//! ## Quick start
//!
//! ```rust,ignore
//! use rustykrab_memory::{MemorySystem, MemoryConfig};
//! use rustykrab_memory::embedding::HashEmbedder;
//! use rustykrab_memory::storage::SqliteMemoryStorage;
//! use std::sync::Arc;
//! use uuid::Uuid;
//!
//! # async fn example() -> rustykrab_core::Result<()> {
//! let storage = Arc::new(SqliteMemoryStorage::open("memory.db")?);
//! let embedder = Arc::new(HashEmbedder::new(768));
//! let config = MemoryConfig::default();
//!
//! let system = MemorySystem::new(config, storage, embedder);
//!
//! // Write
//! let agent_id = Uuid::new_v4();
//! let turn = rustykrab_memory::types::ConversationTurn {
//!     id: Uuid::new_v4(),
//!     session_id: Uuid::new_v4(),
//!     turn_number: 1,
//!     speaker: "user".into(),
//!     content: "I prefer Rust for systems programming.".into(),
//!     token_count: None,
//!     metadata: Default::default(),
//! };
//! let memory_id = system.retain(turn, agent_id).await?;
//!
//! // Read
//! let results = system.recall("Rust programming", agent_id, 5).await?;
//! # Ok(())
//! # }
//! ```

pub mod backend;
pub mod chunking;
pub mod config;
pub mod embedding;
pub mod extraction;
pub mod lifecycle;
pub mod retrieval;
pub mod scoring;
pub mod storage;
pub mod types;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use config::MemoryConfig;

use lifecycle::{LifecycleManager, LifecycleSweepStats};
use retrieval::MemoryRetriever;
use storage::MemoryStorage;
use types::{ConversationTurn, LifecycleStage, Memory, RetrievalResult};
use writer::MemoryWriter;

mod writer;

/// Statistics returned from an end-session operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EndSessionStats {
    /// Number of Working memories transitioned to Episodic.
    pub working_to_episodic: u32,
    /// Lifecycle sweep stats (promotion, demotion, tombstoning).
    pub sweep: LifecycleSweepStats,
    /// Number of near-duplicate links created.
    pub near_duplicate_links: u32,
}

/// The main entry point for the memory system.
///
/// Composes the write path ([MemoryWriter]), read path ([MemoryRetriever]),
/// and lifecycle management ([LifecycleManager]) into a single facade.
pub struct MemorySystem {
    writer: MemoryWriter,
    retriever: MemoryRetriever,
    lifecycle: LifecycleManager,
    storage: Arc<dyn MemoryStorage>,
    config: MemoryConfig,
}

impl MemorySystem {
    /// Create a new memory system.
    ///
    /// - `config`: tuning parameters for chunking, retrieval, and lifecycle.
    /// - `storage`: the backing store (SQLite, PostgreSQL, etc.).
    /// - `embedder`: the text embedding model (fastembed, API-based, etc.).
    pub fn new(
        config: MemoryConfig,
        storage: Arc<dyn MemoryStorage>,
        embedder: Arc<dyn embedding::Embedder>,
    ) -> Self {
        let writer = MemoryWriter::new(Arc::clone(&storage), Arc::clone(&embedder), config.clone());

        let retriever =
            MemoryRetriever::new(Arc::clone(&storage), Arc::clone(&embedder), config.clone());

        let lifecycle =
            LifecycleManager::new(Arc::clone(&storage), Arc::clone(&embedder), config.clone());

        Self {
            writer,
            retriever,
            lifecycle,
            storage,
            config,
        }
    }

    // ── Write path ──────────────────────────────────────────────

    /// Retain a conversation turn in memory (dual-track write).
    pub async fn retain(
        &self,
        turn: ConversationTurn,
        agent_id: Uuid,
    ) -> rustykrab_core::Result<Uuid> {
        self.writer.retain(turn, agent_id).await
    }

    /// Retain a conversation turn with an explicit lifecycle stage.
    /// Used for auto-persist (Working) vs explicit save (Episodic).
    pub async fn retain_with_stage(
        &self,
        turn: ConversationTurn,
        agent_id: Uuid,
        stage: LifecycleStage,
    ) -> rustykrab_core::Result<Uuid> {
        self.writer.retain_with_stage(turn, agent_id, stage).await
    }

    /// Access the writer directly (e.g., for `save_fact`).
    pub fn writer(&self) -> &MemoryWriter {
        &self.writer
    }

    // ── Read path ───────────────────────────────────────────────

    /// Recall memories relevant to a query using four-way parallel
    /// retrieval with RRF fusion and lifecycle scoring.
    pub async fn recall(
        &self,
        query: &str,
        agent_id: Uuid,
        limit: usize,
    ) -> rustykrab_core::Result<Vec<RetrievalResult>> {
        self.retriever.recall(query, agent_id, limit).await
    }

    // ── Lifecycle management ────────────────────────────────────

    /// Run a lifecycle sweep: promote, demote, and tombstone memories.
    pub async fn lifecycle_sweep(
        &self,
        agent_id: Uuid,
    ) -> rustykrab_core::Result<LifecycleSweepStats> {
        self.lifecycle.sweep(agent_id).await
    }

    /// Finalize a session: promote all Working memories for this session
    /// to Episodic.
    ///
    /// Call this when a conversation/session ends.
    pub async fn finalize_session(
        &self,
        agent_id: Uuid,
        session_id: Uuid,
    ) -> rustykrab_core::Result<u32> {
        self.lifecycle.finalize_session(agent_id, session_id).await
    }

    /// Detect near-duplicate memories and create similarity links.
    pub async fn detect_near_duplicates(&self, agent_id: Uuid) -> rustykrab_core::Result<u32> {
        self.lifecycle.detect_near_duplicates(agent_id).await
    }

    /// Check for embedding drift (re-embed sample, compare to stored).
    pub async fn check_embedding_drift(
        &self,
        agent_id: Uuid,
        sample_size: usize,
    ) -> rustykrab_core::Result<f64> {
        self.lifecycle
            .check_embedding_drift(agent_id, sample_size)
            .await
    }

    /// End a session: finalize (Working → Episodic), run lifecycle sweep,
    /// and detect near-duplicates.
    ///
    /// Combines `finalize_session` + `lifecycle_sweep` + `detect_near_duplicates`
    /// into a single call for convenience.
    pub async fn end_session(
        &self,
        agent_id: Uuid,
        session_id: Uuid,
    ) -> rustykrab_core::Result<EndSessionStats> {
        // 1. Finalize: transition Working memories for this session to Episodic.
        let working_to_episodic = self
            .lifecycle
            .finalize_session(agent_id, session_id)
            .await?;

        // 2. Run lifecycle sweep (promote/demote/tombstone).
        let sweep = self.lifecycle.sweep(agent_id).await?;

        // 3. Detect near-duplicates.
        let near_duplicate_links = self.lifecycle.detect_near_duplicates(agent_id).await?;

        tracing::info!(
            %agent_id,
            %session_id,
            working_to_episodic,
            promoted = sweep.promoted_to_semantic,
            demoted = sweep.demoted_to_archival,
            tombstoned = sweep.tombstoned,
            near_duplicate_links,
            "session ended, lifecycle complete"
        );

        Ok(EndSessionStats {
            working_to_episodic,
            sweep,
            near_duplicate_links,
        })
    }

    // ── Direct access ───────────────────────────────────────────

    /// Get a memory by ID.
    pub async fn get_memory(&self, id: Uuid) -> rustykrab_core::Result<Option<Memory>> {
        self.storage.get_memory(id).await
    }

    /// Invalidate (soft-delete) a memory.
    pub async fn invalidate_memory(
        &self,
        id: Uuid,
        superseded_by: Option<Uuid>,
    ) -> rustykrab_core::Result<()> {
        self.lifecycle.invalidate_memory(id, superseded_by).await
    }

    /// Access the underlying storage (for advanced queries).
    pub fn storage(&self) -> &Arc<dyn MemoryStorage> {
        &self.storage
    }

    /// Access the configuration.
    pub fn config(&self) -> &MemoryConfig {
        &self.config
    }

    /// Rebuild the FTS5 index from persisted memories (call on startup).
    pub async fn rebuild_indexes(&self, agent_id: Uuid) -> rustykrab_core::Result<usize> {
        self.writer.rebuild_fts_index(agent_id).await
    }
}
