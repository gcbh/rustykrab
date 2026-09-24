//! Evals over the consolidation loop. See `rustykrab_dream::eval` for the
//! protocol, and `invariants.rs` for the harness these build on.
//!
//! The invariant harness proves five properties over populations its
//! generator can produce. These target what it cannot: each is a specific
//! way the loop can be wrong that the generator never reaches -- a link
//! weight in the band the memory system calls "similar but distinct", a
//! component above the size cap that is a chain rather than a clique, a
//! second writer acting inside the promote window. Where the code does not
//! meet the target yet, the eval says so (`Expected::XFail`) and names what
//! is missing; the suite stays green, the report shows what is owed, and
//! the eval turns the suite red the day the code catches up, so it gets
//! promoted rather than forgotten.
//!
//! Randomized evals honour `DREAM_EVAL_SEEDS`; the nightly workflow runs
//! them wider than a pull request does.

mod common;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use common::*;
use rustykrab_core::dream::{CycleStatus, DreamCycle, RollbackBlocker, StagedChange};
use rustykrab_core::Result as CoreResult;
use rustykrab_dream::cluster_source::MAX_CLUSTER_SIZE;
use rustykrab_dream::consolidation::{run_consolidation_cycle, CycleOutcome};
use rustykrab_dream::engine::{promote, rollback, CyclePolicy};
use rustykrab_dream::eval::{self, Expected};
use rustykrab_dream::memory_mutator::MemorySystemMutator;
use rustykrab_dream::mutation::{MemoryFacts, MemoryMutator};
use rustykrab_dream::planner::{plan_consolidation, ConsolidationSource};
use rustykrab_dream::report::Readiness;
use rustykrab_dream::MemoryClusterSource;
use rustykrab_memory::types::RetrievalSource;
use rustykrab_memory::MemorySystem;
use uuid::Uuid;

fn seed_of(seed: u64, salt: u64) -> Rng {
    Rng::new(seed.wrapping_mul(salt))
}

fn first_retirement(changes: &[StagedChange]) -> Option<(Uuid, Uuid)> {
    changes.iter().find_map(|c| match c {
        StagedChange::InvalidateMemory {
            memory_id,
            superseded_by,
            ..
        } => Some((*memory_id, *superseded_by)),
        StagedChange::CreateMemory { .. } => None,
    })
}

fn retired_ids(changes: &[StagedChange]) -> Vec<Uuid> {
    changes
        .iter()
        .filter_map(|c| match c {
            StagedChange::InvalidateMemory { memory_id, .. } => Some(*memory_id),
            StagedChange::CreateMemory { .. } => None,
        })
        .collect()
}

// ── Clustering ──────────────────────────────────────────────────────────

/// The memory system classifies a similarity in `[dedup_distinct_threshold,
/// dedup_auto_merge_threshold)` as "similar but distinct" -- close enough to
/// retrieve together, not close enough to be one thing. A consolidation
/// threshold below the top of that band merges what the system itself
/// says is different.
#[tokio::test]
async fn links_in_the_distinct_band_are_never_consolidated() {
    let seeds = eval::seeds(30);
    eval::run(
        "links_in_the_distinct_band_are_never_consolidated",
        Expected::XFail(
            "MIN_DUPLICATE_WEIGHT (0.92) sits below the memory config's \
             dedup_auto_merge_threshold (0.95), so a link the memory system \
             classifies as distinct is consolidated",
        ),
        seeds,
        async move {
            let policy = CyclePolicy::default();
            for seed in 1..=seeds {
                let mut rng = seed_of(seed, 0xA5A5_5A5A_C3C3_3C3C);
                let system = live_system();
                let band = {
                    let cfg = system.config();
                    (cfg.dedup_distinct_threshold, cfg.dedup_auto_merge_threshold)
                };
                let world = generate_linked_in(&mut rng, Arc::clone(&system), band).await;
                let store = temp_store();
                let cycles = store.dream_cycles();

                for cycle in 1..=25 {
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
                    .map_err(|e| format!("seed {seed}, cycle {cycle}: {e}"))?;

                    check_no_fact_was_merged_into_another(&world)
                        .await
                        .map_err(|e| format!("seed {seed}, cycle {cycle}: {e}"))?;
                    check_no_information_lost(&world)
                        .await
                        .map_err(|e| format!("seed {seed}, cycle {cycle}: {e}"))?;

                    if matches!(outcome, CycleOutcome::NothingToDo { .. }) {
                        break;
                    }
                }
            }
            Ok(())
        },
    )
    .await;
}

