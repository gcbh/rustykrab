//! The controller against a real store and scripted workers.
//!
//! Each test opens a store in its own temporary directory (the way the
//! store's and the tools' tests open one), drives the loop with a manual
//! clock that moves one second per tick, and runs scripted [`Worker`]s that
//! return canned [`ResultReport`]s per item title.

mod paths;
mod review;
mod scenarios;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use rustykrab_core::work::{
    ArtifactRef, Budget, DraftEdge, EdgeKind, ErrorSubclass, EventKind, ItemRef, PlanAccepted,
    PlanOutcome, ResultReport, Rung, RungBudgets, Status, WorkError, WorkEvent, WorkItem,
    WorkItemDraft, WorkItemId, WorkPlan, WorkerKind,
};
use rustykrab_core::Error;
use rustykrab_store::{OutboxRow, Store};
use rustykrab_tools::work_backend::{Provenance, WorkBackend};
use tokio::sync::Notify;
use uuid::Uuid;

use super::load::decode_rung;
use super::{merge, Clock, Controller, ControllerConfig, ManualClock, StaticCatalog};
use crate::graph::FilingSource;
use crate::handle::{ControlHandle, TickReport};
use crate::worker::{Brief, Worker, WorkerCapabilities};

// ── scripted workers ────────────────────────────────────────────────────

/// What a scripted run does.
pub(super) enum Step {
    Report(Box<ResultReport>),
    Fail(Error),
    /// Wait for the gate, then report.
    Wait(Arc<Notify>, Box<ResultReport>),
    /// Never finish; set the flag when the run is dropped (aborted).
    Hang(Arc<AtomicBool>),
}

/// Canned steps per item title, shared by every worker of a harness, and
/// what the workers saw.
#[derive(Default)]
pub(super) struct Script {
    steps: Mutex<HashMap<String, VecDeque<Step>>>,
    briefs: Mutex<Vec<(String, Brief)>>,
    active: Mutex<HashMap<String, usize>>,
    peak: Mutex<HashMap<String, usize>>,
}

impl Script {
    pub fn push(&self, title: &str, step: Step) {
        self.steps
            .lock()
            .unwrap()
            .entry(title.to_string())
            .or_default()
            .push_back(step);
    }

    fn next(&self, title: &str) -> Option<Step> {
        self.steps.lock().unwrap().get_mut(title)?.pop_front()
    }

    /// Every brief a worker received, with the worker's name.
    pub fn briefs(&self) -> Vec<(String, Brief)> {
        self.briefs.lock().unwrap().clone()
    }

    pub fn briefs_for(&self, title: &str) -> Vec<Brief> {
        self.briefs()
            .into_iter()
            .filter(|(_, b)| b.title == title)
            .map(|(_, b)| b)
            .collect()
    }

    /// The most runs writing `resource` that were ever in flight at once.
    pub fn peak(&self, resource: &str) -> usize {
        self.peak
            .lock()
            .unwrap()
            .get(resource)
            .copied()
            .unwrap_or(0)
    }
}

/// Counts runs per writable resource while they are in flight.
struct Occupancy {
    script: Arc<Script>,
    resources: Vec<String>,
}

impl Occupancy {
    fn enter(script: &Arc<Script>, resources: &[String]) -> Occupancy {
        let mut active = script.active.lock().unwrap();
        let mut peak = script.peak.lock().unwrap();
        for r in resources {
            let n = active.entry(r.clone()).or_default();
            *n += 1;
            let p = peak.entry(r.clone()).or_default();
            *p = (*p).max(*n);
        }
        Occupancy {
            script: Arc::clone(script),
            resources: resources.to_vec(),
        }
    }
}

impl Drop for Occupancy {
    fn drop(&mut self) {
        let mut active = self.script.active.lock().unwrap();
        for r in &self.resources {
            if let Some(n) = active.get_mut(r) {
                *n = n.saturating_sub(1);
            }
        }
    }
}

struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

pub(super) struct Scripted {
    name: String,
    concurrency: usize,
    script: Arc<Script>,
}

#[async_trait]
impl Worker for Scripted {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> WorkerKind {
        WorkerKind::Local
    }

    fn capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities {
            models: vec![format!("model-of-{}", self.name)],
            tools: vec!["*".to_string()],
            mcp_servers: vec!["*".to_string()],
            writable_resources: vec!["*".to_string()],
            ..WorkerCapabilities::default()
        }
    }

    fn concurrency(&self) -> usize {
        self.concurrency
    }

    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        self.script
            .briefs
            .lock()
            .unwrap()
            .push((self.name.clone(), brief.clone()));
        let _in_flight = Occupancy::enter(&self.script, &brief.writable_resources);
        match self.script.next(&brief.title) {
            None => Ok(done(&brief.title)),
            Some(Step::Report(r)) => Ok(*r),
            Some(Step::Fail(e)) => Err(e),
            Some(Step::Wait(gate, r)) => {
                gate.notified().await;
                Ok(*r)
            }
            Some(Step::Hang(flag)) => {
                let _dropped = SetOnDrop(flag);
                std::future::pending().await
            }
        }
    }
}

