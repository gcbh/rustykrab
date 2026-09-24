//! Evals over the outcome contract: is the evidence that authorizes the
//! outer loop independent of the agent whose runs it grades?
//!
//! See `rustykrab_dream::eval` for the protocol. These live in the binary
//! crate for the same reason `complete_execution.rs` does: the question is
//! about the production wiring -- the real probes over the real memory
//! backend, driven by a real runner -- and only this crate depends on all
//! of it at once.
//!
//! DREAMING.md rests one claim on top of another: a check is a post-
//! condition probe; a probe is independent of the run; therefore a
//! satisfied check is ground truth, ground truth reaches `Readiness::Ready`,
//! and `Ready` permits the loop to change memory. Each eval here pulls on
//! one link of that chain.

use std::collections::BTreeMap;
use std::sync::Arc;

use rustykrab_agent::sandbox::NoSandbox;
use rustykrab_agent::AgentRunner;
use rustykrab_core::capability::CapabilitySet;
use rustykrab_core::outcome::{OutcomeVerdict, SignalClass};
use rustykrab_core::session::Session;
use rustykrab_core::tool::Tool;
use rustykrab_core::types::{Conversation, Message, MessageContent, Role};
use rustykrab_core::{
    evaluate_contract, MemoryBackend, MemoryWritten, Observation, OutcomeContract, PostCondition,
    ProbeRegistry, ProbeWindow, Result as CoreResult,
};
use rustykrab_dream::eval::{self, Expected};
use rustykrab_memory::backend::HybridMemoryBackend;
use rustykrab_memory::embedding::HashEmbedder;
use rustykrab_memory::storage::SqliteMemoryStorage;
use rustykrab_memory::types::{ConversationTurn, LifecycleStage, TurnMetadata};
use rustykrab_memory::{MemoryConfig, MemorySystem};
use rustykrab_providers::{Script, ScriptedProvider};
use uuid::Uuid;

const SKILL: &str = "note-taking";
const CHECK: &str = "memory_written";

fn memory_system() -> Arc<MemorySystem> {
    Arc::new(MemorySystem::new(
        MemoryConfig::default(),
        Arc::new(SqliteMemoryStorage::open_in_memory().unwrap()),
        Arc::new(HashEmbedder::new(32)),
    ))
}

fn store() -> rustykrab_store::Store {
    let dir = std::env::temp_dir().join(format!("rk-outcome-eval-{}", Uuid::new_v4()));
    rustykrab_store::Store::open(&dir, vec![13u8; 32]).expect("store opens")
}

/// The turn `orchestrate::message_to_turn` builds for a message, reduced
/// to what the write-back needs.
fn turn(session_id: Uuid, speaker: &str, content: &str) -> ConversationTurn {
    ConversationTurn {
        id: Uuid::new_v4(),
        session_id,
        turn_number: 1,
        speaker: speaker.to_string(),
        content: content.to_string(),
        token_count: None,
        metadata: TurnMetadata::default(),
    }
}

