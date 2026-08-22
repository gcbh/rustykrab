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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rustykrab_core::dream::CycleStatus;
use rustykrab_dream::consolidation::{run_consolidation_cycle, CycleOutcome};
use rustykrab_dream::engine::CyclePolicy;
use rustykrab_dream::memory_mutator::MemorySystemMutator;
use rustykrab_dream::planner::{ConsolidationSource, MemoryCandidate};
use rustykrab_dream::report::Readiness;
use rustykrab_memory::embedding::HashEmbedder;
use rustykrab_memory::storage::SqliteMemoryStorage;
use rustykrab_memory::types::{ImportanceSource, LifecycleStage, Memory, MemoryScope};
use rustykrab_memory::{MemoryConfig, MemorySystem};
use uuid::Uuid;

/// Deterministic PRNG, so a failing case can be replayed from its seed
/// rather than hoping it recurs.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        self.next() % n
    }
}

fn live_system() -> Arc<MemorySystem> {
    Arc::new(MemorySystem::new(
        MemoryConfig::default(),
        Arc::new(SqliteMemoryStorage::open_in_memory().unwrap()),
        Arc::new(HashEmbedder::new(32)),
    ))
}

fn temp_store() -> rustykrab_store::Store {
    let dir = std::env::temp_dir().join(format!("rk-dream-inv-{}", Uuid::new_v4()));
    rustykrab_store::Store::open(&dir, vec![3u8; 32]).expect("store opens")
}

async fn seed_memory(
    system: &MemorySystem,
    agent: Uuid,
    content: &str,
    accesses: u32,
    importance: f64,
) -> Memory {
    let m = Memory {
        id: Uuid::new_v4(),
        agent_id: agent,
        content: content.to_string(),
        content_hash: rustykrab_memory::hash_content(content),
        scope: MemoryScope::User,
        session_id: None,
        user_id: None,
        lifecycle_stage: LifecycleStage::Episodic,
        importance,
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

/// Groups memories by identical content, which is the ground truth this
/// harness checks against: those groups are the facts that must survive.
struct ContentClusters {
    clusters: Vec<Vec<Uuid>>,
    system: Arc<MemorySystem>,
}

#[async_trait::async_trait]
impl ConsolidationSource for ContentClusters {
    async fn duplicate_clusters(
        &self,
        _: Uuid,
    ) -> rustykrab_core::Result<Vec<Vec<MemoryCandidate>>> {
        let mut out = Vec::new();
        for cluster in &self.clusters {
            let memories = self.system.storage().get_memories(cluster).await?;
            // Only still-retrievable members are candidates; a cycle must
            // not plan against something an earlier cycle already retired.
            let live: Vec<MemoryCandidate> = memories
                .into_iter()
                .filter(|m| m.is_valid)
                .map(|m| MemoryCandidate {
                    id: m.id,
                    content_hash: m.content_hash.clone(),
                    importance: m.importance,
                    access_count: m.access_count,
                    proof_count: m.proof_count,
                })
                .collect();
            if live.len() >= 2 {
                out.push(live);
            }
        }
        Ok(out)
    }
}

/// The world a single trial runs against.
struct World {
    system: Arc<MemorySystem>,
    agent: Uuid,
    /// content → every memory id ever created with it.
    facts: HashMap<String, Vec<Uuid>>,
    clusters: Vec<Vec<Uuid>>,
}

async fn generate(rng: &mut Rng, system: Arc<MemorySystem>) -> World {
    let agent = Uuid::new_v4();
    let fact_count = 1 + rng.below(6); // 1..=6 distinct facts
    let mut facts: HashMap<String, Vec<Uuid>> = HashMap::new();
    let mut clusters = Vec::new();

    for f in 0..fact_count {
        let content = format!("fact-{f}");
        // 1..=5 copies. A single copy is not a duplicate and must survive
        // untouched, which is itself worth exercising.
        let copies = 1 + rng.below(5);
        let mut ids = Vec::new();
        for _ in 0..copies {
            let accesses = rng.below(10) as u32;
            let importance = (rng.below(100) as f64) / 100.0;
            let m = seed_memory(&system, agent, &content, accesses, importance).await;
            ids.push(m.id);
        }
        facts.insert(content, ids.clone());
        if ids.len() >= 2 {
            clusters.push(ids);
        }
    }

    World {
        system,
        agent,
        facts,
        clusters,
    }
}

/// Invariants 1 and 2, checked against live storage.
async fn assert_no_information_lost(world: &World, seed: u64, cycle: usize) {
    let live = world
        .system
        .storage()
        .list_retrievable(world.agent)
        .await
        .unwrap();
    let live_ids: HashSet<Uuid> = live.iter().map(|m| m.id).collect();
    let live_content: HashSet<&str> = live.iter().map(|m| m.content.as_str()).collect();

    for content in world.facts.keys() {
        assert!(
            live_content.contains(content.as_str()),
            "seed {seed}, cycle {cycle}: every copy of {content:?} was retired -- \
             consolidation destroyed a fact"
        );
    }

    // Invariant 2: a retired memory must point at something still live,
    // or the information it held has been orphaned.
    for ids in world.facts.values() {
        let all = world.system.storage().get_memories(ids).await.unwrap();
        for m in all.iter().filter(|m| !m.is_valid) {
            if let Some(successor) = m.invalidated_by {
                assert!(
                    live_ids.contains(&successor),
                    "seed {seed}, cycle {cycle}: memory {} was retired against {} \
                     which is not retrievable",
                    m.id,
                    successor
                );
            }
        }
    }
}

#[tokio::test]
async fn consolidation_preserves_every_fact_across_randomized_populations() {
    let policy = CyclePolicy::default();

    for seed in 1..=40u64 {
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
    for seed in 1..=15u64 {
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
    for seed in 1..=25u64 {
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
