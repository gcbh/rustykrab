//! Evidence that the consolidation loop does what it claims.
//!
//! The unit tests check situations someone thought of. These check
//! *properties* over situations nobody did: a randomized population of
//! memories, with duplicate groups of varying shape, run through many
//! cycles, asserting after every one that the things which must never
//! happen have not happened.
//!
//! Five invariants, each the negation of a specific way a self-modifying
//! memory system destroys itself:
//!
//! 1. **No fact is ever lost.** Consolidation removes redundancy; if it
//!    removes the last copy of something, it has destroyed data.
//! 2. **Nothing is retired without a survivor to point at.** A tombstone
//!    whose `superseded_by` is itself retired orphans the information.
//! 3. **The gate is never bypassed.** No mutation on evidence that cannot
//!    carry it, however many cycles run.
//! 4. **Changes stay bounded.** No cycle exceeds its budget, so a bad plan
//!    cannot cause a large correction.
//! 5. **The loop converges.** Repeated cycles reach a fixed point instead
//!    of churning forever.
//!
//! The generator is seeded and the seed is printed on failure, so any
//! counterexample it finds is reproducible.
//!
//! ## These assertions are load-bearing
//!
//! A test that cannot fail proves nothing, so each invariant was checked
//! by deliberately breaking the code it guards and confirming it caught
//! the break — and that it caught the *right* one:
//!
//! | Injected fault | Caught by | Reported as |
//! |---|---|---|
//! | Planner retires the survivor too | invariant 1 | `every copy of "fact-0" was retired` |
//! | `permits_mutation` check disabled | invariant 3 | `memory changed despite every cycle being refused` |
//! | `revert` skips the restore | reversibility | `reversal did not restore the exact retrievable set` |
//!
//! In each case the other two invariants stayed green, so the harness
//! localizes a fault rather than merely reacting to one. Re-run that
//! exercise after changing the engine: an invariant that no longer fails
//! when its property is violated has stopped being evidence.
mod common;

use common::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rustykrab_core::dream::CycleStatus;
use rustykrab_dream::consolidation::{run_consolidation_cycle, CycleOutcome};
use rustykrab_dream::engine::CyclePolicy;
use rustykrab_dream::eval;
use rustykrab_dream::memory_mutator::MemorySystemMutator;
use rustykrab_dream::report::Readiness;
use rustykrab_dream::MemoryClusterSource;
use uuid::Uuid;

#[tokio::test]
async fn consolidation_preserves_every_fact_across_randomized_populations() {
    let policy = CyclePolicy::default();

    for seed in 1..=eval::seeds(40) {
        let mut rng = Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let system = live_system();
        let world = generate(&mut rng, Arc::clone(&system)).await;
        let store = temp_store();
        let cycles = store.dream_cycles();

        assert_no_information_lost(&world, seed, 0).await;

        // Run until the loop says there is nothing left to do, with a hard
        // cap so a non-converging loop fails loudly rather than hanging.
        let mut cycle_count = 0usize;
        loop {
            cycle_count += 1;
            assert!(
                cycle_count <= 25,
                "seed {seed}: consolidation did not converge within 25 cycles"
            );

            let source = ContentClusters {
                clusters: world.clusters.clone(),
                system: Arc::clone(&system),
            };
            let mutator =
                MemorySystemMutator::new(Arc::clone(&system), world.agent, Uuid::new_v4());

            let outcome = run_consolidation_cycle(
                &cycles,
                &source,
                &mutator,
                world.agent,
                Readiness::Ready,
                &policy,
            )
            .await
            .unwrap();

            assert_no_information_lost(&world, seed, cycle_count).await;

            match outcome {
                CycleOutcome::Promoted { applied, .. } => {
                    // Invariant 4: never more than the budget.
                    assert!(
                        applied <= policy.max_changes_per_cycle,
                        "seed {seed}: applied {applied} changes, budget {}",
                        policy.max_changes_per_cycle
                    );
                }
                // Invariant 5: the loop reaches a fixed point.
                CycleOutcome::NothingToDo { .. } => break,
                CycleOutcome::Refused { reason } => {
                    panic!("seed {seed}: unexpected refusal on ready evidence: {reason}")
                }
            }
        }

        // At the fixed point, each fact has exactly one retrievable copy.
        let live = system
            .storage()
            .list_retrievable(world.agent)
            .await
            .unwrap();
        let mut per_fact: HashMap<&str, usize> = HashMap::new();
        for m in &live {
            *per_fact.entry(m.content.as_str()).or_default() += 1;
        }
        for (content, count) in per_fact {
            assert_eq!(
                count, 1,
                "seed {seed}: {content:?} still has {count} copies after convergence"
            );
        }
        assert_eq!(
            live.len(),
            world.facts.len(),
            "seed {seed}: expected one surviving copy per distinct fact"
        );
    }
}

