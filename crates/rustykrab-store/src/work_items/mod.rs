//! Work items: the durable half of the control layer.
//!
//! `docs/plans/control-layer-and-worker-fleet.md`, section 13. The controller
//! in `rustykrab-control` computes readiness, cascade and roll-up over an
//! in-memory snapshot; this module is where that snapshot is read from and
//! where every decision the controller makes is written back. The shared
//! vocabulary is `rustykrab_core::work`; the few types only the store needs
//! (a filter, transition and re-point specs, the plan, outbox and archive
//! rows, the typed error) are defined here.
//!
//! What the store guarantees, whatever the caller does:
//!
//! - **Closed is final.** A transition out of `done | failed | cancelled |
//!   expired` is refused with [`WorkStoreError::Closed`], and so is one whose
//!   `expected_from` no longer matches, so two writers racing on one item
//!   cannot both win.
//! - **Every status change is an event.** The status columns and the
//!   `work_item_events` row are written in one transaction. A note event
//!   (rung, rejection, warning) may not carry a status, so the log never
//!   claims a status the row does not have.
//! - **One call, one transaction.** Each method is one `with_conn` closure
//!   and, where it writes more than one row, one transaction.
//!   [`Store::work_apply`] runs a caller-ordered list of writes in one
//!   transaction, so a filing with its supersede, a closing transition with
//!   its cascade, and the outbox notice they cause land together or not at
//!   all (sections 4.4, 4.5 and 6.7).
//! - **Unreadable rows read as failed.** An unknown status, an unknown kind,
//!   or a JSON or time column that does not parse makes the whole item read
//!   as `failed`: a row the controller cannot interpret never becomes work
//!   that runs early (section 13). An unknown edge kind reads as `blocks`.
//! - **A lease lives only while its item is active.** Acquiring one moves the
//!   item from `ready` to `leased`; a transition into a waiting or closed
//!   status drops it in the same transaction.
//!
//! What it does not decide: readiness, cascade, roll-up, filing validation
//! and which items age belong to the control crate. The store writes what it
//! is told, in one piece, and refuses only what would corrupt the record.
//!
//! References are enforced where they are ownership and left as plain text
//! where they are history. `work_item_deps.item` and `leases.item` cascade
//! from `work_items`; `depends_on`, `parent`, `status_origin`, `plan_id`,
//! `held_by`, and every id in the event, evidence, outbox, plan and archive
//! tables are unenforced, because a `supersedes` or `discovered_from` edge,
//! an event or a piece of evidence must outlive the live row it names
//! (section 4.6).
//!
//! The API is `impl Store` methods prefixed `work_`, not a repository handle
//! like the rest of the crate: see this crate's `ARCHITECTURE.md`.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use rustykrab_core::work::{
    CancelReason, Edge, EdgeKind, EventKind, Evidence, InputRef, Lease, Status, WorkEvent,
    WorkItem, WorkItemId, WorkKind,
};
use rustykrab_core::Error;

use crate::{with_conn, Store};

mod ops;
mod rows;
#[cfg(test)]
mod tests;

// ── errors ─────────────────────────────────────────────────────────────

/// Why a work-item call was refused or failed. The refusals are typed so the
/// controller can tell "someone else already moved this item" from a broken
/// disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkStoreError {
    /// Nothing live has this id: a work item, an edge, an input, a lease, a
    /// plan or an outbox row, named in the message.
    NotFound(String),
    /// An insert named an id that is already live.
    AlreadyExists(String),
    /// Closed is final: the item is already `done`, `failed`, `cancelled` or
    /// `expired`, and no transition leaves it.
    Closed { item: WorkItemId, status: Status },
    /// The caller expected the item in one status and found another.
    StatusMismatch {
        item: WorkItemId,
        expected: Status,
        actual: Status,
    },
    /// The item already has a lease.
    LeaseHeld { item: WorkItemId, worker: String },
    /// Compaction was given an item that is not closed.
    NotClosed { item: WorkItemId, status: Status },
    /// A note event carried a status. Status changes go through a transition,
    /// so the log never disagrees with the row.
    EventCarriesStatus { item: WorkItemId, kind: EventKind },
    /// SQLite, serialisation or the blocking pool failed.
    Storage(String),
}

