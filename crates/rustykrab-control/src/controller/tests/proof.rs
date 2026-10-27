//! Functional capability proof against a real store (`proof.rs`): a
//! capability item for a need with a registered probe closes `done` only on
//! a host probe's `Proof` on the worker that failed the item waiting on it,
//! and that item resumes only on `Proof` for its current repair generation.
//!
//! The probing worker here is scripted ([`Prober`]): it answers
//! `probe_scope` with its own tool ceiling and `probe` with the outcome a
//! test queued, and records what it was asked. The evidence kinds are
//! spelled out, not imported, so a reader sees the wire names.

use std::collections::VecDeque;

use rustykrab_core::work::{BlockedReason, ErrorSubclass, EventKind, Evidence, WorkKind};
use rustykrab_tools::work_backend::ToolState;

use super::*;
use crate::capability_proof::{
    probe_inputs_digest, tool_ceiling_digest, ExecutionPath, ExpectedOperation, OperationProbe,
    ProbeContext, ProbeOutcome, ProbeRequest, ProbeScope, RegisteredAction,
};
use crate::controller::proof::{PlannedProbe, RepairGeneration};

const NODE: &str = "Run the node script";
const NEEDS_NODE: &str = "needs tool: node";

/// What a scripted probe does.
enum Answer {
    Outcome(ProbeOutcome),
    /// Never answer: the controller's deadline has to end it.
    Hang,
}

/// A scripted worker that can run registered probes.
pub(super) struct Prober {
    run: Scripted,
    kind: WorkerKind,
    caps: WorkerCapabilities,
    /// Whether it offers a probe scope (a worker that cannot probe does not).
    scope: bool,
    policy: std::sync::atomic::AtomicUsize,
    answers: Mutex<VecDeque<Answer>>,
    requests: Mutex<Vec<ProbeRequest>>,
    /// The store, to check at probe time that the generation the request is
    /// bound to is already on record.
    store: Mutex<Option<Store>>,
    generation_first: Mutex<Vec<bool>>,
    /// The run directory each probe was scoped to, and whether it existed
    /// while the probe ran.
    dirs: Mutex<Vec<(Option<PathBuf>, bool)>>,
}

impl Prober {
    pub(super) fn new(name: &str, script: &Arc<Script>) -> Prober {
        let run = Scripted {
            name: name.to_string(),
            concurrency: 1,
            script: Arc::clone(script),
        };
        let caps = run.capabilities();
        Prober {
            run,
            kind: WorkerKind::Local,
            caps,
            scope: true,
            policy: std::sync::atomic::AtomicUsize::new(0),
            answers: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            store: Mutex::new(None),
            generation_first: Mutex::new(Vec::new()),
            dirs: Mutex::new(Vec::new()),
        }
    }

    /// A native coding agent: Codex, with no RustyKrab tools in its ceiling.
    pub(super) fn native(name: &str, script: &Arc<Script>) -> Prober {
        let mut p = Prober::new(name, script);
        p.kind = WorkerKind::Codex;
        p.caps.tools = Vec::new();
        p.scope = false;
        p
    }

    fn answer(&self, answer: Answer) {
        self.answers.lock().unwrap().push_back(answer);
    }

    fn requests(&self) -> Vec<ProbeRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Worker for Prober {
    fn name(&self) -> &str {
        &self.run.name
    }

    fn kind(&self) -> WorkerKind {
        self.kind
    }

    fn capabilities(&self) -> WorkerCapabilities {
        self.caps.clone()
    }

    fn concurrency(&self) -> usize {
        self.run.concurrency
    }

    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        self.run.run(brief).await
    }

    fn usage(&self, run: &str) -> Option<crate::worker::RunUsage> {
        self.run.usage(run)
    }

    fn access_profile(&self) -> serde_json::Value {
        serde_json::json!({"capabilities": self.caps, "policy": self.policy.load(std::sync::atomic::Ordering::SeqCst)})
    }
    fn probe_scope(&self, context: &ProbeContext) -> Option<ProbeScope> {
        self.scope.then(|| ProbeScope {
            worker: self.run.name.clone(),
            path: if context.requirements.uses_host() {
                ExecutionPath::Host
            } else {
                ExecutionPath::Native
            },
            policy_digest: crate::capability_proof::binding_digest(&self.access_profile()),
            ceiling_digest: tool_ceiling_digest(&self.caps.tools),
        })
    }

    async fn probe(&self, request: &ProbeRequest) -> OperationProbe {
        self.requests.lock().unwrap().push(request.clone());
        let dir = request.context.bound_dir.clone();
        let exists = dir.as_ref().is_some_and(|d| d.is_dir());
        self.dirs.lock().unwrap().push((dir, exists));
        let store = self.store.lock().unwrap().clone();
        if let Some(store) = store {
            let generation = &request.expected.generation;
            let waiting = generation.rsplit_once(':').map(|(w, _)| w).unwrap_or("");
            let on_record = store
                .work_evidence_list(waiting)
                .await
                .unwrap()
                .iter()
                .any(|e| e.kind == "repair_generation" && e.hash.as_deref() == Some(generation));
            self.generation_first.lock().unwrap().push(on_record);
        }
        let answer = self.answers.lock().unwrap().pop_front();
        match answer {
            Some(Answer::Outcome(outcome)) => request.expected.observation(
                outcome,
                format!("scripted probe observed {}", serde_json::json!(outcome)),
            ),
            Some(Answer::Hang) => std::future::pending().await,
            None => request
                .expected
                .observation(ProbeOutcome::Unobserved, "no scripted observation"),
        }
    }
}

/// A harness over `fleet`, sharing `script`, with `catalog`.
pub(super) fn harness(
    config: ControllerConfig,
    catalog: StaticCatalog,
    script: Arc<Script>,
    fleet: Vec<Arc<dyn Worker>>,
) -> Harness {
    routed_harness(config, catalog, script, fleet, None)
}

/// [`harness`], routed by `routing` when one is given.
fn routed_harness(
    config: ControllerConfig,
    catalog: StaticCatalog,
    script: Arc<Script>,
    fleet: Vec<Arc<dyn Worker>>,
    routing: Option<Arc<dyn crate::controller::Routing>>,
) -> Harness {
    let path = std::env::temp_dir().join(format!("rk-controller-{}", Uuid::new_v4()));
    let store = Store::open(&path, vec![9u8; 32]).expect("store opens");
    let dir = Arc::new(TempDir(path));
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let catalog: Arc<dyn ToolCatalog> = Arc::new(catalog);
    let mut ctl = Controller::new(store.clone(), fleet, config.clone())
        .with_clock(clock.clone())
        .with_catalog(catalog.clone());
    if let Some(routing) = routing {
        ctl = ctl.with_routing(routing);
    }
    Harness {
        ctl,
        clock,
        script,
        store,
        config,
        catalog,
        dir,
    }
}

/// One prober, `pinch`, that checks the generation is on record first.
fn with_prober(catalog: StaticCatalog) -> (Harness, Arc<Prober>) {
    let script = Arc::new(Script::default());
    let pinch = Arc::new(Prober::new("pinch", &script));
    let h = harness(
        ControllerConfig::default(),
        catalog,
        script,
        vec![pinch.clone() as Arc<dyn Worker>],
    );
    *pinch.store.lock().unwrap() = Some(h.store().clone());
    (h, pinch)
}