/// A component above `MAX_CLUSTER_SIZE` means the threshold is mis-tuned,
/// and the source promises to leave it alone rather than consolidate on
/// that basis. The promise has to hold for every shape of component, not
/// only the clique the unit test draws.
#[tokio::test]
async fn oversized_components_are_refused_whatever_their_shape() {
    let seeds = eval::seeds(20);
    eval::run(
        "oversized_components_are_refused_whatever_their_shape",
        Expected::XFail(
            "overflow is judged by whether the BFS queue is non-empty when the \
             cap is reached, which a chain walked from one end never leaves it; \
             the first MAX_CLUSTER_SIZE nodes are returned as a cluster and the \
             rest stranded",
        ),
        seeds,
        async move {
            for seed in 1..=seeds {
                let mut rng = seed_of(seed, 0x5EED_5EED_0C0C_0C0C);
                let system = live_system();
                let agent = Uuid::new_v4();
                let n = MAX_CLUSTER_SIZE + 1 + rng.below(4) as usize;
                let mut ids = Vec::with_capacity(n);
                for _ in 0..n {
                    ids.push(seed_memory(&system, agent, "fact-0", 0, 0.5).await.id);
                }
                let star = rng.below(2) == 0;
                for i in 1..n {
                    let from = if star { ids[0] } else { ids[i - 1] };
                    let w = 0.96 + (rng.below(4) as f64) / 100.0;
                    link(&system, from, ids[i], w).await;
                }

                let clusters = MemoryClusterSource::new(Arc::clone(&system))
                    .duplicate_clusters(agent)
                    .await
                    .map_err(|e| format!("seed {seed}: {e}"))?;
                if !clusters.is_empty() {
                    let sizes: Vec<usize> = clusters.iter().map(|c| c.len()).collect();
                    return Err(format!(
                        "seed {seed}: a {} of {n} memories (cap {MAX_CLUSTER_SIZE}) came back as \
                         {} cluster(s) of sizes {sizes:?} instead of being refused whole",
                        if star { "star" } else { "chain" },
                        clusters.len()
                    ));
                }
            }
            Ok(())
        },
    )
    .await;
}

// ── A second writer inside the promote window ───────────────────────────

/// A mutator that lets "someone else" act between the engine's checks and
/// the transaction: the lifecycle sweep, or the agent, retiring a memory
/// in the window staging exists to close. `interfere` is handed the
/// change-set and does the interfering; the real mutator then applies it.
struct Interference<F> {
    inner: MemorySystemMutator,
    system: Arc<MemorySystem>,
    interfere: F,
    /// What the interference touched, for the assertion.
    touched: Mutex<Option<Uuid>>,
}

#[async_trait::async_trait]
impl<F> MemoryMutator for Interference<F>
where
    F: Fn(&[StagedChange]) -> Option<(Uuid, Option<Uuid>)> + Send + Sync,
{
    async fn facts(&self, ids: &[Uuid]) -> CoreResult<Vec<MemoryFacts>> {
        self.inner.facts(ids).await
    }

    async fn apply_all(&self, changes: &[StagedChange]) -> CoreResult<()> {
        if let Some((id, by)) = (self.interfere)(changes) {
            self.system.storage().invalidate(id, by).await?;
            *self.touched.lock().unwrap() = Some(id);
        }
        self.inner.apply_all(changes).await
    }

    async fn revert_all(&self, changes: &[StagedChange]) -> CoreResult<usize> {
        self.inner.revert_all(changes).await
    }
}