// ── canned results and drafts ───────────────────────────────────────────

pub(super) fn slug(title: &str) -> String {
    title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// A done report with one path artifact.
pub(super) fn done(title: &str) -> ResultReport {
    ResultReport {
        summary: format!("did {title}"),
        artifacts: vec![ArtifactRef {
            kind: "path".to_string(),
            value: format!("/tmp/{}.txt", slug(title)),
        }],
        ..ResultReport::default()
    }
}

/// A report carrying a worker's own error.
pub(super) fn failure(subclass: ErrorSubclass, detail: &str) -> ResultReport {
    ResultReport {
        summary: String::new(),
        error: Some(WorkError {
            class: subclass.class(),
            subclass,
            fingerprint: String::new(),
            detail: detail.to_string(),
            artifact_refs: Vec::new(),
            observed_by: String::new(),
        }),
        ..ResultReport::default()
    }
}

pub(super) fn report(r: ResultReport) -> Step {
    Step::Report(Box::new(r))
}

pub(super) fn draft(tmp: &str, title: &str) -> WorkItemDraft {
    WorkItemDraft {
        tmp: Some(tmp.to_string()),
        title: title.to_string(),
        objective: format!("objective of {title}"),
        done_when: format!("{title} is done"),
        ..WorkItemDraft::default()
    }
}

pub(super) fn tmp(t: &str) -> ItemRef {
    ItemRef::Tmp { tmp: t.to_string() }
}

pub(super) fn on(kind: EdgeKind, upstream: &str) -> DraftEdge {
    DraftEdge {
        kind,
        depends_on: tmp(upstream),
    }
}

/// A graph rooted at the first draft.
pub(super) fn plan(items: Vec<WorkItemDraft>) -> WorkPlan {
    let root = items[0].tmp.clone().expect("the root draft has a temp id");
    WorkPlan {
        root: tmp(&root),
        items,
        edges: Vec::new(),
        rationale: "test plan".to_string(),
    }
}

/// A budget with no ladder rungs: the first failure goes past orders 0 to 3.
pub(super) fn no_rungs() -> Budget {
    Budget {
        iterations: 5,
        tokens: 10_000,
        wall_seconds: 600,
        repairs: 0,
        rungs: RungBudgets {
            retries: 0,
            repairs: 0,
            worker_switches: 0,
            acquisitions: 0,
            builds: 0,
            requests: 0,
            improvements: 0,
            replans: 0,
        },
    }
}

// ── the harness ─────────────────────────────────────────────────────────

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) struct Harness {
    pub ctl: Controller,
    pub clock: Arc<ManualClock>,
    pub script: Arc<Script>,
    store: Store,
    config: ControllerConfig,
    catalog: Arc<StaticCatalog>,
    dir: Arc<TempDir>,
}

impl Harness {
    pub fn new(workers: &[&str]) -> Harness {
        Harness::with(
            ControllerConfig::default(),
            StaticCatalog::default(),
            workers,
        )
    }

