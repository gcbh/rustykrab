//! `/api/work`: the control layer's HTTP surface
//! (`docs/plans/control-layer-and-worker-fleet.md`, section 14).
//!
//! Reads come straight from the store's `work_*` API. Every write goes
//! through the controller's [`ControlHandle`], so no route sets a status: a
//! filing is validated whole, and approve, reject and cancel enter as typed
//! commands the controller applies (section 11). Each command carries the
//! authenticated principal as its actor (`user:master`, `user:<device>`).
//!
//! The reply types are public so the CLI (`rustykrab work ...`) reads the
//! same shapes this module writes.
//!
//! Without a controller wired into [`AppState`], the read routes still
//! answer from the store (a parent's roll-up then counts its children from
//! the store and repeats its own status column), and every route that needs
//! the controller answers 503 `control_unavailable`.
//!
//! Filings: `POST /api/work` files one `work_file` draft, `POST
//! /api/work/plan` a whole `work_plan` graph, and `POST /api/work/import`
//! a delivery `StackManifest` as its layered `code` graph (the only path by
//! which a `code` graph enters). Each answers with the `PlanOutcome`, 422
//! when rejected, and may carry `origin_conversation_id` so the
//! controller reports to that conversation's channel as it does for a
//! conversation that called `work_file`.
//!
//! `GET /api/work/events` is the SSE progress stream (section 14): every
//! `work_item_events` row written after the stream opened, one frame each,
//! polled from the store by row id so no row is sent twice or skipped.
//! Cascade events carry their `origin`.

use std::collections::{BTreeMap, HashSet};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use chrono::{DateTime, NaiveDate, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_stream::wrappers::ReceiverStream;

use rustykrab_control::graph::FilingSource;
use rustykrab_control::handle::{ControlHandle, GraphView, LockState, TickReport};
use rustykrab_control::import::{self, StackManifest};
use rustykrab_control::ladder::{self, RUNGS};
use rustykrab_control::lock::LOCK_FILE;
use rustykrab_control::Provenance;
use rustykrab_core::work::{
    BlockedReason, Budget, CancelReason, Edge, Evidence, Lease, PlanOutcome, Rung, RungEvent,
    Status, WorkError, WorkEvent, WorkItem, WorkItemDraft, WorkItemId, WorkKind, WorkPlan,
};
use rustykrab_core::Error;
use rustykrab_store::{ArchivedItem, Principal, WorkFilter, WorkPlanRow, WorkStoreError};

use crate::AppState;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/work", get(list_items).post(file_one))
        .route("/api/work/ready", get(ready_items))
        .route("/api/work/events", get(event_stream))
        .route("/api/work/plan", post(file_plan))
        .route("/api/work/import", post(import_slice))
        .route("/api/work/tick", post(tick))
        .route("/api/work/archive", get(archive_list))
        .route("/api/work/archive/{id}", get(archive_get))
        .route("/api/work/{id}", get(item_detail))
        .route("/api/work/{id}/graph", get(item_graph))
        .route("/api/work/{id}/events", get(item_events))
        .route("/api/work/{id}/evidence", get(item_evidence))
        .route("/api/work/{id}/plan", get(plan_preview))
        .route("/api/work/{id}/approve", post(approve))
        .route("/api/work/{id}/reject", post(reject))
        .route("/api/work/{id}/cancel", post(cancel))
}

// ── reply shapes ───────────────────────────────────────────────────────

/// A parent's roll-up (plan section 4.2): its computed status and how many
/// of its children are done.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollupView {
    pub status: Status,
    pub children_done: u32,
    pub children_total: u32,
}

/// One row of `GET /api/work` and `GET /api/work/ready`. The item carries
/// `status_origin`, so a cascade status names its root cause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkRow {
    pub item: WorkItem,
    /// Set for a parent, `None` for a leaf.
    #[serde(default)]
    pub rollup: Option<RollupView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkList {
    pub items: Vec<WorkRow>,
}

/// An edge the item holds, with its upstream's archive line when the
/// upstream has aged out (plan section 4.6: a `discovered_from` or
/// `supersedes` edge onto an archived item resolves to its summary).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeView {
    #[serde(flatten)]
    pub edge: Edge,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<String>,
}

