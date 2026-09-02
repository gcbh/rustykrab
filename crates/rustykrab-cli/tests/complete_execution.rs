//! One complete pass of the outer loop, driven by a real agent.
//!
//! Everything else in this repo tests a stage. This tests the *arc*: a
//! conversation is run by an actual `AgentRunner` against a skill that
//! declared what success requires, the runner records what it observed,
//! the analysis decides that evidence can carry a decision, and a mutating
//! cycle consolidates memory and can be undone.
//!
//! It lives here, in the binary crate, because this is the only crate that
//! depends on every piece at once — agent, providers, store, memory and
//! dream. That is the point: a test that stubbed any of them would be
//! testing the stub.
//!
//! An earlier version of this file *did* stub the first link. It
//! reimplemented the runner's capture logic rather than running a runner,
//! and so it passed just as happily when the production wiring was
//! missing entirely — which it was. The rule this file now follows is that
//! the record under test must be written by the same code path that writes
//! it in production.

use std::collections::HashMap;
use std::sync::Arc;

use rustykrab_agent::sandbox::NoSandbox;
use rustykrab_agent::AgentRunner;
use rustykrab_core::capability::CapabilitySet;
use rustykrab_core::dream::CycleStatus;
use rustykrab_core::outcome::{OutcomeVerdict, SignalClass};
use rustykrab_core::session::Session;
use rustykrab_core::tool::Tool;
use rustykrab_core::types::{Conversation, Message, MessageContent, Role, ToolSchema};
use rustykrab_core::{OutcomeContract, Result as CoreResult};
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
use rustykrab_providers::{Scenario, Script, ScriptStep, ScriptToolCall, ScriptedProvider};
use serde_json::json;
use uuid::Uuid;

const SKILL: &str = "calendar-booking";

/// A tool that succeeds and records nothing. The run's *effects* are what
/// the contract checks, and for this test the effect is simply "a tool of
/// this name completed successfully".
struct Effect(&'static str);

#[async_trait::async_trait]
impl Tool for Effect {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "test effect"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.0.to_string(),
            description: "test effect".to_string(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }
    async fn execute(&self, _args: serde_json::Value) -> CoreResult<serde_json::Value> {
        Ok(json!({"ok": true}))
    }
}

/// A conversation with one user turn, matching what the gateway hands the
/// runner in production.
fn conversation(id: Uuid) -> Conversation {
    Conversation {
        id,
        messages: vec![Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text("please book the meeting".into()),
            created_at: chrono::Utc::now(),
            agent_version: None,
        }],
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        title: None,
        summary: None,
        detected_profile: None,
        channel_source: None,
        channel_id: None,
        channel_thread_id: None,
    }
}

fn step(tool: &str) -> ScriptStep {
    ScriptStep {
        text: None,
        tool_calls: vec![ScriptToolCall {
            name: tool.to_string(),
            arguments: json!({}),
        }],
    }
}

/// Run one conversation turn through a real `AgentRunner`, with the skill's
/// contract attached exactly as `orchestrate.rs` attaches it.
async fn run_turn(
    outcomes: Arc<rustykrab_store::OutcomeStore>,
    tools_to_call: &[&str],
    conversation_id: Uuid,
) {
    let script = Script {
        default_text: "Done.".to_string(),
        scenarios: vec![Scenario {
            trigger: "book".to_string(),
            steps: tools_to_call.iter().map(|t| step(t)).collect(),
        }],
    };
    let provider = Arc::new(ScriptedProvider::new(script));

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(Effect("calendar_create")),
        Arc::new(Effect("email_confirm")),
    ];

    let runner = AgentRunner::new(provider, tools, Arc::new(NoSandbox))
        .with_outcome_sink(outcomes)
        .with_active_skill(SKILL)
        // The declaration under test. This is the line that was missing
        // from production: without it every record below is `Implicit`.
        .with_outcome_contract(OutcomeContract::new(
            SKILL,
            vec!["calendar_create".to_string(), "email_confirm".to_string()],
            SignalClass::Verifiable,
        ));

    let caps = CapabilitySet::for_tools_permissive(&["calendar_create", "email_confirm"]);
    let session = Session::with_capabilities(conversation_id, caps);
    let mut conv = conversation(conversation_id);

    let _ = runner.run(&mut conv, &session).await;
}

fn store() -> rustykrab_store::Store {
    let dir = std::env::temp_dir().join(format!("rk-arc-{}", Uuid::new_v4()));
    rustykrab_store::Store::open(&dir, vec![11u8; 32]).expect("store opens")
}

