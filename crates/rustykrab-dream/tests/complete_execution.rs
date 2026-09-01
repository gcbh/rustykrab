//! One complete pass of the outer loop, end to end.
//!
//! Everything else in this repo tests a stage. This tests the *arc*: a new
//! user arrives, holds a conversation driven by a skill that declares what
//! success means, the runs are recorded as ground truth, the analysis
//! decides the evidence is good enough to act on, and a mutating cycle
//! consolidates memory and can be undone.
//!
//! It is written as a narrative for a reason. The failure this guards
//! against is not a broken function — every stage has its own tests — but
//! a system where each stage works and the chain between them does not:
//! records that never reach the analysis, an analysis whose verdict never
//! reaches the gate, a gate that blocks forever because nothing upstream
//! can produce the signal it demands.
//!
//! That last one was real. Until a skill's declared outcome was actually
//! checked, every record was `Implicit`, every report said `proxy_only`,
//! and Phase 2 was correct machinery that could never fire. The first act
//! below is the step that closes it.

use std::collections::HashMap;
use std::sync::Arc;

use rustykrab_core::dream::CycleStatus;
use rustykrab_core::outcome::{
    Attribution, ExecutionCounters, OutcomeRecord, OutcomeVerdict, SignalClass,
};
use rustykrab_core::{evaluate_contract, OutcomeContract};
use rustykrab_dream::consolidation::{run_consolidation_cycle, CycleOutcome};
use rustykrab_dream::engine::{rollback, CyclePolicy};
use rustykrab_dream::memory_mutator::MemorySystemMutator;
use rustykrab_dream::planner::{ConsolidationSource, MemoryCandidate};
use rustykrab_dream::report::{analyze, Readiness};
use rustykrab_dream::StoreOutcomeSource;
use rustykrab_memory::embedding::HashEmbedder;
use rustykrab_memory::storage::SqliteMemoryStorage;
use rustykrab_memory::types::{ImportanceSource, LifecycleStage, Memory, MemoryScope};
use rustykrab_memory::{MemoryConfig, MemorySystem};
use uuid::Uuid;

/// The skill our user's conversation is driven by. It declares what
/// success requires, which is what makes its runs checkable.
const SKILL: &str = "calendar-booking";

fn contract() -> OutcomeContract {
    OutcomeContract::new(
        SKILL,
        vec!["calendar_create".to_string(), "email_confirm".to_string()],
    )
}

fn store() -> rustykrab_store::Store {
    let dir = std::env::temp_dir().join(format!("rk-complete-{}", Uuid::new_v4()));
    rustykrab_store::Store::open(&dir, vec![11u8; 32]).expect("store opens")
}

fn memory_system() -> Arc<MemorySystem> {
    Arc::new(MemorySystem::new(
        MemoryConfig::default(),
        Arc::new(SqliteMemoryStorage::open_in_memory().unwrap()),
        Arc::new(HashEmbedder::new(32)),
    ))
}

/// Stands in for a turn of the agent loop: which tools succeeded, and what
/// the run recalled. The agent's own tests cover the loop itself; here we
/// only need the shape of what it produces.
struct Turn {
    tools_that_succeeded: Vec<&'static str>,
    recalled: Vec<Uuid>,
}

/// Record one turn exactly as `AgentRunner::capture_outcome` does —
/// evaluating the skill's declared contract against what actually ran, and
/// falling back to the implicit signal when it decides nothing.
fn record_for(turn: &Turn, conversation: Uuid, session: Uuid) -> OutcomeRecord {
    let verdict = evaluate_contract(&contract(), |tool| {
        turn.tools_that_succeeded.contains(&tool)
    });

    let counters = ExecutionCounters {
        tool_calls: turn.tools_that_succeeded.len() as u32,
        tool_failures: 0,
        iterations: 2,
        compactions: 0,
    };

    let mut attributions = vec![Attribution::skill(SKILL)];
    for id in &turn.recalled {
        attributions.push(Attribution::memory(*id));
    }
    for tool in &turn.tools_that_succeeded {
        attributions.push(Attribution::tool((*tool).to_string()));
    }

    match verdict {
        Some(v) => OutcomeRecord::new(conversation, session, v.verdict, v.signal)
            .with_confidence(v.confidence)
            .with_detail(v.detail)
            .with_counters(counters)
            .with_attributions(attributions),
        None => OutcomeRecord::new(
            conversation,
            session,
            OutcomeVerdict::Success,
            SignalClass::Implicit,
        )
        .with_confidence(0.3)
        .with_counters(counters)
        .with_attributions(attributions),
    }
}

