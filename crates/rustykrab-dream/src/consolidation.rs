//! One consolidation cycle, start to finish (see `DREAMING.md`).
//!
//! Sequences the pieces in the order the safety argument requires:
//!
//! 1. **Analyze** — is the evidence good enough to act on at all?
//! 2. **Plan** — what would change, computed against current state?
//! 3. **Stage** — write the change-set down, still inert.
//! 4. **Promote** — apply it, re-checking preconditions first.
//!
//! Every exit before step 4 leaves live state untouched, and step 4 leaves
//! behind a manifest that makes what it did reversible. A cycle that finds
//! nothing to do is recorded as aborted rather than skipped silently, so
//! "the loop ran and declined" is distinguishable from "the loop never
//! ran".

use std::sync::Arc;

use rustykrab_core::dream::{CycleStatus, DreamCycle};
use rustykrab_core::Result;
use rustykrab_store::DreamCycleStore;
use uuid::Uuid;

use crate::engine::{promote, CyclePolicy, PromotionRefusal};
use crate::mutation::MemoryMutator;
use crate::planner::{plan_consolidation, ConsolidationSource};
use crate::report::Readiness;

/// What a cycle did, for logging and for the caller to assert on.
#[derive(Debug, Clone, PartialEq)]
pub enum CycleOutcome {
    /// The evidence could not justify changing anything.
    Refused { reason: String },
    /// The cycle ran and found nothing worth doing.
    NothingToDo { clusters_considered: usize },
    /// A change-set was applied.
    Promoted {
        cycle_id: Uuid,
        applied: usize,
        skipped_stale: usize,
    },
}

impl CycleOutcome {
    pub fn describe(&self) -> String {
        match self {
            Self::Refused { reason } => format!("consolidation refused: {reason}"),
            Self::NothingToDo {
                clusters_considered,
            } => {
                format!("consolidation found nothing to do across {clusters_considered} cluster(s)")
            }
            Self::Promoted {
                applied,
                skipped_stale,
                ..
            } => {
                let mut s = format!("consolidation promoted {applied} change(s)");
                if *skipped_stale > 0 {
                    s.push_str(&format!(", {skipped_stale} skipped as stale"));
                }
                s
            }
        }
    }
}

/// Run one consolidation cycle.
///
/// The `readiness` argument comes from the Analyze stage and is checked
/// before anything is planned: computing a change-set the loop is not
/// permitted to apply wastes the connection it is competing for.
pub async fn run_consolidation_cycle(
    cycles: &DreamCycleStore,
    source: &dyn ConsolidationSource,
    mutator: &dyn MemoryMutator,
    agent_id: Uuid,
    readiness: Readiness,
    policy: &CyclePolicy,
) -> Result<CycleOutcome> {
    if !readiness.permits_mutation() {
        return Ok(CycleOutcome::Refused {
            reason: format!(
                "evidence is {} and cannot justify changing memory",
                readiness.as_str()
            ),
        });
    }

    let plan = plan_consolidation(source, agent_id, policy).await?;
    let cycle = DreamCycle::new(agent_id, "memory_consolidation").with_summary(plan.summary());

    if plan.is_empty() {
        // Recorded, not skipped: a cycle that declined is evidence the
        // loop is running and finding the system already tidy.
        cycles.stage(&cycle, &[]).await?;
        cycles.set_status(cycle.id, CycleStatus::Aborted).await?;
        return Ok(CycleOutcome::NothingToDo {
            clusters_considered: plan.clusters_considered,
        });
    }

    // Written down before anything is applied. A crash here leaves a
    // staged, inert cycle rather than a half-changed memory set.
    cycles.stage(&cycle, &plan.changes).await?;

    let promotion = promote(
        mutator,
        CycleStatus::Staged,
        readiness,
        &plan.changes,
        policy,
    )
    .await?;

    match promotion {
        Ok(applied) => {
            // Which changes actually landed, recorded before the cycle is
            // marked live. The manifest's whole job is to make a promoted
            // cycle reversible, and a manifest that records only what was
            // intended cannot do it: reversal would restore memories
            // promotion skipped as stale, undoing whatever decision made
            // them stale in the first place.
            cycles.mark_applied(cycle.id, &applied.applied).await?;
            cycles.set_status(cycle.id, CycleStatus::Promoted).await?;
            let summary = format!(
                "{} | applied {}, skipped {} stale",
                plan.summary(),
                applied.applied.len(),
                applied.skipped_stale.len()
            );
            cycles.set_summary(cycle.id, &summary).await?;
            Ok(CycleOutcome::Promoted {
                cycle_id: cycle.id,
                applied: applied.applied.len(),
                skipped_stale: applied.skipped_stale.len(),
            })
        }
        Err(refusal) => {
            cycles.set_status(cycle.id, CycleStatus::Aborted).await?;
            Ok(CycleOutcome::Refused {
                reason: refusal.describe(),
            })
        }
    }
}