async fn evidence(h: &Harness, item: &str, kind: &str) -> Vec<Evidence> {
    h.store()
        .work_evidence_list(item)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .collect()
}

async fn generations(h: &Harness, item: &str) -> Vec<RepairGeneration> {
    evidence(h, item, "repair_generation")
        .await
        .iter()
        .map(|e| serde_json::from_str(&e.reference).unwrap())
        .collect()
}

async fn receipts(h: &Harness, item: &str) -> Vec<(Evidence, OperationProbe)> {
    evidence(h, item, "operation_probe")
        .await
        .into_iter()
        .map(|e| {
            let p = serde_json::from_str(&e.reference).unwrap();
            (e, p)
        })
        .collect()
}

/// Step until `done` holds, at most `limit` ticks.
async fn step_until(h: &Harness, limit: usize, mut done: impl FnMut() -> bool) {
    for _ in 0..limit {
        if done() {
            return;
        }
        h.step().await;
    }
    assert!(done(), "did not happen within {limit} ticks");
}

/// The waiting item parked behind its capability item: (waiting, capability).
async fn parked_on_node(h: &Harness) -> (WorkItemId, WorkItem) {
    h.script
        .push(NODE, report(failure(ErrorSubclass::ToolGap, NEEDS_NODE)));
    let w = h.file_one(draft("w", NODE)).await;
    h.step().await;
    h.step().await;
    assert_eq!(
        h.status(&w).await,
        Status::Blocked(BlockedReason::NeedsTool)
    );
    let caps = h.of_kind(WorkKind::Capability).await;
    assert_eq!(caps.len(), 1, "{caps:#?}");
    (w, caps[0].clone())
}

#[tokio::test]
async fn a_failed_probe_leaves_the_capability_unproven_and_its_waiter_parked_until_a_genuine_fix_proves_it(
) {
    let (h, pinch) = with_prober(StaticCatalog::default());
    pinch.answer(Answer::Outcome(ProbeOutcome::Failed));
    pinch.answer(Answer::Outcome(ProbeOutcome::Ok));
    let (w, cap) = parked_on_node(&h).await;
    assert_eq!(cap.title, "Build tool: node");

    // The build reports done; the probe on pinch, the worker that failed
    // the waiting item, fails: the build is not done and nothing resumes.
    step_until(&h, 12, || pinch.requests().len() == 1).await;
    assert_ne!(h.status(&cap.id).await, Status::Done);
    assert_eq!(
        h.status(&w).await,
        Status::Blocked(BlockedReason::NeedsTool)
    );
    assert_eq!(h.leases(&w).await, 1, "the waiting item was not revived");
    let first = &pinch.requests()[0];
    let action = RegisteredAction::BinaryAvailable {
        binary: "node".into(),
    };
    assert_eq!(first.action, action);
    assert_eq!(first.expected.operation, "binary.available/node");
    assert_eq!(first.expected.executor, "pinch");
    assert_eq!(first.expected.generation, format!("{w}:1"));
    assert_eq!(
        first.expected.inputs_digest,
        probe_inputs_digest(&action, &first.scope, &first.context)
    );
    assert!(first.expected.run.starts_with("probe-"));
    assert!(first.deadline <= Duration::from_secs(30));
    assert!(first.valid());
    let gens = generations(&h, &w).await;
    assert_eq!(gens.len(), 1);
    assert_eq!(gens[0].n, 1);
    assert_eq!(gens[0].cause, "capability");
    assert_eq!(gens[0].source_item, cap.id);
    assert!(gens[0].permission_digest.is_some());
    assert_eq!(
        gens[0].probes,
        vec![PlannedProbe {
            capability: cap.id.clone(),
            expected: first.expected.clone(),
        }]
    );
    let got = receipts(&h, &cap.id).await;
    assert_eq!(got.len(), 1);
    let (row, probe) = &got[0];
    assert_eq!(probe.outcome, ProbeOutcome::Failed);
    assert_eq!(probe.generation, format!("{w}:1"));
    assert_eq!(
        row.hash.as_deref(),
        Some(first.expected.inputs_digest.as_str())
    );
    assert_eq!(row.verified_by.as_deref(), Some("executor:pinch"));
    let errors = evidence(&h, &cap.id, "error").await;
    assert!(
        errors
            .iter()
            .any(|e| e.reference.starts_with("verification/claim_mismatch")
                && e.reference.contains("the operation probe failed")
                && e.reference.contains("binary.available/node")),
        "{errors:#?}"
    );

    // The build is repaired and claims done again: a new generation, whose
    // probe passes. The build closes and the waiting item resumes on it.
    h.drain().await;
    assert_eq!(h.status(&cap.id).await, Status::Done);
    assert_eq!(h.status(&w).await, Status::Done);
    assert_eq!(h.leases(&w).await, 2);
    let resumed = h.script.briefs_for(NODE).remove(1);
    assert!(
        !resumed.required_tools.contains(&"node".to_string()),
        "a host binary is not activated as a RustyKrab tool: {:?}",
        resumed.required_tools
    );
    let requests = pinch.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].expected.generation, format!("{w}:2"));
    assert_ne!(requests[1].expected.run, requests[0].expected.run);
    assert_eq!(
        *pinch.generation_first.lock().unwrap(),
        vec![true, true],
        "each generation was on record before its probe was dispatched"
    );
    let gens = generations(&h, &w).await;
    assert_eq!(gens.iter().map(|g| g.n).collect::<Vec<_>>(), vec![1, 2]);
    let outcomes: Vec<ProbeOutcome> = receipts(&h, &cap.id)
        .await
        .iter()
        .map(|(_, p)| p.outcome)
        .collect();
    assert_eq!(outcomes, vec![ProbeOutcome::Failed, ProbeOutcome::Ok]);
    let done = h
        .events(&cap.id)
        .await
        .into_iter()
        .find(|e| e.to == Some(Status::Done))
        .unwrap();
    assert!(
        done.reason
            .as_deref()
            .unwrap()
            .contains("operation proven: binary.available/node"),
        "{done:?}"
    );
    let revived = h
        .events(&w)
        .await
        .into_iter()
        .find(|e| e.actor == "controller" && e.to == Some(Status::Queued))
        .unwrap();
    assert_eq!(
        revived.reason.as_deref(),
        Some("capability landed and its operation was proven on its executor")
    );
}

