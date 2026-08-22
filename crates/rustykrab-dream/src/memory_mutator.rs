//! Backs the engine's [`MemoryMutator`] with the real memory system.
//!
//! Provenance is written into a memory's existing `metadata` JSON rather
//! than a new column. `Memory` has no constructor and is built by struct
//! literal in two dozen places, and its upsert uses hand-numbered
//! placeholders, so a new field is a wide and error-prone change for
//! something free-form JSON already models well. A later migration can
//! promote it to a column if it ever needs to be queried directly.

use std::sync::Arc;

use rustykrab_core::dream::MemoryOrigin;
use rustykrab_core::{Error, Result};
use rustykrab_memory::types::{ImportanceSource, LifecycleStage, Memory, MemoryScope};
use rustykrab_memory::MemorySystem;
use uuid::Uuid;

use crate::mutation::{MemoryFacts, MemoryMutator};

/// Metadata key recording how a memory came to exist.
pub const ORIGIN_KEY: &str = "origin";
/// Metadata key recording which cycle produced a memory, so a promoted
/// change can be traced back to the reasoning that proposed it.
pub const CYCLE_KEY: &str = "dream_cycle_id";

/// Applies dream changes to the hybrid memory system.
pub struct MemorySystemMutator {
    system: Arc<MemorySystem>,
    agent_id: Uuid,
    cycle_id: Uuid,
}

impl MemorySystemMutator {
    pub fn new(system: Arc<MemorySystem>, agent_id: Uuid, cycle_id: Uuid) -> Self {
        Self {
            system,
            agent_id,
            cycle_id,
        }
    }
}

#[async_trait::async_trait]
impl MemoryMutator for MemorySystemMutator {
    async fn facts(&self, ids: &[Uuid]) -> Result<Vec<MemoryFacts>> {
        let memories = self.system.storage().get_memories(ids).await?;
        Ok(memories
            .into_iter()
            .map(|m| MemoryFacts {
                id: m.id,
                content_hash: m.content_hash,
                access_count: m.access_count,
                is_valid: m.is_valid,
            })
            .collect())
    }

    async fn create(&self, memory_id: Uuid, content: &str, parent_ids: &[Uuid]) -> Result<()> {
        let now = chrono::Utc::now();
        let content_hash = rustykrab_memory::hash_content(content);

        // Inherit the highest importance among the parents rather than
        // assuming a default: a consolidation of things that mattered
        // should not quietly demote them.
        let parents = self.system.storage().get_memories(parent_ids).await?;
        let importance = parents
            .iter()
            .map(|p| p.importance)
            .fold(0.0_f64, f64::max)
            .max(0.5);
        let generation = parents
            .iter()
            .map(|p| p.consolidation_generation)
            .max()
            .unwrap_or(0)
            .saturating_add(1);

        let memory = Memory {
            id: memory_id,
            agent_id: self.agent_id,
            content: content.to_string(),
            content_hash,
            scope: MemoryScope::User,
            session_id: None,
            user_id: None,
            // Enters as episodic and re-earns promotion through the normal
            // lifecycle; a consolidation does not get to declare itself
            // semantic on arrival.
            lifecycle_stage: LifecycleStage::Episodic,
            importance,
            importance_source: ImportanceSource::Heuristic,
            decay_rate: 1.0,
            confidence: 1.0,
            access_count: 0,
            last_accessed_at: None,
            last_relevant_at: None,
            created_at: now,
            parent_memory_ids: parent_ids.to_vec(),
            consolidation_generation: generation,
            proof_count: parents.len().max(1) as u32,
            occurred_start: None,
            occurred_end: None,
            is_valid: true,
            invalidated_by: None,
            invalidated_at: None,
            tags: Vec::new(),
            metadata: serde_json::json!({
                ORIGIN_KEY: MemoryOrigin::Dream.as_str(),
                CYCLE_KEY: self.cycle_id.to_string(),
            }),
        };

        self.system.storage().upsert_memory(&memory).await
    }