    pub fn with(config: ControllerConfig, catalog: StaticCatalog, workers: &[&str]) -> Harness {
        let path = std::env::temp_dir().join(format!("rk-controller-{}", Uuid::new_v4()));
        let store = Store::open(&path, vec![9u8; 32]).expect("store opens");
        let dir = Arc::new(TempDir(path));
        let clock = Arc::new(ManualClock::new(Utc::now()));
        let script = Arc::new(Script::default());
        Harness::build(
            store,
            config,
            Arc::new(catalog),
            workers,
            clock,
            script,
            dir,
            1,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        store: Store,
        config: ControllerConfig,
        catalog: Arc<StaticCatalog>,
        workers: &[&str],
        clock: Arc<ManualClock>,
        script: Arc<Script>,
        dir: Arc<TempDir>,
        concurrency: usize,
    ) -> Harness {
        let fleet: Vec<Arc<dyn Worker>> = workers
            .iter()
            .map(|name| {
                Arc::new(Scripted {
                    name: name.to_string(),
                    concurrency,
                    script: Arc::clone(&script),
                }) as Arc<dyn Worker>
            })
            .collect();
        let ctl = Controller::new(store.clone(), fleet, config.clone())
            .with_clock(clock.clone())
            .with_catalog(catalog.clone());
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

    /// The same store, clock and script under a fresh controller, as after
    /// a daemon restart. The old controller is dropped first, which stops
    /// its runs.
    pub fn restart(self, workers: &[&str]) -> Harness {
        let Harness {
            ctl,
            clock,
            script,
            store,
            config,
            catalog,
            dir,
        } = self;
        drop(ctl);
        Harness::build(store, config, catalog, workers, clock, script, dir, 1)
    }

    /// A harness whose workers advertise `concurrency`.
    pub fn with_concurrency(workers: &[&str], concurrency: usize) -> Harness {
        let h = Harness::new(&[]);
        let Harness {
            ctl,
            clock,
            script,
            store,
            config,
            catalog,
            dir,
        } = h;
        drop(ctl);
        Harness::build(
            store,
            config,
            catalog,
            workers,
            clock,
            script,
            dir,
            concurrency,
        )
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// One tick, one second later.
    pub async fn tick(&self) -> TickReport {
        self.clock.advance(TimeDelta::seconds(1));
        ControlHandle::tick(&self.ctl).await.expect("tick")
    }

    /// Let spawned runs finish, then tick.
    pub async fn step(&self) -> TickReport {
        self.ctl.wait_for_runs(Duration::from_millis(300)).await;
        self.tick().await
    }

    /// Tick until nothing moves. Runs that never finish (hung, gated) are
    /// waited on briefly and left running.
    pub async fn drain(&self) -> TickReport {
        let mut total = TickReport::default();
        for _ in 0..80 {
            let r = self.step().await;
            let quiet = r.leased.is_empty()
                && r.reconciled.is_empty()
                && r.transitions == 0
                && r.notices == 0;
            merge(&mut total, r);
            let settled = {
                let state = self.ctl.state();
                state.finished.is_empty() && state.runs.values().all(|r| !r.handle.is_finished())
            };
            if quiet && settled {
                break;
            }
        }
        total
    }

    fn provenance() -> Provenance {
        Provenance {
            conversation_id: Some("conv-1".to_string()),
            filed_by_item: None,
            actor: "planner".to_string(),
        }
    }

    /// File a graph through the planner path; it must be accepted.
    pub async fn file(&self, plan: WorkPlan) -> PlanAccepted {
        self.clock.advance(TimeDelta::seconds(1));
        let outcome = ControlHandle::file_plan(
            &self.ctl,
            plan,
            Harness::provenance(),
            FilingSource::Planner,
        )
        .await
        .expect("filing");
        match outcome {
            PlanOutcome::Accepted(a) => a,
            PlanOutcome::Rejected(r) => panic!("rejected: {r:?}"),
        }
    }

    /// File one draft through `work_file`; returns its id.
    pub async fn file_one(&self, draft: WorkItemDraft) -> WorkItemId {
        self.clock.advance(TimeDelta::seconds(1));
        let outcome = WorkBackend::file(
            &self.ctl,
            draft,
            Provenance {
                conversation_id: Some("conv-1".to_string()),
                filed_by_item: None,
                actor: "agent".to_string(),
            },
        )
        .await
        .expect("filing");
        match outcome {
            PlanOutcome::Accepted(a) => a.root,
            PlanOutcome::Rejected(r) => panic!("rejected: {r:?}"),
        }
    }

    pub async fn item(&self, id: &str) -> WorkItem {
        self.store
            .work_get(id)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("no live item {id}"))
    }

    pub async fn status(&self, id: &str) -> Status {
        self.item(id).await.status
    }

    pub async fn origin(&self, id: &str) -> Option<WorkItemId> {
        self.item(id).await.status_origin
    }

    pub async fn events(&self, id: &str) -> Vec<WorkEvent> {
        self.store.work_events(id).await.unwrap()
    }

    /// The rungs climbed on `id`, in order.
    pub async fn rungs(&self, id: &str) -> Vec<Rung> {
        self.events(id)
            .await
            .iter()
            .filter_map(decode_rung)
            .map(|r| r.rung)
            .collect()
    }

    pub async fn leases(&self, id: &str) -> usize {
        self.events(id)
            .await
            .iter()
            .filter(|e| e.kind == EventKind::Lease)
            .count()
    }

    pub async fn outbox(&self) -> Vec<OutboxRow> {
        self.store.work_outbox_pending().await.unwrap()
    }

    /// Every live item of a kind.
    pub async fn of_kind(&self, kind: rustykrab_core::work::WorkKind) -> Vec<WorkItem> {
        self.store
            .work_list(&rustykrab_store::WorkFilter {
                kind: Some(kind),
                include_closed: true,
                ..Default::default()
            })
            .await
            .unwrap()
    }
}