#[tokio::test]
async fn a_worker_that_cannot_observe_never_proves_a_capability() {
    // A worker that offers no probe scope (the trait's default): no probe
    // can be asked for, the generation records none, and the build stays
    // unproven however often it claims done.
    let h = Harness::new(&["pinch"]);
    let (w, cap) = parked_on_node(&h).await;
    h.drain().await;
    assert_ne!(h.status(&cap.id).await, Status::Done);
    assert_eq!(
        h.status(&w).await,
        Status::Blocked(BlockedReason::NeedsTool)
    );
    assert_eq!(h.leases(&w).await, 1);
    assert!(
        receipts(&h, &cap.id).await.is_empty(),
        "nothing was observed"
    );
    let gens = generations(&h, &w).await;
    assert!(!gens.is_empty());
    assert!(gens.iter().all(|g| g.probes.is_empty()));
    let errors = evidence(&h, &cap.id, "error").await;
    assert!(
        errors
            .iter()
            .any(|e| e.reference.starts_with("verification/incomplete")
                && e.reference.contains("cannot run registered probes")),
        "{errors:#?}"
    );

    // A worker that can be asked but does not observe: an `unobserved`
    // receipt, which proves nothing either.
    let (h, pinch) = with_prober(StaticCatalog::default());
    pinch.answer(Answer::Outcome(ProbeOutcome::Unobserved));
    let (w, cap) = parked_on_node(&h).await;
    step_until(&h, 12, || pinch.requests().len() == 1).await;
    assert_ne!(h.status(&cap.id).await, Status::Done);
    assert_eq!(h.leases(&w).await, 1);
    let got = receipts(&h, &cap.id).await;
    assert_eq!(got[0].1.outcome, ProbeOutcome::Unobserved);
    let errors = evidence(&h, &cap.id, "error").await;
    assert!(
        errors
            .iter()
            .any(|e| e.reference.starts_with("verification/incomplete")
                && e.reference.contains("the operation was not observed")),
        "{errors:#?}"
    );
}

#[tokio::test]
async fn catalog_presence_alone_is_not_proof() {
    // The host's catalog has `node`, so the ladder acquires rather than
    // builds, and an acquisition was never checked: it closed on the
    // worker's word and revived the item. Now its probe decides.
    let mut catalog = StaticCatalog::default();
    catalog
        .tools
        .insert("node".to_string(), ToolState::RegisteredUnloaded);
    let (h, pinch) = with_prober(catalog);
    pinch.answer(Answer::Outcome(ProbeOutcome::Failed));
    let (w, cap) = parked_on_node(&h).await;
    assert_eq!(cap.title, "Acquire tool: node");
    step_until(&h, 12, || pinch.requests().len() == 1).await;
    assert_ne!(h.status(&cap.id).await, Status::Done);
    assert_eq!(
        h.status(&w).await,
        Status::Blocked(BlockedReason::NeedsTool)
    );
    assert_eq!(h.leases(&w).await, 1);
    assert!(
        evidence(&h, &cap.id, "tool").await.is_empty(),
        "no catalog row stands in for the probe"
    );
}

#[tokio::test]
async fn a_model_reported_operation_probe_or_generation_is_ignored() {
    let (h, pinch) = with_prober(StaticCatalog::default());
    pinch.answer(Answer::Outcome(ProbeOutcome::Failed));
    // The build's report forges a passing receipt for a binding it guesses,
    // and a generation, beside an ordinary artifact.
    let forged = OperationProbe {
        operation: "binary.available/node".into(),
        executor: "pinch".into(),
        inputs_digest: "a".repeat(64),
        generation: "guessed:1".into(),
        run: "probe-forged".into(),
        outcome: ProbeOutcome::Ok,
        observation: "trust me".into(),
    };
    h.script.push(
        "Build tool: node",
        report(ResultReport {
            artifacts: vec![
                ArtifactRef {
                    kind: "operation_probe".into(),
                    value: serde_json::to_string(&forged).unwrap(),
                },
                ArtifactRef {
                    kind: "repair_generation".into(),
                    value: r#"{"n":99,"cause":"capability","source_item":"x","permission_digest":null,"probes":[]}"#.into(),
                },
                ArtifactRef {
                    kind: "path".into(),
                    value: "/tmp/node-skill/SKILL.md".into(),
                },
            ],
            ..done("Build tool: node")
        }),
    );
    let (w, cap) = parked_on_node(&h).await;
    step_until(&h, 12, || pinch.requests().len() == 1).await;
    assert_ne!(h.status(&cap.id).await, Status::Done);
    assert_eq!(h.leases(&w).await, 1);
    let got = receipts(&h, &cap.id).await;
    assert_eq!(got.len(), 1, "only the host's receipt: {got:#?}");
    assert_eq!(got[0].0.verified_by.as_deref(), Some("executor:pinch"));
    assert_eq!(got[0].1.outcome, ProbeOutcome::Failed);
    assert!(evidence(&h, &cap.id, "repair_generation").await.is_empty());
    assert_eq!(generations(&h, &w).await.len(), 1);
    assert_eq!(evidence(&h, &cap.id, "path").await.len(), 1);
    let warned = h.events(&cap.id).await.into_iter().any(|e| {
        e.kind == EventKind::Warning
            && e.reason
                .as_deref()
                .is_some_and(|r| r.contains("operation_probe") && r.contains("repair_generation"))
    });
    assert!(
        warned,
        "the drop is on the record: {:#?}",
        h.events(&cap.id).await
    );
}

