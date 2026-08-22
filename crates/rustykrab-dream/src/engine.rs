//! The stage → promote → revert engine (see `DREAMING.md`).
//!
//! Four properties define it, and each exists because of a specific way
//! this kind of loop goes wrong:
//!
//! 1. **Staging is inert.** A cycle computes its whole change-set against
//!    a frozen view and records it. Live state is untouched until promote,
//!    so an interrupted cycle leaves nothing half-applied.
//! 2. **Promotion is gated.** A cycle refuses to go live unless the
//!    evidence behind it could carry the decision. Without this the loop
//!    optimizes its own measurement.
//! 3. **Promotion re-checks its assumptions.** A change whose target moved
//!    while the cycle was thinking is skipped, not forced — otherwise a
//!    slow cycle silently clobbers a fresh edit.
//! 4. **Low gain.** A cycle changes few things at once. A control loop
//!    that makes large corrections from noisy measurements oscillates.
//!
//! Reversal is best-effort by contract, and says so: once the live system
//! has built on a cycle's output, undoing it destroys work that came
//! after. The engine reports that rather than pretending otherwise.

use rustykrab_core::dream::{CycleStatus, RollbackBlocker, StagedChange};
use rustykrab_core::{Error, Result};
use uuid::Uuid;

use crate::mutation::{apply, fact_for, revert, MemoryMutator};
use crate::report::Readiness;

/// How conservative a mutating cycle is.
#[derive(Debug, Clone)]
pub struct CyclePolicy {
    /// Maximum changes a single cycle may apply. Low gain: a loop that
    /// makes large corrections from noisy measurements oscillates.
    pub max_changes_per_cycle: usize,
    /// Whether a promoted cycle may still be reverted after one of its
    /// outputs has been retrieved. Off by default — beyond that point,
    /// reverting destroys whatever was built on it.
    pub allow_rollback_after_access: bool,
}

impl Default for CyclePolicy {
    fn default() -> Self {
        Self {
            max_changes_per_cycle: 8,
            allow_rollback_after_access: false,
        }
    }
}

/// Why a staged cycle was not promoted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionRefusal {
    /// The evidence behind the cycle cannot carry a mutation.
    InsufficientEvidence { readiness: Readiness },
    /// The cycle proposed more changes than the policy allows at once.
    TooManyChanges { proposed: usize, limit: usize },
    /// The cycle is not in a state that can be promoted.
    NotStaged { status: CycleStatus },
    /// Every change was stale, so promoting would have done nothing.
    AllChangesStale,
}

impl PromotionRefusal {
    pub fn describe(&self) -> String {
        match self {
            Self::InsufficientEvidence { readiness } => format!(
                "evidence is {} and cannot justify changing memory",
                readiness.as_str()
            ),
            Self::TooManyChanges { proposed, limit } => {
                format!("cycle proposed {proposed} changes, above the per-cycle limit of {limit}")
            }
            Self::NotStaged { status } => {
                format!("cycle is {}, not staged", status.as_str())
            }
            Self::AllChangesStale => {
                "every staged change targeted a memory that has since moved".to_string()
            }
        }
    }
}

/// What a promotion did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Promotion {
    pub applied: Vec<StagedChange>,
    /// Changes skipped because their target moved after planning. Reported
    /// rather than silently dropped: a high skip rate means cycles are
    /// racing live traffic and should run in quieter windows.
    pub skipped_stale: Vec<StagedChange>,
}

impl Promotion {
    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }
}

/// Decide whether a staged change still matches the world it was planned
/// against.
///
/// A `CreateMemory` has no precondition — it introduces a new id, so there
/// is nothing to have moved.
///
/// For a retirement, the collision that actually happens is *someone else
/// retired it first*: memory content is immutable in this system
/// (`upsert_memory` omits content from its conflict clause), so an "edit"
/// is really a retire-and-recreate. The content-hash comparison is kept as
/// defence-in-depth in case that ever changes, but `is_valid` is the check
/// that earns its keep today.
async fn is_still_applicable(mutator: &dyn MemoryMutator, change: &StagedChange) -> Result<bool> {
    match change {
        StagedChange::CreateMemory { .. } => Ok(true),
        StagedChange::InvalidateMemory {
            memory_id,
            expected_content_hash,
            ..
        } => {
            let Some(facts) = fact_for(mutator, *memory_id).await? else {
                // Gone entirely; nothing to retire.
                return Ok(false);
            };
            Ok(facts.is_valid && &facts.content_hash == expected_content_hash)
        }
    }
}

