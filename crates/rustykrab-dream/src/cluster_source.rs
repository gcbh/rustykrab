//! Finding duplicate clusters in the real memory system.
//!
//! Builds on the `SemanticSimilar` links that `LifecycleManager::
//! detect_near_duplicates` already writes, so this stage consumes work the
//! memory system was doing and discarding rather than re-deriving it.
//!
//! Clusters are connected components over those links, capped in size. An
//! unbounded component is a warning sign rather than an opportunity: if
//! forty memories are all "similar", the similarity threshold is doing
//! something other than finding duplicates, and consolidating them would
//! destroy distinctions the threshold failed to see.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use rustykrab_core::Result;
use rustykrab_memory::types::LinkType;
use rustykrab_memory::MemorySystem;
use uuid::Uuid;

use crate::planner::{ConsolidationSource, MemoryCandidate};

/// Largest component that will be treated as a duplicate cluster.
///
/// Beyond this, "similar" has stopped meaning "the same", and merging
/// would collapse distinctions rather than remove redundancy.
pub const MAX_CLUSTER_SIZE: usize = 6;

/// Minimum link weight to treat two memories as saying the same thing.
///
/// Higher than the threshold that creates the links: being related enough
/// to help retrieval is a much weaker claim than being redundant.
pub const MIN_DUPLICATE_WEIGHT: f64 = 0.92;

/// Reads duplicate clusters out of the memory graph.
pub struct MemoryClusterSource {
    system: Arc<MemorySystem>,
}

impl MemoryClusterSource {
    pub fn new(system: Arc<MemorySystem>) -> Self {
        Self { system }
    }
}

#[async_trait::async_trait]
impl ConsolidationSource for MemoryClusterSource {
    async fn duplicate_clusters(&self, agent_id: Uuid) -> Result<Vec<Vec<MemoryCandidate>>> {
        let memories = self.system.storage().list_retrievable(agent_id).await?;
        if memories.len() < 2 {
            return Ok(Vec::new());
        }

        let by_id: HashMap<Uuid, &rustykrab_memory::types::Memory> =
            memories.iter().map(|m| (m.id, m)).collect();
        let ids: Vec<Uuid> = memories.iter().map(|m| m.id).collect();

        // Adjacency over strong similarity links only, and only between
        // memories that are both still retrievable.
        let links = self.system.storage().get_links_from_many(&ids).await?;
        let mut adjacency: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for link in links {
            if link.link_type != LinkType::SemanticSimilar
                || link.weight < MIN_DUPLICATE_WEIGHT
                || !by_id.contains_key(&link.target_id)
                || link.source_id == link.target_id
            {
                continue;
            }
            adjacency
                .entry(link.source_id)
                .or_default()
                .push(link.target_id);
            adjacency
                .entry(link.target_id)
                .or_default()
                .push(link.source_id);
        }

        let mut seen: HashSet<Uuid> = HashSet::new();
        let mut clusters = Vec::new();

        for id in &ids {
            if seen.contains(id) || !adjacency.contains_key(id) {
                continue;
            }

            // Breadth-first over the component, stopping at the cap.
            let mut component = Vec::new();
            let mut queue = VecDeque::from([*id]);
            seen.insert(*id);
            let mut overflowed = false;

            while let Some(current) = queue.pop_front() {
                component.push(current);
                if component.len() >= MAX_CLUSTER_SIZE {
                    overflowed = !queue.is_empty();
                    break;
                }
                for neighbour in adjacency.get(&current).into_iter().flatten() {
                    if seen.insert(*neighbour) {
                        queue.push_back(*neighbour);
                    }
                }
            }

            if overflowed {
                // Leave it alone and say so. A component this large means
                // the similarity threshold is mis-tuned, and consolidating
                // on that basis would destroy real distinctions.
                tracing::debug!(
                    root = %id,
                    "skipping oversized similarity component; threshold may be mis-tuned"
                );
                continue;
            }

            if component.len() >= 2 {
                clusters.push(
                    component
                        .into_iter()
                        .filter_map(|cid| by_id.get(&cid))
                        .map(|m| MemoryCandidate {
                            id: m.id,
                            content_hash: m.content_hash.clone(),
                            importance: m.importance,
                            access_count: m.access_count,
                            proof_count: m.proof_count,
                        })
                        .collect(),
                );
            }
        }

        Ok(clusters)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_memory::embedding::HashEmbedder;
    use rustykrab_memory::storage::SqliteMemoryStorage;
    use rustykrab_memory::types::{
        ImportanceSource, LifecycleStage, Memory, MemoryLink, MemoryScope,
    };
    use rustykrab_memory::MemoryConfig;

    fn live_system() -> Arc<MemorySystem> {
        Arc::new(MemorySystem::new(
            MemoryConfig::default(),
            Arc::new(SqliteMemoryStorage::open_in_memory().unwrap()),
            Arc::new(HashEmbedder::new(64)),
        ))
    }

    async fn seed(system: &MemorySystem, agent: Uuid, content: &str) -> Uuid {
        let now = chrono::Utc::now();
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
            access_count: 0,
            last_accessed_at: None,
            last_relevant_at: None,
            created_at: now,
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
        m.id
    }

    async fn link(system: &MemorySystem, a: Uuid, b: Uuid, weight: f64) {
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

    fn ids(clusters: &[Vec<MemoryCandidate>]) -> Vec<std::collections::HashSet<Uuid>> {
        clusters
            .iter()
            .map(|c| c.iter().map(|m| m.id).collect())
            .collect()
    }

    #[tokio::test]
    async fn a_strongly_linked_pair_is_a_cluster() {
        let system = live_system();
        let agent = Uuid::new_v4();
        let a = seed(&system, agent, "the same fact").await;
        let b = seed(&system, agent, "the same fact, said again").await;
        link(&system, a, b, 0.97).await;

        let clusters = MemoryClusterSource::new(Arc::clone(&system))
            .duplicate_clusters(agent)
            .await
            .unwrap();

        assert_eq!(ids(&clusters), vec![[a, b].into_iter().collect()]);
    }

    #[tokio::test]
    async fn merely_related_memories_are_not_duplicates() {
        // This is the difference that matters most.
        //
        // `detect_near_duplicates` writes a `SemanticSimilar` link at a
        // threshold chosen for *retrieval* -- related enough to be worth
        // surfacing together. Consolidation retires memories, so it needs
        // a much stronger claim: that they say the same thing. A source
        // that consumed the retrieval threshold directly would merge
        // distinct facts and call it deduplication.
        let system = live_system();
        let agent = Uuid::new_v4();
        let a = seed(&system, agent, "Geoff's flight is on Tuesday").await;
        let b = seed(&system, agent, "Geoff's hotel is booked for Tuesday").await;
        // Comfortably above the link threshold, below the duplicate one.
        link(&system, a, b, MIN_DUPLICATE_WEIGHT - 0.05).await;

        let clusters = MemoryClusterSource::new(Arc::clone(&system))
            .duplicate_clusters(agent)
            .await
            .unwrap();

        assert!(
            clusters.is_empty(),
            "related is not the same as redundant; these are different facts"
        );
    }

    #[tokio::test]
    async fn an_oversized_similarity_component_is_left_alone() {
        // A component this large means the threshold is finding something
        // other than duplicates. Consolidating it would collapse
        // distinctions rather than remove redundancy, so it is skipped and
        // reported -- not truncated to the cap and merged anyway.
        let system = live_system();
        let agent = Uuid::new_v4();
        let mut all = Vec::new();
        for i in 0..(MAX_CLUSTER_SIZE + 3) {
            all.push(seed(&system, agent, &format!("crowded fact {i}")).await);
        }
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                link(&system, all[i], all[j], 0.99).await;
            }
        }

        let clusters = MemoryClusterSource::new(Arc::clone(&system))
            .duplicate_clusters(agent)
            .await
            .unwrap();

        assert!(
            clusters.is_empty(),
            "an oversized component must be skipped, not truncated and merged"
        );
    }