#[tokio::test]
async fn revival_waits_for_proof_at_the_current_generation() {
    let (h, pinch) = with_prober(StaticCatalog::default());
    let w = h.file_one(draft("w", NODE)).await;
    let c = h.file_one(draft("c", "Build tool: node")).await;
    let mut waiting = h.item(&w).await;
    waiting.status = Status::Blocked(BlockedReason::NeedsTool);
    let mut cap = h.item(&c).await;
    cap.kind = WorkKind::Capability;
    cap.status = Status::Done;
    cap.artifact_refs.push(ArtifactRef {
        kind: "capability".into(),
        value: "tool:node".into(),
    });
    let edge = rustykrab_core::work::Edge {
        item: w.clone(),
        kind: EdgeKind::Blocks,
        depends_on: c.clone(),
    };
    let snap = crate::graph::Snapshot::new(vec![waiting.clone(), cap.clone()], vec![edge]);
    let proven = || async { h.ctl.revival_proven(&snap, &w).await.unwrap() };

    // A controller-owned lease names the intended executor; no active run
    // is needed for this receipt-validation fixture.
    h.store()
        .work_event_append(&rustykrab_core::work::WorkEvent {
            item: w.clone(),
            kind: EventKind::Lease,
            actor: "worker:pinch".into(),
            from: None,
            to: None,
            reason: None,
            evidence_ref: None,
            origin: None,
            upstream: None,
            at: h.clock.now(),
        })
        .await
        .unwrap();
    let context = h
        .ctl
        .repair_context(&waiting, &h.events(&w).await)
        .await
        .unwrap();
    let scope = pinch.probe_scope(&context).unwrap();
    let binding = h
        .ctl
        .repair_binding(&snap, &w, &h.events(&w).await, &context)
        .await
        .unwrap();
    let permission_digest = Some(h.ctl.access_digest(pinch.as_ref()).await);
    let action = RegisteredAction::BinaryAvailable {
        binary: "node".into(),
    };
    let expected = |n: u64, run: &str| ExpectedOperation {
        operation: action.operation(),
        executor: "pinch".into(),
        inputs_digest: probe_inputs_digest(&action, &scope, &context),
        generation: format!("{w}:{n}"),
        run: run.into(),
    };
    let at = || h.clock.now();
    let generation = |n: u64, run: &str| Evidence {
        item: w.clone(),
        kind: "repair_generation".into(),
        reference: serde_json::to_string(&RepairGeneration {
            n,
            cause: "capability".into(),
            source_item: c.clone(),
            permission_digest: permission_digest.clone(),
            binding: Some(binding.clone()),
            probes: vec![PlannedProbe {
                capability: c.clone(),
                expected: expected(n, run),
            }],
        })
        .unwrap(),
        hash: Some(format!("{w}:{n}")),
        verified_by: Some("controller".into()),
        at: at(),
    };
    let receipt = |n: u64, run: &str, outcome: ProbeOutcome, by: &str| Evidence {
        item: c.clone(),
        kind: "operation_probe".into(),
        reference: serde_json::to_string(
            &expected(n, run).observation(outcome, "scripted observation"),
        )
        .unwrap(),
        hash: Some(expected(n, run).inputs_digest),
        verified_by: Some(by.into()),
        at: at(),
    };
    let add = |e: Evidence| async { h.store().work_evidence_add(e).await.unwrap() };

    add(generation(1, "probe-1")).await;
    assert!(!proven().await, "a generation without a receipt");
    h.clock.advance(TimeDelta::seconds(1));
    add(receipt(1, "probe-1", ProbeOutcome::Ok, "executor:pinch")).await;
    assert!(proven().await, "proof for the current generation");

    // A newer generation: the old proof is stale.
    h.clock.advance(TimeDelta::seconds(1));
    add(generation(2, "probe-2")).await;
    assert!(
        !proven().await,
        "an older generation's proof does not count"
    );
    h.clock.advance(TimeDelta::seconds(1));
    add(receipt(
        2,
        "probe-2",
        ProbeOutcome::Failed,
        "executor:pinch",
    ))
    .await;
    assert!(!proven().await);
    h.clock.advance(TimeDelta::seconds(1));
    add(receipt(
        2,
        "probe-other",
        ProbeOutcome::Ok,
        "executor:pinch",
    ))
    .await;
    assert!(!proven().await, "another invocation's receipt");
    h.clock.advance(TimeDelta::seconds(1));
    add(receipt(2, "probe-2", ProbeOutcome::Ok, "executor:pinch")).await;
    assert!(proven().await);
    h.clock.advance(TimeDelta::seconds(1));
    add(receipt(2, "probe-2", ProbeOutcome::Ok, "result_report")).await;
    assert!(!proven().await, "a later untrusted row fails closed");

    // A need without a registered probe is availability only.
    let mut other = cap.clone();
    other.artifact_refs = vec![ArtifactRef {
        kind: "capability".into(),
        value: "tool:pdf_render".into(),
    }];
    let snap = crate::graph::Snapshot::new(
        vec![waiting, other],
        vec![rustykrab_core::work::Edge {
            item: w.clone(),
            kind: EdgeKind::Blocks,
            depends_on: c.clone(),
        }],
    );
    assert!(h.ctl.revival_proven(&snap, &w).await.unwrap());
}

#[tokio::test]
async fn a_done_claim_waits_for_a_free_slot_on_the_probes_executor() {
    let (h, pinch) = with_prober(StaticCatalog::default());
    pinch.answer(Answer::Outcome(ProbeOutcome::Ok));
    let gate = Arc::new(Notify::new());
    h.script.push(
        "Build tool: node",
        Step::Wait(gate.clone(), Box::new(done("Build tool: node"))),
    );
    let (w, cap) = parked_on_node(&h).await;
    // The build runs; while it runs, pinch's one slot is taken (as by
    // another run or probe), so its done claim is not judged.
    for _ in 0..6 {
        if h.status(&cap.id).await == Status::Running {
            break;
        }
        h.step().await;
    }
    assert_eq!(h.status(&cap.id).await, Status::Running);
    h.ctl.state().probing.insert("pinch".to_string(), 1);
    gate.notify_one();
    {
        let pinch: &dyn Worker = pinch.as_ref();
        assert!(!h.ctl.has_free_slot(pinch), "a probe holds a slot");
    }
    for _ in 0..4 {
        h.step().await;
    }
    assert!(
        pinch.requests().is_empty(),
        "no probe while the slot is held"
    );
    assert_eq!(h.status(&cap.id).await, Status::Running);
    assert!(generations(&h, &w).await.is_empty());
    assert!(
        h.ctl.state().finished.contains_key(&cap.id),
        "the claim waits"
    );

    h.ctl.state().probing.clear();
    h.drain().await;
    assert_eq!(pinch.requests().len(), 1);
    assert_eq!(h.status(&cap.id).await, Status::Done);
    assert_eq!(h.status(&w).await, Status::Done);
    assert!(
        h.ctl.state().probing.is_empty(),
        "the probe gave its slot back"
    );
}

#[tokio::test]
async fn a_probe_that_never_answers_is_bounded_by_its_deadline() {
    let script = Arc::new(Script::default());
    let pinch = Arc::new(Prober::new("pinch", &script));
    let h = harness(
        ControllerConfig {
            probe_deadline: Duration::from_millis(50),
            ..ControllerConfig::default()
        },
        StaticCatalog::default(),
        script,
        vec![pinch.clone() as Arc<dyn Worker>],
    );
    pinch.answer(Answer::Hang);
    let (w, cap) = parked_on_node(&h).await;
    step_until(&h, 12, || pinch.requests().len() == 1).await;
    assert_eq!(pinch.requests()[0].deadline, Duration::from_millis(50));
    assert_ne!(h.status(&cap.id).await, Status::Done);
    assert_eq!(h.leases(&w).await, 1);
    assert!(
        receipts(&h, &cap.id).await.is_empty(),
        "no receipt, none stored"
    );
    let errors = evidence(&h, &cap.id, "error").await;
    assert!(
        errors.iter().any(|e| e
            .reference
            .contains("returned no receipt within its deadline")),
        "{errors:#?}"
    );
    assert!(h.ctl.state().probing.is_empty());

    // A deadline configured past the cap is sent at the cap.
    let script = Arc::new(Script::default());
    let pinch = Arc::new(Prober::new("pinch", &script));
    let h = harness(
        ControllerConfig {
            probe_deadline: Duration::from_secs(600),
            ..ControllerConfig::default()
        },
        StaticCatalog::default(),
        script,
        vec![pinch.clone() as Arc<dyn Worker>],
    );
    pinch.answer(Answer::Outcome(ProbeOutcome::Ok));
    parked_on_node(&h).await;
    step_until(&h, 12, || pinch.requests().len() == 1).await;
    assert_eq!(pinch.requests()[0].deadline, Duration::from_secs(30));
}

