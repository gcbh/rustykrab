//! The controller loop (plan section 6): a deterministic loop in code, never
//! a model, that schedules the work-item graph, runs leased items on
//! workers, verifies what they report, climbs the ladder on every failure
//! and writes one notice per parent into the outbox.
//!
//! One [`ControlHandle::tick`] is, in order:
//!
//! 1. **Sweep** (6.7, every tick; `sweep` in `tick.rs`), one store
//!    transaction: stored holds and, on the first tick, roll-ups that
//!    disagree with what the edges derive are corrected with `resume`
//!    events and an `internal` item (section 9); expiries apply with their
//!    cascade; on the first tick a leaf whose worker still holds its run (a
//!    peer's task, `reattach.rs`) is re-attached and keeps its lease;
//!    leases past their TTL either stall (a live run: the ladder climbs) or
//!    return to `ready` (no run: a restart); on the first tick every other
//!    active leaf this process is not running returns to `ready`;
//!    items parked on a capability item that is now `done` are released;
//!    then readiness (time triggers), roll-ups and parent verification are
//!    settled.
//! 2. **Reconcile** (step 5 and section 5): each finished run is verified
//!    (a `code` result against git), its evidence attached and its typed
//!    transition, cascade, `discovered` filings and ladder rungs written in
//!    one transaction; the producing worker's routing record is written
//!    after it.
//! 3. **Select, match, lease and run** (steps 1 to 4): ready leaves by
//!    priority, the single-writer rule over every active item, the
//!    local-model rule of 12.1 (one run per local model, and none while
//!    [`ModelActivity`] says an interactive turn holds it), the cheapest
//!    tier of healthy workers that cover the item and that routing
//!    qualifies for its class (waiting for that tier when it is busy); the
//!    lease carries the fan-in inputs of 4.3 and 6.3 and, for a `code`
//!    item, its workspace, and the run is spawned under tokio. A run that
//!    ends or is stopped has its spend recorded (`spend.rs`).
//! 4. **Aging** (4.6), when nothing is running.
//!
//! Every transaction that closes a root, expires an item or surfaces a
//! question also writes that root's outbox notice (6.6), at most one per
//! root per tick: a later reconcile for a root already noticed this tick
//! waits for the next tick instead of writing a second notice.
//!
//! Layout: `batch.rs` builds one transaction over a working snapshot;
//! `load.rs` reads the store back (the snapshot, rung histories, a
//! firing's conversation); `spend.rs` records what runs spend and gives
//! parents their remaining budgets; `reattach.rs` keeps a run that
//! outlived a restart (a peer's task) on its lease;
//! `brief.rs` builds a run's inputs and brief; `filing.rs` is every filing
//! path and the user's approve, reject and cancel; `tick.rs` is the loop;
//! `notice.rs` renders the 6.6 message.

mod batch;
mod brief;
mod commit;
mod filing;
mod load;
mod notice;
mod reattach;
mod review;
mod spend;
mod tick;

#[cfg(test)]
mod tests;

/// The evidence kind a verified result's full summary is kept under: what
/// a host delivering the result (a scheduled job's firing) reads back.
pub use brief::SUMMARY as SUMMARY_EVIDENCE;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use rustykrab_core::work::{
    Budget, GraphCaps, PlanOutcome, Precondition, ResultReport, WorkItem, WorkItemDraft,
    WorkItemId, WorkKind, WorkPlan, WorkerKind,
};
use rustykrab_core::Error;
use rustykrab_store::{Spend, Store};
use rustykrab_tools::work_backend::{
    Principal, Provenance, StatusQuery, ToolState, WorkBackend, WorkStatusView,
};
use tokio::task::JoinHandle;

use crate::errors::{LearnedRule, Recurrence, DEFAULT_PROMOTE_THRESHOLD};
use crate::graph::{ApprovalPolicy, FilingSource, SplitMode};
use crate::handle::{ControlHandle, GraphView, LockState, LoopStatus, TickReport};
use crate::lock::LoopLock;
use crate::registry::WorkerRegistry;
use crate::routing::Judged;
use crate::worker::Worker;

// ── injected seams ───────────────────────────────────────────────────────

/// Where the controller reads the time. Every decision that depends on
/// time (triggers, expiry, lease TTLs, aging) reads it here, so tests move
/// time by hand. The store stamps its own rows with the wall clock.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock that moves only when told to.
#[derive(Debug)]
pub struct ManualClock {
    at: Mutex<DateTime<Utc>>,
}

impl ManualClock {
    pub fn new(at: DateTime<Utc>) -> ManualClock {
        ManualClock { at: Mutex::new(at) }
    }