/// Convenience wiring for the daemon: cluster source and mutator over the
/// same memory system.
pub struct ConsolidationContext {
    pub cycles: DreamCycleStore,
    pub system: Arc<rustykrab_memory::MemorySystem>,
    pub agent_id: Uuid,
    pub policy: CyclePolicy,
}

impl ConsolidationContext {
    /// Run a cycle, constructing a fresh cycle id for provenance.
    pub async fn run(&self, readiness: Readiness) -> Result<CycleOutcome> {
        let cycle_id = Uuid::new_v4();
        let source = crate::cluster_source::MemoryClusterSource::new(Arc::clone(&self.system));
        let mutator = crate::memory_mutator::MemorySystemMutator::new(
            Arc::clone(&self.system),
            self.agent_id,
            cycle_id,
        );
        run_consolidation_cycle(
            &self.cycles,
            &source,
            &mutator,
            self.agent_id,
            readiness,
            &self.policy,
        )
        .await
    }
}

/// Refusal rendered for a log line.
pub fn refusal_line(refusal: &PromotionRefusal) -> String {
    format!("consolidation refused: {}", refusal.describe())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_mutator::MemorySystemMutator;
    use crate::planner::MemoryCandidate;
    use rustykrab_core::dream::StagedChange;
    use rustykrab_memory::embedding::HashEmbedder;
    use rustykrab_memory::storage::SqliteMemoryStorage;
    use rustykrab_memory::types::{ImportanceSource, LifecycleStage, Memory, MemoryScope};
    use rustykrab_memory::{MemoryConfig, MemorySystem};

    fn store() -> rustykrab_store::Store {
        // A temp directory the test owns for the lifetime of the process;
        // the store needs a real path for its master key handling.
        let dir = std::env::temp_dir().join(format!("rk-dream-{}", Uuid::new_v4()));
        rustykrab_store::Store::open(&dir, vec![7u8; 32]).expect("store opens")
    }

    fn live_system() -> Arc<MemorySystem> {
        let storage = Arc::new(SqliteMemoryStorage::open_in_memory().unwrap());
        Arc::new(MemorySystem::new(
            MemoryConfig::default(),
            storage,
            Arc::new(HashEmbedder::new(64)),
        ))
    }

    async fn seed(system: &MemorySystem, agent: Uuid, content: &str, accesses: u32) -> Memory {
        let m = Memory {
            id: Uuid::new_v4(),
            agent_id: agent,
            content: content.to_string(),
            content_hash: rustykrab_memory::hash_content(content),
            scope: MemoryScope::User,
            session_id: None,
            user_id: None,
            lifecycle_stage: LifecycleStage::Episodic,
            importance: 0.6,
            importance_source: ImportanceSource::Heuristic,
            decay_rate: 1.0,
            confidence: 1.0,
            access_count: accesses,
            last_accessed_at: None,
            last_relevant_at: None,
            created_at: chrono::Utc::now(),
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
        };
        system.storage().upsert_memory(&m).await.unwrap();
        m
    }

    /// Feeds the planner a fixed cluster so a cycle can be driven without
    /// depending on embedding similarity.
    struct FixedClusters(Vec<Vec<MemoryCandidate>>);

    #[async_trait::async_trait]
    impl ConsolidationSource for FixedClusters {
        async fn duplicate_clusters(&self, _: Uuid) -> Result<Vec<Vec<MemoryCandidate>>> {
            Ok(self.0.clone())
        }
    }

    fn candidates(memories: &[&Memory]) -> Vec<MemoryCandidate> {
        memories
            .iter()
            .map(|m| MemoryCandidate {
                id: m.id,
                content_hash: m.content_hash.clone(),
                importance: m.importance,
                access_count: m.access_count,
                proof_count: m.proof_count,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_full_cycle_removes_duplicates_and_records_what_it_did() {
        let store = store();
        let cycles = store.dream_cycles();
        let system = live_system();
        let agent = Uuid::new_v4();

        let keeper = seed(&system, agent, "user prefers aisle seats", 9).await;
        let dup = seed(&system, agent, "user prefers aisle seats", 0).await;

        let source = FixedClusters(vec![candidates(&[&keeper, &dup])]);
        let mutator = MemorySystemMutator::new(Arc::clone(&system), agent, Uuid::new_v4());

        let outcome = run_consolidation_cycle(
            &cycles,
            &source,
            &mutator,
            agent,
            Readiness::Ready,
            &CyclePolicy::default(),
        )
        .await
        .unwrap();

        let cycle_id = match outcome {
            CycleOutcome::Promoted {
                cycle_id, applied, ..
            } => {
                assert_eq!(applied, 1, "one duplicate retired");
                cycle_id
            }
            other => panic!("expected a promotion, got {other:?}"),
        };

        // Live state: the used memory survived, the duplicate did not.
        let keeper_after = system
            .storage()
            .get_memory(keeper.id)
            .await
            .unwrap()
            .unwrap();
        let dup_after = system.storage().get_memory(dup.id).await.unwrap().unwrap();
        assert!(keeper_after.is_valid, "the used memory survives");
        assert!(!dup_after.is_valid, "the duplicate is retired");
        assert_eq!(dup_after.invalidated_by, Some(keeper.id));

        // The manifest describes it, which is what makes it reversible.
        let recorded = cycles.get(cycle_id).await.unwrap().unwrap();
        assert_eq!(recorded.status, CycleStatus::Promoted);
        assert!(recorded.promoted_at.is_some());
        assert!(recorded.summary.unwrap().contains("applied 1"));

        let changes = cycles.applied_changes(cycle_id).await.unwrap();
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], StagedChange::InvalidateMemory { .. }));
    }

    #[tokio::test]
    async fn a_promoted_cycle_can_be_undone_from_its_manifest_alone() {
        // The manifest is the whole input to reversal — nothing else about
        // the cycle needs to still be in memory.
        let store = store();
        let cycles = store.dream_cycles();
        let system = live_system();
        let agent = Uuid::new_v4();

        let keeper = seed(&system, agent, "duplicated fact", 4).await;
        let dup = seed(&system, agent, "duplicated fact", 0).await;
        let source = FixedClusters(vec![candidates(&[&keeper, &dup])]);
        let mutator = MemorySystemMutator::new(Arc::clone(&system), agent, Uuid::new_v4());

        let CycleOutcome::Promoted { cycle_id, .. } = run_consolidation_cycle(
            &cycles,
            &source,
            &mutator,
            agent,
            Readiness::Ready,
            &CyclePolicy::default(),
        )
        .await
        .unwrap() else {
            panic!("expected a promotion");
        };

        assert!(
            !system
                .storage()
                .get_memory(dup.id)
                .await
                .unwrap()
                .unwrap()
                .is_valid
        );

        // Read the change-set back and reverse it.
        let recorded = cycles.get(cycle_id).await.unwrap().unwrap();
        let changes = cycles.changes(cycle_id).await.unwrap();
        let reverted =
            crate::engine::rollback(&mutator, recorded.status, &changes, &CyclePolicy::default())
                .await
                .unwrap()
                .expect("nothing has depended on it");
        cycles
            .set_status(cycle_id, CycleStatus::RolledBack)
            .await
            .unwrap();

        assert_eq!(reverted, 1);
        let dup_after = system.storage().get_memory(dup.id).await.unwrap().unwrap();
        assert!(dup_after.is_valid, "the retired duplicate came back");
        assert!(dup_after.lifecycle_stage.is_retrievable());
        assert_eq!(
            cycles.get(cycle_id).await.unwrap().unwrap().status,
            CycleStatus::RolledBack
        );
    }

    #[tokio::test]
    async fn proxy_only_evidence_stops_the_cycle_before_it_plans() {
        // The gate must bite before any work is done, not after.
        let store = store();
        let cycles = store.dream_cycles();
        let system = live_system();
        let agent = Uuid::new_v4();
        let a = seed(&system, agent, "dup", 1).await;
        let b = seed(&system, agent, "dup", 0).await;

        let source = FixedClusters(vec![candidates(&[&a, &b])]);
        let mutator = MemorySystemMutator::new(Arc::clone(&system), agent, Uuid::new_v4());

        let outcome = run_consolidation_cycle(
            &cycles,
            &source,
            &mutator,
            agent,
            Readiness::ProxyOnly,
            &CyclePolicy::default(),
        )
        .await
        .unwrap();

        assert!(matches!(outcome, CycleOutcome::Refused { .. }));
        assert!(
            system
                .storage()
                .get_memory(b.id)
                .await
                .unwrap()
                .unwrap()
                .is_valid,
            "nothing may be retired on proxy evidence"
        );
        assert!(
            cycles
                .list_by_status(agent, CycleStatus::Staged, 10)
                .await
                .unwrap()
                .is_empty(),
            "a refused cycle must not even stage a change-set"
        );
    }

    #[tokio::test]
    async fn a_cycle_that_finds_nothing_is_recorded_as_having_run() {
        // 'The loop ran and declined' must be distinguishable from 'the
        // loop never ran'.
        let store = store();
        let cycles = store.dream_cycles();
        let system = live_system();
        let agent = Uuid::new_v4();

        let outcome = run_consolidation_cycle(
            &cycles,
            &FixedClusters(vec![]),
            &MemorySystemMutator::new(Arc::clone(&system), agent, Uuid::new_v4()),
            agent,
            Readiness::Ready,
            &CyclePolicy::default(),
        )
        .await
        .unwrap();

        assert!(matches!(outcome, CycleOutcome::NothingToDo { .. }));
        let aborted = cycles
            .list_by_status(agent, CycleStatus::Aborted, 10)
            .await
            .unwrap();
        assert_eq!(aborted.len(), 1, "the declined cycle is on the record");
        assert!(aborted[0]
            .summary
            .as_ref()
            .unwrap()
            .contains("nothing worth changing"));
    }

    #[tokio::test]
    async fn a_cycle_never_retires_every_copy_of_a_fact() {
        // The property that matters most: consolidation must reduce
        // redundancy without losing the information itself.
        let store = store();
        let cycles = store.dream_cycles();
        let system = live_system();
        let agent = Uuid::new_v4();

        let a = seed(&system, agent, "the one fact", 3).await;
        let b = seed(&system, agent, "the one fact", 1).await;
        let c = seed(&system, agent, "the one fact", 0).await;

        run_consolidation_cycle(
            &cycles,
            &FixedClusters(vec![candidates(&[&a, &b, &c])]),
            &MemorySystemMutator::new(Arc::clone(&system), agent, Uuid::new_v4()),
            agent,
            Readiness::Ready,
            &CyclePolicy::default(),
        )
        .await
        .unwrap();

        let survivors = system.storage().list_retrievable(agent).await.unwrap();
        assert_eq!(
            survivors.len(),
            1,
            "exactly one copy must remain, got {}",
            survivors.len()
        );
        assert_eq!(survivors[0].id, a.id, "the most-used copy is the survivor");
        assert_eq!(
            survivors[0].content, "the one fact",
            "the information itself survives"
        );
    }
}