#[tokio::test]
async fn a_repository_scoped_waiter_is_probed_in_a_fresh_worktree_that_is_removed_after() {
    use crate::workspace::tests::{run_git, Repo};
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let script = Arc::new(Script::default());
    let pinch = Arc::new(Prober::new("pinch", &script));
    let h = harness(
        ControllerConfig {
            worktree_root: Some(root.path().to_path_buf()),
            ..ControllerConfig::default()
        },
        StaticCatalog::default(),
        script,
        vec![pinch.clone() as Arc<dyn Worker>],
    );
    pinch.answer(Answer::Outcome(ProbeOutcome::Ok));
    h.script
        .push(NODE, report(failure(ErrorSubclass::ToolGap, NEEDS_NODE)));
    let mut d = draft("w", NODE);
    d.writable_resources = vec![format!("repo:{}", repo.path().display())];
    let w = h.file_one(d).await;
    h.drain().await;
    let cap = h.of_kind(WorkKind::Capability).await.remove(0);
    assert_eq!(h.status(&cap.id).await, Status::Done);
    assert!(
        h.leases(&w).await >= 2,
        "proven, so the waiting item ran again"
    );
    let dirs = pinch.dirs.lock().unwrap().clone();
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    let (dir, existed) = &dirs[0];
    let dir = dir
        .as_ref()
        .expect("a repository-scoped waiter's probe has a directory");
    assert!(dir.starts_with(root.path()), "{dir:?}");
    assert!(*existed, "the directory existed while the probe ran");
    assert!(!dir.exists(), "and is gone after it");
    let branches = run_git(repo.path(), &["branch", "--list", "rustykrab/*"]);
    let probe_run = pinch.requests()[0].expected.run.clone();
    assert!(
        !branches.contains(&crate::workspace::Workspace::slug_of_run(&probe_run)),
        "the probe's branch is deleted: {branches}"
    );
}

#[tokio::test]
async fn a_need_without_a_registered_probe_stays_availability_only() {
    let mut catalog = StaticCatalog::default();
    catalog
        .tools
        .insert("pdf_render".to_string(), ToolState::RegisteredUnloaded);
    let script = Arc::new(Script::default());
    let mut worker = Prober::new("pinch", &script);
    worker.caps.tools.push("pdf_render".into());
    let pinch = Arc::new(worker);
    let h = harness(
        ControllerConfig::default(),
        catalog,
        script,
        vec![pinch.clone()],
    );
    h.script.push(
        "Render the invoice",
        report(failure(ErrorSubclass::ToolGap, "needs tool: pdf_render")),
    );
    let w = h.file_one(draft("w", "Render the invoice")).await;
    h.drain().await;
    let cap = h.of_kind(WorkKind::Capability).await.remove(0);
    assert_eq!(h.status(&cap.id).await, Status::Done);
    assert_eq!(h.status(&w).await, Status::Done);
    assert!(pinch.requests().is_empty(), "nothing to probe");
    assert!(generations(&h, &w).await.is_empty());
    let done = h
        .events(&cap.id)
        .await
        .into_iter()
        .find(|e| e.to == Some(Status::Done))
        .unwrap();
    assert!(
        done.reason
            .as_deref()
            .unwrap()
            .contains("availability only"),
        "{done:?}"
    );
    let revived = h
        .events(&w)
        .await
        .into_iter()
        .find(|e| e.actor == "controller" && e.to == Some(Status::Queued))
        .unwrap();
    assert!(revived
        .reason
        .as_deref()
        .unwrap()
        .contains("availability only"));
}

// ── a RustyKrab tool a native coding agent cannot host ───────────────────

const RESOURCES: &str = "List the fleet's resources";

/// Cost tiers by worker name, so the native agent takes the first run.
struct Tiers(HashMap<String, u32>);

impl crate::controller::Routing for Tiers {
    fn cost_tier(&self, worker: &dyn Worker) -> u32 {
        self.0.get(worker.name()).copied().unwrap_or(9)
    }
}

/// A native Codex worker first in line, and `local` beside it.
fn native_first(local: Option<Prober>) -> Harness {
    let mut catalog = StaticCatalog::default();
    catalog
        .tools
        .insert("work_resources".to_string(), ToolState::RegisteredUnloaded);
    let script = Arc::new(Script::default());
    let mut fleet: Vec<Arc<dyn Worker>> = vec![Arc::new(Prober::native("codex", &script))];
    let mut tiers = HashMap::from([("codex".to_string(), 0)]);
    if let Some(mut local) = local {
        local.run.script = Arc::clone(&script);
        tiers.insert(local.run.name.clone(), 1);
        fleet.push(Arc::new(local));
    }
    routed_harness(
        ControllerConfig::default(),
        catalog,
        script,
        fleet,
        Some(Arc::new(Tiers(tiers))),
    )
}

#[tokio::test]
async fn a_native_agent_without_a_rustykrab_tool_is_routed_not_given_a_capability_item() {
    let detached = Arc::new(Script::default());
    let mut local = Prober::new("pinch", &detached);
    local.caps.tools.push("work_resources".to_string());
    let h = native_first(Some(local));
    h.script.push(
        RESOURCES,
        report(failure(
            ErrorSubclass::ToolGap,
            "needs tool: work_resources",
        )),
    );
    let w = h.file_one(draft("w", RESOURCES)).await;
    h.drain().await;
    assert_eq!(h.status(&w).await, Status::Done);
    assert!(
        h.of_kind(WorkKind::Capability).await.is_empty(),
        "no capability item is filed for a tool the native agent cannot host"
    );
    let runs: Vec<(String, Brief)> = h
        .script
        .briefs()
        .into_iter()
        .filter(|(_, b)| b.title == RESOURCES)
        .collect();
    assert_eq!(
        runs.iter().map(|(w, _)| w.as_str()).collect::<Vec<_>>(),
        vec!["codex", "pinch"]
    );
    assert!(runs[1]
        .1
        .required_tools
        .contains(&"work_resources".to_string()));
    let rung = h
        .events(&w)
        .await
        .iter()
        .filter_map(decode_rung)
        .find(|r| r.rung == Rung::SwitchWorker)
        .unwrap();
    assert!(
        rung.outcome
            .contains("routed to a worker whose tool ceiling has `work_resources`"),
        "{rung:?}"
    );
}