/// `GET /api/work/{id}`: the item with what section 4's item shape keeps
/// beside it, its lease, ladder, last error, evidence and events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemDetail {
    pub item: WorkItem,
    /// What the item waits on: the edges it holds.
    pub edges: Vec<EdgeView>,
    /// What waits on it: the edges naming it.
    pub dependents: Vec<Edge>,
    #[serde(default)]
    pub rollup: Option<RollupView>,
    /// The live lease, with the fan-in inputs copied into its brief. A
    /// lease lives only while the item is active.
    #[serde(default)]
    pub lease: Option<Lease>,
    /// Every lease the item has had, oldest first, from its `lease`
    /// events: which named worker ran each attempt (plan section 5), so a
    /// closed item still names the worker that finished it.
    #[serde(default)]
    pub leases: Vec<LeaseRecord>,
    /// Every rung climbed, oldest first (sections 8 and 9).
    #[serde(default)]
    pub ladder: Vec<RungEvent>,
    /// The error behind the latest rung that carried one.
    #[serde(default)]
    pub last_error: Option<WorkError>,
    /// Oldest first.
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    /// Oldest first.
    #[serde(default)]
    pub events: Vec<WorkEvent>,
}

/// One lease an item had: the worker and when.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub worker: String,
    pub since: DateTime<Utc>,
}

/// `GET /api/work/{id}/graph`: the controller's [`GraphView`] plus the
/// ladder rungs each item has climbed, in first-climbed order, for the
/// items that climbed any.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphReply {
    #[serde(flatten)]
    pub graph: GraphView,
    #[serde(default)]
    pub rungs: BTreeMap<WorkItemId, Vec<Rung>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventsReply {
    pub item: WorkItemId,
    pub events: Vec<WorkEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceReply {
    pub item: WorkItemId,
    pub evidence: Vec<Evidence>,
}

/// `GET /api/work/{id}/plan`: the plan filed under root `{id}` (the one
/// awaiting approval when there is one), its tree, and the items it holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanPreview {
    pub plan: WorkPlanRow,
    pub graph: GraphView,
    pub held: Vec<WorkItemId>,
}

/// `POST /api/work/{id}/approve`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApproveReply {
    pub root: WorkItemId,
    pub released: Vec<WorkItemId>,
}

/// An item of the subtree that was already closed when a cancel or a
/// reject arrived, so it kept its status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinishedItem {
    pub id: WorkItemId,
    pub title: String,
    pub status: Status,
}

/// `POST /api/work/{id}/reject` and `POST /api/work/{id}/cancel`: what was
/// cancelled, and what in the subtree had already finished (section 14.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelReply {
    pub item: WorkItemId,
    pub cancelled: Vec<WorkItemId>,
    #[serde(default)]
    pub already_finished: Vec<FinishedItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveList {
    pub archived: Vec<ArchivedItem>,
}

// ── errors ─────────────────────────────────────────────────────────────

/// The gateway's error shape: a status and `{ "error": code, "message" }`.
#[derive(Debug)]
struct WorkApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl WorkApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    fn not_found(id: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no work item {id}"),
        )
    }

    /// Compaction is lossy and not reversible (plan section 4.6): the live
    /// row is gone for good, and the archive line is what remains.
    fn archived(id: &str) -> Self {
        Self::new(
            StatusCode::GONE,
            "archived",
            format!("work item {id} is archived; see /api/work/archive/{id}"),
        )
    }

    fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "control_unavailable",
            "the control layer is not running in this daemon",
        )
    }
}

impl IntoResponse for WorkApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "error": self.code, "message": self.message })),
        )
            .into_response()
    }
}

