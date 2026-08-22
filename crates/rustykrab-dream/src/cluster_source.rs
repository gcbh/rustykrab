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
