use super::*;
use crate::controller::{
    tests::{draft, Harness},
    Clock, ControllerConfig,
};
use crate::handle::ControlHandle;
use crate::worker::{Brief, Worker, WorkerCapabilities};
use crate::workspace::tests::{run_git, Repo};
use async_trait::async_trait;
use rustykrab_core::work::{ErrorSubclass, ResultReport, Rung, Status, WorkerKind};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

struct ObservedWorker {
    name: &'static str,
    kind: WorkerKind,
    pulse: AtomicU64,
    seen: Mutex<Vec<Brief>>,
    fail_loop: bool,
    complete: AtomicBool,
    model: &'static str,
}
impl ObservedWorker {
    fn new(name: &'static str, kind: WorkerKind, fail_loop: bool) -> Arc<Self> {
        Self::with_model(
            name,
            kind,
            fail_loop,
            if kind == WorkerKind::Local {
                "same-local-model"
            } else {
                "native-model"
            },
        )
    }
    fn with_model(
        name: &'static str,
        kind: WorkerKind,
        fail_loop: bool,
        model: &'static str,
    ) -> Arc<Self> {
        Arc::new(Self {
            name,
            kind,
            pulse: AtomicU64::new(0),
            seen: Mutex::new(vec![]),
            fail_loop,
            complete: AtomicBool::new(false),
            model,
        })
    }
}
#[async_trait]
impl Worker for ObservedWorker {
    fn name(&self) -> &str {
        self.name
    }
    fn kind(&self) -> WorkerKind {
        self.kind
    }
    fn capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities {
            models: vec![self.model.into()],
            ..Default::default()
        }
    }
    fn activity(&self, run: &str) -> Option<u64> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .any(|b| b.run.as_deref() == Some(run))
            .then(|| self.pulse.load(Ordering::Relaxed))
    }
    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        let title = brief.title.clone();
        self.seen.lock().unwrap().push(brief);
        if self.fail_loop {
            return Err(crate::worker::RunFailure::Model {
                problem: ProviderProblem::Loop,
                detail: "fixture repeatedly inspects without changing result".into(),
            }
            .into_error());
        }
        if self.complete.load(Ordering::Relaxed) {
            return Ok(crate::controller::tests::done(&title));
        }
        std::future::pending().await
    }
}

async fn running() -> (Harness, Arc<ObservedWorker>, String) {
    let worker = ObservedWorker::new("local", WorkerKind::Local, false);
    let h = Harness::with_fleet(
        ControllerConfig {
            lease_ttl_seconds: 10,
            ..Default::default()
        },
        vec![worker.clone()],
    );
    let mut d = draft("job", "inspect repository");
    d.budget = Some(crate::controller::tests::no_rungs());
    let id = h.file_one(d).await;
    assert_eq!(h.tick().await.leased, vec![id.clone()]);
    h.ctl.wait_for_runs(Duration::from_millis(5)).await;
    (h, worker, id)
}

#[tokio::test]
async fn actual_activity_renews_multiple_leases_but_silence_still_times_out() {
    let (h, worker, id) = running().await;
    let before = h.store().work_lease_get(&id).await.unwrap().unwrap();
    for n in 1..=8 {
        h.clock.advance(TimeDelta::seconds(8));
        worker.pulse.store(n, Ordering::Relaxed);
        h.tick().await;
        assert_eq!(h.status(&id).await, Status::Running);
        assert!(crate::ladder::last_error(&h.events(&id).await).is_none());
    }
    let after = h.store().work_lease_get(&id).await.unwrap().unwrap();
    assert_eq!(before.since, after.since, "attempt identity is retained");
    assert!(after.heartbeat_at >= before.heartbeat_at);
    h.clock.advance(TimeDelta::seconds(11));
    h.tick().await;
    assert_ne!(h.status(&id).await, Status::Running);
    assert_eq!(
        crate::ladder::last_error(&h.events(&id).await)
            .unwrap()
            .subclass,
        ErrorSubclass::Timeout
    );
}