impl From<Error> for WorkApiError {
    fn from(error: Error) -> Self {
        match error {
            Error::NotFound(message) => Self::new(StatusCode::NOT_FOUND, "not_found", message),
            Error::AlreadyExists(message) => Self::new(StatusCode::CONFLICT, "conflict", message),
            // The store's refusals (closed is final, a status that moved
            // under the caller, a lease already held) reach the
            // controller's callers as `Internal`: the command met an item
            // in a state that forbids it.
            Error::Internal(message) => {
                tracing::warn!(%message, "work command refused");
                Self::new(StatusCode::CONFLICT, "conflict", message)
            }
            Error::Auth(message) => Self::new(StatusCode::FORBIDDEN, "forbidden", message),
            other => {
                tracing::error!(error = %other, "work API operation failed");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "work operation failed",
                )
            }
        }
    }
}

impl From<WorkStoreError> for WorkApiError {
    fn from(error: WorkStoreError) -> Self {
        Error::from(error).into()
    }
}

// ── request parsing ────────────────────────────────────────────────────

/// A `status` query value: an exact status, or a bare `blocked` or
/// `cancelled` that matches every reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    Exact(Status),
    Name(&'static str),
}

/// Parse `ready`, `blocked`, `blocked(needs_consent)` or
/// `blocked:needs_consent`. Strict, unlike `Status::parse`: a typo is the
/// caller's mistake to hear about, not a row to read conservatively.
pub fn parse_status_filter(raw: &str) -> Result<StatusFilter, String> {
    let raw = raw.trim();
    let (name, reason) = match raw.split_once(['(', ':']) {
        Some((name, rest)) => (name, Some(rest.trim_end_matches(')'))),
        None => (raw, None),
    };
    let bad_reason = |r: &str| format!("unknown reason `{r}` for status `{name}`");
    let exact = match (name, reason) {
        ("queued", None) => Status::Queued,
        ("ready", None) => Status::Ready,
        ("leased", None) => Status::Leased,
        ("running", None) => Status::Running,
        ("verifying", None) => Status::Verifying,
        ("done", None) => Status::Done,
        ("failed", None) => Status::Failed,
        ("expired", None) => Status::Expired,
        ("blocked", None) => return Ok(StatusFilter::Name("blocked")),
        ("cancelled", None) => return Ok(StatusFilter::Name("cancelled")),
        ("blocked", Some(r)) => {
            Status::Blocked(BlockedReason::parse(r).ok_or_else(|| bad_reason(r))?)
        }
        ("cancelled", Some(r)) => {
            Status::Cancelled(CancelReason::parse(r).ok_or_else(|| bad_reason(r))?)
        }
        (
            "queued" | "ready" | "leased" | "running" | "verifying" | "done" | "failed" | "expired",
            Some(_),
        ) => return Err(format!("status `{name}` takes no reason")),
        _ => return Err(format!("unknown status `{raw}`")),
    };
    Ok(StatusFilter::Exact(exact))
}

fn parse_kind(raw: &str) -> Result<WorkKind, WorkApiError> {
    WorkKind::parse(raw.trim())
        .ok_or_else(|| WorkApiError::bad_request(format!("unknown kind `{raw}`")))
}

fn parse_flag(name: &str, raw: Option<&str>) -> Result<bool, WorkApiError> {
    match raw.map(str::trim) {
        None | Some("") | Some("false") | Some("0") => Ok(false),
        Some("true") | Some("1") => Ok(true),
        Some(other) => Err(WorkApiError::bad_request(format!(
            "`{name}` must be true or false, got `{other}`"
        ))),
    }
}

/// RFC 3339, or a bare `YYYY-MM-DD` read as midnight UTC.
fn parse_since(raw: &str) -> Result<DateTime<Utc>, WorkApiError> {
    let raw = raw.trim();
    if let Ok(at) = DateTime::parse_from_rfc3339(raw) {
        return Ok(at.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|at| at.and_utc())
        .ok_or_else(|| {
            WorkApiError::bad_request(format!(
                "`since` must be RFC 3339 or YYYY-MM-DD, got `{raw}`"
            ))
        })
}

/// The optional `{ "reason": "..." }` body of reject and cancel. An empty
/// body is no reason.
fn reason_of(body: &Bytes) -> Result<Option<String>, WorkApiError> {
    #[derive(Deserialize)]
    struct ReasonBody {
        #[serde(default)]
        reason: Option<String>,
    }
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let parsed: ReasonBody = serde_json::from_slice(body)
        .map_err(|e| WorkApiError::bad_request(format!("invalid body: {e}")))?;
    Ok(parsed
        .reason
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty()))
}

