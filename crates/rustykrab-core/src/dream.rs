//! Types for the mutating stages of the self-improvement outer loop —
//! the Plan and Execute halves of MAPE-K (see `DREAMING.md`).
//!
//! Phase 1 only reads. From here on the loop can change durable state, so
//! the governing constraint changes: every mutation must be **staged before
//! it is live** and **reversible after it is**.
//!
//! The shape that provides both is a cycle. A cycle computes its whole
//! change-set against a frozen view, records it, and touches nothing. A
//! later promote applies the set atomically. A manifest of what it did is
//! what makes the change reversible afterwards.
//!
//! Nothing here performs a mutation; these are the vocabulary and the
//! record. The engine lives in `rustykrab-dream`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Where a cycle is in the stage → promote → (maybe) revert progression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CycleStatus {
    /// Change-set computed and recorded. Live state is untouched.
    Staged,
    /// Applied to live state.
    Promoted,
    /// Applied, then undone.
    RolledBack,
    /// Discarded before ever being applied — the ordinary outcome when a
    /// cycle is preempted or finds nothing worth doing.
    Aborted,
}

impl CycleStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Promoted => "promoted",
            Self::RolledBack => "rolled_back",
            Self::Aborted => "aborted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "staged" => Some(Self::Staged),
            "promoted" => Some(Self::Promoted),
            "rolled_back" => Some(Self::RolledBack),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }

    /// Whether live state currently reflects this cycle.
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Promoted)
    }
}

/// One proposed alteration to durable state.
///
/// Deliberately small and closed. A change the engine cannot describe here
/// is a change it cannot reverse, and an irreversible mutation is exactly
/// what this design exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StagedChange {
    /// Write a new memory consolidated from existing ones.
    CreateMemory {
        memory_id: Uuid,
        content: String,
        /// Memories this one was distilled from. Recorded on the memory so
        /// its lineage survives independently of the manifest.
        parent_ids: Vec<Uuid>,
    },
    /// Retire a memory that a consolidation supersedes.
    ///
    /// Never a delete — retiring is a soft-delete, which is what makes the
    /// reverse operation possible at all.
    InvalidateMemory {
        memory_id: Uuid,
        superseded_by: Uuid,
        /// Content hash observed while planning. Promote refuses the change
        /// if the memory has moved since, so a cycle cannot silently
        /// clobber an edit made while it was thinking.
        expected_content_hash: String,
    },
}

impl StagedChange {
    /// The memory this change acts on.
    pub fn target_id(&self) -> Uuid {
        match self {
            Self::CreateMemory { memory_id, .. } => *memory_id,
            Self::InvalidateMemory { memory_id, .. } => *memory_id,
        }
    }

    pub fn op_name(&self) -> &'static str {
        match self {
            Self::CreateMemory { .. } => "create_memory",
            Self::InvalidateMemory { .. } => "invalidate_memory",
        }
    }

    /// Whether applying this change adds retrievable content rather than
    /// removing it. Used to keep a cycle's net effect balanced.
    pub fn is_additive(&self) -> bool {
        matches!(self, Self::CreateMemory { .. })
    }
}

/// One run of the mutating loop, and what it did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DreamCycle {
    pub id: Uuid,
    pub agent_id: Uuid,
    /// What kind of work this cycle performed, e.g.
    /// `"memory_consolidation"`.
    pub kind: String,
    pub status: CycleStatus,
    pub started_at: DateTime<Utc>,
    /// When the change-set went live. `None` until promoted.
    pub promoted_at: Option<DateTime<Utc>>,
    /// Human-readable account of what changed, for review.
    pub summary: Option<String>,
    pub rustykrab_version: Option<String>,
}

impl DreamCycle {
    pub fn new(agent_id: Uuid, kind: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            agent_id,
            kind: kind.into(),
            status: CycleStatus::Staged,
            started_at: Utc::now(),
            promoted_at: None,
            summary: None,
            rustykrab_version: Some(crate::VERSION.to_string()),
        }
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }
}