/// Apply a staged change-set to live state, subject to the policy.
///
/// Returns `Err(refusal)` when the cycle must not go live at all, and
/// `Ok(promotion)` describing what was applied otherwise.
pub async fn promote(
    mutator: &dyn MemoryMutator,
    status: CycleStatus,
    readiness: Readiness,
    changes: &[StagedChange],
    policy: &CyclePolicy,
) -> Result<std::result::Result<Promotion, PromotionRefusal>> {
    if status != CycleStatus::Staged {
        return Ok(Err(PromotionRefusal::NotStaged { status }));
    }

    // The phase gate. Evidence that cannot carry the decision must not be
    // allowed to make it, however tidy the proposed change looks.
    if !readiness.permits_mutation() {
        return Ok(Err(PromotionRefusal::InsufficientEvidence { readiness }));
    }

    if changes.len() > policy.max_changes_per_cycle {
        return Ok(Err(PromotionRefusal::TooManyChanges {
            proposed: changes.len(),
            limit: policy.max_changes_per_cycle,
        }));
    }

    // Staleness reconciliation happens before anything is written, so a
    // cycle never applies half a consolidation.
    let mut applicable = Vec::new();
    let mut skipped_stale = Vec::new();
    for change in changes {
        if is_still_applicable(mutator, change).await? {
            applicable.push(change.clone());
        } else {
            skipped_stale.push(change.clone());
        }
    }

    // A consolidation whose parents have all moved would otherwise write
    // the merged memory and retire nothing, leaving a duplicate behind.
    if applicable.iter().all(|c| c.is_additive()) && !skipped_stale.is_empty() {
        return Ok(Err(PromotionRefusal::AllChangesStale));
    }

    let mut applied = Vec::new();
    for change in &applicable {
        if let Err(e) = apply(mutator, change).await {
            // Unwind what this promotion already applied so live state does not
            // keep a partial change-set.
            for done in applied.iter().rev() {
                let _ = revert(mutator, done).await;
            }
            return Err(Error::Internal(format!(
                "promotion failed and was unwound: {e}"
            )));
        }
        applied.push(change.clone());
    }

    Ok(Ok(Promotion {
        applied,
        skipped_stale,
    }))
}

/// Whether a promoted cycle can still be cleanly reversed.
///
/// The probation window: once one of a cycle's outputs has been retrieved
/// into a turn, later state may depend on it, and undoing the cycle would
/// discard that. Reported as a blocker rather than silently proceeding.
pub async fn rollback_blockers(
    mutator: &dyn MemoryMutator,
    status: CycleStatus,
    changes: &[StagedChange],
    policy: &CyclePolicy,
) -> Result<Vec<RollbackBlocker>> {
    if !status.is_live() {
        return Ok(vec![RollbackBlocker::NotPromoted]);
    }

    let mut blockers = Vec::new();
    if !policy.allow_rollback_after_access {
        for change in changes {
            if let StagedChange::CreateMemory { memory_id, .. } = change {
                if let Some(facts) = fact_for(mutator, *memory_id).await? {
                    if facts.access_count > 0 {
                        blockers.push(RollbackBlocker::OutputAccessed {
                            memory_id: *memory_id,
                            access_count: facts.access_count,
                        });
                    }
                }
            }
        }
    }
    Ok(blockers)
}

/// Undo a promoted cycle.
///
/// Walks the change-set backwards, so a create/invalidate pair is undone
/// in the opposite order to the one that applied it. Refuses when the
/// window has closed, unless the policy says otherwise.
pub async fn rollback(
    mutator: &dyn MemoryMutator,
    status: CycleStatus,
    changes: &[StagedChange],
    policy: &CyclePolicy,
) -> Result<std::result::Result<usize, Vec<RollbackBlocker>>> {
    let blockers = rollback_blockers(mutator, status, changes, policy).await?;
    if !blockers.is_empty() {
        return Ok(Err(blockers));
    }

    let mut reverted = 0usize;
    for change in changes.iter().rev() {
        revert(mutator, change).await?;
        reverted += 1;
    }
    Ok(Ok(reverted))
}