async fn remember(system: &MemorySystem, agent: Uuid, content: &str, accesses: u32) -> Memory {
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

struct Duplicates {
    clusters: Vec<Vec<Uuid>>,
    system: Arc<MemorySystem>,
}

#[async_trait::async_trait]
impl ConsolidationSource for Duplicates {
    async fn duplicate_clusters(
        &self,
        _: Uuid,
    ) -> rustykrab_core::Result<Vec<Vec<MemoryCandidate>>> {
        let mut out = Vec::new();
        for cluster in &self.clusters {
            let live: Vec<MemoryCandidate> = self
                .system
                .storage()
                .get_memories(cluster)
                .await?
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

#[tokio::test]
async fn a_new_user_is_learned_from_end_to_end() {
    // ── Act 0: a new user, an empty system ────────────────────────────
    let store = store();
    let outcomes = store.outcomes();
    let cycles = store.dream_cycles();
    let system = memory_system();
    let agent = Uuid::new_v4();
    let conversation = Uuid::new_v4();

    assert_eq!(
        outcomes.count().await.unwrap(),
        0,
        "a new user starts with no history"
    );

    // Two things the user told us, said twice in different sessions --
    // the redundancy a consolidation exists to remove.
    let pref_a = remember(&system, agent, "user prefers morning meetings", 5).await;
    let pref_b = remember(&system, agent, "user prefers morning meetings", 0).await;
    let tz = remember(&system, agent, "user is in Pacific time", 2).await;

    // ── Act 1: a conversation, recorded as ground truth ───────────────
    // The skill declared that booking requires creating the event *and*
    // confirming it. Four turns do that; one books without confirming.
    let turns = vec![
        Turn {
            tools_that_succeeded: vec!["calendar_create", "email_confirm"],
            recalled: vec![pref_a.id, tz.id],
        },
        Turn {
            tools_that_succeeded: vec!["calendar_create", "email_confirm"],
            recalled: vec![pref_a.id],
        },
        Turn {
            tools_that_succeeded: vec!["calendar_create", "email_confirm"],
            recalled: vec![tz.id],
        },
        Turn {
            tools_that_succeeded: vec!["calendar_create", "email_confirm"],
            recalled: vec![pref_a.id],
        },
        Turn {
            // Booked, never confirmed. The skill's own declaration is what
            // makes this a failure rather than an indistinguishable
            // "clean run".
            tools_that_succeeded: vec!["calendar_create"],
            recalled: vec![pref_b.id],
        },
    ];

    for turn in &turns {
        let record = record_for(turn, conversation, Uuid::new_v4());
        outcomes.record(&record).await.unwrap();
    }

    let recorded = outcomes.recent(100).await.unwrap();
    assert_eq!(recorded.len(), 5, "every turn left a record");
    assert!(
        recorded.iter().all(|r| r.signal == SignalClass::Verifiable),
        "a skill that declares its outcome produces ground truth, not proxy"
    );
    assert_eq!(
        recorded
            .iter()
            .filter(|r| r.verdict == OutcomeVerdict::Failure)
            .count(),
        1,
        "the unconfirmed booking is a verified failure"
    );

    // ── Act 2: the analysis decides the evidence can carry a decision ──
    let source = StoreOutcomeSource::new(outcomes.clone());
    let report = analyze(&source).await.unwrap();

    assert_eq!(
        report.readiness,
        Readiness::Ready,
        "ground-truth evidence in usable quantity is what Ready means; \
         got {:?} -- if this regresses, Phase 2 silently stops firing",
        report.readiness
    );
    assert!(
        report.readiness.permits_mutation(),
        "and Ready is what permits a mutating cycle to run at all"
    );

    // The skill that drove these runs is visible, and its one failure is
    // counted rather than averaged away.
    let finding = report
        .skills
        .iter()
        .find(|f| f.id == SKILL)
        .unwrap_or_else(|| {
            panic!(
                "the skill that drove every run should appear in the report: {:?}",
                report.skills.iter().map(|f| &f.id).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        finding.tally.harmful, 1,
        "the unconfirmed booking is counted"
    );
    assert_eq!(finding.tally.helpful, 4);

    // ── Act 3: a mutating cycle actually runs ─────────────────────────
    let mutator = MemorySystemMutator::new(Arc::clone(&system), agent, Uuid::new_v4());
    let duplicates = Duplicates {
        clusters: vec![vec![pref_a.id, pref_b.id]],
        system: Arc::clone(&system),
    };

    let outcome = run_consolidation_cycle(
        &cycles,
        &duplicates,
        &mutator,
        agent,
        report.readiness,
        &CyclePolicy::default(),
    )
    .await
    .unwrap();

    let cycle_id = match outcome {
        CycleOutcome::Promoted {
            cycle_id, applied, ..
        } => {
            assert_eq!(applied, 1, "one redundant copy retired");
            cycle_id
        }
        other => panic!(
            "the loop had ground-truth evidence and duplicate memories, \
             yet did not act: {other:?}"
        ),
    };

    // The system genuinely changed: one copy of the preference, both facts
    // still present.
    let live = system.storage().list_retrievable(agent).await.unwrap();
    let mut by_content: HashMap<&str, usize> = HashMap::new();
    for m in &live {
        *by_content.entry(m.content.as_str()).or_default() += 1;
    }
    assert_eq!(
        by_content.get("user prefers morning meetings"),
        Some(&1),
        "the duplicate is gone"
    );
    assert_eq!(
        by_content.get("user is in Pacific time"),
        Some(&1),
        "the unrelated fact is untouched"
    );
    assert!(
        live.iter().any(|m| m.id == pref_a.id),
        "the copy the system actually used is the one that survived"
    );

    // ── Act 4: and it can be taken back ───────────────────────────────
    let recorded_cycle = cycles.get(cycle_id).await.unwrap().unwrap();
    assert_eq!(recorded_cycle.status, CycleStatus::Promoted);
    let changes = cycles.changes(cycle_id).await.unwrap();
    assert!(!changes.is_empty(), "the manifest describes what it did");

    rollback(
        &mutator,
        recorded_cycle.status,
        &changes,
        &CyclePolicy::default(),
    )
    .await
    .unwrap()
    .expect("nothing has depended on the change yet");
    cycles
        .set_status(cycle_id, CycleStatus::RolledBack)
        .await
        .unwrap();

    let restored = system.storage().list_retrievable(agent).await.unwrap();
    assert_eq!(
        restored.len(),
        3,
        "reversal restores every memory the cycle retired"
    );
    assert_eq!(
        cycles.get(cycle_id).await.unwrap().unwrap().status,
        CycleStatus::RolledBack
    );
}

#[tokio::test]
async fn the_same_conversation_without_a_declared_outcome_changes_nothing() {
    // The control. Identical traffic from a skill that never declared what
    // success means yields proxy evidence, and the loop correctly declines
    // to touch anything -- which is the behaviour every phase before this
    // one shipped with.
    let store = store();
    let outcomes = store.outcomes();
    let cycles = store.dream_cycles();
    let system = memory_system();
    let agent = Uuid::new_v4();
    let conversation = Uuid::new_v4();

    let a = remember(&system, agent, "user prefers morning meetings", 5).await;
    let b = remember(&system, agent, "user prefers morning meetings", 0).await;

    // Same five turns, but the skill declared nothing to check against.
    let undeclared = OutcomeContract::new(SKILL, vec![]);
    for _ in 0..5 {
        assert!(
            evaluate_contract(&undeclared, |_| true).is_none(),
            "an empty declaration must decide nothing"
        );
        let record = OutcomeRecord::new(
            conversation,
            Uuid::new_v4(),
            OutcomeVerdict::Success,
            SignalClass::Implicit,
        )
        .with_confidence(0.3)
        .with_attributions(vec![Attribution::skill(SKILL)]);
        outcomes.record(&record).await.unwrap();
    }

    let report = analyze(&StoreOutcomeSource::new(outcomes.clone()))
        .await
        .unwrap();
    assert_eq!(
        report.readiness,
        Readiness::ProxyOnly,
        "no declaration means no ground truth, whatever the volume"
    );
    assert!(!report.readiness.permits_mutation());

    let mutator = MemorySystemMutator::new(Arc::clone(&system), agent, Uuid::new_v4());
    let outcome = run_consolidation_cycle(
        &cycles,
        &Duplicates {
            clusters: vec![vec![a.id, b.id]],
            system: Arc::clone(&system),
        },
        &mutator,
        agent,
        report.readiness,
        &CyclePolicy::default(),
    )
    .await
    .unwrap();

    assert!(
        matches!(outcome, CycleOutcome::Refused { .. }),
        "the loop must decline on proxy evidence, got {outcome:?}"
    );
    assert_eq!(
        system
            .storage()
            .list_retrievable(agent)
            .await
            .unwrap()
            .len(),
        2,
        "and the duplicate survives untouched"
    );
}