/// The event actor for a user command: the authenticated principal.
fn actor_of(principal: Option<Extension<Principal>>) -> String {
    match principal {
        Some(Extension(p)) => format!("user:{}", p.describe()),
        None => "user".to_string(),
    }
}

fn control(state: &AppState) -> Result<Arc<dyn ControlHandle>, WorkApiError> {
    state.control.clone().ok_or_else(WorkApiError::unavailable)
}

/// The rungs a list of events climbed, in first-climbed order. A rung
/// event's reason is its `RungEvent` as JSON, as the controller writes it
/// (`rustykrab_control::ladder::encode_rung_event`).
pub fn rungs_climbed(events: &[WorkEvent]) -> Vec<Rung> {
    let mut climbed = Vec::new();
    for rung in events.iter().filter_map(ladder::rung_event).map(|e| e.rung) {
        if RUNGS.contains(&rung) && !climbed.contains(&rung) {
            climbed.push(rung);
        }
    }
    climbed
}

/// A filing body: the filing itself plus the optional
/// `origin_conversation_id` whose channel the controller reports to.
fn filing_body<T: DeserializeOwned>(
    body: &Bytes,
    what: &str,
) -> Result<(serde_json::Value, T, Option<String>), WorkApiError> {
    let raw: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| WorkApiError::bad_request(format!("invalid {what}: {e}")))?;
    let origin = raw
        .get("origin_conversation_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let parsed: T = serde_json::from_value(raw.clone())
        .map_err(|e| WorkApiError::bad_request(format!("invalid {what}: {e}")))?;
    Ok((raw, parsed, origin))
}

/// A filing's answer: `accepted` with the given status, 422 when rejected
/// with every failed check.
fn outcome_response(outcome: PlanOutcome, accepted: StatusCode) -> Response {
    let status = match outcome {
        PlanOutcome::Accepted(_) => accepted,
        PlanOutcome::Rejected(_) => StatusCode::UNPROCESSABLE_ENTITY,
    };
    (status, Json(outcome)).into_response()
}

// ── helpers over the store and the controller ──────────────────────────

/// 410 for an id that has aged into the archive, else 404.
async fn missing(state: &AppState, id: &str) -> WorkApiError {
    match state.agent.store.work_archive_get(id).await {
        Ok(Some(_)) => WorkApiError::archived(id),
        Ok(None) => WorkApiError::not_found(id),
        Err(e) => e.into(),
    }
}

/// The controller's error for `id`, with a `NotFound` told apart from an
/// archived item.
async fn control_error(state: &AppState, id: &str, error: Error) -> WorkApiError {
    match error {
        Error::NotFound(_) => missing(state, id).await,
        other => other.into(),
    }
}

/// A parent's roll-up: the controller's when it is wired, else counted from
/// the store. `None` for a leaf.
async fn rollup_of(state: &AppState, item: &WorkItem) -> Result<Option<RollupView>, WorkApiError> {
    if let Some(control) = &state.control {
        let view = match control.graph(&item.id).await {
            Ok(view) => view,
            Err(Error::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        return Ok(view
            .nodes
            .into_iter()
            .find(|n| n.item.id == item.id)
            .filter(|n| n.rollup.is_some() || n.children_total > 0)
            .map(|n| RollupView {
                status: n.rollup.unwrap_or(n.item.status),
                children_done: n.children_done,
                children_total: n.children_total,
            }));
    }
    let children = state.agent.store.work_children(&item.id).await?;
    if children.is_empty() {
        return Ok(None);
    }
    Ok(Some(RollupView {
        status: item.status,
        children_done: count_u32(children.iter().filter(|c| c.status == Status::Done)),
        children_total: count_u32(children.iter()),
    }))
}

fn count_u32<I: Iterator>(iter: I) -> u32 {
    u32::try_from(iter.count()).unwrap_or(u32::MAX)
}

/// Rows for `items`, each parent with its roll-up.
async fn with_rollups(state: &AppState, items: Vec<WorkItem>) -> Result<WorkList, WorkApiError> {
    if items.is_empty() {
        return Ok(WorkList { items: Vec::new() });
    }
    let every_live = state
        .agent
        .store
        .work_list(&WorkFilter {
            include_closed: true,
            ..WorkFilter::default()
        })
        .await?;
    let parents: HashSet<WorkItemId> = every_live.into_iter().filter_map(|i| i.parent).collect();
    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        let rollup = if parents.contains(&item.id) {
            rollup_of(state, &item).await?
        } else {
            None
        };
        rows.push(WorkRow { item, rollup });
    }
    Ok(WorkList { items: rows })
}

/// The subtree's items that were already closed, from a graph read before
/// the command.
fn finished_in(graph: Option<&GraphView>, cancelled: &[WorkItemId]) -> Vec<FinishedItem> {
    graph
        .map(|g| {
            g.nodes
                .iter()
                .filter(|n| n.item.status.is_closed() && !cancelled.contains(&n.item.id))
                .map(|n| FinishedItem {
                    id: n.item.id.clone(),
                    title: n.item.title.clone(),
                    status: n.item.status,
                })
                .collect()
        })
        .unwrap_or_default()
}

// ── read handlers ──────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    status: Option<String>,
    kind: Option<String>,
    parent: Option<String>,
    include_closed: Option<String>,
}