fn memory_system() -> Arc<MemorySystem> {
    Arc::new(MemorySystem::new(
        MemoryConfig::default(),
        Arc::new(SqliteMemoryStorage::open_in_memory().unwrap()),
        Arc::new(HashEmbedder::new(32)),
    ))
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
        metadata: json!({}),
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
    async fn duplicate_clusters(&self, _: Uuid) -> CoreResult<Vec<Vec<MemoryCandidate>>> {
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
async fn a_declaring_skill_drives_the_loop_end_to_end() {
    let store = store();
    let outcomes = Arc::new(store.outcomes());
    let cycles = store.dream_cycles();
    let system = memory_system();
    let agent = Uuid::new_v4();

    assert_eq!(
        outcomes.count().await.unwrap(),
        0,
        "a new user has no history"
    );

    let pref_a = remember(&system, agent, "user prefers morning meetings", 5).await;
    let pref_b = remember(&system, agent, "user prefers morning meetings", 0).await;
    let tz = remember(&system, agent, "user is in Pacific time", 2).await;

    // ── Act 1: real runs, recorded by the real runner ─────────────────
    // Six turns do both declared effects; one books without confirming.
    // Six rather than five because an outstanding check is deliberately
    // *not* decisive -- it buys no readiness in either direction -- so the
    // decisive evidence has to clear MIN_OBSERVATIONS on its own.
    for _ in 0..6 {
        run_turn(
            outcomes.clone(),
            &["calendar_create", "email_confirm"],
            Uuid::new_v4(),
        )
        .await;
    }
    run_turn(outcomes.clone(), &["calendar_create"], Uuid::new_v4()).await;

    let recorded = outcomes.recent(100).await.unwrap();
    assert_eq!(recorded.len(), 7, "every run left a record");
    assert!(
        recorded.iter().all(|r| r.signal == SignalClass::Verifiable),
        "a skill that declared its outcome must produce ground truth. \
         If this fails, the contract is not reaching the runner in \
         production and every later stage is blocked: {:?}",
        recorded.iter().map(|r| r.signal).collect::<Vec<_>>()
    );
    assert_eq!(
        recorded
            .iter()
            .filter(|r| r.verdict == OutcomeVerdict::Success)
            .count(),
        6
    );
    assert_eq!(
        recorded
            .iter()
            .filter(|r| r.verdict == OutcomeVerdict::Ambiguous)
            .count(),
        1,
        "the unconfirmed booking is outstanding, not condemned"
    );

    // ── Act 2: the analysis will act on it ────────────────────────────
    let report = analyze(&StoreOutcomeSource::new(store.outcomes()))
        .await
        .unwrap();
    assert_eq!(
        report.readiness,
        Readiness::Ready,
        "ground truth in usable quantity is what Ready means; got {:?}",
        report.readiness
    );
    assert!(report.readiness.permits_mutation());

    // ── Act 3: a mutating cycle actually runs ─────────────────────────
    let mutator = MemorySystemMutator::new(Arc::clone(&system), agent, Uuid::new_v4());
    let outcome = run_consolidation_cycle(
        &cycles,
        &Duplicates {
            clusters: vec![vec![pref_a.id, pref_b.id]],
            system: Arc::clone(&system),
        },
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
        other => panic!("had ground truth and duplicates, yet did not act: {other:?}"),
    };

    let live = system.storage().list_retrievable(agent).await.unwrap();
    let mut by_content: HashMap<&str, usize> = HashMap::new();
    for m in &live {
        *by_content.entry(m.content.as_str()).or_default() += 1;
    }
    assert_eq!(by_content.get("user prefers morning meetings"), Some(&1));
    assert_eq!(by_content.get("user is in Pacific time"), Some(&1));
    assert!(
        live.iter().any(|m| m.id == pref_a.id),
        "the copy the system actually used is the one that survived"
    );
    assert!(live.iter().any(|m| m.id == tz.id));

    // ── Act 4: and it can be taken back ───────────────────────────────
    let recorded_cycle = cycles.get(cycle_id).await.unwrap().unwrap();
    assert_eq!(recorded_cycle.status, CycleStatus::Promoted);
    let changes = cycles.changes(cycle_id).await.unwrap();
    assert!(!changes.is_empty());

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

    assert_eq!(
        system
            .storage()
            .list_retrievable(agent)
            .await
            .unwrap()
            .len(),
        3,
        "reversal restores every memory the cycle retired"
    );
}

#[tokio::test]
async fn without_a_declaration_the_same_runs_change_nothing() {
    // The control. Identical traffic through the same real runner, from a
    // skill that declared nothing, must leave the system untouched.
    let store = store();
    let outcomes = Arc::new(store.outcomes());
    let cycles = store.dream_cycles();
    let system = memory_system();
    let agent = Uuid::new_v4();

    let a = remember(&system, agent, "user prefers morning meetings", 5).await;
    let b = remember(&system, agent, "user prefers morning meetings", 0).await;

    for _ in 0..5 {
        let script = Script {
            default_text: "Done.".to_string(),
            scenarios: vec![Scenario {
                trigger: "book".to_string(),
                steps: vec![step("calendar_create"), step("email_confirm")],
            }],
        };
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(Effect("calendar_create")),
            Arc::new(Effect("email_confirm")),
        ];
        // Same runner, same work -- only the contract is absent.
        let runner = AgentRunner::new(
            Arc::new(ScriptedProvider::new(script)),
            tools,
            Arc::new(NoSandbox),
        )
        .with_outcome_sink(outcomes.clone())
        .with_active_skill(SKILL);

        let cid = Uuid::new_v4();
        let caps = CapabilitySet::for_tools_permissive(&["calendar_create", "email_confirm"]);
        let session = Session::with_capabilities(cid, caps);
        let mut conv = conversation(cid);
        let _ = runner.run(&mut conv, &session).await;
    }

    let recorded = outcomes.recent(100).await.unwrap();
    assert_eq!(recorded.len(), 5);
    assert!(
        recorded.iter().all(|r| r.signal == SignalClass::Implicit),
        "no declaration means no ground truth, however clean the runs"
    );

    let report = analyze(&StoreOutcomeSource::new(store.outcomes()))
        .await
        .unwrap();
    assert_eq!(report.readiness, Readiness::ProxyOnly);
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