    pub fn set(&self, at: DateTime<Utc>) {
        *self.at.lock().unwrap_or_else(|e| e.into_inner()) = at;
    }

    pub fn advance(&self, by: TimeDelta) {
        let mut at = self.at.lock().unwrap_or_else(|e| e.into_inner());
        *at += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.at.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The host's registry and configuration, by name only: which tools exist,
/// which MCP servers and credentials are configured, and the host checks
/// behind lease-time preconditions. The composition root implements it over
/// the tool registry; [`StaticCatalog`] is a fixed table.
pub trait ToolCatalog: Send + Sync {
    fn tool_state(&self, name: &str) -> ToolState;

    fn mcp_server_configured(&self, name: &str) -> bool;

    /// Whether a credential is stored, which fires `on_credential` triggers.
    fn credential_available(&self, _name: &str) -> bool {
        false
    }

    /// A lease-time precondition (plan section 4). An item whose
    /// precondition does not hold stays `ready` and is re-checked on the
    /// next tick.
    fn precondition_holds(&self, _check: &Precondition) -> bool {
        true
    }

    /// Look again for what appeared since the catalog was built: a skill a
    /// capability build wrote at run time (section 8, rung 2b). The
    /// controller calls it before it decides a tool is missing and before
    /// it verifies a build.
    fn refresh(&self) {}
}

/// A fixed [`ToolCatalog`]: what is not listed is unknown or unconfigured.
#[derive(Debug, Clone, Default)]
pub struct StaticCatalog {
    pub tools: HashMap<String, ToolState>,
    pub mcp_servers: HashSet<String>,
    pub credentials: HashSet<String>,
}

impl ToolCatalog for StaticCatalog {
    fn tool_state(&self, name: &str) -> ToolState {
        self.tools.get(name).copied().unwrap_or(ToolState::Unknown)
    }

    fn mcp_server_configured(&self, name: &str) -> bool {
        self.mcp_servers.contains(name)
    }

    fn credential_available(&self, name: &str) -> bool {
        self.credentials.contains(name)
    }
}

/// The routing record's seam (plan sections 5 and 10). [`CheapestFirst`]
/// routes by capabilities and cost tier only;
/// [`crate::routing::RecordRouting`] reads each worker's record for the
/// item's class of work and writes it from every judged result.
#[async_trait]
pub trait Routing: Send + Sync {
    /// Whether the worker's record qualifies it for this item's class of
    /// work: `class` is [`crate::routing::work_class`] of the item and its
    /// capability facet, `None` for work not routed by record.
    fn qualifies(&self, _worker: &dyn Worker, _item: &WorkItem, _class: Option<&str>) -> bool {
        true
    }

    /// Cheaper tiers are preferred.
    fn cost_tier(&self, worker: &dyn Worker) -> u32 {
        default_cost_tier(worker.kind())
    }

    /// Called once a judged result of a routed class is written: verified,
    /// claimed but not verified, or failed.
    async fn record(&self, _judged: &Judged) {}

    /// Called once an accepted routing proposal moved a class's default
    /// tier in the store, so routing reads the new one from now on.
    async fn defaults_moved(&self) {}
}

/// Phase 1 routing: every covering worker qualifies, cheapest tier first.
#[derive(Debug, Clone, Copy, Default)]
pub struct CheapestFirst;

impl Routing for CheapestFirst {}

/// Local work is cheapest, then peers, then the coding agents.
pub fn default_cost_tier(kind: WorkerKind) -> u32 {
    match kind {
        WorkerKind::Any | WorkerKind::Local => 0,
        WorkerKind::Peer => 1,
        WorkerKind::Codex => 2,
        WorkerKind::ClaudeCode => 3,
    }
}

/// The progress ledger's seam (plan step 7). Phase 1 has none: a lease
/// past its TTL is the stall signal. A ledger that reports progress on a
/// run renews its lease instead.
pub trait ProgressLedger: Send + Sync {
    fn progressed(&self, _item: &str, _since: DateTime<Utc>) -> bool {
        false
    }
}

/// No ledger: nothing counts as progress.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoLedger;

impl ProgressLedger for NoLedger {}

/// Work on a local model the controller does not run itself: an
/// interactive turn, a credential wake (plan section 12.1). While a model
/// is busy the controller leases no local worker on it, so a scheduled
/// firing waits for the turn instead of evicting its prefix cache. The
/// composition root implements it over the gateway's activity tracker.
pub trait ModelActivity: Send + Sync {
    /// Whether `model` (a local worker's advertised model) is serving work
    /// outside the controller right now.
    fn busy(&self, _model: &str) -> bool {
        false
    }
}

/// Nothing runs outside the controller.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoActivity;

impl ModelActivity for NoActivity {}

// ── configuration ───────────────────────────────────────────────────────

/// What policy sets for the loop.
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    /// Filing caps (section 6.1) and the inputs block's caps (6.3).
    pub caps: GraphCaps,
    /// A lease not renewed within this many seconds has stalled.
    pub lease_ttl_seconds: u64,
    /// How long after closing an item of each kind ages into the archive
    /// (4.6). A kind with no window never ages.
    pub aging: HashMap<WorkKind, TimeDelta>,
    /// Aging prefers idle ticks, when no run is in flight, but runs at
    /// least this often whatever is running, so a daemon that always has a
    /// long run going (an external agent, a peer) still ages. Compaction
    /// never touches an item open work names, so it is safe mid-run.
    pub aging_max_gap: TimeDelta,
    /// The outbox channel notices are written for.
    pub notice_channel: String,
    /// The approval triggers of 6.1: item count, total budget, delegated
    /// writable resources.
    pub approval: ApprovalPolicy,
    pub split_mode: SplitMode,
    /// Supersede filings allowed per root within `supersede_window`: the
    /// caps' re-plans per parent by default (one, a placeholder until
    /// Phase 0 measures it; plan sections 6.1 and 17).
    pub supersede_limit: u32,
    pub supersede_window: TimeDelta,
    /// 12.1: at most one running item per local worker, and per local
    /// model, so scheduled and interactive work never share a KV slot.
    pub serialise_local: bool,
    /// The budget of a new item that names none.
    pub default_budget: Budget,
    /// Items a fingerprint must fail on before the ladder files the
    /// improvement (section 9).
    pub promote_threshold: u32,
    /// How far back recurrence counts are rebuilt from rung events on the
    /// first tick.
    pub recurrence_window: TimeDelta,
    /// Where `code` runs get their worktrees (section 5): the daemon's
    /// `<data dir>/worktrees`. `None`: no workspace is planned, and a
    /// `code` result's claims cannot be checked against git.
    pub worktree_root: Option<PathBuf>,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        let month = TimeDelta::days(30);
        let caps = GraphCaps::default();
        ControllerConfig {
            caps,
            lease_ttl_seconds: 1_800,
            aging: WorkKind::ALL.iter().map(|k| (*k, month)).collect(),
            aging_max_gap: TimeDelta::minutes(15),
            notice_channel: "default".to_string(),
            approval: ApprovalPolicy::default(),
            split_mode: SplitMode::Warn,
            supersede_limit: caps.max_replans_per_parent,
            supersede_window: TimeDelta::hours(1),
            serialise_local: true,
            default_budget: Budget::default(),
            promote_threshold: DEFAULT_PROMOTE_THRESHOLD,
            recurrence_window: month,
            worktree_root: None,
        }
    }
}

