use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use rustykrab_core::Result;
use uuid::Uuid;

use crate::types::{
    ExtractedFact, LifecycleStage, LinkType, Memory, MemoryChunk, MemoryLink, MemoryScope,
};

/// Decoded chunk embeddings for one agent: `(memory_id, embedding)` pairs.
///
/// Embeddings are `Arc`-shared so snapshots of the set can be copied and
/// filtered without duplicating the underlying vector data.
pub type EmbeddingSet = Vec<(Uuid, Arc<[f32]>)>;

/// Abstract storage backend for the memory system.
///
/// All retrieval, write, and lifecycle operations go through this trait,
/// allowing different backends (SQLite, PostgreSQL) to be swapped.
#[async_trait]
pub trait MemoryStorage: Send + Sync {
    // ── FTS (keyword search) ───────────────────────────────────

    /// Index a memory's content in the full-text search index.
    async fn fts_index(&self, memory_id: Uuid, agent_id: Uuid, content: &str) -> Result<()>;

    /// Re-index a batch of memories in one transaction (used by index rebuild).
    async fn fts_index_batch(&self, agent_id: Uuid, entries: Vec<(Uuid, String)>) -> Result<()>;

    /// Search the FTS index for memories matching a query, scoped to an agent.
    /// Returns (memory_id, rank) pairs sorted by relevance.
    async fn fts_search(
        &self,
        query: &str,
        agent_id: Uuid,
        limit: usize,
    ) -> Result<Vec<(Uuid, usize)>>;

    // ── Memory CRUD ─────────────────────────────────────────────

    /// Insert or update a memory record.
    async fn upsert_memory(&self, memory: &Memory) -> Result<()>;

    /// Retrieve a memory by ID.
    async fn get_memory(&self, id: Uuid) -> Result<Option<Memory>>;

    /// Retrieve multiple memories by IDs.
    async fn get_memories(&self, ids: &[Uuid]) -> Result<Vec<Memory>>;

    /// Check for exact content duplicate within a time window.
    async fn find_by_content_hash(
        &self,
        agent_id: Uuid,
        content_hash: &str,
    ) -> Result<Option<Memory>>;

    /// List all valid memories for an agent in a given lifecycle stage.
    async fn list_by_stage(&self, agent_id: Uuid, stage: LifecycleStage) -> Result<Vec<Memory>>;

    /// List all valid memories for a specific session and lifecycle stage.
    async fn list_by_session_and_stage(
        &self,
        agent_id: Uuid,
        session_id: Uuid,
        stage: LifecycleStage,
    ) -> Result<Vec<Memory>>;

    /// List all valid, retrievable memories for an agent.
    async fn list_retrievable(&self, agent_id: Uuid) -> Result<Vec<Memory>>;

    /// List memories created within a time range, sorted by recency.
    async fn list_by_time_range(
        &self,
        agent_id: Uuid,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<Memory>>;

    /// Update lifecycle stage for a memory.
    async fn update_stage(&self, id: Uuid, stage: LifecycleStage) -> Result<()>;

    /// Record an access (increment access_count, update last_accessed_at).
    async fn record_access(&self, id: Uuid) -> Result<()>;

    /// Record an access for each memory in a single round-trip.
    async fn record_access_batch(&self, ids: &[Uuid]) -> Result<()>;

    /// Soft-delete: mark a memory as invalid.
    async fn invalidate(&self, id: Uuid, invalidated_by: Option<Uuid>) -> Result<()>;

    // ── Chunk operations ────────────────────────────────────────

    /// Store embedding chunks for a memory.
    async fn store_chunks(&self, chunks: &[MemoryChunk]) -> Result<()>;

    /// Retrieve all chunks for a memory.
    async fn get_chunks_for_memory(&self, memory_id: Uuid) -> Result<Vec<MemoryChunk>>;

    /// Retrieve all chunks with embeddings for an agent (for vector search).
    ///
    /// Returns a shared snapshot; implementations may serve it from a cache.
    async fn get_all_chunk_embeddings(&self, agent_id: Uuid) -> Result<Arc<EmbeddingSet>>;

    // ── Extracted facts ─────────────────────────────────────────

    /// Store extracted facts.
    async fn store_facts(&self, facts: &[ExtractedFact]) -> Result<()>;

    /// Get facts extracted from a specific memory.
    async fn get_facts_for_memory(&self, memory_id: Uuid) -> Result<Vec<ExtractedFact>>;

    // ── Memory links (graph) ────────────────────────────────────

    /// Add or update a link between two memories.
    async fn upsert_link(&self, link: &MemoryLink) -> Result<()>;

    /// Get all outgoing links from a memory.
    async fn get_links_from(&self, source_id: Uuid) -> Result<Vec<MemoryLink>>;

    /// Get all outgoing links from a set of memories in one query.
    async fn get_links_from_many(&self, source_ids: &[Uuid]) -> Result<Vec<MemoryLink>>;

    /// Get all links involving a memory (incoming + outgoing).
    async fn get_links_for(&self, memory_id: Uuid) -> Result<Vec<MemoryLink>>;

    // ── Bulk operations ─────────────────────────────────────────

    /// Batch update lifecycle stages (used by sweep).
    async fn batch_update_stages(&self, updates: &[(Uuid, LifecycleStage)]) -> Result<u32>;

    /// Hard-delete tombstoned memories older than `older_than`, cascading to
    /// their chunks, extracted facts, links, and FTS rows in one transaction.
    /// Returns the number of memories purged.
    async fn purge_tombstones(&self, agent_id: Uuid, older_than: DateTime<Utc>) -> Result<u32>;
}

// ── SQLite helpers ──────────────────────────────────────────────

fn storage_err(e: impl std::fmt::Display) -> rustykrab_core::Error {
    rustykrab_core::Error::Storage(e.to_string())
}

fn lifecycle_to_str(stage: LifecycleStage) -> &'static str {
    match stage {
        LifecycleStage::Working => "working",
        LifecycleStage::Episodic => "episodic",
        LifecycleStage::Semantic => "semantic",
        LifecycleStage::Archival => "archival",
        LifecycleStage::Tombstone => "tombstone",
    }
}

fn str_to_lifecycle(s: &str) -> LifecycleStage {
    match s {
        "working" => LifecycleStage::Working,
        "episodic" => LifecycleStage::Episodic,
        "semantic" => LifecycleStage::Semantic,
        "archival" => LifecycleStage::Archival,
        _ => LifecycleStage::Tombstone,
    }
}

fn str_to_link_type(s: &str) -> LinkType {
    match s {
        "semantic_similar" => LinkType::SemanticSimilar,
        "entity_cooccurrence" => LinkType::EntityCooccurrence,
        "causal_chain" => LinkType::CausalChain,
        "consolidation" => LinkType::Consolidation,
        "contradicts" => LinkType::Contradicts,
        _ => LinkType::SemanticSimilar,
    }
}

fn link_type_to_str(lt: LinkType) -> &'static str {
    match lt {
        LinkType::SemanticSimilar => "semantic_similar",
        LinkType::EntityCooccurrence => "entity_cooccurrence",
        LinkType::CausalChain => "causal_chain",
        LinkType::Consolidation => "consolidation",
        LinkType::Contradicts => "contradicts",
    }
}

fn scope_to_str(scope: MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Session => "session",
        MemoryScope::User => "user",
        MemoryScope::Agent => "agent",
        MemoryScope::Global => "global",
    }
}

fn str_to_scope(s: &str) -> MemoryScope {
    match s {
        "session" => MemoryScope::Session,
        "agent" => MemoryScope::Agent,
        "global" => MemoryScope::Global,
        _ => MemoryScope::User,
    }
}

