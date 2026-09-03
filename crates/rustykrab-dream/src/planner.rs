//! Deciding what a consolidation cycle should change (see `DREAMING.md`).
//!
//! This is the Plan stage, and it is deliberately **deterministic**. The
//! design reserves model-driven synthesis for later; the first thing worth
//! doing to a set of near-identical memories is not to rewrite them but to
//! stop keeping several copies.
//!
//! So a plan here keeps the best member of a duplicate cluster and retires
//! the rest against it. That preserves the survivor's access history and
//! decay state, which minting a fresh "merged" memory would throw away —
//! and for byte-similar content there is nothing to merge anyway.
//!
//! The change vocabulary already supports creating a synthesized memory;
//! no planner emits one yet.

use rustykrab_core::dream::StagedChange;
use rustykrab_core::Result;
use uuid::Uuid;

use crate::engine::CyclePolicy;

/// What the planner needs to know about a candidate memory.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryCandidate {
    pub id: Uuid,
    pub content_hash: String,
    pub importance: f64,
    pub access_count: u32,
    /// How many observations back this memory.
    pub proof_count: u32,
}

/// Supplies clusters of memories that say the same thing.
#[async_trait::async_trait]
pub trait ConsolidationSource: Send + Sync {
    /// Groups of near-identical memories. Each group has at least two
    /// members; a group of one is not a duplicate.
    async fn duplicate_clusters(&self, agent_id: Uuid) -> Result<Vec<Vec<MemoryCandidate>>>;
}

/// A proposed change-set, with enough context to explain itself.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsolidationPlan {
    pub changes: Vec<StagedChange>,
    pub clusters_considered: usize,
    pub clusters_planned: usize,
    /// Clusters left for a later cycle because this one hit its change
    /// limit. Reported rather than dropped silently, so a persistently
    /// large backlog is visible.
    pub clusters_deferred: usize,
}

impl ConsolidationPlan {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn summary(&self) -> String {
        if self.is_empty() {
            return format!(
                "consolidation: {} cluster(s) considered, nothing worth changing",
                self.clusters_considered
            );
        }
        let mut s = format!(
            "consolidation: {} of {} cluster(s) planned, {} memories retired",
            self.clusters_planned,
            self.clusters_considered,
            self.changes.len()
        );
        if self.clusters_deferred > 0 {
            s.push_str(&format!(
                ", {} deferred to a later cycle",
                self.clusters_deferred
            ));
        }
        s
    }
}