impl fmt::Display for WorkStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkStoreError::NotFound(what) => write!(f, "not found: {what}"),
            WorkStoreError::AlreadyExists(what) => write!(f, "already exists: {what}"),
            WorkStoreError::Closed { item, status } => {
                write!(f, "work item {item} is closed ({status}); closed is final")
            }
            WorkStoreError::StatusMismatch {
                item,
                expected,
                actual,
            } => write!(f, "work item {item} is {actual}, expected {expected}"),
            WorkStoreError::LeaseHeld { item, worker } => {
                write!(f, "work item {item} is already leased to {worker}")
            }
            WorkStoreError::NotClosed { item, status } => {
                write!(f, "work item {item} is {status}; only closed items compact")
            }
            WorkStoreError::EventCarriesStatus { item, kind } => write!(
                f,
                "a {} event on {item} may not carry a status; use a transition",
                kind.as_str()
            ),
            WorkStoreError::Storage(msg) => write!(f, "storage error: {msg}"),
        }
    }
}

impl std::error::Error for WorkStoreError {}

impl From<rusqlite::Error> for WorkStoreError {
    fn from(e: rusqlite::Error) -> Self {
        WorkStoreError::Storage(e.to_string())
    }
}

impl From<serde_json::Error> for WorkStoreError {
    fn from(e: serde_json::Error) -> Self {
        WorkStoreError::Storage(e.to_string())
    }
}

impl From<Error> for WorkStoreError {
    fn from(e: Error) -> Self {
        WorkStoreError::Storage(e.to_string())
    }
}

/// So a caller that speaks the crate-wide error can use `?`. The refusals
/// become `Internal`: each one means the caller's view of the item was
/// stale or wrong, which is a controller defect, not a storage failure.
impl From<WorkStoreError> for Error {
    fn from(e: WorkStoreError) -> Self {
        match e {
            WorkStoreError::NotFound(what) => Error::NotFound(what),
            WorkStoreError::AlreadyExists(what) => Error::AlreadyExists(what),
            WorkStoreError::Storage(msg) => Error::Storage(msg),
            other => Error::Internal(other.to_string()),
        }
    }
}

// ── crate-local types ──────────────────────────────────────────────────

/// A query over live work items (archived items are never included).
///
/// `status`, when set, matches the status column and, for `blocked` and
/// `cancelled`, the reason too, and it overrides `include_closed`. Without
/// it, closed items are left out unless `include_closed` is set.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkFilter {
    pub status: Option<Status>,
    pub kind: Option<WorkKind>,
    pub parent: Option<WorkItemId>,
    pub include_closed: bool,
}

/// One status change, as [`Store::work_transition`] takes it and
/// [`Store::work_transition_many`] takes a list of them.
///
/// `origin` is written to the item's `status_origin` column as well as the
/// event, so it names the root cause of whatever status the item is now in
/// and is cleared by a transition that gives none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionSpec {
    pub item: WorkItemId,
    /// Refuse unless the item is in exactly this status (reason included).
    pub expected_from: Option<Status>,
    pub to: Status,
    /// The event's kind. [`TransitionSpec::new`] picks `cascade` for the
    /// cascade-only statuses and `transition` otherwise; a resume correction
    /// sets `resume`.
    pub kind: EventKind,
    /// `controller | worker:<name> | user | policy | planner`
    pub actor: String,
    pub reason: Option<String>,
    /// The direct upstream, for cascades.
    pub upstream: Option<WorkItemId>,
    /// The root-cause item, for cascades and supersedes.
    pub origin: Option<WorkItemId>,
    pub evidence_ref: Option<String>,
}

impl TransitionSpec {
    pub fn new(item: impl Into<WorkItemId>, to: Status, actor: impl Into<String>) -> Self {
        TransitionSpec {
            item: item.into(),
            expected_from: None,
            to,
            kind: default_event_kind(&to),
            actor: actor.into(),
            reason: None,
            upstream: None,
            origin: None,
            evidence_ref: None,
        }
    }
}

/// `cascade` for the statuses only a cascade sets (section 4.5), else
/// `transition`.
fn default_event_kind(to: &Status) -> EventKind {
    match to {
        Status::Blocked(r) if r.is_cascade() => EventKind::Cascade,
        Status::Cancelled(CancelReason::Cascade) => EventKind::Cascade,
        _ => EventKind::Transition,
    }
}