/// Encode a Vec<f32> embedding as little-endian bytes for SQLite BLOB storage.
fn embedding_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Decode a BLOB back to Vec<f32>.
fn blob_to_embedding(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Encode a Vec<Uuid> as JSON text for SQLite TEXT storage.
fn uuids_to_json(ids: &[Uuid]) -> String {
    serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_string())
}

/// Decode JSON text back to Vec<Uuid>.
fn json_to_uuids(s: &str) -> Vec<Uuid> {
    serde_json::from_str(s).unwrap_or_default()
}

// ── Embedding cache ─────────────────────────────────────────────

/// Maximum number of per-agent cache entries before the whole cache is
/// cleared to make room. Full invalidation keeps the eviction policy trivial.
const MAX_CACHED_AGENTS: usize = 16;

/// Agents with more embeddings than this are never cached; every query falls
/// through to SQLite. Bounds worst-case memory to roughly
/// `MAX_CACHED_AGENTS * MAX_CACHED_VECTORS * dims * 4` bytes.
const MAX_CACHED_VECTORS: usize = 65_536;

/// Per-agent cache of decoded chunk embeddings.
///
/// Policy:
/// - Entries are populated lazily by `get_all_chunk_embeddings`.
/// - `store_chunks` refreshes a present entry in place (embeddings are
///   `Arc`-shared, so a refresh copies pointers, not vector data).
/// - `invalidate` precisely removes the memory's pairs; mutations that can
///   change retrievability less predictably (`upsert_memory` of an existing
///   row, `update_stage`, `purge_tombstones`) drop the owning agent's entry,
///   and `batch_update_stages` clears the cache entirely. Dropped entries
///   repopulate on the next recall.
/// - A generation counter guards against a stale snapshot being inserted by
///   a reader that raced a concurrent mutation.
struct EmbeddingCache {
    entries: RwLock<HashMap<Uuid, Arc<EmbeddingSet>>>,
    generation: AtomicU64,
}

impl EmbeddingCache {
    fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            generation: AtomicU64::new(0),
        }
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Bump the generation so in-flight populates discard their snapshot.
    /// Called at the start of every mutating operation.
    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn lock_write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<Uuid, Arc<EmbeddingSet>>> {
        self.entries.write().unwrap_or_else(|p| p.into_inner())
    }

    fn get(&self, agent_id: &Uuid) -> Option<Arc<EmbeddingSet>> {
        self.entries
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(agent_id)
            .cloned()
    }

    /// Insert a freshly loaded snapshot unless a mutation happened since
    /// `expected_generation` was observed (the snapshot could be stale).
    fn insert_if_unchanged(
        &self,
        agent_id: Uuid,
        set: Arc<EmbeddingSet>,
        expected_generation: u64,
    ) {
        if set.len() > MAX_CACHED_VECTORS {
            return;
        }
        let mut entries = self.lock_write();
        if self.generation.load(Ordering::SeqCst) != expected_generation {
            return;
        }
        if !entries.contains_key(&agent_id) && entries.len() >= MAX_CACHED_AGENTS {
            entries.clear();
        }
        entries.insert(agent_id, set);
    }

    fn drop_agent(&self, agent_id: &Uuid) {
        self.bump();
        self.lock_write().remove(agent_id);
    }

    fn clear(&self) {
        self.bump();
        self.lock_write().clear();
    }

    /// Remove one memory's pairs from every cached entry.
    fn remove_memory(&self, memory_id: Uuid) {
        self.bump();
        let mut entries = self.lock_write();
        for set in entries.values_mut() {
            if set.iter().any(|(id, _)| *id == memory_id) {
                let filtered: EmbeddingSet = set
                    .iter()
                    .filter(|(id, _)| *id != memory_id)
                    .cloned()
                    .collect();
                *set = Arc::new(filtered);
            }
        }
    }

    /// Replace `memory_id`'s pairs in `agent_id`'s entry with `pairs`
    /// (empty `pairs` removes the memory). No-op when the agent isn't cached.
    fn refresh_memory(&self, agent_id: Uuid, memory_id: Uuid, pairs: EmbeddingSet) {
        self.bump();
        let mut entries = self.lock_write();
        if let Some(set) = entries.get_mut(&agent_id) {
            let mut next: EmbeddingSet = set
                .iter()
                .filter(|(id, _)| *id != memory_id)
                .cloned()
                .collect();
            next.extend(pairs);
            if next.len() > MAX_CACHED_VECTORS {
                entries.remove(&agent_id);
            } else {
                *set = Arc::new(next);
            }
        }
    }
}

// ── SQLite implementation ───────────────────────────────────────

/// SQLite-backed implementation of [MemoryStorage].
///
/// Uses WAL mode for concurrent reads, with proper indexes for each
/// query pattern. All blocking SQLite calls are dispatched to a
/// `spawn_blocking` pool to avoid starving the async runtime.
/// Decoded chunk embeddings are served from an in-memory per-agent cache
/// (see [EmbeddingCache]) so recall and dedup avoid full-table scans.
pub struct SqliteMemoryStorage {
    conn: Arc<tokio::sync::Mutex<rusqlite::Connection>>,
    embedding_cache: EmbeddingCache,
}