#[tokio::test]
async fn a_native_agent_without_a_rustykrab_tool_and_no_worker_with_it_surfaces_once() {
    let detached = Arc::new(Script::default());
    // A local worker without the tool cannot take it either.
    let h = native_first(Some(Prober::new("pinch", &detached)));
    h.script.push(
        RESOURCES,
        report(failure(
            ErrorSubclass::ToolGap,
            "needs tool: work_resources",
        )),
    );
    let w = h.file_one(draft("w", RESOURCES)).await;
    h.drain().await;
    h.drain().await;
    assert_eq!(
        h.status(&w).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    assert!(h.of_kind(WorkKind::Capability).await.is_empty());
    assert_eq!(h.leases(&w).await, 1, "one run, no loop");
    let asked = h
        .store()
        .questions_list(&rustykrab_store::QuestionFilter {
            item: Some(w.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(asked.len(), 1, "{asked:#?}");
    assert!(asked[0].text.contains("`work_resources`"), "{asked:?}");
    assert!(asked[0].text.contains("codex"), "{asked:?}");
}

/// A completed capability and its parked original item, with one real
/// controller lease event but no running future/slot. Receipt producer scripted;
/// store, restart reconstruction, generation and both gates are production code.
async fn revival_fixture() -> (Harness, Arc<Prober>, crate::graph::Snapshot, String) {
    let (h, pinch) = with_prober(StaticCatalog::default());
    let w = h.file_one(draft("w", NODE)).await;
    let c = h.file_one(draft("c", "Build tool: node")).await;
    let mut waiting = h.item(&w).await;
    waiting.status = Status::Blocked(BlockedReason::NeedsTool);
    waiting.held_by = Some("independent-owner-hold".into());
    let mut cap = h.item(&c).await;
    cap.kind = WorkKind::Capability;
    cap.status = Status::Done;
    cap.artifact_refs.push(ArtifactRef {
        kind: "capability".into(),
        value: "tool:node".into(),
    });
    h.store()
        .work_event_append(&rustykrab_core::work::WorkEvent {
            item: w.clone(),
            kind: EventKind::Lease,
            actor: "worker:pinch".into(),
            from: None,
            to: None,
            reason: None,
            evidence_ref: None,
            origin: None,
            upstream: None,
            at: h.clock.now(),
        })
        .await
        .unwrap();
    let snap = crate::graph::Snapshot::new(
        vec![waiting, cap],
        vec![rustykrab_core::work::Edge {
            item: w.clone(),
            kind: EdgeKind::Blocks,
            depends_on: c,
        }],
    );
    (h, pinch, snap, w)
}

#[tokio::test]
async fn revival_rebinds_same_path_policy_once_and_does_not_storm_on_unobserved_receipts() {
    let (h, pinch, snap, w) = revival_fixture().await;
    pinch.answer(Answer::Outcome(ProbeOutcome::Ok));
    assert!(h.ctl.revival_proven(&snap, &w).await.unwrap());
    assert_eq!(generations(&h, &w).await.len(), 1);
    // A policy change with the same path/ceiling invalidates the old success.
    pinch.policy.store(1, std::sync::atomic::Ordering::SeqCst);
    h.ctl.state().probing.insert("pinch".into(), 1);
    assert!(!h.ctl.revival_proven(&snap, &w).await.unwrap());
    assert_eq!(
        generations(&h, &w).await.len(),
        1,
        "busy executor is not oversubscribed"
    );
    h.ctl.state().probing.remove("pinch");
    pinch.answer(Answer::Outcome(ProbeOutcome::Unobserved));
    assert!(!h.ctl.revival_proven(&snap, &w).await.unwrap());
    assert!(!h.ctl.revival_proven(&snap, &w).await.unwrap());
    let gens = generations(&h, &w).await;
    assert_eq!(
        gens.len(),
        2,
        "same context on later ticks creates no more generations"
    );
    assert_eq!(pinch.requests().len(), 2);
    assert_eq!(
        gens[0]
            .binding
            .as_ref()
            .unwrap()
            .scope
            .as_ref()
            .unwrap()
            .path,
        gens[1]
            .binding
            .as_ref()
            .unwrap()
            .scope
            .as_ref()
            .unwrap()
            .path
    );
    assert_ne!(gens[0].binding, gens[1].binding);
    assert_eq!(
        snap.item(&w).unwrap().held_by.as_deref(),
        Some("independent-owner-hold")
    );
    assert!(evidence(&h, &w, "budget_grant").await.is_empty());
}

#[tokio::test]
async fn revival_rebinds_path_requirements_executor_and_legacy_after_restart() {
    let (h, pinch, mut snap, w) = revival_fixture().await;
    pinch.answer(Answer::Outcome(ProbeOutcome::Ok));
    assert!(h.ctl.revival_proven(&snap, &w).await.unwrap());
    let mut item = snap.item(&w).unwrap().clone();
    item.required_tools = vec!["work_resources".into()];
    let cap = snap.items().iter().find(|i| i.id != w).unwrap().clone();
    let edges = snap.edges_held_by(&w).cloned().collect();
    snap = crate::graph::Snapshot::new(vec![item, cap], edges);
    pinch.answer(Answer::Outcome(ProbeOutcome::Ok));
    assert!(h.ctl.revival_proven(&snap, &w).await.unwrap());
    assert_eq!(pinch.requests()[0].scope.path, ExecutionPath::Native);
    assert_eq!(pinch.requests()[1].scope.path, ExecutionPath::Host);
    // Reconstruct a controller over the same real store and a changed policy.
    let ctl = Controller::new(
        h.store().clone(),
        vec![pinch.clone() as Arc<dyn Worker>],
        h.config.clone(),
    )
    .with_clock(h.clock.clone())
    .with_catalog(h.catalog.clone());
    pinch.policy.store(2, std::sync::atomic::Ordering::SeqCst);
    pinch.answer(Answer::Outcome(ProbeOutcome::Ok));
    assert!(ctl.revival_proven(&snap, &w).await.unwrap());
    assert_eq!(generations(&h, &w).await.len(), 3);
    // Deserialize the legacy wire row (without binding), even with a prior
    // success. It is not current proof; refresh once, then stay on that generation.
    let mut legacy = serde_json::to_value(generations(&h, &w).await.last().unwrap()).unwrap();
    legacy["n"] = serde_json::json!(4);
    legacy.as_object_mut().unwrap().remove("binding");
    h.store()
        .work_evidence_add(Evidence {
            item: w.clone(),
            kind: "repair_generation".into(),
            reference: legacy.to_string(),
            hash: Some(format!("{w}:4")),
            verified_by: Some("controller".into()),
            at: h.clock.now(),
        })
        .await
        .unwrap();
    pinch.answer(Answer::Outcome(ProbeOutcome::Unobserved));
    assert!(!ctl.revival_proven(&snap, &w).await.unwrap());
    assert!(!ctl.revival_proven(&snap, &w).await.unwrap());
    assert_eq!(generations(&h, &w).await.last().unwrap().n, 5);
    h.store()
        .work_event_append(&rustykrab_core::work::WorkEvent {
            item: w.clone(),
            kind: EventKind::Lease,
            actor: "worker:removed-worker".into(),
            from: None,
            to: None,
            reason: None,
            evidence_ref: None,
            origin: None,
            upstream: None,
            at: h.clock.now(),
        })
        .await
        .unwrap();
    assert!(!ctl.revival_proven(&snap, &w).await.unwrap());
    assert!(!ctl.revival_proven(&snap, &w).await.unwrap());
    assert_eq!(
        generations(&h, &w).await.last().unwrap().n,
        6,
        "an unavailable executor is recorded once, not on every tick"
    );
}

#[test]
fn recovery_requirements_share_post_activatable_tools_with_the_real_brief() {
    let (h, _) = with_prober(StaticCatalog::default());
    let mut catalog = StaticCatalog::default();
    catalog
        .tools
        .insert("work_resources".into(), ToolState::Loaded);
    let ctl = h.ctl.with_catalog(Arc::new(catalog));
    let history = ctl.activatable(super::super::load::History {
        activate: vec!["node".into(), "work_resources".into()],
        ..Default::default()
    });
    let item: WorkItem = serde_json::from_value(serde_json::json!({
        "id":"fixture", "kind":"task", "title":"fixture", "objective":"fixture", "done_when":"fixture",
        "status":{"status":"ready"}, "created_at":"2026-10-15T00:00:00Z", "updated_at":"2026-10-15T00:00:00Z"
    })).unwrap();
    let brief =
        super::super::brief::brief_for(&item, vec![], vec![], vec![], &history, item.budget);
    let requirements = super::super::brief::effective_requirements(&item, &history);
    assert_eq!(
        requirements,
        crate::capability_proof::EffectiveRequirements::from_brief(&brief)
    );
    assert_eq!(requirements.required_tools, vec!["work_resources"]);
    assert!(requirements.uses_host());
}

#[tokio::test]
async fn a_native_path_refusal_is_visible_once_per_generation_without_questions_or_retries() {
    let (h, pinch, mut snap, w) = revival_fixture().await;
    let mut item = snap.item(&w).unwrap().clone();
    item.required_tools = vec!["work_resources".into()];
    let cap = snap.items().iter().find(|i| i.id != w).unwrap().clone();
    let edges: Vec<_> = snap.edges_held_by(&w).cloned().collect();
    snap = crate::graph::Snapshot::new(vec![item.clone(), cap.clone()], edges.clone());
    pinch.answer(Answer::Outcome(ProbeOutcome::Ok));
    assert!(h.ctl.revival_proven(&snap, &w).await.unwrap());
    item.required_tools.clear();
    snap = crate::graph::Snapshot::new(vec![item, cap], edges);
    pinch.answer(Answer::Outcome(ProbeOutcome::Unobserved));
    for _ in 0..3 {
        assert!(!h.ctl.revival_proven(&snap, &w).await.unwrap());
    }
    let events = h.events(&w).await;
    let warnings: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::Warning)
        .collect();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].actor, "controller");
    assert_eq!(
        warnings[0].evidence_ref.as_deref(),
        Some(format!("capability_revival_blocked:{w}:2").as_str())
    );
    let reason = warnings[0].reason.as_deref().unwrap();
    assert!(
        reason.contains("Native") && reason.contains("pinch") && reason.contains("unobserved"),
        "{reason}"
    );
    assert_eq!(
        pinch.requests().len(),
        2,
        "later checks neither re-probe nor duplicate warnings"
    );
    assert_eq!(generations(&h, &w).await.len(), 2);
    assert_eq!(
        snap.item(&w).unwrap().held_by.as_deref(),
        Some("independent-owner-hold")
    );
    assert!(
        events.iter().all(|e| e.kind != EventKind::Rung),
        "no invented original-run ladder attempt"
    );
    assert!(evidence(&h, &w, "budget_grant").await.is_empty());
}

/// A real parked item and completed capability in SQLite, with no process
/// executing the original. Host-owned fixture transitions establish the setup;
/// actual ticks below allocate, persist, reconstruct and revive production work.
async fn tick_revival_fixture(improvements: u32) -> (Harness, Arc<Prober>, String) {
    let (h, pinch) = with_prober(StaticCatalog::default());
    h.ctl.set_draining(true);
    let mut w = draft("w", NODE);
    w.budget = Some(Budget {
        rungs: RungBudgets {
            improvements,
            ..Default::default()
        },
        ..Default::default()
    });
    let w = h.file_one(w).await;
    h.store()
        .work_transition(
            &w,
            None,
            Status::Blocked(BlockedReason::NeedsTool),
            "controller",
            Some("waiting on node proof"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let mut cap = draft("cap", "Build tool: node");
    cap.kind = Some(WorkKind::Capability);
    cap.artifact_refs = vec![ArtifactRef {
        kind: "capability".into(),
        value: "tool:node".into(),
    }];
    let cap = h.file_one(cap).await;
    h.store()
        .work_transition(
            &cap,
            None,
            Status::Done,
            "controller",
            Some("fixture capability landed"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    h.store()
        .work_apply(vec![rustykrab_store::WorkOp::AddEdge(
            rustykrab_core::work::Edge {
                item: w.clone(),
                kind: EdgeKind::Blocks,
                depends_on: cap,
            },
        )])
        .await
        .unwrap();
    h.store()
        .work_event_append(&WorkEvent {
            item: w.clone(),
            kind: EventKind::Lease,
            actor: "worker:pinch".into(),
            from: None,
            to: None,
            reason: None,
            evidence_ref: None,
            origin: None,
            upstream: None,
            at: h.clock.now(),
        })
        .await
        .unwrap();
    pinch.answer(Answer::Outcome(ProbeOutcome::Ok));
    assert!(h
        .ctl
        .revival_proven(&h.ctl.load().await.unwrap(), &w)
        .await
        .unwrap());
    pinch.policy.store(1, std::sync::atomic::Ordering::SeqCst);
    pinch.answer(Answer::Outcome(ProbeOutcome::Unobserved));
    (h, pinch, w)
}

#[tokio::test]
async fn terminal_revival_allocates_one_owned_investigation_across_ticks_and_restart_then_reprobes_a_verified_repair(
) {
    let (h, pinch, w) = tick_revival_fixture(1).await;
    h.tick().await;
    let diagnoses = h.of_kind(WorkKind::Internal).await;
    assert_eq!(diagnoses.len(), 1, "{:?}", h.events(&w).await);
    let diagnosis = &diagnoses[0];
    let fp = diagnosis
        .artifact_refs
        .iter()
        .find(|r| r.kind == crate::investigate::FAILURE_FINGERPRINT)
        .unwrap();
    let history = crate::investigate::failure_history(h.store(), &fp.value, &Default::default())
        .await
        .unwrap()
        .expect("the allocated investigator can retrieve the actual refusal by its fingerprint");
    assert_eq!(history.occurrences.len(), 1);
    let observed = evidence(&h, &w, "error").await;
    assert_eq!(observed.len(), 1);
    assert_eq!(
        observed[0].verified_by.as_deref(),
        Some("capability_revival")
    );
    assert!(
        evidence(&h, &w, "attempt_failure").await.is_empty(),
        "a probe is not an original execution"
    );
    assert_eq!(
        h.status(&w).await,
        Status::Blocked(BlockedReason::NeedsTool)
    );
    for _ in 0..3 {
        h.tick().await;
    }
    assert_eq!(h.of_kind(WorkKind::Internal).await.len(), 1);
    assert_eq!(pinch.requests().len(), 2);
    let ctl = Controller::new(
        h.store().clone(),
        vec![pinch.clone() as Arc<dyn Worker>],
        h.config.clone(),
    )
    .with_clock(h.clock.clone())
    .with_catalog(h.catalog.clone());
    ctl.set_draining(true);
    for _ in 0..2 {
        ControlHandle::tick(&ctl).await.unwrap();
    }
    assert_eq!(h.of_kind(WorkKind::Internal).await.len(), 1);
    assert_eq!(
        h.status(&w).await,
        Status::Blocked(BlockedReason::NeedsTool)
    );
    assert_eq!(generations(&h, &w).await.len(), 2);
    let actions: Vec<_> = h
        .events(&w)
        .await
        .iter()
        .filter_map(super::super::load::decode_rung)
        .filter(|r| r.outcome.starts_with("capability revival action "))
        .collect();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].rung, Rung::Improve);
    assert!(!actions[0].outcome.contains(super::super::load::SWITCHED));
    assert!(
        crate::investigate::resolution_item(&actions[0].outcome).is_none(),
        "generic resolution recovery must not requeue the original"
    );
    assert!(h
        .store()
        .questions_list(&Default::default())
        .await
        .unwrap()
        .is_empty());

    // Merely claiming Done cannot refresh or release the original.
    h.store()
        .work_transition(
            &diagnosis.id,
            None,
            Status::Done,
            "controller",
            Some("fixture landed claim"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    ControlHandle::tick(&ctl).await.unwrap();
    assert_eq!(pinch.requests().len(), 2);
    assert_eq!(
        h.status(&w).await,
        Status::Blocked(BlockedReason::NeedsTool)
    );
    // A fixture host-verifier receipt names an actual temporary Git commit.
    let repo = crate::workspace::tests::Repo::new();
    let commit = crate::workspace::tests::run_git(repo.path(), &["rev-parse", "HEAD"]);
    h.store()
        .work_evidence_add(Evidence {
            item: diagnosis.id.clone(),
            kind: "commit".into(),
            reference: commit,
            hash: None,
            verified_by: Some("git".into()),
            at: h.clock.now(),
        })
        .await
        .unwrap();
    let mut b = super::super::batch::Batch::new(ctl.load().await.unwrap(), h.clock.now());
    assert!(ctl
        .recover_resolution_waiters(&mut b)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        b.status(&w),
        Some(Status::Blocked(BlockedReason::NeedsTool))
    );
    pinch.answer(Answer::Outcome(ProbeOutcome::Ok));
    ControlHandle::tick(&ctl).await.unwrap();
    assert_eq!(
        generations(&h, &w).await.len(),
        3,
        "one fresh proof after verified landing, same policy"
    );
    assert_eq!(pinch.requests().len(), 3);
    assert!(matches!(h.status(&w).await, Status::Ready | Status::Queued));
    assert!(
        h.script.briefs_for(NODE).is_empty(),
        "draining test never runs the original or invents spend"
    );
    assert!(evidence(&h, &w, "budget_grant").await.is_empty());
}

#[tokio::test]
async fn exhausted_revival_diagnosis_notifies_once_without_a_question_or_proof_bypass() {
    let (h, pinch, w) = tick_revival_fixture(0).await;
    let first = h.tick().await;
    assert!(
        first.notices > 0,
        "allocation failure is visible through the existing owner outbox"
    );
    assert!(h.of_kind(WorkKind::Internal).await.is_empty());
    for _ in 0..3 {
        assert_eq!(h.tick().await.notices, 0);
    }
    assert_eq!(
        h.status(&w).await,
        Status::Blocked(BlockedReason::NeedsTool)
    );
    assert_eq!(pinch.requests().len(), 2);
    assert!(h
        .store()
        .questions_list(&Default::default())
        .await
        .unwrap()
        .is_empty());
    let ctl = Controller::new(
        h.store().clone(),
        vec![pinch.clone() as Arc<dyn Worker>],
        h.config.clone(),
    )
    .with_clock(h.clock.clone())
    .with_catalog(h.catalog.clone());
    ctl.set_draining(true);
    assert_eq!(ControlHandle::tick(&ctl).await.unwrap().notices, 0);
    let actions: Vec<_> = h
        .events(&w)
        .await
        .iter()
        .filter_map(super::super::load::decode_rung)
        .filter(|r| r.outcome.starts_with("capability revival action "))
        .collect();
    assert_eq!(actions.len(), 1);
    assert!(actions[0].outcome.contains("no remaining Improve rung"));
}

#[tokio::test]
async fn unrelated_landed_internal_work_cannot_mint_a_revival_generation() {
    let (h, pinch, snap, w) = revival_fixture().await;
    let mut unrelated = h
        .item(&h.file_one(draft("other", "unrelated work")).await)
        .await;
    unrelated.kind = WorkKind::Internal;
    unrelated.status = Status::Done;
    h.store()
        .work_evidence_add(Evidence {
            item: unrelated.id.clone(),
            kind: "commit".into(),
            reference: "fixture-unrelated-commit".into(),
            hash: None,
            verified_by: Some("git".into()),
            at: h.clock.now(),
        })
        .await
        .unwrap();
    let mut items = snap.items().to_vec();
    items.push(unrelated.clone());
    let mut edges = snap.edges().to_vec();
    edges.push(rustykrab_core::work::Edge {
        item: w.clone(),
        kind: EdgeKind::Blocks,
        depends_on: unrelated.id,
    });
    let with_unrelated = crate::graph::Snapshot::new(items, edges);
    pinch.answer(Answer::Outcome(ProbeOutcome::Ok));
    assert!(h.ctl.revival_proven(&snap, &w).await.unwrap());
    assert!(h.ctl.revival_proven(&with_unrelated, &w).await.unwrap());
    assert_eq!(generations(&h, &w).await.len(), 1);
    assert_eq!(pinch.requests().len(), 1);
}

#[tokio::test]
async fn rejected_revival_investigation_notifies_once_and_never_creates_an_answer_bypass() {
    let (h, pinch, w) = tick_revival_fixture(1).await;
    let mut config = h.config.clone();
    config.caps.max_depth = 0;
    let ctl = Controller::new(
        h.store().clone(),
        vec![pinch.clone() as Arc<dyn Worker>],
        config,
    )
    .with_clock(h.clock.clone())
    .with_catalog(h.catalog.clone());
    ctl.set_draining(true);
    assert!(ControlHandle::tick(&ctl).await.unwrap().notices > 0);
    assert_eq!(ControlHandle::tick(&ctl).await.unwrap().notices, 0);
    assert!(h.of_kind(WorkKind::Internal).await.is_empty());
    assert!(h
        .store()
        .questions_list(&Default::default())
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        h.status(&w).await,
        Status::Blocked(BlockedReason::NeedsTool)
    );
    let actions: Vec<_> = h
        .events(&w)
        .await
        .iter()
        .filter_map(super::super::load::decode_rung)
        .filter(|r| r.outcome.starts_with("capability revival action "))
        .collect();
    assert_eq!(actions.len(), 1);
    assert!(
        actions[0].outcome.contains("depth limit"),
        "{}",
        actions[0].outcome
    );
}

#[tokio::test]
async fn a_new_daemon_build_requires_fresh_proof_without_claiming_the_repair_effective() {
    let (h, pinch, snap, w) = revival_fixture().await;
    pinch.answer(Answer::Outcome(ProbeOutcome::Ok));
    assert!(h.ctl.revival_proven(&snap, &w).await.unwrap());
    let mut config = h.config.clone();
    config.build = Some(crate::investigate::BuildId {
        commit: "feed1234".into(),
        dirty: false,
    });
    let ctl = Controller::new(
        h.store().clone(),
        vec![pinch.clone() as Arc<dyn Worker>],
        config,
    )
    .with_clock(h.clock.clone())
    .with_catalog(h.catalog.clone());
    pinch.answer(Answer::Outcome(ProbeOutcome::Unobserved));
    assert!(
        !ctl.revival_proven(&snap, &w).await.unwrap(),
        "a build change is not passing proof"
    );
    assert!(!ctl.revival_proven(&snap, &w).await.unwrap());
    assert_eq!(generations(&h, &w).await.len(), 2);
    assert_eq!(
        pinch.requests().len(),
        2,
        "same deployed identity does not generate on each tick"
    );
    assert_eq!(
        pinch.requests()[0].scope,
        pinch.requests()[1].scope,
        "policy/path stayed the same"
    );
    assert!(generations(&h, &w).await[1]
        .binding
        .as_ref()
        .unwrap()
        .daemon_build
        .contains("feed1234"));
}