/// Pick the member of a cluster worth keeping.
///
/// Most-used first, then best-evidenced, then most important, with the id
/// as a final tie-break so the choice is stable across runs. Access count
/// leads because a memory the system actually retrieves has demonstrated
/// its worth, where importance is only ever an estimate of it.
fn representative(cluster: &[MemoryCandidate]) -> &MemoryCandidate {
    cluster
        .iter()
        .max_by(|a, b| {
            a.access_count
                .cmp(&b.access_count)
                .then(a.proof_count.cmp(&b.proof_count))
                .then(
                    a.importance
                        .partial_cmp(&b.importance)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                .then(a.id.cmp(&b.id))
        })
        .expect("clusters are never empty")
}

/// Plan a consolidation cycle for one agent.
///
/// Stops at the policy's change limit and reports what it left behind,
/// rather than proposing an unbounded change-set that promotion would
/// simply refuse.
pub async fn plan_consolidation(
    source: &dyn ConsolidationSource,
    agent_id: Uuid,
    policy: &CyclePolicy,
) -> Result<ConsolidationPlan> {
    let clusters = source.duplicate_clusters(agent_id).await?;
    let clusters_considered = clusters.len();

    let mut changes = Vec::new();
    let mut clusters_planned = 0usize;
    let mut clusters_deferred = 0usize;

    for cluster in clusters {
        // A cluster of one is not a duplicate.
        if cluster.len() < 2 {
            continue;
        }

        let keep = representative(&cluster).clone();
        let retire: Vec<&MemoryCandidate> = cluster.iter().filter(|m| m.id != keep.id).collect();

        // Take the cluster whole or not at all. Half-pruning a cluster
        // leaves duplicates behind and spends the budget achieving nothing.
        if changes.len() + retire.len() > policy.max_changes_per_cycle {
            clusters_deferred += 1;
            continue;
        }

        for member in retire {
            changes.push(StagedChange::InvalidateMemory {
                memory_id: member.id,
                superseded_by: keep.id,
                expected_content_hash: member.content_hash.clone(),
            });
        }
        clusters_planned += 1;
    }

    Ok(ConsolidationPlan {
        changes,
        clusters_considered,
        clusters_planned,
        clusters_deferred,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeSource(Vec<Vec<MemoryCandidate>>);

    #[async_trait::async_trait]
    impl ConsolidationSource for FakeSource {
        async fn duplicate_clusters(&self, _: Uuid) -> Result<Vec<Vec<MemoryCandidate>>> {
            Ok(self.0.clone())
        }
    }

    fn candidate(importance: f64, accesses: u32, proofs: u32) -> MemoryCandidate {
        MemoryCandidate {
            id: Uuid::new_v4(),
            content_hash: "h".into(),
            importance,
            access_count: accesses,
            proof_count: proofs,
        }
    }

    fn retired_ids(plan: &ConsolidationPlan) -> Vec<Uuid> {
        plan.changes.iter().map(|c| c.target_id()).collect()
    }

    #[tokio::test]
    async fn nothing_to_do_produces_no_changes() {
        let plan = plan_consolidation(&FakeSource(vec![]), Uuid::new_v4(), &CyclePolicy::default())
            .await
            .unwrap();
        assert!(plan.is_empty());
        assert!(plan.summary().contains("nothing worth changing"));
    }

    #[tokio::test]
    async fn a_cluster_of_one_is_not_a_duplicate() {
        let plan = plan_consolidation(
            &FakeSource(vec![vec![candidate(0.9, 5, 3)]]),
            Uuid::new_v4(),
            &CyclePolicy::default(),
        )
        .await
        .unwrap();
        assert!(plan.is_empty(), "a lone memory must never be retired");
    }

    #[tokio::test]
    async fn the_most_used_member_survives() {
        // A memory the system actually retrieves has demonstrated its
        // worth; importance is only an estimate of it.
        let used = candidate(0.4, 12, 1);
        let important_but_unused = candidate(0.99, 0, 9);
        let plan = plan_consolidation(
            &FakeSource(vec![vec![important_but_unused.clone(), used.clone()]]),
            Uuid::new_v4(),
            &CyclePolicy::default(),
        )
        .await
        .unwrap();

        assert_eq!(retired_ids(&plan), vec![important_but_unused.id]);
        match &plan.changes[0] {
            StagedChange::InvalidateMemory { superseded_by, .. } => {
                assert_eq!(*superseded_by, used.id, "the used memory survives")
            }
            other => panic!("expected a retirement, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn selection_is_stable_across_runs() {
        // Identical candidates must not produce a different survivor each
        // cycle, or consolidation would churn forever.
        let mut a = candidate(0.5, 3, 2);
        let mut b = candidate(0.5, 3, 2);
        a.id = Uuid::from_u128(1);
        b.id = Uuid::from_u128(2);

        for _ in 0..5 {
            let plan = plan_consolidation(
                &FakeSource(vec![vec![a.clone(), b.clone()]]),
                Uuid::new_v4(),
                &CyclePolicy::default(),
            )
            .await
            .unwrap();
            assert_eq!(retired_ids(&plan), vec![a.id], "survivor must be stable");
        }
    }

    #[tokio::test]
    async fn every_duplicate_in_a_cluster_is_retired_against_one_survivor() {
        let keep = candidate(0.5, 10, 1);
        let dup1 = candidate(0.5, 1, 1);
        let dup2 = candidate(0.5, 0, 1);
        let plan = plan_consolidation(
            &FakeSource(vec![vec![keep.clone(), dup1.clone(), dup2.clone()]]),
            Uuid::new_v4(),
            &CyclePolicy::default(),
        )
        .await
        .unwrap();

        assert_eq!(plan.changes.len(), 2);
        for change in &plan.changes {
            match change {
                StagedChange::InvalidateMemory { superseded_by, .. } => {
                    assert_eq!(*superseded_by, keep.id)
                }
                other => panic!("expected a retirement, got {other:?}"),
            }
        }
        assert!(
            !retired_ids(&plan).contains(&keep.id),
            "the survivor must never retire itself"
        );
    }

    #[tokio::test]
    async fn a_cluster_is_taken_whole_or_deferred() {
        // Half-pruning leaves duplicates behind and spends the budget
        // achieving nothing.
        let big: Vec<MemoryCandidate> = (0..6).map(|i| candidate(0.5, i, 1)).collect();
        let plan = plan_consolidation(
            &FakeSource(vec![big]),
            Uuid::new_v4(),
            &CyclePolicy {
                max_changes_per_cycle: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(
            plan.is_empty(),
            "the cluster needed 5 changes, budget was 3"
        );
        assert_eq!(plan.clusters_deferred, 1);
        assert!(plan.summary().contains("nothing worth changing"));
    }

    #[tokio::test]
    async fn deferred_clusters_are_reported_not_dropped() {
        // A persistently large backlog should be visible, not silent.
        let small = vec![candidate(0.5, 1, 1), candidate(0.5, 0, 1)];
        let large: Vec<MemoryCandidate> = (0..8).map(|i| candidate(0.5, i, 1)).collect();
        let plan = plan_consolidation(
            &FakeSource(vec![small, large]),
            Uuid::new_v4(),
            &CyclePolicy {
                max_changes_per_cycle: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(plan.clusters_planned, 1);
        assert_eq!(plan.clusters_deferred, 1);
        assert!(plan.summary().contains("deferred"));
    }

    #[tokio::test]
    async fn a_plan_never_exceeds_the_change_budget() {
        // Whatever the input, promotion must not be handed a change-set it
        // is obliged to refuse.
        let policy = CyclePolicy {
            max_changes_per_cycle: 5,
            ..Default::default()
        };
        let clusters: Vec<Vec<MemoryCandidate>> = (0..10)
            .map(|_| vec![candidate(0.5, 2, 1), candidate(0.5, 1, 1)])
            .collect();

        let plan = plan_consolidation(&FakeSource(clusters), Uuid::new_v4(), &policy)
            .await
            .unwrap();

        assert!(
            plan.changes.len() <= policy.max_changes_per_cycle,
            "planned {} changes against a limit of {}",
            plan.changes.len(),
            policy.max_changes_per_cycle
        );
    }
}
