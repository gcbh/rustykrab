use super::*;
use crate::controller::{
    tests::{draft, Harness},
    Clock, ControllerConfig,
};
use crate::worker::{Brief, Worker, WorkerCapabilities};
use crate::workspace::tests::{run_git, Repo};
use async_trait::async_trait;
use rustykrab_core::work::{ErrorSubclass, ResultReport, Rung, Status, WorkerKind};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

struct ObservedWorker {
    name: &'static str,
    kind: WorkerKind,
    pulse: AtomicU64,
    seen: Mutex<Vec<Brief>>,
    fail_loop: bool,
}
impl ObservedWorker {
    fn new(name: &'static str, kind: WorkerKind, fail_loop: bool) -> Arc<Self> {
        Arc::new(Self {
            name,
            kind,
            pulse: AtomicU64::new(0),
            seen: Mutex::new(vec![]),
            fail_loop,
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
            models: vec![if self.kind == WorkerKind::Local {
                "same-local-model"
            } else {
                "native-model"
            }
            .into()],
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
        self.seen.lock().unwrap().push(brief);
        if self.fail_loop {
            return Err(crate::worker::RunFailure::Model {
                problem: ProviderProblem::Loop,
                detail: "fixture repeatedly inspects without changing result".into(),
            }
            .into_error());
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