/// A plan retires a cluster against the one member it keeps. If that member
/// is retired by someone else after the engine checks it and before the
/// retirements commit, every remaining copy is retired against a dead
/// memory and the fact leaves the retrievable set. This is the one way the
/// loop can destroy data, and the race is exactly what staging is for.
#[tokio::test]
async fn a_survivor_retired_inside_the_promote_window_does_not_strand_the_fact() {
    let seeds = eval::seeds(25);
    eval::run(
        "a_survivor_retired_inside_the_promote_window_does_not_strand_the_fact",
        Expected::XFail(
            "the survivor's liveness is checked before apply_validity_batch, not \
             inside its transaction; the batch guards only the retired rows",
        ),
        seeds,
        async move {
            for seed in 1..=seeds {
                let mut rng = seed_of(seed, 0xBEEF_F00D_1234_5678);
                let system = live_system();
                let world = generate(&mut rng, Arc::clone(&system)).await;
                if world.clusters.is_empty() {
                    continue;
                }
                let store = temp_store();
                let cycles = store.dream_cycles();
                let source = ContentClusters {
                    clusters: world.clusters.clone(),
                    system: Arc::clone(&system),
                };
                let mutator = Interference {
                    inner: MemorySystemMutator::new(
                        Arc::clone(&system),
                        world.agent,
                        Uuid::new_v4(),
                    ),
                    system: Arc::clone(&system),
                    interfere: |changes: &[StagedChange]| {
                        first_retirement(changes).map(|(_, survivor)| (survivor, None))
                    },
                    touched: Mutex::new(None),
                };

                // Refusing is fine; applying is fine; losing the fact is not.
                let _ = run_consolidation_cycle(
                    &cycles,
                    &source,
                    &mutator,
                    world.agent,
                    Readiness::Ready,
                    &CyclePolicy::default(),
                )
                .await;

                let survivor = mutator.touched.lock().unwrap().unwrap_or_default();
                check_no_information_lost(&world).await.map_err(|e| {
                    format!("seed {seed}: survivor {survivor} was retired mid-promote and {e}")
                })?;
            }
            Ok(())
        },
    )
    .await;
}

/// The manifest is what reversal walks, so it must record what the batch
/// changed, not what the engine meant to change. A target retired by
/// someone else in the window matches no row; recording it as applied
/// makes a later rollback restore a memory this cycle never retired.
#[tokio::test]
async fn the_manifest_records_what_the_batch_changed_not_what_was_intended() {
    let seeds = eval::seeds(25);
    eval::run(
        "the_manifest_records_what_the_batch_changed_not_what_was_intended",
        Expected::XFail(
            "MemorySystemMutator::apply_all discards apply_validity_batch's \
             changed-row count and promotion records its pre-check list as applied",
        ),
        seeds,
        async move {
            for seed in 1..=seeds {
                let mut rng = seed_of(seed, 0xC0FF_EE00_C0FF_EE00);
                let system = live_system();
                let world = generate(&mut rng, Arc::clone(&system)).await;
                if world.clusters.is_empty() {
                    continue;
                }
                let store = temp_store();
                let cycles = store.dream_cycles();
                let source = ContentClusters {
                    clusters: world.clusters.clone(),
                    system: Arc::clone(&system),
                };
                let mutator = Interference {
                    inner: MemorySystemMutator::new(
                        Arc::clone(&system),
                        world.agent,
                        Uuid::new_v4(),
                    ),
                    system: Arc::clone(&system),
                    // Someone else retired the target, against a decision
                    // of their own.
                    interfere: |changes: &[StagedChange]| {
                        first_retirement(changes).map(|(target, _)| (target, Some(Uuid::new_v4())))
                    },
                    touched: Mutex::new(None),
                };

                let outcome = run_consolidation_cycle(
                    &cycles,
                    &source,
                    &mutator,
                    world.agent,
                    Readiness::Ready,
                    &CyclePolicy::default(),
                )
                .await
                .map_err(|e| format!("seed {seed}: {e}"))?;
                let CycleOutcome::Promoted { cycle_id, .. } = outcome else {
                    continue;
                };
                let Some(target) = *mutator.touched.lock().unwrap() else {
                    continue;
                };

                let applied = cycles
                    .applied_changes(cycle_id)
                    .await
                    .map_err(|e| format!("seed {seed}: {e}"))?;
                if retired_ids(&applied).contains(&target) {
                    return Err(format!(
                        "seed {seed}: the manifest says this cycle retired {target}, but it was \
                         already retired by someone else and the batch changed no row for it"
                    ));
                }
            }
            Ok(())
        },
    )
    .await;
}

// ── The manifest and reversal ───────────────────────────────────────────