/// How a memory came to exist.
///
/// Kept separate from `ImportanceSource`, which describes where a memory's
/// *score* came from. A memory the loop wrote can still carry a
/// user-derived importance, so conflating the two would lose one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryOrigin {
    /// Captured from a conversation.
    #[default]
    Conversation,
    /// Written by the self-improvement outer loop.
    Dream,
}

impl MemoryOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Dream => "dream",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "conversation" => Some(Self::Conversation),
            "dream" => Some(Self::Dream),
            _ => None,
        }
    }
}

/// Why a promoted cycle can no longer be cleanly reversed.
///
/// Rollback is honest about its limits rather than pretending to be a time
/// machine: once the live system has built on a cycle's output, undoing it
/// destroys work that came after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RollbackBlocker {
    /// The cycle was never applied, so there is nothing to undo.
    NotPromoted,
    /// A memory the cycle produced has since been retrieved into a turn,
    /// so downstream state may depend on it.
    OutputAccessed { memory_id: Uuid, access_count: u32 },
    /// A memory the cycle retired has been changed since.
    ParentModified { memory_id: Uuid },
}

impl RollbackBlocker {
    pub fn describe(&self) -> String {
        match self {
            Self::NotPromoted => "cycle was never promoted".to_string(),
            Self::OutputAccessed {
                memory_id,
                access_count,
            } => format!(
                "memory {memory_id} produced by this cycle has been retrieved {access_count} time(s) since"
            ),
            Self::ParentModified { memory_id } => {
                format!("memory {memory_id} retired by this cycle has changed since")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_status_round_trips() {
        for s in [
            CycleStatus::Staged,
            CycleStatus::Promoted,
            CycleStatus::RolledBack,
            CycleStatus::Aborted,
        ] {
            assert_eq!(CycleStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(CycleStatus::parse("nonsense"), None);
    }

    #[test]
    fn only_promoted_is_live() {
        assert!(CycleStatus::Promoted.is_live());
        for s in [
            CycleStatus::Staged,
            CycleStatus::RolledBack,
            CycleStatus::Aborted,
        ] {
            assert!(!s.is_live(), "{s:?} must not read as live");
        }
    }

    #[test]
    fn staged_changes_round_trip_through_json() {
        // The manifest stores these as JSON, so a change that cannot
        // survive the round trip is a change that cannot be reversed.
        let changes = vec![
            StagedChange::CreateMemory {
                memory_id: Uuid::new_v4(),
                content: "merged fact".into(),
                parent_ids: vec![Uuid::new_v4(), Uuid::new_v4()],
            },
            StagedChange::InvalidateMemory {
                memory_id: Uuid::new_v4(),
                superseded_by: Uuid::new_v4(),
                expected_content_hash: "abc123".into(),
            },
        ];
        for change in changes {
            let json = serde_json::to_string(&change).unwrap();
            let back: StagedChange = serde_json::from_str(&json).unwrap();
            assert_eq!(change, back);
        }
    }

    #[test]
    fn additive_and_retiring_changes_are_distinguished() {
        let create = StagedChange::CreateMemory {
            memory_id: Uuid::new_v4(),
            content: "x".into(),
            parent_ids: vec![],
        };
        let retire = StagedChange::InvalidateMemory {
            memory_id: Uuid::new_v4(),
            superseded_by: Uuid::new_v4(),
            expected_content_hash: "h".into(),
        };
        assert!(create.is_additive());
        assert!(!retire.is_additive());
    }

    #[test]
    fn origin_is_separate_from_importance_source() {
        // A dream-written memory can still carry user-set importance, so
        // origin must be its own axis.
        assert_eq!(MemoryOrigin::default(), MemoryOrigin::Conversation);
        assert_eq!(MemoryOrigin::parse("dream"), Some(MemoryOrigin::Dream));
        assert_eq!(MemoryOrigin::parse("llm"), None);
    }

    #[test]
    fn blockers_explain_themselves() {
        let id = Uuid::new_v4();
        let accessed = RollbackBlocker::OutputAccessed {
            memory_id: id,
            access_count: 3,
        };
        assert!(accessed.describe().contains("3 time"));
        assert!(RollbackBlocker::NotPromoted
            .describe()
            .contains("never promoted"));
    }
}
