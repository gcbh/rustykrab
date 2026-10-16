//! The controller's handle: what the gateway routes, the CLI and the work
//! tools call. Defined here so those callers can be built against it while
//! the controller itself is written; `controller::Controller` implements it.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rustykrab_core::proposal::{ReviewDecision, ReviewOutcome};
use rustykrab_core::work::{
    Edge, ItemRef, PlanOutcome, Status, WorkItem, WorkItemDraft, WorkItemId, WorkPlan,
};
use rustykrab_core::Error;
use rustykrab_store::{JudgmentRow, QuestionRow};
use rustykrab_tools::work_backend::Provenance;
use serde::{Deserialize, Serialize};

use crate::graph::FilingSource;
pub use rustykrab_tools::work_discuss::WorkDiscussion;

/// One node of a `work show --graph` tree (plan section 14.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    pub item: WorkItem,
    /// Depth below the root; the root is 0.
    pub depth: u32,
    /// Edges the item holds (it is the downstream).
    pub edges: Vec<Edge>,
    /// The computed roll-up for a parent, `None` for a leaf.
    pub rollup: Option<Status>,
    pub children_done: u32,
    pub children_total: u32,
    /// Set when the item has aged into the archive: its one-line summary.
    pub archived_summary: Option<String>,
}

/// The tree under a root, in depth-first order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphView {
    pub root: WorkItemId,
    pub nodes: Vec<GraphNode>,
}

/// What one pass of the loop did (plan section 6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickReport {
    pub made_ready: Vec<WorkItemId>,
    pub leased: Vec<WorkItemId>,
    pub reconciled: Vec<WorkItemId>,
    pub expired: Vec<WorkItemId>,
    pub archived: Vec<WorkItemId>,
    pub transitions: usize,
    pub notices: usize,
}

/// What the loop reports about itself, for `GET /api/version`: read from
/// memory, never from the store, so asking cannot wait on a tick.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopStatus {
    /// When the last tick that completed finished; `None` before the first.
    pub last_tick: Option<DateTime<Utc>>,
    /// Runs live in this process.
    pub runs_in_flight: usize,
    /// When the last tick that failed gave up; `None` before the first
    /// failure. Kept after a later tick completes, so a recovered loop
    /// still shows when it last failed.
    pub last_failed_tick: Option<DateTime<Utc>>,
    /// The error class of that failure (`storage`, `internal`, ...), the
    /// variant of the error the tick returned; `None` before the first.
    pub last_failure_class: Option<String>,
    /// Ticks that failed in a row since the last one that completed; 0
    /// once a tick completes. A stuck loop shows an old `last_tick` and a
    /// count that stops moving; a failing one shows the count climbing.
    pub consecutive_failed_ticks: u32,
    /// Whether this process holds `controller.lock` and may tick; `None`
    /// before the loop first tries it, and for a controller no loop drives.
    #[serde(default)]
    pub lock: Option<LockState>,
    /// The daemon is shutting down: ticks lease nothing new but still
    /// reconcile what finishes.
    #[serde(default)]
    pub draining: bool,
}

/// The loop's hold on its data directory's `controller.lock`
/// ([`crate::lock`]): only the holder ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockState {
    /// This process holds the lock and runs the loop.
    Held,
    /// Another process holds it; no tick runs until it is released.
    Waiting,
}

/// What an answer did (`POST /api/questions/{id}/answer`, `/answer`,
/// `work answer`; plan section 7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnswerReply {
    pub question: QuestionRow,
    /// Items that resumed on it: the item, not the conversation.
    #[serde(default)]
    pub resumed: Vec<WorkItemId>,
    /// Items an approval released.
    #[serde(default)]
    pub released: Vec<WorkItemId>,
    /// Items a rejection or a declined consent cancelled.
    #[serde(default)]
    pub cancelled: Vec<WorkItemId>,
}

/// The standing judgment in force (plan section 7): every active grant with
/// what it compiled to, and the rules they add up to, one line each.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgmentView {
    pub grants: Vec<JudgmentRow>,
    pub rules: Vec<String>,
}

fn unsupported(what: &str) -> Error {
    Error::Internal(format!("{what} is not supported by this controller"))
}