// ── the controller ───────────────────────────────────────────────────────

type RunResult = Result<ResultReport, Error>;

/// A spawned worker run.
struct Run {
    worker: String,
    /// The id the controller gave the run (the brief's `run`), which the
    /// worker's [`Worker::usage`] answers for.
    run_id: String,
    /// When it started, on the wall clock: the run's wall time when the
    /// worker keeps none.
    started: std::time::Instant,
    handle: JoinHandle<RunResult>,
    /// When the run started, or last showed progress, on the controller's
    /// clock: the lease TTL counts from here.
    since: DateTime<Utc>,
    /// The run already handed in its result through `result_report`; what
    /// its task returns is ignored.
    reported: bool,
}

/// A result waiting for the reconcile step: a report handed in through
/// [`WorkBackend::report`], or a finished run deferred to the next tick.
struct Finished {
    worker: String,
    outcome: RunResult,
}

/// What the loop keeps in memory. Everything here is either rebuilt from
/// the store (recurrence, approvals) or dies with the process on purpose
/// (runs: a restart returns their items to `ready`).
struct State {
    runs: HashMap<WorkItemId, Run>,
    finished: HashMap<WorkItemId, Finished>,
    recurrence: Recurrence,
    /// The first tick has run: resume checks are done.
    resumed: bool,
    /// Whether an item held for approval carries its approval event.
    approved: HashMap<WorkItemId, bool>,
    /// Supersede filings per root, for the rate limit.
    supersedes: Vec<(WorkItemId, DateTime<Utc>)>,
    /// Planning items with an accepted graph (`already_planned`).
    planned: HashSet<WorkItemId>,
    /// Classifier rules `internal` items landed (section 9), rebuilt from
    /// their evidence on the first tick.
    learned: Vec<LearnedRule>,
    /// What each item's runs spent (`work_spend`), read once on the first
    /// load and kept current as runs end: a parent's remaining budget is
    /// its budget less what its subtree spent (4.2).
    spent: Option<HashMap<WorkItemId, Spend>>,
    /// When the last aging pass ran, for `aging_max_gap`.
    last_aged: Option<DateTime<Utc>>,
    /// When the last tick that completed finished.
    last_tick: Option<DateTime<Utc>>,
    /// The loop's hold on `controller.lock`, as it last found it.
    lock: Option<LockState>,
}

