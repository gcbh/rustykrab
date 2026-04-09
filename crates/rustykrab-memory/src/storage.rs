use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::params;
use rustykrab_core::Result;
use uuid::Uuid;

use crate::types::{
    ExtractedFact, LifecycleStage, LinkType, Memory, MemoryChunk, MemoryLink,
};

/// Abstract storage backend for the memory system.
///
/// All retrieval, write, and lifecycle operations go through this trait,
/// allowing different backends (SQLite, PostgreSQL) to be swapped.
#[async_trait]
pub trait MemoryStorage: Send + Sync {
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
    async fn list_by_stage(
        &self,
        agent_id: Uuid,
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

    /// Soft-delete: mark a memory as invalid.
    async fn invalidate(&self, id: Uuid, invalidated_by: Option<Uuid>) -> Result<()>;

    // ── Chunk operations ────────────────────────────────────────

    /// Store embedding chunks for a memory.
    async fn store_chunks(&self, chunks: &[MemoryChunk]) -> Result<()>;

    /// Retrieve all chunks for a memory.
    async fn get_chunks_for_memory(&self, memory_id: Uuid) -> Result<Vec<MemoryChunk>>;

    /// Retrieve all chunks with embeddings for an agent (for vector search).
    async fn get_all_chunk_embeddings(
        &self,
        agent_id: Uuid,
    ) -> Result<Vec<(Uuid, Vec<f32>)>>;

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

    /// Get all links involving a memory (incoming + outgoing).
    async fn get_links_for(&self, memory_id: Uuid) -> Result<Vec<MemoryLink>>;

    // ── Bulk operations ─────────────────────────────────────────

    /// Batch update lifecycle stages (used by sweep).
    async fn batch_update_stages(
        &self,
        updates: &[(Uuid, LifecycleStage)],
    ) -> Result<u32>;
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

// ── SQLite implementation ───────────────────────────────────────

/// SQLite-backed implementation of [MemoryStorage].
///
/// Uses WAL mode for concurrent reads, with proper indexes for each
/// query pattern. All blocking SQLite calls are dispatched to a
/// `spawn_blocking` pool to avoid starving the async runtime.
pub struct SqliteMemoryStorage {
    conn: Arc<tokio::sync::Mutex<rusqlite::Connection>>,
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
             PRAGMA busy_timeout = 5000;",
        )
        .map_err(storage_err)?;

        Self::run_migrations(&conn)?;

        Ok(Self {
            conn: Arc::new(tokio::sync::Mutex::new(conn)),
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
            ",
        )
        .map_err(storage_err)?;

        Ok(())
    }

    /// Helper: run a blocking closure on the connection inside spawn_blocking.
    async fn with_conn<F, T>(&self, f: F) -> Result<T>
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
        invalidated_by: invalidated_by_str
            .and_then(|s| Uuid::parse_str(&s).ok()),
        invalidated_at: invalidated_at_str
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
        tags: serde_json::from_str(&tags_str).unwrap_or_default(),
        metadata: serde_json::from_str(&metadata_str).unwrap_or_default(),
    })
}

#[async_trait]
impl MemoryStorage for SqliteMemoryStorage {
    async fn upsert_memory(&self, memory: &Memory) -> Result<()> {
        let m = memory.clone();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO memories (
                    id, agent_id, content, content_hash,
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
                    ?17, ?18,
                    ?19, ?20, ?21,
                    ?22, ?23
                ) ON CONFLICT(id) DO UPDATE SET
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
            Ok(())
        })
        .await
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
            let placeholders: String = id_strings
                .iter()
                .map(|s| format!("'{s}'"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("SELECT * FROM memories WHERE id IN ({placeholders})");
            let mut stmt = conn.prepare(&sql).map_err(storage_err)?;
            let rows = stmt.query_map([], row_to_memory).map_err(storage_err)?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row.map_err(storage_err)?);
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

    async fn list_by_stage(
        &self,
        agent_id: Uuid,
        stage: LifecycleStage,
    ) -> Result<Vec<Memory>> {
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
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE memories SET lifecycle_stage = ?2 WHERE id = ?1",
                params![id_str, stage_str],
            )
            .map_err(storage_err)?;
            Ok(())
        })
        .await
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
        .await
    }

    // ── Chunks ──────────────────────────────────────────────────

    async fn store_chunks(&self, chunks: &[MemoryChunk]) -> Result<()> {
        let chunks = chunks.to_vec();
        self.with_conn(move |conn| {
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
            tx.commit().map_err(storage_err)?;
            Ok(())
        })
        .await
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

    async fn get_all_chunk_embeddings(
        &self,
        agent_id: Uuid,
    ) -> Result<Vec<(Uuid, Vec<f32>)>> {
        let agent_str = agent_id.to_string();
        self.with_conn(move |conn| {
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
            let mut results = Vec::new();
            for row in rows {
                let (id, emb) = row.map_err(storage_err)?;
                if !emb.is_empty() {
                    results.push((id, emb));
                }
            }
            Ok(results)
        })
        .await
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
                .query_map(params![src_str], |row| {
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
                .query_map(params![id_str], |row| {
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

    // ── Bulk operations ─────────────────────────────────────────

    async fn batch_update_stages(
        &self,
        updates: &[(Uuid, LifecycleStage)],
    ) -> Result<u32> {
        let updates: Vec<(String, String)> = updates
            .iter()
            .map(|(id, stage)| (id.to_string(), lifecycle_to_str(*stage).to_string()))
            .collect();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction().map_err(storage_err)?;
            let mut count = 0u32;
            {
                let mut stmt = tx
                    .prepare("UPDATE memories SET lifecycle_stage = ?2 WHERE id = ?1")
                    .map_err(storage_err)?;
                for (id_str, stage_str) in &updates {
                    let affected = stmt
                        .execute(params![id_str, stage_str])
                        .map_err(storage_err)?;
                    count += affected as u32;
                }
            }
            tx.commit().map_err(storage_err)?;
            Ok(count)
        })
        .await
    }
}