impl SqliteMemoryStorage {
    /// Open (or create) the memory database at the given path.
    /// Runs migrations to ensure the schema is up to date.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = rusqlite::Connection::open(path).map_err(storage_err)?;
        Self::init(conn)
    }

    /// Create an in-memory database (useful for tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = rusqlite::Connection::open_in_memory().map_err(storage_err)?;
        Self::init(conn)
    }

    fn init(conn: rusqlite::Connection) -> Result<Self> {
        // WAL mode: concurrent readers + single writer, crash-safe.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA cache_size = -65536;
             PRAGMA mmap_size = 268435456;
             PRAGMA temp_store = MEMORY;",
        )
        .map_err(storage_err)?;

        Self::run_migrations(&conn)?;

        Ok(Self {
            conn: Arc::new(tokio::sync::Mutex::new(conn)),
            embedding_cache: EmbeddingCache::new(),
        })
    }

    fn run_migrations(conn: &rusqlite::Connection) -> Result<()> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS memories (
                id              TEXT PRIMARY KEY,
                agent_id        TEXT NOT NULL,
                content         TEXT NOT NULL,
                content_hash    TEXT NOT NULL,

                -- Scoping
                scope       TEXT NOT NULL DEFAULT 'user'
                    CHECK (scope IN ('session','user','agent','global')),
                session_id  TEXT,
                user_id     TEXT,

                -- Lifecycle
                lifecycle_stage TEXT NOT NULL DEFAULT 'episodic'
                    CHECK (lifecycle_stage IN ('working','episodic','semantic','archival','tombstone')),

                -- Scoring
                importance          REAL NOT NULL DEFAULT 0.5,
                importance_source   TEXT NOT NULL DEFAULT 'heuristic',
                decay_rate          REAL NOT NULL DEFAULT 1.0,
                confidence          REAL NOT NULL DEFAULT 1.0,

                -- Access patterns
                access_count        INTEGER NOT NULL DEFAULT 0,
                last_accessed_at    TEXT,
                last_relevant_at    TEXT,
                created_at          TEXT NOT NULL,

                -- Consolidation
                parent_memory_ids   TEXT NOT NULL DEFAULT '[]',
                consolidation_generation INTEGER NOT NULL DEFAULT 0,
                proof_count         INTEGER NOT NULL DEFAULT 1,

                -- Temporal context
                occurred_start      TEXT,
                occurred_end        TEXT,

                -- Soft delete
                is_valid            INTEGER NOT NULL DEFAULT 1,
                invalidated_by      TEXT,
                invalidated_at      TEXT,

                -- Tags
                tags                TEXT NOT NULL DEFAULT '[]',

                -- Metadata
                metadata            TEXT NOT NULL DEFAULT '{}'
            );

            -- Hot-path indexes
            CREATE INDEX IF NOT EXISTS idx_memories_agent_stage
                ON memories(agent_id, lifecycle_stage) WHERE is_valid = 1;
            CREATE INDEX IF NOT EXISTS idx_memories_agent_time
                ON memories(agent_id, created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_memories_dedup
                ON memories(agent_id, content_hash);
            CREATE INDEX IF NOT EXISTS idx_memories_user
                ON memories(user_id, scope) WHERE is_valid = 1;
            CREATE INDEX IF NOT EXISTS idx_memories_session
                ON memories(session_id) WHERE is_valid = 1;
            CREATE INDEX IF NOT EXISTS idx_memories_agent_session_stage
                ON memories(agent_id, session_id, lifecycle_stage) WHERE is_valid = 1;

            CREATE TABLE IF NOT EXISTS chunks (
                id                      TEXT PRIMARY KEY,
                memory_id               TEXT NOT NULL REFERENCES memories(id),
                chunk_index             INTEGER NOT NULL,
                content                 TEXT NOT NULL,
                embedding               BLOB,
                embedding_model_version TEXT,
                created_at              TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_chunks_memory
                ON chunks(memory_id, chunk_index);

            CREATE TABLE IF NOT EXISTS extracted_facts (
                id                  TEXT PRIMARY KEY,
                source_memory_id    TEXT NOT NULL REFERENCES memories(id),
                fact_type           TEXT,
                subject             TEXT,
                predicate           TEXT,
                object              TEXT,
                confidence          REAL DEFAULT 1.0,
                valid_from          TEXT,
                valid_to            TEXT,
                extraction_method   TEXT,
                created_at          TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_facts_memory
                ON extracted_facts(source_memory_id);

            CREATE TABLE IF NOT EXISTS memory_links (
                source_id   TEXT NOT NULL,
                target_id   TEXT NOT NULL,
                link_type   TEXT NOT NULL,
                weight      REAL NOT NULL DEFAULT 1.0,
                created_at  TEXT NOT NULL,
                PRIMARY KEY (source_id, target_id, link_type)
            );
            CREATE INDEX IF NOT EXISTS idx_links_source
                ON memory_links(source_id);
            CREATE INDEX IF NOT EXISTS idx_links_target
                ON memory_links(target_id);

            -- FTS5 index for keyword search (replaces in-memory BM25)
            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                memory_id UNINDEXED,
                agent_id UNINDEXED,
                content,
                tokenize='unicode61 remove_diacritics 2'
            );

            ",
        )
        .map_err(storage_err)?;

        Ok(())
    }

    /// Helper: run a blocking closure on the connection inside spawn_blocking.
    pub async fn with_conn<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            f(&conn)
        })
        .await
        .map_err(|e| rustykrab_core::Error::Storage(format!("join error: {e}")))?
    }
}