/// The controller as its callers see it. Every method is one store
/// transaction or one pass of the loop; none lets a caller set a status.
#[async_trait]
pub trait ControlHandle: Send + Sync {
    /// File a whole graph through validation (plan section 14.1). Every
    /// filing path uses this: the planner, a `work_file` draft wrapped in a
    /// plan, a worker's `discovered` drafts, the delivery import.
    async fn file_plan(
        &self,
        plan: WorkPlan,
        provenance: Provenance,
        source: FilingSource,
    ) -> Result<PlanOutcome, Error>;

    /// File one draft as a one-item plan (`work_file`'s contract, 14.1),
    /// through the same validator, as `FilingSource::WorkFile`. The REST
    /// face of `work_file`; the model-facing tool reaches the same path
    /// through the controller's `WorkBackend`.
    async fn file_draft(
        &self,
        mut draft: WorkItemDraft,
        provenance: Provenance,
    ) -> Result<PlanOutcome, Error> {
        let tmp = draft.tmp.get_or_insert_with(|| "draft".to_string()).clone();
        let plan = WorkPlan {
            root: ItemRef::Tmp { tmp },
            items: vec![draft],
            edges: Vec::new(),
            rationale: String::new(),
        };
        self.file_plan(plan, provenance, FilingSource::WorkFile)
            .await
    }

    /// Release the items held for approval under `root` (plan section 6.1).
    /// Returns the released ids.
    async fn approve(&self, root: &str, actor: &str) -> Result<Vec<WorkItemId>, Error>;

    /// Start work and record the human's one-time/automatic preference atomically.
    async fn start_work(
        &self,
        item: &str,
        mode: rustykrab_core::work_preference::WorkStartMode,
        daily_limit: u32,
        note: &str,
        actor: &str,
    ) -> Result<rustykrab_core::work_preference::WorkStartReply, Error> {
        let _ = (item, mode, daily_limit, note, actor);
        Err(unsupported("reviewed work templates"))
    }

    /// Decline a pending plan: the held items under `root` become
    /// `cancelled(requested)` and cascade (plan section 4.5). Returns the
    /// cancelled ids.
    async fn reject(
        &self,
        root: &str,
        reason: Option<String>,
        actor: &str,
    ) -> Result<Vec<WorkItemId>, Error>;

    /// Cancel an item and its open subtree (`cancelled(requested)`, then
    /// cascade). Returns the cancelled ids; a leased child's lease is revoked.
    async fn cancel(
        &self,
        item: &str,
        reason: Option<String>,
        actor: &str,
    ) -> Result<Vec<WorkItemId>, Error>;

    /// One pass of the loop: sweep expiries and time triggers, settle
    /// readiness and roll-ups, lease and run ready items on available
    /// workers, reconcile finished runs, climb ladders, write notices, age
    /// closed items. The daemon calls it on a timer; tests and the CLI call
    /// it directly.
    async fn tick(&self) -> Result<TickReport, Error>;

    /// The tree under `root` for `work show --graph` and
    /// `GET /api/work/{id}/graph`.
    async fn graph(&self, root: &str) -> Result<GraphView, Error>;

    /// Current derived queue ranks, using the scheduler's complete snapshot.
    async fn queue_priorities(
        &self,
    ) -> Result<std::collections::HashMap<WorkItemId, crate::graph::SchedulingPriority>, Error>
    {
        Ok(std::collections::HashMap::new())
    }

    /// The same ordered pending leaves used by dispatch, including held work.
    async fn priority_list(&self) -> Result<Option<crate::graph::PriorityList>, Error> {
        Ok(None)
    }

    /// Human-owned ordering; changes urgency, never authorization or status.
    async fn reorder_priority(
        &self,
        _revision: &str,
        _ordered: &[WorkItemId],
        _actor: &str,
    ) -> Result<crate::graph::PriorityList, Error> {
        Err(unsupported("priority ordering"))
    }

