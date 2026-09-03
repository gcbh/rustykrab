//! Shared scaffolding for the consolidation harness and the evals: a
//! seeded generator of memory populations, the storage-level checks the
//! invariants are stated in, and the sources that feed the engine.
//!
//! Two flavours of every check. `assert_*` panics with the seed and cycle
//! in the message, for the invariant tests. `check_*` returns the same
//! message as `Err`, for evals, whose protocol wants the reason in the
//! report rather than in a panic.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rustykrab_dream::planner::{ConsolidationSource, MemoryCandidate};
use rustykrab_memory::embedding::{Embedder, HashEmbedder};
use rustykrab_memory::storage::SqliteMemoryStorage;
use rustykrab_memory::types::{
    ImportanceSource, LifecycleStage, LinkType, Memory, MemoryChunk, MemoryLink, MemoryScope,
};
use rustykrab_memory::{MemoryConfig, MemorySystem};
use uuid::Uuid;

/// Deterministic PRNG, so a failing case can be replayed from its seed
/// rather than hoping it recurs.
pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    pub fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        self.next() % n
    }
}

pub fn live_system() -> Arc<MemorySystem> {
    Arc::new(MemorySystem::new(
        MemoryConfig::default(),
        Arc::new(SqliteMemoryStorage::open_in_memory().unwrap()),
        Arc::new(HashEmbedder::new(32)),
    ))
}

pub fn temp_store() -> rustykrab_store::Store {
    let dir = std::env::temp_dir().join(format!("rk-dream-inv-{}", Uuid::new_v4()));
    rustykrab_store::Store::open(&dir, vec![3u8; 32]).expect("store opens")
}

