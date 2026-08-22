//! Applying and reversing a cycle's change-set.
//!
//! Declared as a trait so the engine can be exercised against fabricated
//! state, and so this crate does not depend on the memory implementation.
//! The contract is narrow on purpose: every method here has an inverse,
//! because a cycle that cannot be undone must not be applied.

use rustykrab_core::dream::StagedChange;
use rustykrab_core::Result;
use uuid::Uuid;

/// What the engine needs to know about a memory in order to plan against
/// it and to check, later, whether the ground has shifted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryFacts {
    pub id: Uuid,
    pub content_hash: String,
    /// Times this memory has been retrieved into a turn. The rollback
    /// probation window turns on this.
    pub access_count: u32,
    pub is_valid: bool,
}

/// The durable state a cycle acts on.
#[async_trait::async_trait]
pub trait MemoryMutator: Send + Sync {
    /// Facts about the given memories as they stand right now.
    async fn facts(&self, ids: &[Uuid]) -> Result<Vec<MemoryFacts>>;

    /// Write a new consolidated memory.
    async fn create(&self, memory_id: Uuid, content: &str, parent_ids: &[Uuid]) -> Result<()>;

    /// Retire a memory, recording what superseded it.
    async fn invalidate(&self, memory_id: Uuid, superseded_by: Uuid) -> Result<()>;

    /// Undo a retirement, returning the memory to the retrievable set.
    async fn restore(&self, memory_id: Uuid) -> Result<()>;

    /// Retire a memory the loop itself created, undoing a `create`.
    ///
    /// Distinct from `invalidate` so an implementation can tell "this was
    /// superseded" from "this should never have existed".
    async fn discard(&self, memory_id: Uuid) -> Result<()>;
}

/// Look up one memory's facts, if it exists.
pub(crate) async fn fact_for(mutator: &dyn MemoryMutator, id: Uuid) -> Result<Option<MemoryFacts>> {
    Ok(mutator.facts(&[id]).await?.into_iter().find(|f| f.id == id))
}

/// Apply one change to live state.
pub(crate) async fn apply(mutator: &dyn MemoryMutator, change: &StagedChange) -> Result<()> {
    match change {
        StagedChange::CreateMemory {
            memory_id,
            content,
            parent_ids,
        } => mutator.create(*memory_id, content, parent_ids).await,
        StagedChange::InvalidateMemory {
            memory_id,
            superseded_by,
            ..
        } => mutator.invalidate(*memory_id, *superseded_by).await,
    }
}

/// Undo one change.
///
/// The inverse of `apply`, and the reason the change vocabulary is closed:
/// every variant has exactly one way back.
pub(crate) async fn revert(mutator: &dyn MemoryMutator, change: &StagedChange) -> Result<()> {
    match change {
        StagedChange::CreateMemory { memory_id, .. } => mutator.discard(*memory_id).await,
        StagedChange::InvalidateMemory { memory_id, .. } => mutator.restore(*memory_id).await,
    }
}
