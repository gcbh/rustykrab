//! Applying and reversing a cycle's change-set.
//!
//! Declared as a trait so the engine can be exercised against fabricated
//! state, and so this crate does not depend on the memory implementation.
//!
//! The contract is deliberately coarse: whole change-sets, not individual
//! operations. A per-operation trait cannot express atomicity, and an
//! engine that applies a consolidation one row at a time leaves live
//! memory half-changed if the process dies partway — the exact state
//! staging exists to prevent. Pushing the batch into the implementation is
//! what lets the real one wrap it in a transaction.

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
    /// What retired this memory, when it is retired.
    ///
    /// The reversal path turns on it: restoring a tombstone that points
    /// somewhere else would undo a decision this cycle never made, and
    /// resurrect a memory something else deliberately retired.
    pub invalidated_by: Option<Uuid>,
}

/// The durable state a cycle acts on.
#[async_trait::async_trait]
pub trait MemoryMutator: Send + Sync {
    /// Facts about the given memories as they stand right now.
    async fn facts(&self, ids: &[Uuid]) -> Result<Vec<MemoryFacts>>;

    /// Apply a whole change-set, all of it or none of it.
    ///
    /// The implementation is responsible for atomicity. Returning `Err`
    /// must mean live state is unchanged.
    async fn apply_all(&self, changes: &[StagedChange]) -> Result<()>;

    /// Undo a change-set, walking it backwards, all of it or none of it.
    ///
    /// Returns how many changes were actually reversed. A change whose
    /// precondition no longer holds — a tombstone that now points
    /// somewhere else, a created memory that something has since edited —
    /// is skipped rather than forced, so a reversal cannot clobber a
    /// decision made after the cycle ran.
    async fn revert_all(&self, changes: &[StagedChange]) -> Result<usize>;
}

/// Look up one memory's facts, if it exists.
pub(crate) async fn fact_for(mutator: &dyn MemoryMutator, id: Uuid) -> Result<Option<MemoryFacts>> {
    Ok(mutator.facts(&[id]).await?.into_iter().find(|f| f.id == id))
}