pub async fn seed_memory(
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
pub struct ContentClusters {
    pub clusters: Vec<Vec<Uuid>>,
    pub system: Arc<MemorySystem>,
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
pub struct World {
    pub system: Arc<MemorySystem>,
    pub agent: Uuid,
    /// content → every memory id ever created with it.
    pub facts: HashMap<String, Vec<Uuid>>,
    pub clusters: Vec<Vec<Uuid>>,
}

pub async fn generate(rng: &mut Rng, system: Arc<MemorySystem>) -> World {
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

pub async fn check_no_information_lost(world: &World) -> Result<(), String> {
    let live = world
        .system
        .storage()
        .list_retrievable(world.agent)
        .await
        .map_err(|e| e.to_string())?;
    let live_ids: HashSet<Uuid> = live.iter().map(|m| m.id).collect();
    let live_content: HashSet<&str> = live.iter().map(|m| m.content.as_str()).collect();

    for content in world.facts.keys() {
        if !live_content.contains(content.as_str()) {
            return Err(format!(
                "every copy of {content:?} was retired -- consolidation destroyed a fact"
            ));
        }
    }

    // Invariant 2: a retired memory must point at something still live,
    // or the information it held has been orphaned.
    for ids in world.facts.values() {
        let all = world
            .system
            .storage()
            .get_memories(ids)
            .await
            .map_err(|e| e.to_string())?;
        for m in all.iter().filter(|m| !m.is_valid) {
            if let Some(successor) = m.invalidated_by {
                if !live_ids.contains(&successor) {
                    return Err(format!(
                        "memory {} was retired against {successor} which is not retrievable",
                        m.id
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Invariants 1 and 2, checked against live storage.
pub async fn assert_no_information_lost(world: &World, seed: u64, cycle: usize) {
    if let Err(e) = check_no_information_lost(world).await {
        panic!("seed {seed}, cycle {cycle}: {e}");
    }
}

/// Seeds a population and the similarity links a real deployment would
/// have: strong links between copies of a fact, weaker cross-links between
/// different facts.
pub async fn generate_linked(rng: &mut Rng, system: Arc<MemorySystem>) -> World {
    generate_linked_in(rng, system, (0.70, 0.91)).await
}

/// [`generate_linked`] with the cross-fact link weights drawn from
/// `cross_band` (`[lo, hi)`, in hundredths), so an eval can put the
/// "related but not the same" links exactly where a threshold is doubtful.
pub async fn generate_linked_in(
    rng: &mut Rng,
    system: Arc<MemorySystem>,
    cross_band: (f64, f64),
) -> World {
    let agent = Uuid::new_v4();
    let fact_count = 2 + rng.below(4); // 2..=5 distinct facts
    let mut facts: HashMap<String, Vec<Uuid>> = HashMap::new();
    let mut per_fact: Vec<Vec<Uuid>> = Vec::new();

    for f in 0..fact_count {
        let content = format!("fact-{f}");
        let copies = 1 + rng.below(4);
        let mut ids = Vec::new();
        for _ in 0..copies {
            let accesses = rng.below(10) as u32;
            let importance = (rng.below(100) as f64) / 100.0;
            ids.push(
                seed_memory(&system, agent, &content, accesses, importance)
                    .await
                    .id,
            );
        }
        facts.insert(content, ids.clone());
        per_fact.push(ids);
    }

    // Copies of one fact are duplicates of each other.
    for ids in &per_fact {
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                link(
                    &system,
                    ids[i],
                    ids[j],
                    0.93 + (rng.below(6) as f64) / 100.0,
                )
                .await;
            }
        }
    }

    // Different facts are related but not the same. This is the case that
    // must never be consolidated, and the reason the duplicate threshold
    // sits above the threshold that created the link.
    for a in 0..per_fact.len() {
        for b in (a + 1)..per_fact.len() {
            if rng.below(2) == 0 {
                continue;
            }
            let (lo, hi) = cross_band;
            let steps = ((hi - lo) * 100.0).round().max(1.0) as u64;
            let w = lo + (rng.below(steps) as f64) / 100.0;
            link(&system, per_fact[a][0], per_fact[b][0], w).await;
        }
    }

    World {
        system,
        agent,
        facts,
        clusters: Vec::new(), // unused: the real source derives its own
    }
}

pub async fn link(system: &MemorySystem, a: Uuid, b: Uuid, weight: f64) {
    system
        .storage()
        .upsert_link(&MemoryLink {
            source_id: a,
            target_id: b,
            link_type: LinkType::SemanticSimilar,
            weight,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
}

/// Invariant 6: distinct facts are never merged into one another.
///
/// Stronger than "no fact is lost". A cross-fact merge can leave every
/// fact represented -- retire one copy of fact-1 against a copy of fact-2
/// and both contents still exist -- while having silently asserted that
/// two different things were the same. The tombstone is where the claim
/// shows up, so that is where it is checked.
pub async fn check_no_fact_was_merged_into_another(world: &World) -> Result<(), String> {
    let mut content_of: HashMap<Uuid, String> = HashMap::new();
    for (content, ids) in &world.facts {
        for id in ids {
            content_of.insert(*id, content.clone());
        }
    }

    let all: Vec<Uuid> = content_of.keys().copied().collect();
    for m in world
        .system
        .storage()
        .get_memories(&all)
        .await
        .map_err(|e| e.to_string())?
    {
        let Some(successor) = m.invalidated_by else {
            continue;
        };
        let (Some(retired), Some(kept)) = (content_of.get(&m.id), content_of.get(&successor))
        else {
            continue;
        };
        if retired != kept {
            return Err(format!(
                "memory {} ({retired:?}) was retired against {successor} ({kept:?}) -- \
                 consolidation merged two different facts",
                m.id
            ));
        }
    }
    Ok(())
}

pub async fn assert_no_fact_was_merged_into_another(world: &World, seed: u64, cycle: usize) {
    if let Err(e) = check_no_fact_was_merged_into_another(world).await {
        panic!("seed {seed}, cycle {cycle}: {e}");
    }
}

/// Give every memory in `world` the chunk embedding the writer would have
/// produced for it, so the vector path can see it.
///
/// The generator writes rows directly -- the way an import or a migration
/// lands them -- which leaves them retrievable by keyword and time but
/// invisible to vector search. The embedder is the same deterministic one
/// `live_system` installs, so the vectors match what a query embeds to.
pub async fn index_embeddings(world: &World) -> Result<(), String> {
    let embedder = HashEmbedder::new(32);
    let ids: Vec<Uuid> = world.facts.values().flatten().copied().collect();
    let memories = world
        .system
        .storage()
        .get_memories(&ids)
        .await
        .map_err(|e| e.to_string())?;
    let mut chunks = Vec::with_capacity(memories.len());
    for m in memories {
        let mut vectors = embedder
            .embed(vec![m.content.clone()])
            .await
            .map_err(|e| e.to_string())?;
        chunks.push(MemoryChunk {
            id: Uuid::new_v4(),
            memory_id: m.id,
            chunk_index: 0,
            content: m.content,
            embedding: vectors.remove(0),
            embedding_model_version: embedder.model_version().to_string(),
            created_at: chrono::Utc::now(),
        });
    }
    world
        .system
        .storage()
        .store_chunks(&chunks)
        .await
        .map_err(|e| e.to_string())
}