/// The positive half of the manifest claim: a change promotion skipped as
/// stale is recorded as not applied, reversal over the applied set
/// succeeds and restores exactly what this cycle retired, and reversal over
/// the staged set is refused because the stale change now points at
/// someone else's decision.
#[tokio::test]
async fn reversal_walks_the_applied_set_and_refuses_the_staged_one() {
    let seeds = eval::seeds(25);
    eval::run(
        "reversal_walks_the_applied_set_and_refuses_the_staged_one",
        Expected::Pass,
        seeds,
        async move {
            let policy = CyclePolicy::default();
            for seed in 1..=seeds {
                let mut rng = seed_of(seed, 0x00DD_BA11_0DDB_A110);
                let system = live_system();
                let world = generate(&mut rng, Arc::clone(&system)).await;
                if world.clusters.is_empty() {
                    continue;
                }
                let store = temp_store();
                let cycles = store.dream_cycles();
                let source = ContentClusters {
                    clusters: world.clusters.clone(),
                    system: Arc::clone(&system),
                };
                let mutator =
                    MemorySystemMutator::new(Arc::clone(&system), world.agent, Uuid::new_v4());

                let plan = plan_consolidation(&source, world.agent, &policy)
                    .await
                    .map_err(|e| format!("seed {seed}: {e}"))?;
                let Some((stale, _)) = first_retirement(&plan.changes) else {
                    continue;
                };
                // With a single staged change, retiring its target leaves
                // nothing to promote and the engine rightly refuses the
                // whole cycle; the property here is about a mixed set.
                if retired_ids(&plan.changes).len() < 2 {
                    continue;
                }
                let before: HashSet<Uuid> = live_ids(&system, world.agent).await?;

                // Between planning and promotion, someone else retires one
                // of the targets against a decision of their own.
                let elsewhere = Uuid::new_v4();
                system
                    .storage()
                    .invalidate(stale, Some(elsewhere))
                    .await
                    .map_err(|e| format!("seed {seed}: {e}"))?;

                let cycle = DreamCycle::new(world.agent, "memory_consolidation");
                cycles
                    .stage(&cycle, &plan.changes)
                    .await
                    .map_err(|e| format!("seed {seed}: {e}"))?;
                let promotion = promote(
                    &mutator,
                    CycleStatus::Staged,
                    Readiness::Ready,
                    &plan.changes,
                    &policy,
                )
                .await
                .map_err(|e| format!("seed {seed}: {e}"))?
                .map_err(|refusal| format!("seed {seed}: refused: {}", refusal.describe()))?;
                if !retired_ids(&promotion.skipped_stale).contains(&stale) {
                    return Err(format!(
                        "seed {seed}: promotion did not skip {stale}, which was retired \
                         before it ran"
                    ));
                }
                cycles
                    .mark_applied(cycle.id, &promotion.applied)
                    .await
                    .map_err(|e| format!("seed {seed}: {e}"))?;
                cycles
                    .set_status(cycle.id, CycleStatus::Promoted)
                    .await
                    .map_err(|e| format!("seed {seed}: {e}"))?;

                let applied = cycles
                    .applied_changes(cycle.id)
                    .await
                    .map_err(|e| format!("seed {seed}: {e}"))?;
                let staged = cycles
                    .changes(cycle.id)
                    .await
                    .map_err(|e| format!("seed {seed}: {e}"))?;
                if retired_ids(&applied).contains(&stale) {
                    return Err(format!(
                        "seed {seed}: the applied set contains {stale}, which promotion skipped"
                    ));
                }
                if applied.len() + 1 != staged.len() {
                    return Err(format!(
                        "seed {seed}: staged {} changes, applied {}, but exactly one was stale",
                        staged.len(),
                        applied.len()
                    ));
                }

                // The staged set must be refused: the stale change's
                // tombstone now belongs to someone else.
                match rollback(&mutator, CycleStatus::Promoted, &staged, &policy).await {
                    Ok(Err(blockers)) => {
                        let names_it = blockers.iter().any(|b| {
                            matches!(b, RollbackBlocker::ParentModified { memory_id } if *memory_id == stale)
                        });
                        if !names_it {
                            return Err(format!(
                                "seed {seed}: reversal over the staged set was blocked, but not \
                                 on {stale}: {blockers:?}"
                            ));
                        }
                    }
                    Ok(Ok(n)) => {
                        return Err(format!(
                            "seed {seed}: reversal over the staged set went ahead and reversed \
                             {n} changes; it should have refused on {stale}"
                        ))
                    }
                    Err(e) => return Err(format!("seed {seed}: {e}")),
                }

                // The applied set reverses cleanly, to exactly the prior
                // state minus the memory someone else retired.
                rollback(&mutator, CycleStatus::Promoted, &applied, &policy)
                    .await
                    .map_err(|e| format!("seed {seed}: {e}"))?
                    .map_err(|b| format!("seed {seed}: reversal over the applied set blocked: {b:?}"))?;
                let after = live_ids(&system, world.agent).await?;
                let mut expected = before.clone();
                expected.remove(&stale);
                if after != expected {
                    return Err(format!(
                        "seed {seed}: reversal restored {} memories, expected {} (prior set \
                         minus the one retired elsewhere)",
                        after.len(),
                        expected.len()
                    ));
                }
            }
            Ok(())
        },
    )
    .await;
}

