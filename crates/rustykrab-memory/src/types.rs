use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Lifecycle stage for memory promotion/demotion.
///
/// Memories progress through stages based on access patterns and value:
/// Working → Episodic → Semantic (promoted) or Archival → Tombstone (demoted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStage {
    /// Active working memory for the current session.
    Working,
    /// Recent episodic memories from past conversations.
    Episodic,
    /// Consolidated long-term knowledge (promoted from episodic).
    Semantic,
    /// Low-value memories moved out of the hot retrieval set.
    Archival,
    /// Soft-deleted; retained for audit but excluded from all retrieval.
    Tombstone,
}

impl LifecycleStage {
    /// Whether this stage is included in the hot retrieval set.
    pub fn is_retrievable(&self) -> bool {
        matches!(self, Self::Working | Self::Episodic | Self::Semantic)
    }
}

/// How the importance score was determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportanceSource {
    Heuristic,
    Llm,
    User,
}

/// A single memory record — the core unit of the memory system.
///
/// Stores verbatim content as the source of truth, with lifecycle metadata
/// for value-driven retrieval and decay management.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: Uuid,
    pub agent_id: Uuid,
    pub content: String,
    pub content_hash: String,

    // Lifecycle
    pub lifecycle_stage: LifecycleStage,

    // Scoring
    pub importance: f64,
    pub importance_source: ImportanceSource,
    pub decay_rate: f64,
    pub confidence: f64,

    // Access patterns
    pub access_count: u32,
    pub last_accessed_at: Option<DateTime<Utc>>,
    pub last_relevant_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,

    // Consolidation
    pub parent_memory_ids: Vec<Uuid>,
    pub consolidation_generation: u32,
    pub proof_count: u32,

    // Temporal context
    pub occurred_start: Option<DateTime<Utc>>,
    pub occurred_end: Option<DateTime<Utc>>,

    // Soft delete
    pub is_valid: bool,
    pub invalidated_by: Option<Uuid>,
    pub invalidated_at: Option<DateTime<Utc>>,

    // Tags for backward compatibility with existing tag-based retrieval.
    pub tags: Vec<String>,

    // Free-form metadata.
    pub metadata: serde_json::Value,
}

impl Memory {
    /// Compute the effective retrieval score combining importance, temporal
    /// decay, access pattern boost, and query similarity.
    ///
    /// Decay formula: `importance × e^(−effective_decay × idle_hours / 168)`
    /// where `effective_decay = decay_rate × (1 − importance × 0.8)` so that
    /// high-importance memories decay up to 5× slower.
    pub fn effective_score(&self, query_similarity: f64, now: DateTime<Utc>) -> f64 {
        // Use max(0, hours) instead of unsigned_abs() to surface clock skew (#119).
        let idle_hours = (now - self.last_accessed_at.unwrap_or(self.created_at))
            .num_hours()
            .max(0) as f64;

        // High-importance memories decay up to 5× slower.
        let effective_decay = self.decay_rate * (1.0 - self.importance * 0.8);
        let temporal_decay = (-effective_decay * idle_hours / 168.0).exp();

        // Each recall adds 2% boost, capped at 100% bonus.
        let access_boost = 1.0 + (self.access_count.min(50) as f64) * 0.02;

        self.importance * temporal_decay * access_boost * query_similarity
    }
}

/// A chunk of a memory's content, embedded for vector retrieval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryChunk {
    pub id: Uuid,
    pub memory_id: Uuid,
    pub chunk_index: u32,
    pub content: String,
    pub embedding: Vec<f32>,
    pub embedding_model_version: String,
    pub created_at: DateTime<Utc>,
}

/// A conversation turn as received from the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub id: Uuid,
    pub session_id: Uuid,
    pub turn_number: u32,
    pub speaker: String,
    pub content: String,
    pub token_count: Option<u32>,
    pub metadata: TurnMetadata,
}

/// Metadata attached to a conversation turn for importance scoring.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnMetadata {
    pub involves_tool_use: bool,
    pub user_flagged: bool,
    pub tags: Vec<String>,
}

/// An extracted fact (subject-predicate-object triple).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedFact {
    pub id: Uuid,
    pub source_memory_id: Uuid,
    pub fact_type: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub confidence: f64,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
    pub extraction_method: String,
    pub created_at: DateTime<Utc>,
}

/// A link between two memories (for graph-based retrieval).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryLink {
    pub source_id: Uuid,
    pub target_id: Uuid,
    pub link_type: LinkType,
    pub weight: f64,
    pub created_at: DateTime<Utc>,
}

/// Types of relationships between memories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkType {
    /// Semantic similarity (cosine ≥ 0.7).
    SemanticSimilar,
    /// Shared entity co-occurrence.
    EntityCooccurrence,
    /// Causal/temporal chain.
    CausalChain,
    /// Consolidation parent-child.
    Consolidation,
    /// Contradiction between memories.
    Contradicts,
}

/// Stable string representation for link keys (#147).
/// Use this instead of Debug formatting which is not stable across refactors.
impl std::fmt::Display for LinkType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SemanticSimilar => write!(f, "semantic_similar"),
            Self::EntityCooccurrence => write!(f, "entity_cooccurrence"),
            Self::CausalChain => write!(f, "causal_chain"),
            Self::Consolidation => write!(f, "consolidation"),
            Self::Contradicts => write!(f, "contradicts"),
        }
    }
}

/// Which retrieval strategy produced a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RetrievalSource {
    Semantic,
    Keyword,
    Graph,
    Temporal,
}

/// A single result from the retrieval pipeline.
#[derive(Debug, Clone)]
pub struct RetrievalResult {
    pub memory_id: Uuid,
    pub content: String,
    pub rrf_score: f64,
    pub effective_score: f64,
    pub sources: Vec<RetrievalSource>,
    pub memory: Memory,
}