/// The loop of plan section 6 over one store and the workers of a
/// [`WorkerRegistry`]. It implements [`ControlHandle`] for the gateway and
/// the CLI, and [`WorkBackend`] for the model-facing work tools.
pub struct Controller {
    store: Store,
    registry: Arc<WorkerRegistry>,
    config: ControllerConfig,
    clock: Arc<dyn Clock>,
    catalog: Arc<dyn ToolCatalog>,
    routing: Arc<dyn Routing>,
    ledger: Arc<dyn ProgressLedger>,
    activity: Arc<dyn ModelActivity>,
    /// Serialises the loop's writers: a tick, a filing, an approval, a
    /// cancel. Never held while a worker runs.
    loop_lock: tokio::sync::Mutex<()>,
    state: Mutex<State>,
}

impl Controller {
    /// A controller over a fixed set of workers (an in-memory registry), on
    /// the wall clock, with an empty catalog, cheapest-first routing and no
    /// progress ledger.
    pub fn new(store: Store, workers: Vec<Arc<dyn Worker>>, config: ControllerConfig) -> Self {
        let recurrence = Recurrence::with_threshold(config.promote_threshold);
        Controller {
            registry: Arc::new(WorkerRegistry::fixed(store.clone(), workers)),
            store,
            config,
            clock: Arc::new(SystemClock),
            catalog: Arc::new(StaticCatalog::default()),
            routing: Arc::new(CheapestFirst),
            ledger: Arc::new(NoLedger),
            activity: Arc::new(NoActivity),
            loop_lock: tokio::sync::Mutex::new(()),
            state: Mutex::new(State {
                runs: HashMap::new(),
                finished: HashMap::new(),
                recurrence,
                resumed: false,
                approved: HashMap::new(),
                supersedes: Vec::new(),
                planned: HashSet::new(),
                learned: Vec::new(),
                spent: None,
                last_aged: None,
                last_tick: None,
                lock: None,
            }),
        }
    }

    /// Lease to the workers of `registry` instead: the daemon's registry,
    /// which `worker add` grows while the loop runs.
    pub fn with_registry(mut self, registry: Arc<WorkerRegistry>) -> Self {
        self.registry = registry;
        self
    }