#[tokio::test]
async fn same_model_under_another_name_does_not_spend_the_switch() {
    let local = ObservedWorker::new("local", WorkerKind::Local, true);
    let renamed = ObservedWorker::new("renamed-local", WorkerKind::Local, false);
    let native = ObservedWorker::new("native", WorkerKind::Codex, false);
    let h = Harness::with_fleet(
        ControllerConfig::default(),
        vec![local.clone(), renamed.clone(), native.clone()],
    );
    h.ctl
        .registry()
        .register(renamed.clone(), serde_json::json!({}), Some(1))
        .await
        .unwrap();
    let id = h.file_one(draft("job", "resolve conflict")).await;
    for _ in 0..5 {
        h.step().await;
    }
    assert_eq!(local.seen.lock().unwrap().len(), 1);
    assert!(renamed.seen.lock().unwrap().is_empty());
    assert_eq!(native.seen.lock().unwrap().len(), 1);
    let events = h.events(&id).await;
    let history = crate::controller::load::history(&events);
    assert_eq!(history.floor, Some(1));
    assert!(history.excluded.contains("local"));
    assert_eq!(
        events
            .iter()
            .filter_map(crate::ladder::rung_event)
            .filter(|r| r.rung == Rung::SwitchWorker)
            .count(),
        1
    );
    assert!(native.seen.lock().unwrap()[0]
        .last_error
        .as_ref()
        .unwrap()
        .contains("model/loop"));
}