/// Moves one reference from `old_upstream` to `new_upstream` on `item`
/// (section 4.4). With `kind`, the edge row is rewritten in place and an
/// `inputs_from` entry naming the old upstream follows it; without, only
/// the `inputs_from` entry moves, for an input ordered through another path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepointSpec {
    pub item: WorkItemId,
    pub kind: Option<EdgeKind>,
    pub old_upstream: WorkItemId,
    pub new_upstream: WorkItemId,
    pub origin: Option<WorkItemId>,
    pub actor: String,
}

/// One accepted `work_plan` call (`work_plans`), which `work plan <id>`
/// previews.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkPlanRow {
    pub id: String,
    pub root: WorkItemId,
    /// The planning item that filed it, when there was one.
    pub filed_by: Option<WorkItemId>,
    pub rationale: String,
    /// The approval question holding part of the plan (section 6.1).
    pub approval_question: Option<String>,
    /// The policy that required approval.
    pub policy: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// A notice to write into `work_outbox`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxDraft {
    /// The parent the notice reports on; a single item is its own parent.
    pub parent: WorkItemId,
    pub origin: Option<WorkItemId>,
    pub channel: String,
    pub body: String,
}

/// One `work_outbox` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxRow {
    pub id: String,
    pub parent: WorkItemId,
    pub origin: Option<WorkItemId>,
    pub channel: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
}

/// One `work_item_archive` row: what is left of a compacted item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchivedItem {
    pub id: WorkItemId,
    pub kind: WorkKind,
    pub title: String,
    pub parent: Option<WorkItemId>,
    /// The closed status and its reason.
    pub status: Status,
    /// The worker that last held its lease, from the `lease` events.
    pub worker: Option<String>,
    /// What it spent. Nothing records spend yet, so this is `None` until
    /// something does; the column exists so aging need not change then.
    pub cost: Option<serde_json::Value>,
    pub closed_at: DateTime<Utc>,
    pub archived_at: DateTime<Utc>,
    /// One line built from the typed fields; no model is called.
    pub summary: String,
    /// The edges the item held when it was compacted.
    pub edges: Vec<Edge>,
}

/// One write in a [`Store::work_apply`] batch. Applied in the order given.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkOp {
    /// Insert a new item. Fails if the id is live.
    Insert(Box<WorkItem>),
    /// Record an accepted `work_plan` call.
    Plan(WorkPlanRow),
    /// Add an edge; the downstream must be live. An existing identical row
    /// is left alone.
    AddEdge(Edge),
    /// Drop the ordering edges an item holds (a superseded item's upstream
    /// edges, section 4.4); its `supersedes` and `discovered_from` edges are
    /// history and stay.
    RemoveOrderingEdgesOf(WorkItemId),
    Repoint(RepointSpec),
    Transition(TransitionSpec),
    /// An event that changes no status: a rung, a rejection, a warning, a
    /// resume note.
    Note(WorkEvent),
    Outbox(OutboxDraft),
}

/// What a [`Store::work_apply`] batch wrote, in op order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkApplied {
    /// The events written by transitions, re-points and notes.
    pub events: Vec<WorkEvent>,
    /// The ids of the outbox rows written.
    pub outbox_ids: Vec<String>,
}

// ── the API ────────────────────────────────────────────────────────────

impl Store {
    /// Run `f` on the connection, on the blocking pool.
    async fn work_call<T, F>(&self, f: F) -> Result<T, WorkStoreError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T, WorkStoreError> + Send + 'static,
        T: Send + 'static,
    {
        with_conn(&self.conn, move |conn| Ok(f(conn))).await?
    }