/// Reversal claims to restore the exact prior state, and the invariant
/// harness checks that through SQL. Retrieval does not go through SQL
/// alone: the vector path serves a per-agent embedding cache. A memory
/// that is back in the table but not in that cache is restored on paper.
#[tokio::test]
async fn a_reversed_retirement_is_retrievable_by_vector_search_again() {
    let seeds = eval::seeds(15);
    eval::run(
        "a_reversed_retirement_is_retrievable_by_vector_search_again",
        Expected::XFail(
            "apply_validity_batch evicts the agent's embedding cache entry for \
             invalidations only; a Restore leaves the cache as it was after the \
             promotion, so the restored memory is missing from vector retrieval",
        ),
        seeds,
        async move {
            let policy = CyclePolicy::default();
            for seed in 1..=seeds {
                let mut rng = seed_of(seed, 0x7E57_CA5E_7E57_CA5E);
                let system = live_system();
                let world = generate(&mut rng, Arc::clone(&system)).await;
                if world.clusters.is_empty() {
                    continue;
                }
                index_embeddings(&world)
                    .await
                    .map_err(|e| format!("seed {seed}: {e}"))?;
                let store = temp_store();
                let cycles = store.dream_cycles();
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
                .map_err(|e| format!("seed {seed}: {e}"))?;
                let CycleOutcome::Promoted { cycle_id, .. } = outcome else {
                    continue;
                };
                let applied = cycles
                    .applied_changes(cycle_id)
                    .await
                    .map_err(|e| format!("seed {seed}: {e}"))?;
                let Some((restored, survivor)) = first_retirement(&applied) else {
                    continue;
                };

                // Precondition: on the promoted state, the survivor is
                // reachable through the vector path. Otherwise the eval
                // below would measure the retriever, not the reversal.
                let content = memory_content(&system, survivor).await?;
                let sources = sources_for(&system, world.agent, &content, survivor).await?;
                if !sources
                    .iter()
                    .any(|s| matches!(s, RetrievalSource::Semantic))
                {
                    return Err(format!(
                        "seed {seed}: precondition failed -- the live survivor {survivor} is \
                         not reachable by vector search even before reversal ({sources:?})"
                    ));
                }

                rollback(&mutator, CycleStatus::Promoted, &applied, &policy)
                    .await
                    .map_err(|e| format!("seed {seed}: {e}"))?
                    .map_err(|b| format!("seed {seed}: reversal blocked: {b:?}"))?;

                let content = memory_content(&system, restored).await?;
                let sources = sources_for(&system, world.agent, &content, restored).await?;
                if sources.is_empty() {
                    return Err(format!(
                        "seed {seed}: restored memory {restored} is not retrievable at all"
                    ));
                }
                if !sources
                    .iter()
                    .any(|s| matches!(s, RetrievalSource::Semantic))
                {
                    return Err(format!(
                        "seed {seed}: restored memory {restored} is reachable only through \
                         {sources:?}; the vector index still excludes it"
                    ));
                }
            }
            Ok(())
        },
    )
    .await;
}

async fn live_ids(system: &MemorySystem, agent: Uuid) -> Result<HashSet<Uuid>, String> {
    Ok(system
        .storage()
        .list_retrievable(agent)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|m| m.id)
        .collect())
}

async fn memory_content(system: &MemorySystem, id: Uuid) -> Result<String, String> {
    system
        .get_memory(id)
        .await
        .map_err(|e| e.to_string())?
        .map(|m| m.content)
        .ok_or_else(|| format!("memory {id} does not exist"))
}

/// Which retrieval paths return `id` for `query`; empty when none does.
async fn sources_for(
    system: &MemorySystem,
    agent: Uuid,
    query: &str,
    id: Uuid,
) -> Result<Vec<RetrievalSource>, String> {
    let hits = system
        .recall(query, agent, 50)
        .await
        .map_err(|e| e.to_string())?;
    Ok(hits
        .into_iter()
        .find(|r| r.memory_id == id)
        .map(|r| r.sources)
        .unwrap_or_default())
}