/// Read a Memory row from a rusqlite Row.
fn row_to_memory(row: &rusqlite::Row) -> rusqlite::Result<Memory> {
    let id_str: String = row.get("id")?;
    let agent_id_str: String = row.get("agent_id")?;
    let scope_str: String = row.get("scope")?;
    let session_id_str: Option<String> = row.get("session_id")?;
    let user_id_str: Option<String> = row.get("user_id")?;
    let stage_str: String = row.get("lifecycle_stage")?;
    let importance_source_str: String = row.get("importance_source")?;
    let last_accessed_str: Option<String> = row.get("last_accessed_at")?;
    let last_relevant_str: Option<String> = row.get("last_relevant_at")?;
    let created_str: String = row.get("created_at")?;
    let parent_ids_str: String = row.get("parent_memory_ids")?;
    let occurred_start_str: Option<String> = row.get("occurred_start")?;
    let occurred_end_str: Option<String> = row.get("occurred_end")?;
    let invalidated_by_str: Option<String> = row.get("invalidated_by")?;
    let invalidated_at_str: Option<String> = row.get("invalidated_at")?;
    let tags_str: String = row.get("tags")?;
    let metadata_str: String = row.get("metadata")?;

    Ok(Memory {
        id: Uuid::parse_str(&id_str).unwrap_or_default(),
        agent_id: Uuid::parse_str(&agent_id_str).unwrap_or_default(),
        content: row.get("content")?,
        content_hash: row.get("content_hash")?,
        scope: str_to_scope(&scope_str),
        session_id: session_id_str.and_then(|s| Uuid::parse_str(&s).ok()),
        user_id: user_id_str.and_then(|s| Uuid::parse_str(&s).ok()),
        lifecycle_stage: str_to_lifecycle(&stage_str),
        importance: row.get("importance")?,
        importance_source: match importance_source_str.as_str() {
            "llm" => crate::types::ImportanceSource::Llm,
            "user" => crate::types::ImportanceSource::User,
            _ => crate::types::ImportanceSource::Heuristic,
        },
        decay_rate: row.get("decay_rate")?,
        confidence: row.get("confidence")?,
        access_count: row.get::<_, u32>("access_count")?,
        last_accessed_at: last_accessed_str
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
        last_relevant_at: last_relevant_str
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
        created_at: DateTime::parse_from_rfc3339(&created_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        parent_memory_ids: json_to_uuids(&parent_ids_str),
        consolidation_generation: row.get::<_, u32>("consolidation_generation")?,
        proof_count: row.get::<_, u32>("proof_count")?,
        occurred_start: occurred_start_str
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
        occurred_end: occurred_end_str
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
        is_valid: row.get::<_, i32>("is_valid")? != 0,
        invalidated_by: invalidated_by_str.and_then(|s| Uuid::parse_str(&s).ok()),
        invalidated_at: invalidated_at_str
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
        tags: serde_json::from_str(&tags_str).unwrap_or_default(),
        metadata: serde_json::from_str(&metadata_str).unwrap_or_default(),
    })
}

/// Read a MemoryLink row from a rusqlite Row.
fn row_to_link(row: &rusqlite::Row) -> rusqlite::Result<MemoryLink> {
    let src: String = row.get("source_id")?;
    let tgt: String = row.get("target_id")?;
    let lt: String = row.get("link_type")?;
    let created_str: String = row.get("created_at")?;
    Ok(MemoryLink {
        source_id: Uuid::parse_str(&src).unwrap_or_default(),
        target_id: Uuid::parse_str(&tgt).unwrap_or_default(),
        link_type: str_to_link_type(&lt),
        weight: row.get("weight")?,
        created_at: DateTime::parse_from_rfc3339(&created_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
    })
}

/// Batch size for `IN (...)` queries. Keeping it fixed bounds the number of
/// distinct SQL strings so `prepare_cached` can reuse statements.
const IN_BATCH: usize = 32;

/// Build `?N,?N+1,...` placeholders starting at `first` for `count` params.
fn placeholders(first: usize, count: usize) -> String {
    (first..first + count)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[async_trait]
impl MemoryStorage for SqliteMemoryStorage {
    async fn fts_index(&self, memory_id: Uuid, agent_id: Uuid, content: &str) -> Result<()> {
        let mid = memory_id.to_string();
        let aid = agent_id.to_string();
        let text = content.to_string();
        self.with_conn(move |conn| {
            // Delete any existing entry for this memory (re-index safe).
            conn.execute(
                "DELETE FROM memories_fts WHERE memory_id = ?1",
                params![mid],
            )
            .map_err(storage_err)?;
            conn.execute(
                "INSERT INTO memories_fts (memory_id, agent_id, content) VALUES (?1, ?2, ?3)",
                params![mid, aid, text],
            )
            .map_err(storage_err)?;
            Ok(())
        })
        .await
    }

    async fn fts_index_batch(&self, agent_id: Uuid, entries: Vec<(Uuid, String)>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let aid = agent_id.to_string();
        let entries: Vec<(String, String)> = entries
            .into_iter()
            .map(|(id, content)| (id.to_string(), content))
            .collect();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction().map_err(storage_err)?;
            {
                let mut del = tx
                    .prepare("DELETE FROM memories_fts WHERE memory_id = ?1")
                    .map_err(storage_err)?;
                let mut ins = tx
                    .prepare(
                        "INSERT INTO memories_fts (memory_id, agent_id, content)
                         VALUES (?1, ?2, ?3)",
                    )
                    .map_err(storage_err)?;
                for (mid, content) in &entries {
                    del.execute(params![mid]).map_err(storage_err)?;
                    ins.execute(params![mid, aid, content])
                        .map_err(storage_err)?;
                }
            }
            tx.commit().map_err(storage_err)?;
            Ok(())
        })
        .await
    }

    async fn fts_search(
        &self,
        query: &str,
        agent_id: Uuid,
        limit: usize,
    ) -> Result<Vec<(Uuid, usize)>> {
        let agent_str = agent_id.to_string();
        let query = query.to_string();
        self.with_conn(move |conn| {
            // Tokenize query words and join with OR for broad matching.
            let fts_query: String = query
                .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
                .filter(|w| w.len() >= 2)
                .map(|w| format!("\"{w}\""))
                .collect::<Vec<_>>()
                .join(" OR ");

            if fts_query.is_empty() {
                return Ok(Vec::new());
            }

            let mut stmt = conn
                .prepare(
                    "SELECT memory_id, rank
                     FROM memories_fts
                     WHERE memories_fts MATCH ?1 AND agent_id = ?2
                     ORDER BY rank
                     LIMIT ?3",
                )
                .map_err(storage_err)?;

            let rows = stmt
                .query_map(params![fts_query, agent_str, limit as u32], |row| {
                    let mid: String = row.get(0)?;
                    Ok(Uuid::parse_str(&mid).unwrap_or_default())
                })
                .map_err(storage_err)?;

            let mut results = Vec::new();
            for (rank, row) in rows.enumerate() {
                let id = row.map_err(storage_err)?;
                results.push((id, rank));
            }
            Ok(results)
        })
        .await
    }

    async fn upsert_memory(&self, memory: &Memory) -> Result<()> {
        let m = memory.clone();
        let agent_id = memory.agent_id;
        let existed = self
            .with_conn(move |conn| {
                // Updating an existing row can change its retrievability, which
                // stales the embedding cache; new rows have no chunks yet.
                let existed: bool = conn
                    .query_row(
                        "SELECT 1 FROM memories WHERE id = ?1",
                        params![m.id.to_string()],
                        |_| Ok(()),
                    )
                    .optional()
                    .map_err(storage_err)?
                    .is_some();
                conn.execute(
                    "INSERT INTO memories (
                    id, agent_id, content, content_hash,
                    scope, session_id, user_id,
                    lifecycle_stage, importance, importance_source,
                    decay_rate, confidence, access_count,
                    last_accessed_at, last_relevant_at, created_at,
                    parent_memory_ids, consolidation_generation, proof_count,
                    occurred_start, occurred_end,
                    is_valid, invalidated_by, invalidated_at,
                    tags, metadata
                ) VALUES (
                    ?1, ?2, ?3, ?4,
                    ?5, ?6, ?7,
                    ?8, ?9, ?10,
                    ?11, ?12, ?13,
                    ?14, ?15, ?16,
                    ?17, ?18, ?19,
                    ?20, ?21,
                    ?22, ?23, ?24,
                    ?25, ?26
                ) ON CONFLICT(id) DO UPDATE SET
                    scope = excluded.scope,
                    session_id = excluded.session_id,
                    user_id = excluded.user_id,
                    lifecycle_stage = excluded.lifecycle_stage,
                    importance = excluded.importance,
                    importance_source = excluded.importance_source,
                    decay_rate = excluded.decay_rate,
                    confidence = excluded.confidence,
                    access_count = excluded.access_count,
                    last_accessed_at = excluded.last_accessed_at,
                    last_relevant_at = excluded.last_relevant_at,
                    parent_memory_ids = excluded.parent_memory_ids,
                    consolidation_generation = excluded.consolidation_generation,
                    proof_count = excluded.proof_count,
                    occurred_start = excluded.occurred_start,
                    occurred_end = excluded.occurred_end,
                    is_valid = excluded.is_valid,
                    invalidated_by = excluded.invalidated_by,
                    invalidated_at = excluded.invalidated_at,
                    tags = excluded.tags,
                    metadata = excluded.metadata",
                    params![
                        m.id.to_string(),
                        m.agent_id.to_string(),
                        m.content,
                        m.content_hash,
                        scope_to_str(m.scope),
                        m.session_id.map(|u| u.to_string()),
                        m.user_id.map(|u| u.to_string()),
                        lifecycle_to_str(m.lifecycle_stage),
                        m.importance,
                        match m.importance_source {
                            crate::types::ImportanceSource::Heuristic => "heuristic",
                            crate::types::ImportanceSource::Llm => "llm",
                            crate::types::ImportanceSource::User => "user",
                        },
                        m.decay_rate,
                        m.confidence,
                        m.access_count,
                        m.last_accessed_at.map(|t| t.to_rfc3339()),
                        m.last_relevant_at.map(|t| t.to_rfc3339()),
                        m.created_at.to_rfc3339(),
                        uuids_to_json(&m.parent_memory_ids),
                        m.consolidation_generation,
                        m.proof_count,
                        m.occurred_start.map(|t| t.to_rfc3339()),
                        m.occurred_end.map(|t| t.to_rfc3339()),
                        m.is_valid as i32,
                        m.invalidated_by.map(|u| u.to_string()),
                        m.invalidated_at.map(|t| t.to_rfc3339()),
                        serde_json::to_string(&m.tags).unwrap_or_else(|_| "[]".into()),
                        m.metadata.to_string(),
                    ],
                )
                .map_err(storage_err)?;
                Ok(existed)
            })
            .await?;
        if existed {
            self.embedding_cache.drop_agent(&agent_id);
        }
        Ok(())
    }

    async fn get_memory(&self, id: Uuid) -> Result<Option<Memory>> {
        let id_str = id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare("SELECT * FROM memories WHERE id = ?1")
                .map_err(storage_err)?;
            let mut rows = stmt
                .query_map(params![id_str], row_to_memory)
                .map_err(storage_err)?;
            match rows.next() {
                Some(Ok(mem)) => Ok(Some(mem)),
                Some(Err(e)) => Err(storage_err(e)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn get_memories(&self, ids: &[Uuid]) -> Result<Vec<Memory>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let id_strings: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        self.with_conn(move |conn| {
            let mut results = Vec::with_capacity(id_strings.len());
            // Fixed-size placeholder batches keep the SQL text stable so the
            // prepared-statement cache can reuse it (no interpolated literals).
            for batch in id_strings.chunks(IN_BATCH) {
                let sql = format!(
                    "SELECT * FROM memories WHERE id IN ({})",
                    placeholders(1, batch.len())
                );
                let mut stmt = conn.prepare_cached(&sql).map_err(storage_err)?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(batch.iter()), row_to_memory)
                    .map_err(storage_err)?;
                for row in rows {
                    results.push(row.map_err(storage_err)?);
                }
            }
            Ok(results)
        })
        .await
    }

    async fn find_by_content_hash(
        &self,
        agent_id: Uuid,
        content_hash: &str,
    ) -> Result<Option<Memory>> {
        let agent_str = agent_id.to_string();
        let hash = content_hash.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT * FROM memories
                     WHERE agent_id = ?1 AND content_hash = ?2 AND is_valid = 1
                     LIMIT 1",
                )
                .map_err(storage_err)?;
            let mut rows = stmt
                .query_map(params![agent_str, hash], row_to_memory)
                .map_err(storage_err)?;
            match rows.next() {
                Some(Ok(mem)) => Ok(Some(mem)),
                Some(Err(e)) => Err(storage_err(e)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn list_by_stage(&self, agent_id: Uuid, stage: LifecycleStage) -> Result<Vec<Memory>> {
        let agent_str = agent_id.to_string();
        let stage_str = lifecycle_to_str(stage).to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT * FROM memories
                     WHERE agent_id = ?1 AND lifecycle_stage = ?2 AND is_valid = 1",
                )
                .map_err(storage_err)?;
            let rows = stmt
                .query_map(params![agent_str, stage_str], row_to_memory)
                .map_err(storage_err)?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row.map_err(storage_err)?);
            }
            Ok(results)
        })
        .await
    }

    async fn list_by_session_and_stage(
        &self,
        agent_id: Uuid,
        session_id: Uuid,
        stage: LifecycleStage,
    ) -> Result<Vec<Memory>> {
        let agent_str = agent_id.to_string();
        let session_str = session_id.to_string();
        let stage_str = lifecycle_to_str(stage).to_string();
        self.with_conn(move |conn| {
            // Uses the indexed session_id column (idx_memories_agent_session_stage)
            // instead of a per-row json_extract over metadata.
            let mut stmt = conn
                .prepare(
                    "SELECT * FROM memories
                     WHERE agent_id = ?1 AND session_id = ?2
                       AND lifecycle_stage = ?3 AND is_valid = 1",
                )
                .map_err(storage_err)?;
            let rows = stmt
                .query_map(params![agent_str, session_str, stage_str], row_to_memory)
                .map_err(storage_err)?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row.map_err(storage_err)?);
            }
            Ok(results)
        })
        .await
    }

    async fn list_retrievable(&self, agent_id: Uuid) -> Result<Vec<Memory>> {
        let agent_str = agent_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT * FROM memories
                     WHERE agent_id = ?1 AND is_valid = 1
                       AND lifecycle_stage IN ('working', 'episodic', 'semantic')",
                )
                .map_err(storage_err)?;
            let rows = stmt
                .query_map(params![agent_str], row_to_memory)
                .map_err(storage_err)?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row.map_err(storage_err)?);
            }
            Ok(results)
        })
        .await
    }

    async fn list_by_time_range(
        &self,
        agent_id: Uuid,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<Memory>> {
        let agent_str = agent_id.to_string();
        let from_str = from.to_rfc3339();
        let to_str = to.to_rfc3339();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT * FROM memories
                     WHERE agent_id = ?1 AND is_valid = 1
                       AND lifecycle_stage IN ('working', 'episodic', 'semantic')
                       AND created_at >= ?2 AND created_at <= ?3
                     ORDER BY created_at DESC
                     LIMIT ?4",
                )
                .map_err(storage_err)?;
            let rows = stmt
                .query_map(
                    params![agent_str, from_str, to_str, limit as u32],
                    row_to_memory,
                )
                .map_err(storage_err)?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row.map_err(storage_err)?);
            }
            Ok(results)
        })
        .await
    }

    async fn update_stage(&self, id: Uuid, stage: LifecycleStage) -> Result<()> {
        let id_str = id.to_string();
        let stage_str = lifecycle_to_str(stage).to_string();
        let agent = self
            .with_conn(move |conn| {
                let agent: Option<String> = conn
                    .query_row(
                        "SELECT agent_id FROM memories WHERE id = ?1",
                        params![id_str],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(storage_err)?;
                conn.execute(
                    "UPDATE memories SET lifecycle_stage = ?2 WHERE id = ?1",
                    params![id_str, stage_str],
                )
                .map_err(storage_err)?;
                Ok(agent)
            })
            .await?;
        // A stage change can move the memory in or out of the retrievable set.
        if let Some(agent_id) = agent.and_then(|s| Uuid::parse_str(&s).ok()) {
            self.embedding_cache.drop_agent(&agent_id);
        }
        Ok(())
    }

    async fn record_access(&self, id: Uuid) -> Result<()> {
        let id_str = id.to_string();
        let now = Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE memories
                 SET access_count = access_count + 1,
                     last_accessed_at = ?2
                 WHERE id = ?1",
                params![id_str, now],
            )
            .map_err(storage_err)?;
            Ok(())
        })
        .await
    }

    async fn record_access_batch(&self, ids: &[Uuid]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let id_strings: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        let now = Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction().map_err(storage_err)?;
            for batch in id_strings.chunks(IN_BATCH) {
                let sql = format!(
                    "UPDATE memories
                     SET access_count = access_count + 1,
                         last_accessed_at = ?1
                     WHERE id IN ({})",
                    placeholders(2, batch.len())
                );
                let mut stmt = tx.prepare_cached(&sql).map_err(storage_err)?;
                stmt.execute(rusqlite::params_from_iter(
                    std::iter::once(&now).chain(batch.iter()),
                ))
                .map_err(storage_err)?;
            }
            tx.commit().map_err(storage_err)?;
            Ok(())
        })
        .await
    }

    async fn invalidate(&self, id: Uuid, invalidated_by: Option<Uuid>) -> Result<()> {
        let id_str = id.to_string();
        let by_str = invalidated_by.map(|u| u.to_string());
        let now = Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE memories
                 SET is_valid = 0,
                     lifecycle_stage = 'tombstone',
                     invalidated_by = ?2,
                     invalidated_at = ?3
                 WHERE id = ?1",
                params![id_str, by_str, now],
            )
            .map_err(storage_err)?;
            Ok(())
        })
        .await?;
        // Tombstoned memories are no longer retrievable; drop their embeddings.
        self.embedding_cache.remove_memory(id);
        Ok(())
    }

    // ── Chunks ──────────────────────────────────────────────────

    async fn store_chunks(&self, chunks: &[MemoryChunk]) -> Result<()> {
        let chunks = chunks.to_vec();
        let cache_updates = self
            .with_conn(move |conn| {
                let tx = conn.unchecked_transaction().map_err(storage_err)?;
                {
                    let mut stmt = tx
                        .prepare(
                            "INSERT OR REPLACE INTO chunks
                             (id, memory_id, chunk_index, content, embedding,
                              embedding_model_version, created_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        )
                        .map_err(storage_err)?;

                    for chunk in &chunks {
                        let emb_blob = embedding_to_blob(&chunk.embedding);
                        stmt.execute(params![
                            chunk.id.to_string(),
                            chunk.memory_id.to_string(),
                            chunk.chunk_index,
                            chunk.content,
                            emb_blob,
                            chunk.embedding_model_version,
                            chunk.created_at.to_rfc3339(),
                        ])
                        .map_err(storage_err)?;
                    }
                }

                // Map each affected memory to its owning agent and current
                // retrievability so the embedding cache can be refreshed
                // incrementally instead of dropped.
                let mut owners: HashMap<Uuid, Option<(Uuid, bool)>> = HashMap::new();
                for chunk in &chunks {
                    if owners.contains_key(&chunk.memory_id) {
                        continue;
                    }
                    let owner = tx
                        .query_row(
                            "SELECT agent_id, is_valid, lifecycle_stage
                             FROM memories WHERE id = ?1",
                            params![chunk.memory_id.to_string()],
                            |row| {
                                let agent: String = row.get(0)?;
                                let is_valid: i32 = row.get(1)?;
                                let stage: String = row.get(2)?;
                                Ok((agent, is_valid != 0, stage))
                            },
                        )
                        .optional()
                        .map_err(storage_err)?
                        .and_then(|(agent, is_valid, stage)| {
                            let agent_id = Uuid::parse_str(&agent).ok()?;
                            let retrievable = is_valid && str_to_lifecycle(&stage).is_retrievable();
                            Some((agent_id, retrievable))
                        });
                    owners.insert(chunk.memory_id, owner);
                }
                tx.commit().map_err(storage_err)?;

                // (agent_id, memory_id, pairs) per affected memory; pairs is
                // empty when the memory isn't retrievable.
                let mut updates: HashMap<Uuid, (Uuid, EmbeddingSet)> = HashMap::new();
                for chunk in chunks {
                    if let Some(Some((agent_id, retrievable))) = owners.get(&chunk.memory_id) {
                        let entry = updates
                            .entry(chunk.memory_id)
                            .or_insert_with(|| (*agent_id, EmbeddingSet::new()));
                        if *retrievable && !chunk.embedding.is_empty() {
                            entry.1.push((chunk.memory_id, Arc::from(chunk.embedding)));
                        }
                    }
                }
                Ok(updates)
            })
            .await?;

        for (memory_id, (agent_id, pairs)) in cache_updates {
            self.embedding_cache
                .refresh_memory(agent_id, memory_id, pairs);
        }
        Ok(())
    }

    async fn get_chunks_for_memory(&self, memory_id: Uuid) -> Result<Vec<MemoryChunk>> {
        let mem_str = memory_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT * FROM chunks
                     WHERE memory_id = ?1
                     ORDER BY chunk_index",
                )
                .map_err(storage_err)?;
            let rows = stmt
                .query_map(params![mem_str], |row| {
                    let id_str: String = row.get("id")?;
                    let mem_id_str: String = row.get("memory_id")?;
                    let emb_blob: Vec<u8> = row.get("embedding")?;
                    let created_str: String = row.get("created_at")?;

                    Ok(MemoryChunk {
                        id: Uuid::parse_str(&id_str).unwrap_or_default(),
                        memory_id: Uuid::parse_str(&mem_id_str).unwrap_or_default(),
                        chunk_index: row.get("chunk_index")?,
                        content: row.get("content")?,
                        embedding: blob_to_embedding(&emb_blob),
                        embedding_model_version: row
                            .get::<_, Option<String>>("embedding_model_version")?
                            .unwrap_or_default(),
                        created_at: DateTime::parse_from_rfc3339(&created_str)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                    })
                })
                .map_err(storage_err)?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row.map_err(storage_err)?);
            }
            Ok(results)
        })
        .await
    }

    async fn get_all_chunk_embeddings(&self, agent_id: Uuid) -> Result<Arc<EmbeddingSet>> {
        if let Some(cached) = self.embedding_cache.get(&agent_id) {
            return Ok(cached);
        }

        let generation = self.embedding_cache.generation();
        let agent_str = agent_id.to_string();
        let set = self
            .with_conn(move |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT c.memory_id, c.embedding
                         FROM chunks c
                         JOIN memories m ON m.id = c.memory_id
                         WHERE m.agent_id = ?1 AND m.is_valid = 1
                           AND m.lifecycle_stage IN ('working', 'episodic', 'semantic')
                           AND c.embedding IS NOT NULL",
                    )
                    .map_err(storage_err)?;
                let rows = stmt
                    .query_map(params![agent_str], |row| {
                        let mem_id_str: String = row.get(0)?;
                        let emb_blob: Vec<u8> = row.get(1)?;
                        Ok((
                            Uuid::parse_str(&mem_id_str).unwrap_or_default(),
                            blob_to_embedding(&emb_blob),
                        ))
                    })
                    .map_err(storage_err)?;
                let mut results = EmbeddingSet::new();
                for row in rows {
                    let (id, emb) = row.map_err(storage_err)?;
                    if !emb.is_empty() {
                        results.push((id, Arc::from(emb)));
                    }
                }
                Ok(results)
            })
            .await?;

        let set = Arc::new(set);
        self.embedding_cache
            .insert_if_unchanged(agent_id, Arc::clone(&set), generation);
        Ok(set)
    }

    // ── Extracted facts ─────────────────────────────────────────

    async fn store_facts(&self, facts: &[ExtractedFact]) -> Result<()> {
        let facts = facts.to_vec();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction().map_err(storage_err)?;
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT OR REPLACE INTO extracted_facts
                         (id, source_memory_id, fact_type, subject, predicate, object,
                          confidence, valid_from, valid_to, extraction_method, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    )
                    .map_err(storage_err)?;

                for fact in &facts {
                    stmt.execute(params![
                        fact.id.to_string(),
                        fact.source_memory_id.to_string(),
                        fact.fact_type,
                        fact.subject,
                        fact.predicate,
                        fact.object,
                        fact.confidence,
                        fact.valid_from.to_rfc3339(),
                        fact.valid_to.map(|t| t.to_rfc3339()),
                        fact.extraction_method,
                        fact.created_at.to_rfc3339(),
                    ])
                    .map_err(storage_err)?;
                }
            }
            tx.commit().map_err(storage_err)?;
            Ok(())
        })
        .await
    }

    async fn get_facts_for_memory(&self, memory_id: Uuid) -> Result<Vec<ExtractedFact>> {
        let mem_str = memory_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare("SELECT * FROM extracted_facts WHERE source_memory_id = ?1")
                .map_err(storage_err)?;
            let rows = stmt
                .query_map(params![mem_str], |row| {
                    let id_str: String = row.get("id")?;
                    let src_str: String = row.get("source_memory_id")?;
                    let valid_from_str: String = row.get("valid_from")?;
                    let valid_to_str: Option<String> = row.get("valid_to")?;
                    let created_str: String = row.get("created_at")?;

                    Ok(ExtractedFact {
                        id: Uuid::parse_str(&id_str).unwrap_or_default(),
                        source_memory_id: Uuid::parse_str(&src_str).unwrap_or_default(),
                        fact_type: row.get("fact_type")?,
                        subject: row.get("subject")?,
                        predicate: row.get("predicate")?,
                        object: row.get("object")?,
                        confidence: row.get("confidence")?,
                        valid_from: DateTime::parse_from_rfc3339(&valid_from_str)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                        valid_to: valid_to_str
                            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                            .map(|dt| dt.with_timezone(&Utc)),
                        extraction_method: row.get("extraction_method")?,
                        created_at: DateTime::parse_from_rfc3339(&created_str)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                    })
                })
                .map_err(storage_err)?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row.map_err(storage_err)?);
            }
            Ok(results)
        })
        .await
    }

    // ── Memory links ────────────────────────────────────────────

    async fn upsert_link(&self, link: &MemoryLink) -> Result<()> {
        let link = link.clone();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO memory_links (source_id, target_id, link_type, weight, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(source_id, target_id, link_type) DO UPDATE SET
                     weight = excluded.weight,
                     created_at = excluded.created_at",
                params![
                    link.source_id.to_string(),
                    link.target_id.to_string(),
                    link_type_to_str(link.link_type),
                    link.weight,
                    link.created_at.to_rfc3339(),
                ],
            )
            .map_err(storage_err)?;
            Ok(())
        })
        .await
    }

    async fn get_links_from(&self, source_id: Uuid) -> Result<Vec<MemoryLink>> {
        let src_str = source_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare("SELECT * FROM memory_links WHERE source_id = ?1")
                .map_err(storage_err)?;
            let rows = stmt
                .query_map(params![src_str], row_to_link)
                .map_err(storage_err)?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row.map_err(storage_err)?);
            }
            Ok(results)
        })
        .await
    }

    async fn get_links_from_many(&self, source_ids: &[Uuid]) -> Result<Vec<MemoryLink>> {
        if source_ids.is_empty() {
            return Ok(Vec::new());
        }
        let id_strings: Vec<String> = source_ids.iter().map(|id| id.to_string()).collect();
        self.with_conn(move |conn| {
            let mut results = Vec::new();
            for batch in id_strings.chunks(IN_BATCH) {
                let sql = format!(
                    "SELECT * FROM memory_links WHERE source_id IN ({})",
                    placeholders(1, batch.len())
                );
                let mut stmt = conn.prepare_cached(&sql).map_err(storage_err)?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(batch.iter()), row_to_link)
                    .map_err(storage_err)?;
                for row in rows {
                    results.push(row.map_err(storage_err)?);
                }
            }
            Ok(results)
        })
        .await
    }

    async fn get_links_for(&self, memory_id: Uuid) -> Result<Vec<MemoryLink>> {
        let id_str = memory_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT * FROM memory_links
                     WHERE source_id = ?1 OR target_id = ?1",
                )
                .map_err(storage_err)?;
            let rows = stmt
                .query_map(params![id_str], row_to_link)
                .map_err(storage_err)?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row.map_err(storage_err)?);
            }
            Ok(results)
        })
        .await
    }

    // ── Bulk operations ─────────────────────────────────────────

    async fn batch_update_stages(&self, updates: &[(Uuid, LifecycleStage)]) -> Result<u32> {
        if updates.is_empty() {
            return Ok(0);
        }
        let updates: Vec<(String, String)> = updates
            .iter()
            .map(|(id, stage)| (id.to_string(), lifecycle_to_str(*stage).to_string()))
            .collect();
        let now = Utc::now().to_rfc3339();
        let count = self
            .with_conn(move |conn| {
                let tx = conn.unchecked_transaction().map_err(storage_err)?;
                let mut count = 0u32;
                {
                    // Stamp invalidated_at when tombstoning so the retention
                    // sweep can age tombstones from their transition time.
                    let mut stmt = tx
                        .prepare(
                            "UPDATE memories
                             SET lifecycle_stage = ?2,
                                 invalidated_at = CASE
                                     WHEN ?2 = 'tombstone'
                                         THEN COALESCE(invalidated_at, ?3)
                                     ELSE invalidated_at
                                 END
                             WHERE id = ?1",
                        )
                        .map_err(storage_err)?;
                    for (id_str, stage_str) in &updates {
                        let affected = stmt
                            .execute(params![id_str, stage_str, now])
                            .map_err(storage_err)?;
                        count += affected as u32;
                    }
                }
                tx.commit().map_err(storage_err)?;
                Ok(count)
            })
            .await?;
        // Stage changes can alter which memories are retrievable; this path is
        // sweep/session-end only, so a full cache clear keeps it simple.
        self.embedding_cache.clear();
        Ok(count)
    }

    async fn purge_tombstones(&self, agent_id: Uuid, older_than: DateTime<Utc>) -> Result<u32> {
        let agent_str = agent_id.to_string();
        let cutoff = older_than.to_rfc3339();
        let purged = self
            .with_conn(move |conn| {
                // Tombstone age is invalidated_at when set (soft delete or
                // tombstoning sweep), falling back to created_at. RFC 3339
                // UTC strings compare lexicographically.
                const SELECTOR: &str = "SELECT id FROM memories
                     WHERE agent_id = ?1 AND lifecycle_stage = 'tombstone'
                       AND COALESCE(invalidated_at, created_at) < ?2";

                let tx = conn.unchecked_transaction().map_err(storage_err)?;
                for sql in [
                    format!("DELETE FROM chunks WHERE memory_id IN ({SELECTOR})"),
                    format!("DELETE FROM extracted_facts WHERE source_memory_id IN ({SELECTOR})"),
                    format!(
                        "DELETE FROM memory_links
                         WHERE source_id IN ({SELECTOR}) OR target_id IN ({SELECTOR})"
                    ),
                    format!("DELETE FROM memories_fts WHERE memory_id IN ({SELECTOR})"),
                ] {
                    tx.execute(&sql, params![agent_str, cutoff])
                        .map_err(storage_err)?;
                }
                let purged = tx
                    .execute(
                        "DELETE FROM memories
                         WHERE agent_id = ?1 AND lifecycle_stage = 'tombstone'
                           AND COALESCE(invalidated_at, created_at) < ?2",
                        params![agent_str, cutoff],
                    )
                    .map_err(storage_err)?;
                tx.commit().map_err(storage_err)?;
                Ok(purged as u32)
            })
            .await?;
        if purged > 0 {
            self.embedding_cache.drop_agent(&agent_id);
        }
        Ok(purged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ImportanceSource;
    use chrono::Duration;

    fn test_memory(agent_id: Uuid, session_id: Option<Uuid>, content: &str) -> Memory {
        Memory {
            id: Uuid::new_v4(),
            agent_id,
            content: content.to_string(),
            content_hash: format!("hash-{content}"),
            scope: MemoryScope::User,
            session_id,
            user_id: None,
            lifecycle_stage: LifecycleStage::Working,
            importance: 0.5,
            importance_source: ImportanceSource::Heuristic,
            decay_rate: 1.0,
            confidence: 1.0,
            access_count: 0,
            last_accessed_at: None,
            last_relevant_at: None,
            created_at: Utc::now(),
            parent_memory_ids: Vec::new(),
            consolidation_generation: 0,
            proof_count: 1,
            occurred_start: None,
            occurred_end: None,
            is_valid: true,
            invalidated_by: None,
            invalidated_at: None,
            tags: Vec::new(),
            metadata: serde_json::json!({}),
        }
    }

    fn test_chunk(memory_id: Uuid, embedding: Vec<f32>) -> MemoryChunk {
        MemoryChunk {
            id: Uuid::new_v4(),
            memory_id,
            chunk_index: 0,
            content: "chunk".to_string(),
            embedding,
            embedding_model_version: "test".to_string(),
            created_at: Utc::now(),
        }
    }

    /// The session query filters on the indexed session_id column, not on
    /// metadata (which no longer duplicates session_id).
    #[tokio::test]
    async fn list_by_session_and_stage_uses_column() {
        let storage = SqliteMemoryStorage::open_in_memory().unwrap();
        let agent_id = Uuid::new_v4();
        let session_a = Uuid::new_v4();
        let session_b = Uuid::new_v4();

        let in_session = test_memory(agent_id, Some(session_a), "in session");
        let other_session = test_memory(agent_id, Some(session_b), "other session");
        let mut wrong_stage = test_memory(agent_id, Some(session_a), "wrong stage");
        wrong_stage.lifecycle_stage = LifecycleStage::Episodic;

        for mem in [&in_session, &other_session, &wrong_stage] {
            storage.upsert_memory(mem).await.unwrap();
        }

        let found = storage
            .list_by_session_and_stage(agent_id, session_a, LifecycleStage::Working)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, in_session.id);
    }

    /// One batched call updates access metadata on every requested row.
    #[tokio::test]
    async fn record_access_batch_updates_all_rows() {
        let storage = SqliteMemoryStorage::open_in_memory().unwrap();
        let agent_id = Uuid::new_v4();

        let mut ids = Vec::new();
        for i in 0..3 {
            let mem = test_memory(agent_id, None, &format!("memory {i}"));
            storage.upsert_memory(&mem).await.unwrap();
            ids.push(mem.id);
        }

        storage.record_access_batch(&ids).await.unwrap();
        storage.record_access_batch(&ids[..1]).await.unwrap();

        for (i, id) in ids.iter().enumerate() {
            let mem = storage.get_memory(*id).await.unwrap().unwrap();
            let expected = if i == 0 { 2 } else { 1 };
            assert_eq!(mem.access_count, expected, "memory {i}");
            assert!(mem.last_accessed_at.is_some(), "memory {i}");
        }
    }

    /// get_memories uses placeholder batches; ID sets larger than one batch
    /// still return every row.
    #[tokio::test]
    async fn get_memories_spans_placeholder_batches() {
        let storage = SqliteMemoryStorage::open_in_memory().unwrap();
        let agent_id = Uuid::new_v4();

        let mut ids = Vec::new();
        for i in 0..(IN_BATCH + 5) {
            let mem = test_memory(agent_id, None, &format!("memory {i}"));
            storage.upsert_memory(&mem).await.unwrap();
            ids.push(mem.id);
        }

        let found = storage.get_memories(&ids).await.unwrap();
        assert_eq!(found.len(), IN_BATCH + 5);
    }

    /// Consecutive recalls share one cached snapshot; chunk writes refresh it
    /// and invalidation (tombstone) removes the memory's embeddings.
    #[tokio::test]
    async fn embedding_cache_hit_refresh_and_invalidation() {
        let storage = SqliteMemoryStorage::open_in_memory().unwrap();
        let agent_id = Uuid::new_v4();

        let first = test_memory(agent_id, None, "first");
        storage.upsert_memory(&first).await.unwrap();
        storage
            .store_chunks(&[test_chunk(first.id, vec![1.0, 0.0])])
            .await
            .unwrap();

        // Populate, then hit: same Arc snapshot means no second table scan.
        let set1 = storage.get_all_chunk_embeddings(agent_id).await.unwrap();
        let set2 = storage.get_all_chunk_embeddings(agent_id).await.unwrap();
        assert!(
            Arc::ptr_eq(&set1, &set2),
            "second call should be a cache hit"
        );
        assert_eq!(set1.len(), 1);

        // A chunk write for a new memory refreshes the cached entry.
        let second = test_memory(agent_id, None, "second");
        storage.upsert_memory(&second).await.unwrap();
        storage
            .store_chunks(&[test_chunk(second.id, vec![0.0, 1.0])])
            .await
            .unwrap();
        let set3 = storage.get_all_chunk_embeddings(agent_id).await.unwrap();
        assert_eq!(set3.len(), 2);
        assert!(set3.iter().any(|(id, _)| *id == second.id));

        // Tombstoning removes the memory's embeddings from the cache.
        storage.invalidate(second.id, None).await.unwrap();
        let set4 = storage.get_all_chunk_embeddings(agent_id).await.unwrap();
        assert_eq!(set4.len(), 1);
        assert!(set4.iter().all(|(id, _)| *id != second.id));
    }

    /// Purging cascades to chunks, facts, links, and the FTS index, and
    /// leaves untombstoned memories alone.
    #[tokio::test]
    async fn purge_tombstones_cascades() {
        let storage = SqliteMemoryStorage::open_in_memory().unwrap();
        let agent_id = Uuid::new_v4();

        let doomed = test_memory(agent_id, None, "practical zebra facts");
        let survivor = test_memory(agent_id, None, "unrelated survivor");
        storage.upsert_memory(&doomed).await.unwrap();
        storage.upsert_memory(&survivor).await.unwrap();
        storage
            .store_chunks(&[test_chunk(doomed.id, vec![1.0, 0.0])])
            .await
            .unwrap();
        storage
            .store_facts(&[ExtractedFact {
                id: Uuid::new_v4(),
                source_memory_id: doomed.id,
                fact_type: "preference".to_string(),
                subject: "user".to_string(),
                predicate: "likes".to_string(),
                object: "zebras".to_string(),
                confidence: 1.0,
                valid_from: Utc::now(),
                valid_to: None,
                extraction_method: "regex".to_string(),
                created_at: Utc::now(),
            }])
            .await
            .unwrap();
        for (source_id, target_id) in [(doomed.id, survivor.id), (survivor.id, doomed.id)] {
            storage
                .upsert_link(&MemoryLink {
                    source_id,
                    target_id,
                    link_type: LinkType::SemanticSimilar,
                    weight: 1.0,
                    created_at: Utc::now(),
                })
                .await
                .unwrap();
        }
        storage
            .fts_index(doomed.id, agent_id, &doomed.content)
            .await
            .unwrap();

        storage.invalidate(doomed.id, None).await.unwrap();

        // Inside the retention window: nothing is purged.
        let kept = storage
            .purge_tombstones(agent_id, Utc::now() - Duration::days(1))
            .await
            .unwrap();
        assert_eq!(kept, 0);

        // Past the window: the tombstone and all satellite rows go away.
        let purged = storage
            .purge_tombstones(agent_id, Utc::now() + Duration::seconds(5))
            .await
            .unwrap();
        assert_eq!(purged, 1);

        assert!(storage.get_memory(doomed.id).await.unwrap().is_none());
        assert!(storage
            .get_chunks_for_memory(doomed.id)
            .await
            .unwrap()
            .is_empty());
        assert!(storage
            .get_facts_for_memory(doomed.id)
            .await
            .unwrap()
            .is_empty());
        assert!(storage.get_links_for(doomed.id).await.unwrap().is_empty());
        assert!(storage
            .fts_search("zebra", agent_id, 10)
            .await
            .unwrap()
            .is_empty());
        // The valid memory survives untouched.
        assert!(storage.get_memory(survivor.id).await.unwrap().is_some());
    }

    /// Tombstoning via batch_update_stages stamps invalidated_at, so old
    /// memories still get the full retention window after tombstoning.
    #[tokio::test]
    async fn batch_tombstone_starts_retention_clock() {
        let storage = SqliteMemoryStorage::open_in_memory().unwrap();
        let agent_id = Uuid::new_v4();

        let mut old = test_memory(agent_id, None, "ancient memory");
        old.created_at = Utc::now() - Duration::days(400);
        storage.upsert_memory(&old).await.unwrap();

        let updated = storage
            .batch_update_stages(&[(old.id, LifecycleStage::Tombstone)])
            .await
            .unwrap();
        assert_eq!(updated, 1);

        // Retention ages from the tombstone transition, not created_at.
        let purged = storage
            .purge_tombstones(agent_id, Utc::now() - Duration::days(30))
            .await
            .unwrap();
        assert_eq!(purged, 0);
        assert!(storage.get_memory(old.id).await.unwrap().is_some());
    }

    /// Batched link fetch returns the union of per-seed links.
    #[tokio::test]
    async fn get_links_from_many_matches_per_seed_queries() {
        let storage = SqliteMemoryStorage::open_in_memory().unwrap();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        for (source_id, target_id) in [(a, b), (a, c), (b, c)] {
            storage
                .upsert_link(&MemoryLink {
                    source_id,
                    target_id,
                    link_type: LinkType::SemanticSimilar,
                    weight: 0.9,
                    created_at: Utc::now(),
                })
                .await
                .unwrap();
        }

        let mut batched: Vec<(Uuid, Uuid)> = storage
            .get_links_from_many(&[a, b])
            .await
            .unwrap()
            .into_iter()
            .map(|l| (l.source_id, l.target_id))
            .collect();
        batched.sort();

        let mut individual = Vec::new();
        for source in [a, b] {
            for link in storage.get_links_from(source).await.unwrap() {
                individual.push((link.source_id, link.target_id));
            }
        }
        individual.sort();

        assert_eq!(batched, individual);
        assert_eq!(batched.len(), 3);
    }
}
