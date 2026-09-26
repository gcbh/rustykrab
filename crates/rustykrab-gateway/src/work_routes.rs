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
//! Not served yet: SSE progress events (section 14). The hook is a
//! `GET /api/work/stream` fed from `Store::work_events_since` (or a broadcast
//! the controller publishes after each tick), framed like the message stream
//! in `routes.rs`; cascade events already carry their `origin`.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use rustykrab_control::graph::FilingSource;
use rustykrab_control::handle::{ControlHandle, GraphView, TickReport};
use rustykrab_control::ladder::RUNGS;
use rustykrab_core::work::{
    BlockedReason, CancelReason, Edge, EventKind, Evidence, PlanOutcome, Rung, Status, WorkEvent,
    WorkItem, WorkItemId, WorkKind, WorkPlan,
};
use rustykrab_core::Error;
use rustykrab_store::{ArchivedItem, Principal, WorkFilter, WorkPlanRow, WorkStoreError};
use rustykrab_tools::work_backend::Provenance;

use crate::AppState;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/work", get(list_items))
        .route("/api/work/ready", get(ready_items))
        .route("/api/work/plan", post(file_plan))
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

/// `GET /api/work/{id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemDetail {
    pub item: WorkItem,
    /// What the item waits on: the edges it holds.
    pub edges: Vec<EdgeView>,
    /// What waits on it: the edges naming it.
    pub dependents: Vec<Edge>,
    #[serde(default)]
    pub rollup: Option<RollupView>,
    /// How many events and pieces of evidence it has.
    pub events: usize,
    pub evidence: usize,
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
/// event's reason starts with the rung's name (`retry: timeout`).
pub fn rungs_climbed(events: &[WorkEvent]) -> Vec<Rung> {
    let mut climbed = Vec::new();
    for event in events.iter().filter(|e| e.kind == EventKind::Rung) {
        let Some(reason) = event.reason.as_deref() else {
            continue;
        };
        let name = reason.split(':').next().unwrap_or_default().trim();
        if let Some(rung) = RUNGS.iter().copied().find(|r| r.as_str() == name) {
            if !climbed.contains(&rung) {
                climbed.push(rung);
            }
        }
    }
    climbed
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
    let events = store.work_events(&id).await?.len();
    let evidence = store.work_evidence_list(&id).await?.len();
    Ok(Json(ItemDetail {
        item,
        edges,
        dependents,
        rollup,
        events,
        evidence,
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
}

/// `GET /api/work/archive`: one-line summaries, most recently closed first;
/// `q` searches ids, titles and summaries.
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
    let archived = match query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
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

/// `POST /api/work/plan`: file a whole graph (section 14.1). 200 with the
/// accepted outcome, 422 with every failed check.
async fn file_plan(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    body: Bytes,
) -> Result<Response, WorkApiError> {
    let control = control(&state)?;
    let plan: WorkPlan = serde_json::from_slice(&body)
        .map_err(|e| WorkApiError::bad_request(format!("invalid work_plan: {e}")))?;
    let provenance = Provenance {
        conversation_id: None,
        filed_by_item: None,
        actor: actor_of(principal),
    };
    // The orchestration surface files as the planner does: code graphs and
    // `plan: true` drafts are refused, as they are for a planner run.
    let outcome = control
        .file_plan(plan, provenance, FilingSource::Planner)
        .await?;
    let status = match outcome {
        PlanOutcome::Accepted(_) => StatusCode::OK,
        PlanOutcome::Rejected(_) => StatusCode::UNPROCESSABLE_ENTITY,
    };
    Ok((status, Json(outcome)).into_response())
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

/// `POST /api/work/tick`: one pass of the controller loop, for tests and
/// the CLI.
async fn tick(State(state): State<AppState>) -> Result<Json<TickReport>, WorkApiError> {
    Ok(Json(control(&state)?.tick().await?))
}

#[cfg(test)]
mod tests;