#[tokio::test]
async fn owner_resume_of_a_surfaced_loop_keeps_escalation_after_a_spent_legacy_switch() {
    use rustykrab_core::work::{BlockedReason, EventKind, RungEvent, WorkError, WorkEvent};
    let local = ObservedWorker::new("local", WorkerKind::Local, true);
    let alias =
        ObservedWorker::with_model("older-local", WorkerKind::Local, true, "older-provider");
    let native = ObservedWorker::new("native", WorkerKind::Codex, false);
    native.complete.store(true, Ordering::Relaxed);
    let fleet: Vec<Arc<dyn Worker>> = vec![local.clone(), alias.clone(), native.clone()];
    let mut h = Harness::with_fleet(ControllerConfig::default(), fleet.clone());
    h.ctl
        .registry()
        .register(alias.clone(), serde_json::json!({}), Some(1))
        .await
        .unwrap();
    let mut d = draft("job", "finish the retained repair");
    let mut budget = crate::controller::tests::no_rungs();
    budget.rungs.worker_switches = 1;
    budget.rungs.repairs = 1;
    d.budget = Some(budget);
    let id = h.file_one(d).await;
    let prior = RungEvent {
        rung: Rung::SwitchWorker,
        at: chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
        error: None,
        outcome: "switched from retired-planner".into(),
    };
    h.store()
        .work_event_append(&WorkEvent {
            item: id.clone(),
            at: prior.at,
            kind: EventKind::Rung,
            from: None,
            to: None,
            actor: "controller".into(),
            reason: Some(crate::ladder::encode_rung_event(&prior)),
            upstream: None,
            origin: None,
            evidence_ref: None,
        })
        .await
        .unwrap();
    // The older provider also failed, before the explicit-model worker was
    // tried. Resuming must not return to that previously unsuccessful slot.
    h.store()
        .work_event_append(&WorkEvent {
            item: id.clone(),
            at: prior.at,
            kind: EventKind::Lease,
            from: None,
            to: None,
            actor: "worker:older-local".into(),
            reason: None,
            upstream: None,
            origin: None,
            evidence_ref: None,
        })
        .await
        .unwrap();
    let timeout = WorkError {
        class: rustykrab_core::work::ErrorClass::Tool,
        subclass: ErrorSubclass::Timeout,
        fingerprint: "historical-timeout".into(),
        detail: "previous executor stalled".into(),
        artifact_refs: vec![],
        observed_by: "fixture-host".into(),
    };
    h.store()
        .work_event_append(&WorkEvent {
            item: id.clone(),
            at: prior.at,
            kind: EventKind::Rung,
            from: None,
            to: None,
            actor: "controller".into(),
            reason: Some(crate::ladder::encode_rung_event(&RungEvent {
                rung: Rung::Repair,
                at: prior.at,
                error: Some(timeout),
                outcome: "repair prior timeout".into(),
            })),
            upstream: None,
            origin: None,
            evidence_ref: None,
        })
        .await
        .unwrap();
    h.drain_settled().await;
    assert_eq!(
        h.status(&id).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    assert_eq!(local.seen.lock().unwrap().len(), 1);
    assert!(
        native.seen.lock().unwrap().is_empty(),
        "no automatic extra recovery rung"
    );
    let question = h
        .store()
        .questions_list(&rustykrab_store::QuestionFilter {
            item: Some(id.clone()),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .find(|q| q.status == rustykrab_core::questions::QuestionStatus::Open)
        .unwrap();
    let history_before = h.events(&id).await;
    // A fresh controller reconstructs the surfaced failure from durable events.
    let ctl = Controller::new(h.store().clone(), fleet, ControllerConfig::default())
        .with_clock(h.clock.clone());
    let registry = ctl.registry().clone();
    h.ctl = ctl.with_routing(Arc::new(crate::routing::RecordRouting::new(registry)));
    h.ctl
        .registry()
        .register(alias.clone(), serde_json::json!({}), Some(1))
        .await
        .unwrap();
    ControlHandle::answer(
        &h.ctl,
        &question.id,
        "Continue the original repair",
        "user:fixture",
    )
    .await
    .unwrap();
    h.drain_settled().await;
    assert_eq!(h.status(&id).await, Status::Done);
    assert_eq!(local.seen.lock().unwrap().len(), 1);
    assert!(
        alias.seen.lock().unwrap().is_empty(),
        "same-model alias is not an upgrade"
    );
    assert_eq!(native.seen.lock().unwrap().len(), 1);
    let after = h.events(&id).await;
    assert!(
        history_before.iter().all(|event| after.contains(event)),
        "all prior recovery evidence is retained"
    );
    assert_eq!(
        h.rungs(&id)
            .await
            .iter()
            .filter(|r| **r == Rung::SwitchWorker)
            .count(),
        1,
        "the historical switch record is retained"
    );
}

#[tokio::test]
async fn only_an_authorized_answer_to_a_technical_stop_starts_a_new_recovery_cycle() {
    use rustykrab_core::work::{
        BlockedReason, ErrorClass, EventKind, RungEvent, WorkError, WorkEvent,
    };
    let h = Harness::new(&[]);
    let id = h.file_one(draft("job", "bounded technical recovery")).await;
    let item = h.item(&id).await;
    for actor in [
        "user",
        "user:fixture",
        "policy:fixture",
        "worker:fixture",
        "controller",
        "default",
    ] {
        for class in [
            ErrorClass::Model,
            ErrorClass::Verification,
            ErrorClass::Policy,
        ] {
            for answered in [true, false] {
                let at = h.clock.now();
                let event = |kind, reason| WorkEvent {
                    item: id.clone(),
                    at,
                    kind,
                    from: None,
                    to: None,
                    actor: "controller".into(),
                    reason: Some(reason),
                    upstream: None,
                    origin: None,
                    evidence_ref: None,
                };
                let rung = |which| {
                    event(
                        EventKind::Rung,
                        crate::ladder::encode_rung_event(&RungEvent {
                            rung: which,
                            at,
                            error: Some(WorkError {
                                class,
                                subclass: ErrorSubclass::Unclassified,
                                fingerprint: "fixture-technical-stop".into(),
                                detail: "fixture stop".into(),
                                artifact_refs: vec![],
                                observed_by: "fixture-host".into(),
                            }),
                            outcome: "fixture recovery".into(),
                        }),
                    )
                };
                let mut events = vec![rung(Rung::Repair), rung(Rung::Surface)];
                let mut answer = event(
                    EventKind::Transition,
                    if answered {
                        "answered question fixture"
                    } else {
                        "retry requested"
                    }
                    .into(),
                );
                answer.actor = actor.into();
                answer.from = Some(Status::Blocked(BlockedReason::NeedsDecision));
                answer.to = Some(Status::Queued);
                events.push(answer);
                let authorized =
                    actor == "user" || actor.starts_with("user:") || actor.starts_with("policy:");
                let reset = authorized && answered && class != ErrorClass::Policy;
                let cycle = crate::controller::load::ladder_from(&events, &item);
                assert_eq!(
                    cycle.used(Rung::Repair),
                    if reset { 0 } else { 1 },
                    "actor={actor}, class={class:?}, answered={answered}"
                );
                assert_eq!(events.len(), 3, "stored history is not erased");
                assert_eq!(cycle.budgets, item.budget.rungs);
            }
        }
    }
}

#[tokio::test]
async fn resumed_loop_waits_for_an_upgrade_without_repeating_the_failed_model() {
    let local = ObservedWorker::new("local", WorkerKind::Local, true);
    let alias = ObservedWorker::new("alias", WorkerKind::Local, true);
    let h = Harness::with_fleet(
        ControllerConfig::default(),
        vec![local.clone(), alias.clone()],
    );
    let mut d = draft("job", "resume when a better executor is available");
    d.budget = Some(crate::controller::tests::no_rungs());
    let id = h.file_one(d).await;
    h.drain_settled().await;
    let question = h
        .store()
        .questions_list(&rustykrab_store::QuestionFilter {
            item: Some(id.clone()),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .find(|q| q.status == rustykrab_core::questions::QuestionStatus::Open)
        .unwrap();
    ControlHandle::answer(
        &h.ctl,
        &question.id,
        "Continue when an executor can handle it",
        "user:fixture",
    )
    .await
    .unwrap();
    for _ in 0..3 {
        h.tick().await;
    }
    assert_eq!(local.seen.lock().unwrap().len(), 1);
    assert!(alias.seen.lock().unwrap().is_empty());
    assert!(!h.status(&id).await.is_closed());
    let native = ObservedWorker::new("native", WorkerKind::Codex, false);
    native.complete.store(true, Ordering::Relaxed);
    h.ctl
        .registry()
        .register(native.clone(), serde_json::json!({}), Some(2))
        .await
        .unwrap();
    h.drain_settled().await;
    assert_eq!(h.status(&id).await, Status::Done);
    assert_eq!(native.seen.lock().unwrap().len(), 1);
    assert!(h.rungs(&id).await.iter().all(|r| *r != Rung::SwitchWorker));
}

fn attach_goal(h: &Harness, id: &str, ws: Workspace) {
    h.ctl.state().runs.get_mut(id).unwrap().merge_progress = Some(MergeProgress {
        workspace: ws,
        paths: vec!["src/lib.rs".into()],
        observed: None,
        changed: h.clock.now(),
        seen: HashSet::new(),
        unavailable: None,
    });
}

#[tokio::test]
async fn changing_inspection_output_does_not_renew_an_unchanged_merge() {
    let (h, worker, id) = running().await;
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::plan(
        root.path(),
        repo.path(),
        &crate::workspace::head(repo.path()).unwrap(),
        "job",
        "run",
    );
    ws.create().unwrap();
    attach_goal(&h, &id, ws.clone());
    h.tick().await;
    for n in 1..=3 {
        worker.pulse.store(n, Ordering::Relaxed);
        // Unrelated output cannot stand for resolution of a known conflict.
        std::fs::write(ws.path.join("inspection.log"), n.to_string()).unwrap();
        h.clock.advance(TimeDelta::seconds(4));
        h.tick().await;
    }
    assert_ne!(h.status(&id).await, Status::Running);
    let error = crate::ladder::last_error(&h.events(&id).await).unwrap();
    assert_eq!(error.subclass, ErrorSubclass::Loop);
    assert!(error.detail.contains("no new conflict-file content"));
    assert!(
        ws.path.join("inspection.log").exists(),
        "retain partial work"
    );
}

#[tokio::test]
async fn real_resolution_changes_extend_the_goal_window_but_revisited_states_do_not() {
    let (h, worker, id) = running().await;
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::plan(
        root.path(),
        repo.path(),
        &crate::workspace::head(repo.path()).unwrap(),
        "job",
        "run",
    );
    ws.create().unwrap();
    attach_goal(&h, &id, ws.clone());
    h.tick().await;
    for n in 1..=4 {
        worker.pulse.store(n, Ordering::Relaxed);
        std::fs::write(
            ws.path.join("src/lib.rs"),
            format!("pub fn resolved_{n}() {{}}\n"),
        )
        .unwrap();
        h.clock.advance(TimeDelta::seconds(8));
        h.tick().await;
        assert_eq!(h.status(&id).await, Status::Running);
    }
    // A return to earlier content is not new evidence; neither is git status.
    for n in 1..=3 {
        worker.pulse.store(10 + n, Ordering::Relaxed);
        std::fs::write(
            ws.path.join("src/lib.rs"),
            format!("pub fn resolved_{n}() {{}}\n"),
        )
        .unwrap();
        run_git(&ws.path, &["status", "--short"]);
        h.clock.advance(TimeDelta::seconds(4));
        h.tick().await;
    }
    assert_ne!(h.status(&id).await, Status::Running);
    assert_eq!(
        crate::ladder::last_error(&h.events(&id).await)
            .unwrap()
            .subclass,
        ErrorSubclass::Loop
    );
}