/// `GET /api/work`: live items, open ones unless `include_closed`; parents
/// with their roll-up. Archived items never appear (section 4.6).
async fn list_items(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<WorkList>, WorkApiError> {
    let status = query
        .status
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(parse_status_filter)
        .transpose()
        .map_err(WorkApiError::bad_request)?;
    let mut filter = WorkFilter {
        status: None,
        kind: query.kind.as_deref().map(parse_kind).transpose()?,
        parent: query.parent.filter(|p| !p.trim().is_empty()),
        include_closed: parse_flag("include_closed", query.include_closed.as_deref())?,
    };
    let by_name = match status {
        Some(StatusFilter::Exact(s)) => {
            filter.status = Some(s);
            None
        }
        Some(StatusFilter::Name(name)) => {
            // `cancelled` is closed; asking for it asks for closed items.
            filter.include_closed |= name == "cancelled";
            Some(name)
        }
        None => None,
    };
    let mut items = state.agent.store.work_list(&filter).await?;
    if let Some(name) = by_name {
        items.retain(|i| i.status.name() == name);
    }
    Ok(Json(with_rollups(&state, items).await?))
}

/// `GET /api/work/ready`.
async fn ready_items(State(state): State<AppState>) -> Result<Json<WorkList>, WorkApiError> {
    let items = state
        .agent
        .store
        .work_list(&WorkFilter {
            status: Some(Status::Ready),
            ..WorkFilter::default()
        })
        .await?;
    Ok(Json(with_rollups(&state, items).await?))
}

/// `GET /api/work/{id}`: the item, its edges both ways, its roll-up and how
/// much history it has.
async fn item_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ItemDetail>, WorkApiError> {
    let store = &state.agent.store;
    let Some(item) = store.work_get(&id).await? else {
        return Err(missing(&state, &id).await);
    };
    let mut edges = Vec::new();
    for edge in store.work_edges_of(&id).await? {
        let archived = if store.work_get(&edge.depends_on).await?.is_none() {
            store
                .work_archive_get(&edge.depends_on)
                .await?
                .map(|a| a.summary)
        } else {
            None
        };
        edges.push(EdgeView { edge, archived });
    }
    let dependents = store.work_dependents_of(&id).await?;
    let rollup = rollup_of(&state, &item).await?;
    let lease = store.work_lease_get(&id).await?;
    let events = store.work_events(&id).await?;
    let evidence = store.work_evidence_list(&id).await?;
    let leases = events
        .iter()
        .filter(|e| e.kind == rustykrab_core::work::EventKind::Lease)
        .map(|e| LeaseRecord {
            worker: e
                .actor
                .strip_prefix("worker:")
                .unwrap_or(&e.actor)
                .to_string(),
            since: e.at,
        })
        .collect();
    Ok(Json(ItemDetail {
        item,
        edges,
        dependents,
        rollup,
        lease,
        leases,
        ladder: ladder::ladder_of_events(&events),
        last_error: ladder::last_error(&events),
        evidence,
        events,
    }))
}

/// `GET /api/work/{id}/graph`.
async fn item_graph(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<GraphReply>, WorkApiError> {
    let control = control(&state)?;
    let graph = match control.graph(&id).await {
        Ok(graph) => graph,
        Err(e) => return Err(control_error(&state, &id, e).await),
    };
    let mut rungs = BTreeMap::new();
    for node in graph.nodes.iter().filter(|n| n.archived_summary.is_none()) {
        let climbed = rungs_climbed(&state.agent.store.work_events(&node.item.id).await?);
        if !climbed.is_empty() {
            rungs.insert(node.item.id.clone(), climbed);
        }
    }
    Ok(Json(GraphReply { graph, rungs }))
}

/// Whether `id` names anything, live or archived: events and evidence
/// outlive compaction, so both stay readable by id.
async fn known(state: &AppState, id: &str) -> Result<bool, WorkApiError> {
    let store = &state.agent.store;
    Ok(store.work_get(id).await?.is_some() || store.work_archive_get(id).await?.is_some())
}

/// `GET /api/work/{id}/events`, oldest first.
async fn item_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<EventsReply>, WorkApiError> {
    let events = state.agent.store.work_events(&id).await?;
    if events.is_empty() && !known(&state, &id).await? {
        return Err(WorkApiError::not_found(&id));
    }
    Ok(Json(EventsReply { item: id, events }))
}

/// `GET /api/work/{id}/evidence`, oldest first.
async fn item_evidence(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<EvidenceReply>, WorkApiError> {
    let evidence = state.agent.store.work_evidence_list(&id).await?;
    if evidence.is_empty() && !known(&state, &id).await? {
        return Err(WorkApiError::not_found(&id));
    }
    Ok(Json(EvidenceReply { item: id, evidence }))
}

/// `GET /api/work/{id}/plan`: the plan filed under root `{id}`, preferring
/// one whose approval question still holds items, else the latest.
async fn plan_preview(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<PlanPreview>, WorkApiError> {
    let control = control(&state)?;
    let graph = match control.graph(&id).await {
        Ok(graph) => graph,
        Err(e) => return Err(control_error(&state, &id, e).await),
    };
    let mut plan_ids: Vec<&str> = Vec::new();
    for node in &graph.nodes {
        if let Some(plan) = node.item.plan_id.as_deref() {
            if !plan_ids.contains(&plan) {
                plan_ids.push(plan);
            }
        }
    }
    let mut plans: Vec<WorkPlanRow> = Vec::new();
    for plan_id in plan_ids {
        if let Some(row) = state.agent.store.work_plan_get(plan_id).await? {
            if row.root == id {
                plans.push(row);
            }
        }
    }
    let holds = |plan: &WorkPlanRow| -> Vec<WorkItemId> {
        graph
            .nodes
            .iter()
            .filter(|n| {
                n.item.held_by.is_some()
                    && (plan.approval_question.is_none()
                        || n.item.held_by == plan.approval_question)
            })
            .map(|n| n.item.id.clone())
            .collect()
    };
    let pending = plans
        .iter()
        .filter(|p| p.approval_question.is_some() && !holds(p).is_empty())
        .max_by_key(|p| p.created_at);
    let Some(plan) = pending
        .or_else(|| plans.iter().max_by_key(|p| p.created_at))
        .cloned()
    else {
        return Err(WorkApiError::new(
            StatusCode::NOT_FOUND,
            "no_plan",
            format!("no plan is filed under {id}"),
        ));
    };
    let held = holds(&plan);
    Ok(Json(PlanPreview { plan, graph, held }))
}

#[derive(Debug, Default, Deserialize)]
struct ArchiveQuery {
    kind: Option<String>,
    since: Option<String>,
    q: Option<String>,
    /// The CLI's word for `q` (`work archive search <text>`).
    search: Option<String>,
}

/// `GET /api/work/archive`: one-line summaries, most recently closed first;
/// `q` (or `search`) searches ids, titles and summaries.
async fn archive_list(
    State(state): State<AppState>,
    Query(query): Query<ArchiveQuery>,
) -> Result<Json<ArchiveList>, WorkApiError> {
    let store = &state.agent.store;
    let kind = query
        .kind
        .as_deref()
        .filter(|k| !k.trim().is_empty())
        .map(parse_kind)
        .transpose()?;
    let since = query
        .since
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(parse_since)
        .transpose()?;
    let text = query
        .q
        .as_deref()
        .or(query.search.as_deref())
        .map(str::trim)
        .filter(|q| !q.is_empty());
    let archived = match text {
        Some(text) => store
            .work_archive_search(text)
            .await?
            .into_iter()
            .filter(|a| kind.is_none_or(|k| a.kind == k) && since.is_none_or(|s| a.closed_at >= s))
            .collect(),
        None => store.work_archive_list(kind, since).await?,
    };
    Ok(Json(ArchiveList { archived }))
}

/// `GET /api/work/archive/{id}`.
async fn archive_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ArchivedItem>, WorkApiError> {
    state
        .agent
        .store
        .work_archive_get(&id)
        .await?
        .map(Json)
        .ok_or_else(|| WorkApiError::not_found(&id))
}

// ── command handlers ───────────────────────────────────────────────────

/// `POST /api/work`: file one `work_file` draft as a one-item plan
/// (section 14.1), `FilingSource::WorkFile`. 201 with the accepted outcome,
/// 422 with every failed check.
async fn file_one(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    body: Bytes,
) -> Result<Response, WorkApiError> {
    let control = control(&state)?;
    let (_, draft, origin) = filing_body::<WorkItemDraft>(&body, "work item draft")?;
    let provenance = Provenance {
        conversation_id: origin,
        filed_by_item: None,
        actor: actor_of(principal),
    };
    let outcome = control.file_draft(draft, provenance).await?;
    Ok(outcome_response(outcome, StatusCode::CREATED))
}

/// `POST /api/work/plan`: file a whole graph (section 14.1). 200 with the
/// accepted outcome, 422 with every failed check.
async fn file_plan(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    body: Bytes,
) -> Result<Response, WorkApiError> {
    let control = control(&state)?;
    let (_, plan, origin) = filing_body::<WorkPlan>(&body, "work_plan")?;
    let provenance = Provenance {
        conversation_id: origin,
        filed_by_item: None,
        actor: actor_of(principal),
    };
    // The orchestration surface files as the planner does: code graphs and
    // `plan: true` drafts are refused, as they are for a planner run.
    let outcome = control
        .file_plan(plan, provenance, FilingSource::Planner)
        .await?;
    Ok(outcome_response(outcome, StatusCode::OK))
}

/// `POST /api/work/import`: the delivery import (sections 4, 6.1 and
/// 14.1). The body is `{ "manifest": StackManifest }` (or the bare
/// manifest); the slice becomes one parent, a child parent per layer and a
/// `code` child per delivery work item, filed through the validator as
/// `FilingSource::DeliveryImport`, the only source that may file `code`.
/// No `work_plan` call is made and no planner runs. 201 accepted, 422
/// rejected whole (a cyclic manifest with `cycle`).
async fn import_slice(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    body: Bytes,
) -> Result<Response, WorkApiError> {
    let control = control(&state)?;
    let (raw, _, origin) = filing_body::<serde_json::Value>(&body, "import")?;
    let manifest_json = raw.get("manifest").cloned().unwrap_or(raw);
    let manifest: StackManifest = serde_json::from_value(manifest_json)
        .map_err(|e| WorkApiError::bad_request(format!("invalid StackManifest: {e}")))?;
    let plan = import::plan_of(&manifest, Budget::default());
    let provenance = Provenance {
        conversation_id: origin,
        filed_by_item: None,
        actor: actor_of(principal),
    };
    let outcome = control
        .file_plan(plan, provenance, FilingSource::DeliveryImport)
        .await?;
    Ok(outcome_response(outcome, StatusCode::CREATED))
}

/// `POST /api/work/{id}/approve`: release the items held under root `{id}`.
async fn approve(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(id): Path<String>,
) -> Result<Json<ApproveReply>, WorkApiError> {
    let control = control(&state)?;
    let released = match control.approve(&id, &actor_of(principal)).await {
        Ok(ids) => ids,
        Err(e) => return Err(control_error(&state, &id, e).await),
    };
    Ok(Json(ApproveReply { root: id, released }))
}

/// `POST /api/work/{id}/reject`: cancel the held items under root `{id}`,
/// with cascade.
async fn reject(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<CancelReply>, WorkApiError> {
    let control = control(&state)?;
    let reason = reason_of(&body)?;
    let before = control.graph(&id).await.ok();
    let cancelled = match control.reject(&id, reason, &actor_of(principal)).await {
        Ok(ids) => ids,
        Err(e) => return Err(control_error(&state, &id, e).await),
    };
    let already_finished = finished_in(before.as_ref(), &cancelled);
    Ok(Json(CancelReply {
        item: id,
        cancelled,
        already_finished,
    }))
}

/// `POST /api/work/{id}/cancel`: cancel the item and its open subtree.
async fn cancel(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<CancelReply>, WorkApiError> {
    let control = control(&state)?;
    let reason = reason_of(&body)?;
    let before = control.graph(&id).await.ok();
    let cancelled = match control.cancel(&id, reason, &actor_of(principal)).await {
        Ok(ids) => ids,
        Err(e) => return Err(control_error(&state, &id, e).await),
    };
    let already_finished = finished_in(before.as_ref(), &cancelled);
    Ok(Json(CancelReply {
        item: id,
        cancelled,
        already_finished,
    }))
}

/// How often the progress stream polls the store for new events.
const EVENT_POLL: Duration = Duration::from_millis(250);
/// Events read per poll; a burst larger than this drains over later polls.
const EVENT_BATCH: usize = 500;

/// `GET /api/work/events`: the SSE progress stream (section 14). Every
/// event written after the stream opened (or after the row id in
/// `Last-Event-ID`, when a client resumes) is one frame: `event:` the
/// event kind, `id:` its row id, `data:` the `WorkEvent` as JSON (item,
/// at, kind, from, to, actor, reason, upstream, origin, evidence_ref). The
/// store is polled by row id, so a frame is never repeated or skipped.
async fn event_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, WorkApiError> {
    let store = state.agent.store.clone();
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok());
    let mut cursor = match resume {
        Some(id) => id,
        None => store.work_events_last_id().await?,
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(EVENT_BATCH);
    tokio::spawn(async move {
        let mut poll = tokio::time::interval(EVENT_POLL);
        loop {
            poll.tick().await;
            if tx.is_closed() {
                return;
            }
            let rows = match store.work_events_after(cursor, EVENT_BATCH).await {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!(error = %e, "work event stream could not read the store");
                    continue;
                }
            };
            for (id, event) in rows {
                cursor = id;
                let data = match serde_json::to_string(&event) {
                    Ok(data) => data,
                    Err(e) => {
                        tracing::warn!(error = %e, "work event not serialisable");
                        continue;
                    }
                };
                let frame = Event::default()
                    .event(event.kind.as_str())
                    .id(id.to_string())
                    .data(data);
                if tx.send(Ok(frame)).await.is_err() {
                    return;
                }
            }
        }
    });
    Ok(Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// `POST /api/work/tick`: one pass of the controller loop, for tests and
/// the CLI. Only the holder of `controller.lock` ticks, so while the loop
/// reports the lock `waiting` (another process holds it) this refuses with
/// 409 `controller_lock_waiting`. A held lock, or a controller no loop
/// drives (`lock` `None`), ticks.
async fn tick(State(state): State<AppState>) -> Result<Json<TickReport>, WorkApiError> {
    let control = control(&state)?;
    if control.loop_status().and_then(|s| s.lock) == Some(LockState::Waiting) {
        return Err(WorkApiError::new(
            StatusCode::CONFLICT,
            "controller_lock_waiting",
            format!(
                "another process holds {LOCK_FILE}; this daemon's controller is waiting on it and does not tick"
            ),
        ));
    }
    Ok(Json(control.tick().await?))
}

#[cfg(test)]
mod tests;