#[tokio::test]
async fn the_gate_is_never_bypassed_however_many_cycles_run() {
    // Invariant 3. Evidence that cannot carry a decision must not be able
    // to make one by attrition.
    for seed in 1..=eval::seeds(15) {
        let mut rng = Rng::new(seed.wrapping_mul(0x1234_5678_9ABC_DEF1));
        let system = live_system();
        let world = generate(&mut rng, Arc::clone(&system)).await;
        let store = temp_store();
        let cycles = store.dream_cycles();

        let before: Vec<Uuid> = system
            .storage()
            .list_retrievable(world.agent)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();

        for weak in [Readiness::ProxyOnly, Readiness::InsufficientData] {
            for _ in 0..5 {
                let source = ContentClusters {
                    clusters: world.clusters.clone(),
                    system: Arc::clone(&system),
                };
                let mutator =
                    MemorySystemMutator::new(Arc::clone(&system), world.agent, Uuid::new_v4());
                let outcome = run_consolidation_cycle(
                    &cycles,
                    &source,
                    &mutator,
                    world.agent,
                    weak,
                    &CyclePolicy::default(),
                )
                .await
                .unwrap();
                assert!(
                    matches!(outcome, CycleOutcome::Refused { .. }),
                    "seed {seed}: {weak:?} evidence produced {outcome:?}"
                );
            }
        }

        let after: Vec<Uuid> = system
            .storage()
            .list_retrievable(world.agent)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(
            before, after,
            "seed {seed}: memory changed despite every cycle being refused"
        );
        assert!(
            cycles
                .list_by_status(world.agent, CycleStatus::Promoted, 10)
                .await
                .unwrap()
                .is_empty(),
            "seed {seed}: a cycle was promoted on insufficient evidence"
        );
    }
}

#[tokio::test]
async fn any_promoted_cycle_can_be_reversed_to_the_exact_prior_state() {
    // Reversibility over randomized populations: promote one cycle, undo
    // it from the manifest, and require the retrievable set to match
    // exactly what it was before.
    for seed in 1..=eval::seeds(25) {
        let mut rng = Rng::new(seed.wrapping_mul(0xDEAD_BEEF_CAFE_1234));
        let system = live_system();
        let world = generate(&mut rng, Arc::clone(&system)).await;
        let store = temp_store();
        let cycles = store.dream_cycles();

        let before: HashSet<Uuid> = system
            .storage()
            .list_retrievable(world.agent)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();

        let source = ContentClusters {
            clusters: world.clusters.clone(),
            system: Arc::clone(&system),
        };
        let mutator = MemorySystemMutator::new(Arc::clone(&system), world.agent, Uuid::new_v4());
        let outcome = run_consolidation_cycle(
            &cycles,
            &source,
            &mutator,
            world.agent,
            Readiness::Ready,
            &CyclePolicy::default(),
        )
        .await
        .unwrap();

        let CycleOutcome::Promoted { cycle_id, .. } = outcome else {
            // Nothing to reverse in a population with no duplicates.
            continue;
        };

        let recorded = cycles.get(cycle_id).await.unwrap().unwrap();
        let changes = cycles.changes(cycle_id).await.unwrap();
        rustykrab_dream::engine::rollback(
            &mutator,
            recorded.status,
            &changes,
            &CyclePolicy::default(),
        )
        .await
        .unwrap()
        .unwrap_or_else(|blockers| {
            panic!("seed {seed}: rollback blocked immediately after promote: {blockers:?}")
        });

        let after: HashSet<Uuid> = system
            .storage()
            .list_retrievable(world.agent)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();

        assert_eq!(
            before, after,
            "seed {seed}: reversal did not restore the exact retrievable set"
        );
    }
}

// ── Invariant 6: the same properties, over the real clustering ──────────
//
// The harness above supplies clusters grouped by identical content, which
// is the right ground truth for checking what the *engine* does with a
// cluster -- but it means the component that decides what a cluster *is*
// never runs. That component is the one that can merge two different facts
// into one, which is the failure the whole design is most afraid of, so
// checking everything except it leaves the scariest part unproven.
//
// This runs the same loop through `MemoryClusterSource`: real
// `SemanticSimilar` links, real connected components, real size cap. The
// population deliberately includes the tempting case -- distinct facts
// linked to each other strongly enough to be worth retrieving together,
// but not strongly enough to be the same thing.

#[tokio::test]
async fn real_clustering_never_merges_two_different_facts() {
    let policy = CyclePolicy::default();

    for seed in 1..=eval::seeds(30) {
        let mut rng = Rng::new(seed.wrapping_mul(0xD1B5_4A32_D192_ED03));
        let system = live_system();
        let world = generate_linked(&mut rng, Arc::clone(&system)).await;
        let store = temp_store();
        let cycles = store.dream_cycles();

        assert_no_information_lost(&world, seed, 0).await;
        assert_no_fact_was_merged_into_another(&world, seed, 0).await;

        let mut cycle_count = 0usize;
        loop {
            cycle_count += 1;
            assert!(
                cycle_count <= 25,
                "seed {seed}: consolidation did not converge within 25 cycles"
            );

            let source = MemoryClusterSource::new(Arc::clone(&system));
            let mutator =
                MemorySystemMutator::new(Arc::clone(&system), world.agent, Uuid::new_v4());

            let outcome = run_consolidation_cycle(
                &cycles,
                &source,
                &mutator,
                world.agent,
                Readiness::Ready,
                &policy,
            )
            .await
            .unwrap();

            // Every cycle, not just the last: an invariant that only holds
            // at a fixed point is not an invariant.
            assert_no_information_lost(&world, seed, cycle_count).await;
            assert_no_fact_was_merged_into_another(&world, seed, cycle_count).await;

            match outcome {
                CycleOutcome::Promoted { applied, .. } => {
                    assert!(
                        applied <= policy.max_changes_per_cycle,
                        "seed {seed}: a cycle applied {applied} changes, above its budget"
                    );
                }
                CycleOutcome::NothingToDo { .. } => break,
                CycleOutcome::Refused { reason } => {
                    panic!("seed {seed}: cycle refused unexpectedly: {reason}")
                }
            }
        }

        // Converged: every distinct fact survives exactly once per
        // duplicate group the clustering was able to see.
        let live = system
            .storage()
            .list_retrievable(world.agent)
            .await
            .unwrap();
        for content in world.facts.keys() {
            assert!(
                live.iter().any(|m| &m.content == content),
                "seed {seed}: {content:?} did not survive"
            );
        }
    }
}