    /// Everything on record about a failure, for `work investigate` and
    /// `GET /api/work/investigate/{target}`: `target` is a failure
    /// fingerprint (or an unambiguous prefix of one) or a work item's id.
    /// `None` when the store knows neither. The same call fills the
    /// investigation bundle a fix item's run is given
    /// ([`crate::investigate`]). The default refuses, for a handle with no
    /// store behind it.
    async fn investigate(
        &self,
        _target: &str,
    ) -> Result<Option<crate::investigate::FailureHistory>, Error> {
        Err(unsupported("failure histories"))
    }

    /// The loop's own state, or `None` for a handle that runs no loop.
    fn loop_status(&self) -> Option<LoopStatus> {
        None
    }

    /// Start (or stop) draining: while it is set a tick leases nothing
    /// new, and still sweeps and reconciles, so the runs in flight can
    /// finish before the daemon exits. A handle that runs no loop ignores
    /// it.
    fn set_draining(&self, _draining: bool) {}

    /// Apply a decision a human took on the review surface to a proposal
    /// (plan sections 10 and 11), as one transaction with its `review`
    /// event: an acceptance files the `code` item the proposal becomes
    /// (`FilingSource::Proposal`, with a `discovered_from` edge onto it)
    /// and closes the proposal `done`; a decline cancels it; an amendment
    /// is recorded for the acceptance to carry. A proposal already closed
    /// is left alone and says so. The default refuses, for a handle that
    /// takes no decisions.
    async fn review_decision(
        &self,
        proposal: &str,
        decision: ReviewDecision,
        actor: &str,
    ) -> Result<ReviewOutcome, Error> {
        let _ = (decision, actor);
        Err(Error::Internal(format!(
            "this control handle takes no review decisions (proposal {proposal})"
        )))
    }

    /// Answer a question (plan section 7): its id, or the first eight
    /// characters of it. The answer resumes the item that asked; a plan's
    /// approval question takes approve or reject.
    async fn answer(
        &self,
        _question: &str,
        _answer: &str,
        _actor: &str,
    ) -> Result<AnswerReply, Error> {
        Err(unsupported("answering questions"))
    }

    /// Grant standing judgment in ordinary language; it is compiled to a
    /// checklist and in force from the next filing or question.
    /// Append an attributed comment for future execution without releasing holds.
    async fn comment(
        &self,
        _item: &str,
        _text: &str,
        _actor: &str,
    ) -> Result<rustykrab_core::work::WorkEvent, Error> {
        Err(unsupported("work comments"))
    }

    async fn discussion(
        &self,
        _item: &str,
        _offset: usize,
        _limit: usize,
    ) -> Result<rustykrab_tools::work_discuss::WorkDiscussion, Error> {
        Err(unsupported("work discussion"))
    }

    async fn grant_judgment(
        &self,
        _text: &str,
        _scope: &str,
        _actor: &str,
    ) -> Result<JudgmentRow, Error> {
        Err(unsupported("standing judgment"))
    }

    /// Revoke a grant. `false` when it was already revoked.
    async fn revoke_judgment(&self, _id: &str, _actor: &str) -> Result<bool, Error> {
        Err(unsupported("standing judgment"))
    }

    /// The standing judgment in force.
    async fn judgment(&self) -> Result<JudgmentView, Error> {
        Err(unsupported("standing judgment"))
    }

    /// What foreground work the controller could dispatch right now, from
    /// its own matcher, for analysis that should run only while useful work
    /// cannot ([`DispatchDemand`]). `Ok(None)` means unknown, and unknown is
    /// never taken for idle. Default: unknown.
    async fn foreground_dispatchability(&self) -> Result<Option<DispatchDemand>, Error> {
        Ok(None)
    }
}

/// One observation of the controller's foreground dispatch: project-review
/// jobs (read-only `Research` items) excluded, and leaves whose cumulative
/// budget is spent counted as waiting, not dispatchable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchDemand {
    pub observed_at: DateTime<Utc>,
    /// Foreground runs in flight.
    pub active_foreground_runs: usize,
    /// Ready foreground leaves a healthy worker with free capacity could
    /// take now.
    pub dispatchable_foreground_leaves: usize,
    /// Queued or ready foreground leaves no worker can take now, by why
    /// (`"no_worker"`, `"capacity"`, `"single_writer"`, `"budget_spent"`,
    /// `"precondition"`, ...), each with its count.
    pub waiting: std::collections::BTreeMap<String, usize>,
}