    async fn invalidate(&self, memory_id: Uuid, superseded_by: Uuid) -> Result<()> {
        self.system
            .storage()
            .invalidate(memory_id, Some(superseded_by))
            .await
    }

    async fn restore(&self, memory_id: Uuid) -> Result<()> {
        self.system.storage().restore(memory_id).await
    }

    async fn discard(&self, memory_id: Uuid) -> Result<()> {
        // A memory the loop created and is now taking back. Retired
        // without a superseding id, since nothing replaced it -- it simply
        // should not have existed.
        let created_here = self
            .system
            .storage()
            .get_memory(memory_id)
            .await?
            .map(|m| {
                m.metadata
                    .get(ORIGIN_KEY)
                    .and_then(|v| v.as_str())
                    .and_then(MemoryOrigin::parse)
                    == Some(MemoryOrigin::Dream)
            })
            .unwrap_or(false);

        if !created_here {
            // Refusing here is the point: discard is only ever meant to
            // undo the loop's own creation, and applying it to a
            // conversation-derived memory would destroy user data.
            return Err(Error::Internal(format!(
                "refusing to discard memory {memory_id}: not created by the outer loop"
            )));
        }

        self.system.storage().invalidate(memory_id, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{promote, rollback, CyclePolicy};
    use crate::report::Readiness;
    use rustykrab_core::dream::StagedChange;
    use rustykrab_memory::embedding::HashEmbedder;
    use rustykrab_memory::storage::SqliteMemoryStorage;
    use rustykrab_memory::MemoryConfig;

    /// A real memory system on an in-memory database with a deterministic
    /// embedder — no network, no model, but the actual storage layer the
    /// engine will run against in production.
    fn live_system() -> Arc<MemorySystem> {
        let storage = Arc::new(SqliteMemoryStorage::open_in_memory().unwrap());
        let embedder = Arc::new(HashEmbedder::new(64));
        Arc::new(MemorySystem::new(
            MemoryConfig::default(),
            storage,
            embedder,
        ))
    }

    async fn seed(system: &MemorySystem, agent_id: Uuid, content: &str) -> Memory {
        let now = chrono::Utc::now();
        let memory = Memory {
            id: Uuid::new_v4(),
            agent_id,
            content: content.to_string(),
            content_hash: rustykrab_memory::hash_content(content),
            scope: MemoryScope::User,
            session_id: None,
            user_id: None,
            lifecycle_stage: LifecycleStage::Episodic,
            importance: 0.7,
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
        system.storage().upsert_memory(&memory).await.unwrap();
        memory
    }

    fn consolidation(parents: &[&Memory], child: Uuid) -> Vec<StagedChange> {
        let mut changes = vec![StagedChange::CreateMemory {
            memory_id: child,
            content: "the user prefers window seats".into(),
            parent_ids: parents.iter().map(|p| p.id).collect(),
        }];
        for p in parents {
            changes.push(StagedChange::InvalidateMemory {
                memory_id: p.id,
                superseded_by: child,
                expected_content_hash: p.content_hash.clone(),
            });
        }
        changes
    }

    #[tokio::test]
    async fn consolidation_round_trips_against_real_storage() {
        // End-to-end over the actual SQLite layer: promote merges two
        // memories into one, rollback restores exactly the starting state.
        let system = live_system();
        let agent = Uuid::new_v4();
        let cycle = Uuid::new_v4();

        let a = seed(&system, agent, "user likes window seats").await;
        let b = seed(&system, agent, "user asked for a window seat again").await;
        let child = Uuid::new_v4();
        let changes = consolidation(&[&a, &b], child);

        let mutator = MemorySystemMutator::new(Arc::clone(&system), agent, cycle);

        promote(
            &mutator,
            rustykrab_core::dream::CycleStatus::Staged,
            Readiness::Ready,
            &changes,
            &CyclePolicy::default(),
        )
        .await
        .unwrap()
        .expect("ready evidence permits the change");

        // Parents retired, consolidated memory present and attributed.
        let a_after = system.storage().get_memory(a.id).await.unwrap().unwrap();
        assert!(!a_after.is_valid, "parent retired");
        assert_eq!(a_after.invalidated_by, Some(child));

        let merged = system.storage().get_memory(child).await.unwrap().unwrap();
        assert!(merged.is_valid);
        assert_eq!(merged.parent_memory_ids.len(), 2, "lineage recorded");
        assert_eq!(merged.consolidation_generation, 1);
        assert_eq!(
            merged.metadata.get(ORIGIN_KEY).and_then(|v| v.as_str()),
            Some("dream"),
            "a retrieved memory must be able to say it came from the loop"
        );
        assert_eq!(
            merged.metadata.get(CYCLE_KEY).and_then(|v| v.as_str()),
            Some(cycle.to_string().as_str()),
            "and which cycle produced it"
        );

        // Now undo it.
        rollback(
            &mutator,
            rustykrab_core::dream::CycleStatus::Promoted,
            &changes,
            &CyclePolicy::default(),
        )
        .await
        .unwrap()
        .expect("nothing has depended on it yet");

        let a_restored = system.storage().get_memory(a.id).await.unwrap().unwrap();
        assert!(a_restored.is_valid, "parent came back");
        assert!(a_restored.invalidated_by.is_none());
        assert_eq!(
            a_restored.content, a.content,
            "restored content is unchanged"
        );

        let merged_after = system.storage().get_memory(child).await.unwrap().unwrap();
        assert!(
            !merged_after.is_valid,
            "the consolidated memory is retired on revert"
        );
    }

    #[tokio::test]
    async fn a_consolidation_inherits_the_strongest_parent_importance() {
        // Merging things that mattered must not quietly demote them.
        let system = live_system();
        let agent = Uuid::new_v4();

        let low = seed(&system, agent, "minor detail").await;
        let mut high = seed(&system, agent, "critical detail").await;
        high.importance = 0.95;
        system.storage().upsert_memory(&high).await.unwrap();

        let child = Uuid::new_v4();
        let mutator = MemorySystemMutator::new(Arc::clone(&system), agent, Uuid::new_v4());
        mutator
            .create(child, "merged detail", &[low.id, high.id])
            .await
            .unwrap();

        let merged = system.storage().get_memory(child).await.unwrap().unwrap();
        assert!(
            merged.importance >= 0.95,
            "expected the strongest parent's importance, got {}",
            merged.importance
        );
    }

    #[tokio::test]
    async fn a_consolidation_starts_episodic_rather_than_semantic() {
        // It re-earns promotion through the normal lifecycle instead of
        // declaring itself long-term knowledge on arrival.
        let system = live_system();
        let agent = Uuid::new_v4();
        let child = Uuid::new_v4();

        MemorySystemMutator::new(Arc::clone(&system), agent, Uuid::new_v4())
            .create(child, "merged", &[])
            .await
            .unwrap();

        let merged = system.storage().get_memory(child).await.unwrap().unwrap();
        assert_eq!(merged.lifecycle_stage, LifecycleStage::Episodic);
    }

    #[tokio::test]
    async fn discard_refuses_a_memory_the_loop_did_not_create() {
        // discard exists only to undo the loop's own creation. Pointing it
        // at conversation-derived memory would destroy user data.
        let system = live_system();
        let agent = Uuid::new_v4();
        let theirs = seed(&system, agent, "something the user told us").await;

        let mutator = MemorySystemMutator::new(Arc::clone(&system), agent, Uuid::new_v4());
        let result = mutator.discard(theirs.id).await;

        assert!(
            result.is_err(),
            "must refuse to discard user-derived memory"
        );
        let after = system
            .storage()
            .get_memory(theirs.id)
            .await
            .unwrap()
            .unwrap();
        assert!(after.is_valid, "and must leave it untouched");
    }

    #[tokio::test]
    async fn restore_returns_a_memory_to_the_retrievable_set() {
        let system = live_system();
        let agent = Uuid::new_v4();
        let m = seed(&system, agent, "recoverable").await;

        system
            .storage()
            .invalidate(m.id, Some(Uuid::new_v4()))
            .await
            .unwrap();
        let tombstoned = system.storage().get_memory(m.id).await.unwrap().unwrap();
        assert!(!tombstoned.lifecycle_stage.is_retrievable());

        system.storage().restore(m.id).await.unwrap();

        let restored = system.storage().get_memory(m.id).await.unwrap().unwrap();
        assert!(restored.is_valid);
        assert!(
            restored.lifecycle_stage.is_retrievable(),
            "a restored memory must be reachable again"
        );
        assert!(restored.invalidated_at.is_none());
    }

    #[tokio::test]
    async fn memory_content_is_immutable_once_written() {
        // Worth pinning, because it determines what staleness can mean
        // here: `upsert_memory` deliberately omits content and
        // content_hash from its ON CONFLICT clause, so an "edit" in this
        // system is really a retire-and-recreate. The hash precondition is
        // therefore defence-in-depth rather than the primary check.
        let system = live_system();
        let agent = Uuid::new_v4();
        let mut m = seed(&system, agent, "original").await;

        m.content = "rewritten".into();
        m.content_hash = rustykrab_memory::hash_content(&m.content);
        system.storage().upsert_memory(&m).await.unwrap();

        let after = system.storage().get_memory(m.id).await.unwrap().unwrap();
        assert_eq!(
            after.content, "original",
            "content is immutable after creation"
        );
    }

    #[tokio::test]
    async fn a_parent_retired_since_planning_is_not_retired_again() {
        // Retirement is the mutation that actually happens to a memory, so
        // it is the collision a cycle has to notice: something else got
        // there first while this cycle was thinking.
        let system = live_system();
        let agent = Uuid::new_v4();
        let m = seed(&system, agent, "as planned").await;
        let changes = consolidation(&[&m], Uuid::new_v4());

        // Another actor retires it after the cycle planned against it.
        let other = Uuid::new_v4();
        system
            .storage()
            .invalidate(m.id, Some(other))
            .await
            .unwrap();

        let mutator = MemorySystemMutator::new(Arc::clone(&system), agent, Uuid::new_v4());
        let result = promote(
            &mutator,
            rustykrab_core::dream::CycleStatus::Staged,
            Readiness::Ready,
            &changes,
            &CyclePolicy::default(),
        )
        .await
        .unwrap();

        assert!(
            result.is_err(),
            "the only parent was already retired, so there is nothing left to consolidate"
        );
        let after = system.storage().get_memory(m.id).await.unwrap().unwrap();
        assert_eq!(
            after.invalidated_by,
            Some(other),
            "the earlier retirement must not be overwritten by this cycle"
        );
    }

    #[tokio::test]
    async fn a_vanished_parent_is_treated_as_stale() {
        let system = live_system();
        let agent = Uuid::new_v4();
        let ghost = Memory {
            id: Uuid::new_v4(),
            ..seed(&system, agent, "temporary").await
        };
        let changes = consolidation(&[&ghost], Uuid::new_v4());

        let mutator = MemorySystemMutator::new(Arc::clone(&system), agent, Uuid::new_v4());
        let result = promote(
            &mutator,
            rustykrab_core::dream::CycleStatus::Staged,
            Readiness::Ready,
            &changes,
            &CyclePolicy::default(),
        )
        .await
        .unwrap();

        assert!(
            result.is_err(),
            "a parent that no longer exists cannot be consolidated"
        );
    }
}