/// Fresh ids for a consolidation, so a planner never reuses one.
pub fn new_memory_id() -> Uuid {
    Uuid::new_v4()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutation::MemoryFacts;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory stand-in for the durable store, so the engine's promises
    /// can be checked exactly rather than approximately.
    #[derive(Default)]
    struct FakeStore {
        memories: Mutex<HashMap<Uuid, MemoryFacts>>,
        /// Every call made, so a test can assert live state was not
        /// touched at all rather than merely ending up unchanged.
        calls: Mutex<Vec<String>>,
        fail_on: Mutex<Option<Uuid>>,
    }

    impl FakeStore {
        fn with_memory(self, id: Uuid, hash: &str, accesses: u32) -> Self {
            self.memories.lock().unwrap().insert(
                id,
                MemoryFacts {
                    id,
                    content_hash: hash.to_string(),
                    access_count: accesses,
                    is_valid: true,
                },
            );
            self
        }

        fn fail_creating(self, id: Uuid) -> Self {
            *self.fail_on.lock().unwrap() = Some(id);
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn is_valid(&self, id: Uuid) -> Option<bool> {
            self.memories.lock().unwrap().get(&id).map(|m| m.is_valid)
        }

        fn exists(&self, id: Uuid) -> bool {
            self.memories.lock().unwrap().contains_key(&id)
        }
    }

    #[async_trait::async_trait]
    impl MemoryMutator for FakeStore {
        async fn facts(&self, ids: &[Uuid]) -> Result<Vec<MemoryFacts>> {
            let map = self.memories.lock().unwrap();
            Ok(ids.iter().filter_map(|id| map.get(id).cloned()).collect())
        }

        async fn create(&self, memory_id: Uuid, _content: &str, _parents: &[Uuid]) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("create:{memory_id}"));
            if *self.fail_on.lock().unwrap() == Some(memory_id) {
                return Err(Error::Storage("disk on fire".into()));
            }
            self.memories.lock().unwrap().insert(
                memory_id,
                MemoryFacts {
                    id: memory_id,
                    content_hash: "new".into(),
                    access_count: 0,
                    is_valid: true,
                },
            );
            Ok(())
        }

        async fn invalidate(&self, memory_id: Uuid, _superseded_by: Uuid) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("invalidate:{memory_id}"));
            if let Some(m) = self.memories.lock().unwrap().get_mut(&memory_id) {
                m.is_valid = false;
            }
            Ok(())
        }

        async fn restore(&self, memory_id: Uuid) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("restore:{memory_id}"));
            if let Some(m) = self.memories.lock().unwrap().get_mut(&memory_id) {
                m.is_valid = true;
            }
            Ok(())
        }

        async fn discard(&self, memory_id: Uuid) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("discard:{memory_id}"));
            self.memories.lock().unwrap().remove(&memory_id);
            Ok(())
        }
    }

    fn consolidation(parents: &[(Uuid, &str)], child: Uuid) -> Vec<StagedChange> {
        let mut changes = vec![StagedChange::CreateMemory {
            memory_id: child,
            content: "merged".into(),
            parent_ids: parents.iter().map(|(id, _)| *id).collect(),
        }];
        for (id, hash) in parents {
            changes.push(StagedChange::InvalidateMemory {
                memory_id: *id,
                superseded_by: child,
                expected_content_hash: (*hash).to_string(),
            });
        }
        changes
    }

    // ---- Gate ----

    #[tokio::test]
    async fn proxy_only_evidence_cannot_change_memory() {
        // The whole point of the gate. Evidence that cannot carry the
        // decision must not be allowed to make it, however tidy the
        // proposed change looks.
        let parent = Uuid::new_v4();
        let store = FakeStore::default().with_memory(parent, "h1", 0);
        let changes = consolidation(&[(parent, "h1")], new_memory_id());

        let result = promote(
            &store,
            CycleStatus::Staged,
            Readiness::ProxyOnly,
            &changes,
            &CyclePolicy::default(),
        )
        .await
        .unwrap();

        assert!(matches!(
            result,
            Err(PromotionRefusal::InsufficientEvidence { .. })
        ));
        assert!(
            store.calls().is_empty(),
            "a refused promotion must not touch live state at all, got {:?}",
            store.calls()
        );
        assert_eq!(store.is_valid(parent), Some(true));
    }

    #[tokio::test]
    async fn insufficient_data_cannot_change_memory() {
        let store = FakeStore::default();
        let result = promote(
            &store,
            CycleStatus::Staged,
            Readiness::InsufficientData,
            &[],
            &CyclePolicy::default(),
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            Err(PromotionRefusal::InsufficientEvidence { .. })
        ));
    }

    #[tokio::test]
    async fn an_already_promoted_cycle_cannot_be_promoted_again() {
        // Double-promotion would apply the same change-set twice and leave
        // the manifest describing only one of them.
        let store = FakeStore::default();
        let result = promote(
            &store,
            CycleStatus::Promoted,
            Readiness::Ready,
            &[],
            &CyclePolicy::default(),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(PromotionRefusal::NotStaged { .. })));
        assert!(store.calls().is_empty());
    }

    // ---- Low gain ----

    #[tokio::test]
    async fn a_cycle_may_not_exceed_the_per_cycle_change_limit() {
        let parents: Vec<(Uuid, &str)> = (0..10).map(|_| (Uuid::new_v4(), "h")).collect();
        let mut store = FakeStore::default();
        for (id, h) in &parents {
            store = store.with_memory(*id, h, 0);
        }
        let changes = consolidation(&parents, new_memory_id());

        let result = promote(
            &store,
            CycleStatus::Staged,
            Readiness::Ready,
            &changes,
            &CyclePolicy {
                max_changes_per_cycle: 4,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(matches!(
            result,
            Err(PromotionRefusal::TooManyChanges { .. })
        ));
        assert!(
            store.calls().is_empty(),
            "an over-large cycle must be refused whole, not partially applied"
        );
    }

    // ---- Staleness ----

    #[tokio::test]
    async fn a_change_whose_target_moved_is_skipped_not_forced() {
        // The memory was edited while the cycle was thinking. Retiring it
        // anyway would clobber the newer edit.
        let fresh = Uuid::new_v4();
        let moved = Uuid::new_v4();
        let store = FakeStore::default()
            .with_memory(fresh, "unchanged", 0)
            .with_memory(moved, "edited-since", 0);

        let child = new_memory_id();
        let changes = consolidation(&[(fresh, "unchanged"), (moved, "as-planned")], child);

        let promotion = promote(
            &store,
            CycleStatus::Staged,
            Readiness::Ready,
            &changes,
            &CyclePolicy::default(),
        )
        .await
        .unwrap()
        .expect("promotion should proceed with the applicable subset");

        assert_eq!(promotion.skipped_stale.len(), 1);
        assert_eq!(promotion.skipped_stale[0].target_id(), moved);
        assert_eq!(
            store.is_valid(moved),
            Some(true),
            "the edited memory must survive untouched"
        );
        assert_eq!(store.is_valid(fresh), Some(false));
    }

    #[tokio::test]
    async fn a_consolidation_whose_parents_all_moved_is_refused_entirely() {
        // Otherwise the merged memory is written and nothing is retired,
        // leaving a duplicate behind — strictly worse than doing nothing.
        let parent = Uuid::new_v4();
        let store = FakeStore::default().with_memory(parent, "edited-since", 0);
        let child = new_memory_id();
        let changes = consolidation(&[(parent, "as-planned")], child);

        let result = promote(
            &store,
            CycleStatus::Staged,
            Readiness::Ready,
            &changes,
            &CyclePolicy::default(),
        )
        .await
        .unwrap();

        assert_eq!(result, Err(PromotionRefusal::AllChangesStale));
        assert!(
            !store.exists(child),
            "the merged memory must not be left behind as a duplicate"
        );
    }

    // ---- Atomicity ----

    #[tokio::test]
    async fn a_failed_promotion_unwinds_what_it_applied() {
        // Live state must never keep half a change-set.
        let parent = Uuid::new_v4();
        let doomed = new_memory_id();
        let store = FakeStore::default()
            .with_memory(parent, "h1", 0)
            .fail_creating(doomed);

        let changes = vec![
            StagedChange::InvalidateMemory {
                memory_id: parent,
                superseded_by: doomed,
                expected_content_hash: "h1".into(),
            },
            StagedChange::CreateMemory {
                memory_id: doomed,
                content: "merged".into(),
                parent_ids: vec![parent],
            },
        ];

        let err = promote(
            &store,
            CycleStatus::Staged,
            Readiness::Ready,
            &changes,
            &CyclePolicy::default(),
        )
        .await;

        assert!(err.is_err(), "a failed promotion must surface as an error");
        assert_eq!(
            store.is_valid(parent),
            Some(true),
            "the retired parent must be restored when the promotion unwinds"
        );
    }

    // ---- Reversal ----

    #[tokio::test]
    async fn a_promoted_cycle_reverts_to_its_starting_state() {
        let p1 = Uuid::new_v4();
        let p2 = Uuid::new_v4();
        let store = FakeStore::default()
            .with_memory(p1, "h1", 0)
            .with_memory(p2, "h2", 0);
        let child = new_memory_id();
        let changes = consolidation(&[(p1, "h1"), (p2, "h2")], child);

        promote(
            &store,
            CycleStatus::Staged,
            Readiness::Ready,
            &changes,
            &CyclePolicy::default(),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(store.is_valid(p1), Some(false));
        assert!(store.exists(child));

        let reverted = rollback(
            &store,
            CycleStatus::Promoted,
            &changes,
            &CyclePolicy::default(),
        )
        .await
        .unwrap()
        .expect("nothing has depended on the cycle yet");

        assert_eq!(reverted, changes.len());
        assert_eq!(store.is_valid(p1), Some(true), "parents come back");
        assert_eq!(store.is_valid(p2), Some(true));
        assert!(
            !store.exists(child),
            "the consolidated memory is discarded on revert"
        );
    }

    #[tokio::test]
    async fn reversal_walks_the_change_set_backwards() {
        // A create/invalidate pair must undo in the opposite order to the
        // one that applied it.
        let parent = Uuid::new_v4();
        let store = FakeStore::default().with_memory(parent, "h1", 0);
        let child = new_memory_id();
        let changes = consolidation(&[(parent, "h1")], child);

        promote(
            &store,
            CycleStatus::Staged,
            Readiness::Ready,
            &changes,
            &CyclePolicy::default(),
        )
        .await
        .unwrap()
        .unwrap();
        rollback(
            &store,
            CycleStatus::Promoted,
            &changes,
            &CyclePolicy::default(),
        )
        .await
        .unwrap()
        .unwrap();

        let calls = store.calls();
        let applied_create = calls.iter().position(|c| c == &format!("create:{child}"));
        let applied_inv = calls
            .iter()
            .position(|c| c == &format!("invalidate:{parent}"));
        let undo_restore = calls.iter().position(|c| c == &format!("restore:{parent}"));
        let undo_discard = calls.iter().position(|c| c == &format!("discard:{child}"));

        assert!(applied_create < applied_inv, "applied forwards");
        assert!(undo_restore < undo_discard, "reverted backwards");
    }

    #[tokio::test]
    async fn a_cycle_that_was_never_promoted_cannot_be_reverted() {
        let store = FakeStore::default();
        let blockers = rollback(&store, CycleStatus::Staged, &[], &CyclePolicy::default())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(blockers, vec![RollbackBlocker::NotPromoted]);
        assert!(store.calls().is_empty());
    }

    // ---- Probation window ----

    #[tokio::test]
    async fn reversal_is_blocked_once_an_output_has_been_used() {
        // Beyond this point undoing the cycle discards whatever was built
        // on its output, so the engine reports rather than proceeding.
        let parent = Uuid::new_v4();
        let store = FakeStore::default().with_memory(parent, "h1", 0);
        let child = new_memory_id();
        let changes = consolidation(&[(parent, "h1")], child);

        promote(
            &store,
            CycleStatus::Staged,
            Readiness::Ready,
            &changes,
            &CyclePolicy::default(),
        )
        .await
        .unwrap()
        .unwrap();

        // The consolidated memory gets retrieved into a turn.
        store
            .memories
            .lock()
            .unwrap()
            .get_mut(&child)
            .unwrap()
            .access_count = 2;

        let blockers = rollback(
            &store,
            CycleStatus::Promoted,
            &changes,
            &CyclePolicy::default(),
        )
        .await
        .unwrap()
        .unwrap_err();

        assert_eq!(blockers.len(), 1);
        assert!(matches!(
            blockers[0],
            RollbackBlocker::OutputAccessed {
                access_count: 2,
                ..
            }
        ));
        assert_eq!(
            store.is_valid(parent),
            Some(false),
            "a blocked rollback must change nothing"
        );
    }

    #[tokio::test]
    async fn the_probation_window_can_be_overridden_deliberately() {
        // Escape hatch for an operator who accepts the cost, but never the
        // default.
        let parent = Uuid::new_v4();
        let store = FakeStore::default().with_memory(parent, "h1", 0);
        let child = new_memory_id();
        let changes = consolidation(&[(parent, "h1")], child);
        let forceful = CyclePolicy {
            allow_rollback_after_access: true,
            ..Default::default()
        };

        promote(
            &store,
            CycleStatus::Staged,
            Readiness::Ready,
            &changes,
            &CyclePolicy::default(),
        )
        .await
        .unwrap()
        .unwrap();
        store
            .memories
            .lock()
            .unwrap()
            .get_mut(&child)
            .unwrap()
            .access_count = 5;

        let reverted = rollback(&store, CycleStatus::Promoted, &changes, &forceful)
            .await
            .unwrap()
            .expect("the override permits reversal");
        assert_eq!(reverted, changes.len());
        assert_eq!(store.is_valid(parent), Some(true));
    }
}