    #[tokio::test]
    async fn a_component_exactly_at_the_cap_is_still_usable() {
        // The guard must reject components that are too big, not refuse to
        // do any work at the boundary.
        let system = live_system();
        let agent = Uuid::new_v4();
        let mut all = Vec::new();
        for i in 0..MAX_CLUSTER_SIZE {
            all.push(seed(&system, agent, &format!("same fact {i}")).await);
        }
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                link(&system, all[i], all[j], 0.99).await;
            }
        }

        let clusters = MemoryClusterSource::new(Arc::clone(&system))
            .duplicate_clusters(agent)
            .await
            .unwrap();

        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].len(), MAX_CLUSTER_SIZE);
    }

    #[tokio::test]
    async fn a_retired_memory_is_never_a_candidate() {
        // A cycle must not plan against something an earlier cycle already
        // retired, or it would retire it a second time against a different
        // survivor.
        let system = live_system();
        let agent = Uuid::new_v4();
        let a = seed(&system, agent, "fact").await;
        let b = seed(&system, agent, "fact again").await;
        let c = seed(&system, agent, "fact once more").await;
        link(&system, a, b, 0.99).await;
        link(&system, b, c, 0.99).await;

        system.storage().invalidate(c, Some(a)).await.unwrap();

        let clusters = MemoryClusterSource::new(Arc::clone(&system))
            .duplicate_clusters(agent)
            .await
            .unwrap();

        assert_eq!(ids(&clusters), vec![[a, b].into_iter().collect()]);
    }

    #[tokio::test]
    async fn separate_duplicate_groups_stay_separate() {
        // Two unrelated pairs must produce two clusters, not one merged
        // component -- otherwise consolidation would retire one pair
        // against a member of the other.
        let system = live_system();
        let agent = Uuid::new_v4();
        let a1 = seed(&system, agent, "flight fact").await;
        let a2 = seed(&system, agent, "flight fact copy").await;
        let b1 = seed(&system, agent, "hotel fact").await;
        let b2 = seed(&system, agent, "hotel fact copy").await;
        link(&system, a1, a2, 0.99).await;
        link(&system, b1, b2, 0.99).await;

        let clusters = MemoryClusterSource::new(Arc::clone(&system))
            .duplicate_clusters(agent)
            .await
            .unwrap();

        let got = ids(&clusters);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&[a1, a2].into_iter().collect()));
        assert!(got.contains(&[b1, b2].into_iter().collect()));
    }

    #[tokio::test]
    async fn another_agents_memories_are_never_clustered_in() {
        // Clustering is per agent. A cluster spanning two agents would
        // consolidate one agent's memory into another's.
        let system = live_system();
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        let a = seed(&system, mine, "shared-looking fact").await;
        let b = seed(&system, theirs, "shared-looking fact").await;
        link(&system, a, b, 0.99).await;

        let clusters = MemoryClusterSource::new(Arc::clone(&system))
            .duplicate_clusters(mine)
            .await
            .unwrap();

        assert!(
            clusters.is_empty(),
            "a link across agents must not form a cluster"
        );
    }
}