    /// Run `f` in one transaction: committed if it returns `Ok`, rolled back
    /// otherwise.
    async fn work_tx<T, F>(&self, f: F) -> Result<T, WorkStoreError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T, WorkStoreError> + Send + 'static,
        T: Send + 'static,
    {
        self.work_call(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let out = f(&tx)?;
            tx.commit()?;
            Ok(out)
        })
        .await
    }

    /// File a graph: its items, its edges and, for a `work_plan` call, its
    /// plan row, in one transaction. Accepted whole or not at all.
    pub async fn work_insert_graph(
        &self,
        items: &[WorkItem],
        edges: &[Edge],
        plan: Option<&WorkPlanRow>,
    ) -> Result<(), WorkStoreError> {
        let mut batch: Vec<WorkOp> = items
            .iter()
            .map(|i| WorkOp::Insert(Box::new(i.clone())))
            .collect();
        batch.extend(plan.cloned().map(WorkOp::Plan));
        batch.extend(edges.iter().cloned().map(WorkOp::AddEdge));
        self.work_apply(batch).await.map(|_| ())
    }

    /// Apply a list of writes in one transaction, in order: all of them or
    /// none. This is how the controller writes a transition together with
    /// its cascade, its re-points and the outbox notice it causes.
    pub async fn work_apply(&self, batch: Vec<WorkOp>) -> Result<WorkApplied, WorkStoreError> {
        let now = Utc::now();
        self.work_tx(move |conn| {
            let mut applied = WorkApplied::default();
            for op in &batch {
                ops::apply(conn, op, now, &mut applied)?;
            }
            Ok(applied)
        })
        .await
    }

    pub async fn work_get(&self, id: &str) -> Result<Option<WorkItem>, WorkStoreError> {
        let id = id.to_string();
        self.work_call(move |conn| ops::get_item(conn, &id)).await
    }

    /// Live items matching `filter`, oldest first.
    pub async fn work_list(&self, filter: &WorkFilter) -> Result<Vec<WorkItem>, WorkStoreError> {
        let filter = filter.clone();
        self.work_call(move |conn| ops::list_items(conn, &filter))
            .await
    }

    /// Every live child of `parent`, closed ones included, oldest first:
    /// what a roll-up reads.
    pub async fn work_children(&self, parent: &str) -> Result<Vec<WorkItem>, WorkStoreError> {
        self.work_list(&WorkFilter {
            parent: Some(parent.to_string()),
            include_closed: true,
            ..WorkFilter::default()
        })
        .await
    }

    /// Every item whose status column is not closed, oldest first. A row
    /// whose columns do not parse is included and reads as `failed`, so the
    /// controller sees it and holds what depends on it.
    pub async fn work_open_items(&self) -> Result<Vec<WorkItem>, WorkStoreError> {
        self.work_list(&WorkFilter::default()).await
    }

    /// Every edge with an open item at either end: with
    /// [`Store::work_open_items`], the controller's snapshot.
    pub async fn work_edges_all_open(&self) -> Result<Vec<Edge>, WorkStoreError> {
        self.work_call(ops::edges_all_open).await
    }

    /// The edges `item` holds: what it waits on.
    pub async fn work_edges_of(&self, item: &str) -> Result<Vec<Edge>, WorkStoreError> {
        let item = item.to_string();
        self.work_call(move |conn| ops::edges_of(conn, &item)).await
    }

    /// The edges naming `upstream`: its dependents.
    pub async fn work_dependents_of(&self, upstream: &str) -> Result<Vec<Edge>, WorkStoreError> {
        let upstream = upstream.to_string();
        self.work_call(move |conn| ops::dependents_of(conn, &upstream))
            .await
    }

    /// Move one item to `to`: the status columns, `closed_at` when it closes,
    /// and the event, in one transaction. Refused if the item is closed, or
    /// if `expected_from` is given and does not match.
    #[allow(clippy::too_many_arguments)]
    pub async fn work_transition(
        &self,
        id: &str,
        expected_from: Option<Status>,
        to: Status,
        actor: &str,
        reason: Option<&str>,
        upstream: Option<&str>,
        origin: Option<&str>,
        evidence_ref: Option<&str>,
    ) -> Result<WorkEvent, WorkStoreError> {
        let spec = TransitionSpec {
            expected_from,
            reason: reason.map(str::to_string),
            upstream: upstream.map(str::to_string),
            origin: origin.map(str::to_string),
            evidence_ref: evidence_ref.map(str::to_string),
            ..TransitionSpec::new(id, to, actor)
        };
        let now = Utc::now();
        self.work_tx(move |conn| ops::transition(conn, &spec, now))
            .await
    }

    /// Apply transitions in order, all or nothing: one refusal rolls back
    /// every transition before it. For cascades.
    pub async fn work_transition_many(
        &self,
        specs: &[TransitionSpec],
    ) -> Result<Vec<WorkEvent>, WorkStoreError> {
        let batch = specs.iter().cloned().map(WorkOp::Transition).collect();
        Ok(self.work_apply(batch).await?.events)
    }

    /// Add edges, all or nothing. Every downstream must be live; an edge that
    /// already exists is left alone. Returns how many rows were new.
    pub async fn work_add_edges(&self, edges: &[Edge]) -> Result<usize, WorkStoreError> {
        let edges = edges.to_vec();
        self.work_tx(move |conn| {
            let mut added = 0;
            for edge in &edges {
                if ops::add_edge(conn, edge)? {
                    added += 1;
                }
            }
            Ok(added)
        })
        .await
    }

    /// Re-point one edge of `item` from `old_upstream` to `new_upstream`,
    /// rewriting `depends_on` in place; an `inputs_from` entry naming the old
    /// upstream follows. Leaves a `repoint` event whose `upstream` is the old
    /// value.
    pub async fn work_repoint_edge(
        &self,
        item: &str,
        kind: EdgeKind,
        old_upstream: &str,
        new_upstream: &str,
        origin: Option<&str>,
        actor: &str,
    ) -> Result<WorkEvent, WorkStoreError> {
        let spec = RepointSpec {
            item: item.to_string(),
            kind: Some(kind),
            old_upstream: old_upstream.to_string(),
            new_upstream: new_upstream.to_string(),
            origin: origin.map(str::to_string),
            actor: actor.to_string(),
        };
        let now = Utc::now();
        self.work_tx(move |conn| ops::repoint(conn, &spec, now))
            .await
    }

    /// Re-point an `inputs_from` entry that has no edge of its own (an input
    /// ordered transitively or through an ancestor, section 4.3).
    pub async fn work_repoint_input(
        &self,
        item: &str,
        old_upstream: &str,
        new_upstream: &str,
        origin: Option<&str>,
        actor: &str,
    ) -> Result<WorkEvent, WorkStoreError> {
        let spec = RepointSpec {
            item: item.to_string(),
            kind: None,
            old_upstream: old_upstream.to_string(),
            new_upstream: new_upstream.to_string(),
            origin: origin.map(str::to_string),
            actor: actor.to_string(),
        };
        let now = Utc::now();
        self.work_tx(move |conn| ops::repoint(conn, &spec, now))
            .await
    }

    /// Drop the ordering edges `item` holds, for a superseded item
    /// (section 4.4). Its `supersedes` and `discovered_from` edges are
    /// history and stay. Returns how many rows went.
    pub async fn work_remove_edges_of(&self, item: &str) -> Result<usize, WorkStoreError> {
        let item = item.to_string();
        self.work_call(move |conn| ops::remove_ordering_edges_of(conn, &item))
            .await
    }

    /// Append an event that changes no status: a rung climbed, a rejected
    /// filing, a warning, a resume note. Refused if it carries a `to` status.
    pub async fn work_event_append(&self, event: &WorkEvent) -> Result<(), WorkStoreError> {
        let event = event.clone();
        self.work_call(move |conn| ops::append_note(conn, &event))
            .await
    }

    /// Lease a `ready` item to `worker`: the lease row, the move to `leased`
    /// and a `lease` event (actor `worker:<worker>`), in one transaction.
    /// Refused if the item is closed, already leased, or not `ready`.
    pub async fn work_lease_acquire(
        &self,
        item: &str,
        worker: &str,
        ttl_seconds: u64,
        inputs: Vec<InputRef>,
    ) -> Result<Lease, WorkStoreError> {
        let (item, worker) = (item.to_string(), worker.to_string());
        let now = Utc::now();
        self.work_tx(move |conn| ops::lease_acquire(conn, &item, &worker, ttl_seconds, inputs, now))
            .await
    }

    /// Record a heartbeat on the item's lease and return it.
    pub async fn work_lease_heartbeat(&self, item: &str) -> Result<Lease, WorkStoreError> {
        let item = item.to_string();
        let now = Utc::now();
        self.work_call(move |conn| ops::lease_heartbeat(conn, &item, now))
            .await
    }

    /// Drop the item's lease, returning it if there was one. Changes no
    /// status: the controller's transition says what happened.
    pub async fn work_lease_release(&self, item: &str) -> Result<Option<Lease>, WorkStoreError> {
        let item = item.to_string();
        self.work_tx(move |conn| ops::lease_release(conn, &item))
            .await
    }

    /// Leases whose last heartbeat is more than their TTL before `now`.
    pub async fn work_leases_expired(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<Lease>, WorkStoreError> {
        self.work_call(move |conn| ops::leases_expired(conn, now))
            .await
    }

    pub async fn work_lease_get(&self, item: &str) -> Result<Option<Lease>, WorkStoreError> {
        let item = item.to_string();
        self.work_call(move |conn| ops::get_lease(conn, &item))
            .await
    }

    /// Attach evidence to a live item.
    pub async fn work_evidence_add(&self, evidence: Evidence) -> Result<(), WorkStoreError> {
        self.work_call(move |conn| ops::add_evidence(conn, &evidence))
            .await
    }

    /// An item's evidence, oldest first. Live or archived.
    pub async fn work_evidence_list(&self, item: &str) -> Result<Vec<Evidence>, WorkStoreError> {
        let item = item.to_string();
        self.work_call(move |conn| ops::evidence_of(conn, &item))
            .await
    }

    /// An item's events, oldest first. Live or archived.
    pub async fn work_events(&self, item: &str) -> Result<Vec<WorkEvent>, WorkStoreError> {
        let item = item.to_string();
        self.work_call(move |conn| ops::events_of(conn, &item))
            .await
    }

    /// Every event at or after `at`, oldest first.
    pub async fn work_events_since(
        &self,
        at: DateTime<Utc>,
    ) -> Result<Vec<WorkEvent>, WorkStoreError> {
        self.work_call(move |conn| ops::events_since(conn, at))
            .await
    }

    /// Queue a notice. Returns its id.
    pub async fn work_outbox_enqueue(
        &self,
        parent: &str,
        origin: Option<&str>,
        channel: &str,
        body: &str,
    ) -> Result<String, WorkStoreError> {
        let draft = OutboxDraft {
            parent: parent.to_string(),
            origin: origin.map(str::to_string),
            channel: channel.to_string(),
            body: body.to_string(),
        };
        let now = Utc::now();
        self.work_call(move |conn| ops::enqueue_outbox(conn, &draft, now))
            .await
    }

    /// Undelivered notices, oldest first. Coalescing per parent is the
    /// notifier's.
    pub async fn work_outbox_pending(&self) -> Result<Vec<OutboxRow>, WorkStoreError> {
        self.work_call(ops::outbox_pending).await
    }

    /// Mark a notice delivered. `false` if it already was, so a notifier
    /// racing a restart can tell it lost.
    pub async fn work_outbox_mark_delivered(&self, id: &str) -> Result<bool, WorkStoreError> {
        let id = id.to_string();
        let now = Utc::now();
        self.work_call(move |conn| ops::outbox_mark_delivered(conn, &id, now))
            .await
    }

    /// Compact closed items into `work_item_archive`, all in one transaction:
    /// for each, write the archive line and delete its `work_items` row and
    /// the edges it holds. Events and evidence stay. Which items age is the
    /// control crate's call (section 4.6); this refuses any that is not
    /// closed. An id already archived is skipped. Returns how many were
    /// compacted.
    pub async fn work_archive_compact(
        &self,
        ids: &[WorkItemId],
        now: DateTime<Utc>,
    ) -> Result<usize, WorkStoreError> {
        let ids = ids.to_vec();
        self.work_tx(move |conn| {
            let mut compacted = 0;
            for id in &ids {
                if ops::archive_one(conn, id, now)? {
                    compacted += 1;
                }
            }
            Ok(compacted)
        })
        .await
    }

    /// Archived items, most recently closed first, optionally of one kind
    /// and closed at or after `since`.
    pub async fn work_archive_list(
        &self,
        kind: Option<WorkKind>,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<ArchivedItem>, WorkStoreError> {
        self.work_call(move |conn| ops::archive_list(conn, kind, since))
            .await
    }

    pub async fn work_archive_get(&self, id: &str) -> Result<Option<ArchivedItem>, WorkStoreError> {
        let id = id.to_string();
        self.work_call(move |conn| ops::archive_get(conn, &id))
            .await
    }

    /// Archived items whose id is `text` or whose title or summary contains
    /// it (ASCII case-insensitive), most recently closed first.
    pub async fn work_archive_search(
        &self,
        text: &str,
    ) -> Result<Vec<ArchivedItem>, WorkStoreError> {
        let text = text.to_string();
        self.work_call(move |conn| ops::archive_search(conn, &text))
            .await
    }

    pub async fn work_plan_get(&self, id: &str) -> Result<Option<WorkPlanRow>, WorkStoreError> {
        let id = id.to_string();
        self.work_call(move |conn| ops::plan_get(conn, &id)).await
    }
}