    pub fn registry(&self) -> &Arc<WorkerRegistry> {
        &self.registry
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_catalog(mut self, catalog: Arc<dyn ToolCatalog>) -> Self {
        self.catalog = catalog;
        self
    }

    pub fn with_routing(mut self, routing: Arc<dyn Routing>) -> Self {
        self.routing = routing;
        self
    }

    pub fn with_progress_ledger(mut self, ledger: Arc<dyn ProgressLedger>) -> Self {
        self.ledger = ledger;
        self
    }

    /// The busy signal of plan 12.1: local leases wait while `activity`
    /// says their model serves work outside the controller.
    pub fn with_activity(mut self, activity: Arc<dyn ModelActivity>) -> Self {
        self.activity = activity;
        self
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn config(&self) -> &ControllerConfig {
        &self.config
    }

    /// The items with a live run in this process.
    pub fn running(&self) -> Vec<WorkItemId> {
        let mut ids: Vec<WorkItemId> = self.state().runs.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Tick until a tick changes nothing and no run is live, waiting up to
    /// `wait` for live runs to finish before each tick. For the CLI, tests
    /// and scripted daemons; the daemon itself ticks on a timer.
    pub async fn run_until_idle(
        &self,
        max_ticks: usize,
        wait: Duration,
    ) -> Result<TickReport, Error> {
        let mut total = TickReport::default();
        for _ in 0..max_ticks {
            self.wait_for_runs(wait).await;
            let report = ControlHandle::tick(self).await?;
            let quiet =
                report.leased.is_empty() && report.reconciled.is_empty() && report.transitions == 0;
            merge(&mut total, report);
            if quiet && self.state().runs.is_empty() && self.state().finished.is_empty() {
                break;
            }
        }
        Ok(total)
    }

    /// Wait until every live run has finished, or `limit` passes.
    pub async fn wait_for_runs(&self, limit: Duration) {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let busy = self.state().runs.values().any(|r| !r.handle.is_finished());
            if !busy || tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// Try `lock` for the loop and record what it found, for
    /// [`ControlHandle::loop_status`]. The loop ticks only on `Held`; an
    /// error trying the lock counts as `Waiting` and is returned beside it.
    pub fn claim_loop_lock(&self, lock: &mut LoopLock) -> (LockState, Option<std::io::Error>) {
        let (found, error) = match lock.poll() {
            Ok(found) => (found, None),
            Err(e) => (LockState::Waiting, Some(e)),
        };
        self.state().lock = Some(found);
        (found, error)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn worker(&self, name: &str) -> Option<Arc<dyn Worker>> {
        self.registry.get(name)
    }

    fn worker_kind(&self, name: &str) -> WorkerKind {
        self.worker(name).map_or(WorkerKind::Any, |w| w.kind())
    }
}

impl Drop for Controller {
    /// Local runs die with their controller, as they would with the
    /// process; their items return to `ready` on the next start.
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
        for run in state.runs.values() {
            run.handle.abort();
        }
    }
}

/// Add one tick's report to a running total.
pub(crate) fn merge(total: &mut TickReport, add: TickReport) {
    total.made_ready.extend(add.made_ready);
    total.leased.extend(add.leased);
    total.reconciled.extend(add.reconciled);
    total.expired.extend(add.expired);
    total.archived.extend(add.archived);
    total.transitions += add.transitions;
    total.notices += add.notices;
}

#[async_trait]
impl ControlHandle for Controller {
    async fn file_plan(
        &self,
        plan: WorkPlan,
        provenance: Provenance,
        source: FilingSource,
    ) -> Result<PlanOutcome, Error> {
        let _loop = self.loop_lock.lock().await;
        self.file_plan_locked(plan, provenance, source).await
    }

    async fn file_draft(
        &self,
        draft: WorkItemDraft,
        provenance: Provenance,
    ) -> Result<PlanOutcome, Error> {
        WorkBackend::file(self, draft, provenance).await
    }

    async fn approve(&self, root: &str, actor: &str) -> Result<Vec<WorkItemId>, Error> {
        let _loop = self.loop_lock.lock().await;
        self.approve_locked(root, actor).await
    }

    async fn reject(
        &self,
        root: &str,
        reason: Option<String>,
        actor: &str,
    ) -> Result<Vec<WorkItemId>, Error> {
        let _loop = self.loop_lock.lock().await;
        self.reject_locked(root, reason, actor).await
    }

    async fn cancel(
        &self,
        item: &str,
        reason: Option<String>,
        actor: &str,
    ) -> Result<Vec<WorkItemId>, Error> {
        let _loop = self.loop_lock.lock().await;
        self.cancel_locked(item, reason, actor).await
    }

    async fn tick(&self) -> Result<TickReport, Error> {
        let _loop = self.loop_lock.lock().await;
        self.tick_locked().await
    }

    async fn graph(&self, root: &str) -> Result<GraphView, Error> {
        self.graph_view(root).await
    }

    fn loop_status(&self) -> Option<LoopStatus> {
        let state = self.state();
        Some(LoopStatus {
            last_tick: state.last_tick,
            runs_in_flight: state.runs.len(),
            lock: state.lock,
        })
    }

    async fn review_decision(
        &self,
        proposal: &str,
        decision: rustykrab_core::proposal::ReviewDecision,
        actor: &str,
    ) -> Result<rustykrab_core::proposal::ReviewOutcome, Error> {
        let _loop = self.loop_lock.lock().await;
        self.review_decision_locked(proposal, decision, actor).await
    }
}

#[async_trait]
impl WorkBackend for Controller {
    async fn file(
        &self,
        draft: WorkItemDraft,
        provenance: Provenance,
    ) -> rustykrab_core::Result<PlanOutcome> {
        let plan = self.wrap_draft(draft, &provenance).await?;
        self.file_plan(plan, provenance, FilingSource::WorkFile)
            .await
    }

    async fn status(
        &self,
        query: StatusQuery,
        principal: &Principal,
    ) -> rustykrab_core::Result<Vec<WorkStatusView>> {
        self.status_views(query, principal).await
    }

    async fn report(
        &self,
        item: WorkItemId,
        report: ResultReport,
        provenance: Provenance,
    ) -> rustykrab_core::Result<()> {
        self.accept_report(item, report, provenance).await
    }

    fn tool_state(&self, name: &str) -> ToolState {
        self.catalog.tool_state(name)
    }

    fn mcp_server_configured(&self, name: &str) -> bool {
        self.catalog.mcp_server_configured(name)
    }
}