fn conversation(id: Uuid) -> Conversation {
    Conversation {
        id,
        messages: vec![Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text("what did we decide about the venue?".into()),
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

fn memory_probe(system: &Arc<MemorySystem>, agent: Uuid) -> Arc<ProbeRegistry> {
    let backend: Arc<dyn MemoryBackend> =
        Arc::new(HybridMemoryBackend::new(Arc::clone(system), agent, agent));
    Arc::new(ProbeRegistry::new().with(Arc::new(MemoryWritten::new(CHECK, backend))))
}

/// A probe for an effect that never holds.
struct Absent(&'static str);

#[async_trait::async_trait]
impl PostCondition for Absent {
    fn name(&self) -> &str {
        self.0
    }
    async fn observe(&self) -> CoreResult<Observation> {
        Ok(None)
    }
}

// ── Is the probe independent of the run? ────────────────────────────────

/// `orchestrate::build_memory_callback` persists every non-system message
/// the runner produces to working memory, through the same table
/// `MemoryWritten` reads. A probe that counts rows in that table is
/// therefore satisfied by the runtime's own bookkeeping, before the agent
/// has done anything at all.
#[tokio::test]
async fn the_runtime_write_back_is_not_a_skill_effect() {
    eval::run(
        "the_runtime_write_back_is_not_a_skill_effect",
        Expected::XFail(
            "MemoryWritten fingerprints the retrievable row count, and the \
             runtime writes a working-memory row for every assistant turn",
        ),
        1,
        async {
            let system = memory_system();
            let agent = Uuid::new_v4();
            let probes = memory_probe(&system, agent);
            let checks = vec![CHECK.to_string()];

            let before = probes.sample(&checks).await;
            // Exactly the write the runtime makes for an assistant reply
            // that called no tool.
            system
                .retain_with_stage(
                    turn(
                        Uuid::new_v4(),
                        "assistant",
                        "I don't have that written down yet -- let me check and come back to you.",
                    ),
                    agent,
                    LifecycleStage::Working,
                )
                .await
                .map_err(|e| e.to_string())?;
            let after = probes.sample(&checks).await;

            let window = ProbeWindow { before, after };
            match window.produced(CHECK) {
                Some(true) => Err(
                    "the probe credited the runtime's own working-memory write-back \
                     as an effect of the skill"
                        .to_string(),
                ),
                Some(false) => Ok(()),
                None => Err("the probe did not answer on one side of the window".to_string()),
            }
        },
    )
    .await;
}

/// The same fault, end to end: a real runner, a scripted model that calls
/// no tool, the production write-back installed on the runner, and the
/// production probe registry. The outcome record the runner writes must
/// not say `Success`/`Verifiable`, because nothing verifiable happened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_that_calls_no_tool_earns_no_verifiable_evidence() {
    eval::run(
        "a_run_that_calls_no_tool_earns_no_verifiable_evidence",
        Expected::XFail(
            "the memory_written probe sees the write-back of the assistant's own \
             reply and the run is recorded as Success/Verifiable",
        ),
        1,
        async {
            let system = memory_system();
            let agent = Uuid::new_v4();
            let store = store();
            let outcomes = Arc::new(store.outcomes());
            let conversation_id = Uuid::new_v4();

            let provider = Arc::new(ScriptedProvider::new(Script {
                default_text:
                    "I don't have that written down yet -- let me check and come back to you."
                        .to_string(),
                scenarios: Vec::new(),
            }));

            // The write-back, as `build_memory_callback` installs it, made
            // synchronous so the eval is deterministic rather than racing
            // the after-sample as production does.
            let write_back_system = Arc::clone(&system);
            let on_message: Arc<dyn Fn(&Message) + Send + Sync> = Arc::new(move |msg: &Message| {
                if msg.role == Role::System {
                    return;
                }
                let (speaker, text) = match (&msg.role, &msg.content) {
                    (Role::Assistant, MessageContent::Text(t)) => ("assistant", t.clone()),
                    (Role::User, MessageContent::Text(t)) => ("user", t.clone()),
                    _ => return,
                };
                let system = Arc::clone(&write_back_system);
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async move {
                        system
                            .retain_with_stage(
                                turn(conversation_id, speaker, &text),
                                agent,
                                LifecycleStage::Working,
                            )
                            .await
                            .expect("write-back");
                    })
                });
            });

            let tools: Vec<Arc<dyn Tool>> = Vec::new();
            let runner = AgentRunner::new(provider, tools, Arc::new(NoSandbox))
                .with_outcome_sink(outcomes.clone())
                .with_active_skill(SKILL)
                .with_outcome_contract(OutcomeContract::new(
                    SKILL,
                    vec![CHECK.to_string()],
                    SignalClass::Verifiable,
                ))
                .with_probes(memory_probe(&system, agent))
                .with_on_message(on_message);

            let session = Session::with_capabilities(
                conversation_id,
                CapabilitySet::for_tools_permissive(&[]),
            );
            let mut conv = conversation(conversation_id);
            runner
                .run(&mut conv, &session)
                .await
                .map_err(|e| format!("runner: {e}"))?;

            let recorded = outcomes.recent(10).await.map_err(|e| e.to_string())?;
            if recorded.is_empty() {
                return Err("the runner recorded no outcome for the turn".to_string());
            }
            for r in &recorded {
                if r.signal == SignalClass::Verifiable && r.verdict == OutcomeVerdict::Success {
                    return Err(format!(
                        "a run that called no tool was recorded as {:?}/{:?}: {}",
                        r.verdict,
                        r.signal,
                        r.detail.as_deref().unwrap_or("no detail")
                    ));
                }
            }
            Ok(())
        },
    )
    .await;
}

// ── What a verdict means ────────────────────────────────────────────────

/// Without a contract, a run that errors is `Failure`. With one, a run
/// that errors before producing any declared effect is `Ambiguous`, which
/// success rates exclude -- so declaring a contract makes a skill that
/// reliably crashes *less* accountable than saying nothing.
#[tokio::test]
async fn an_errored_run_with_nothing_produced_is_a_failure() {
    eval::run(
        "an_errored_run_with_nothing_produced_is_a_failure",
        Expected::XFail(
            "outcome_contract::evaluate maps (checks unmet, errored) to Ambiguous \
             regardless of the error",
        ),
        1,
        async {
            let check = "event_booked";
            let probes = ProbeRegistry::new().with(Arc::new(Absent(check)));
            let contract = OutcomeContract::new(
                "calendar-booking",
                vec![check.to_string()],
                SignalClass::Verifiable,
            );
            let mut before = BTreeMap::new();
            before.insert(check.to_string(), None);
            let window = ProbeWindow {
                before: before.clone(),
                after: before,
            };

            let verdict = evaluate_contract(&contract, &probes, &window, true)
                .ok_or_else(|| "a checkable contract yielded no verdict".to_string())?;
            if verdict.verdict == OutcomeVerdict::Failure {
                Ok(())
            } else {
                Err(format!(
                    "a run that errored with every check unmet was classified {:?}; without \
                     a contract the same run is Failure, so declaring one made a crashing \
                     skill less accountable",
                    verdict.verdict
                ))
            }
        },
    )
    .await;
}
