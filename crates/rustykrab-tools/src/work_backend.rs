//! The contract the control-layer tools call: `work_file`, `work_status`,
//! `result_report`, `work_plan`, `ask_user` and `capability_request` (plan
//! `docs/plans/control-layer-and-worker-fleet.md`, sections 5, 6.1, 6.5, 7
//! and 14).
//!
//! The tools are the model's side of the control layer: they parse and check
//! what a model sends, fill in who sent it, and render the answer compactly
//! for a small context. Everything that decides (validation of the graph,
//! transitions, verification) sits behind [`WorkBackend`], implemented by
//! the composition root over `rustykrab-control` and the store.
//!
//! The trait lives here, beside its tools, for the same reason as
//! `CronBackend`: its implementor sits above this crate. It is not a pure
//! pass-through to the controller either, because two of its answers
//! ([`WorkBackend::tool_state`], [`WorkBackend::mcp_server_configured`])
//! come from the composition root's tool registry and MCP configuration,
//! which the controller does not own.
//!
//! Provenance is never taken from model arguments. The host fills it from
//! the runner's task-locals: the conversation from
//! [`with_session_context`], and the work item a worker run holds from
//! [`WORK_RUN_CONTEXT`] when the runner scopes one.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use rustykrab_core::active_tools::with_session_context;
use rustykrab_core::questions::{QuestionClass, QuestionKind};
use rustykrab_core::work::{
    BlockedReason, Edge, EdgeKind, ItemRef, PlanAccepted, PlanOutcome, ResultReport, Status,
    WorkItem, WorkItemDraft, WorkItemId, WorkKind, WorkPlan,
};
use rustykrab_core::{Error, Result, ToolError};
use serde::{Deserialize, Serialize};

/// Actor recorded when no worker run is bound: a model in an ordinary
/// conversation, acting for the user.
pub const DEFAULT_ACTOR: &str = "agent";

/// Who filed or reported. Filled by the host from the runner's context,
/// never from model arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Provenance {
    /// The conversation the call came from, when the runner exposes one.
    pub conversation_id: Option<String>,
    /// The work item the calling run holds, when it is a worker run. The
    /// controller uses it for the `discovered_from` edge and to check the
    /// lease on a report.
    pub filed_by_item: Option<WorkItemId>,
    /// Event actor: `worker:<name>` for a worker run, else [`DEFAULT_ACTOR`].
    pub actor: String,
}

/// Whose view a status read is: the conversation and the item the caller
/// holds. The backend decides what that principal may see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Principal {
    pub conversation_id: Option<String>,
    pub item: Option<WorkItemId>,
}

/// Which items a status read asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusSelector {
    /// These items, in this order. Named items are returned whatever their
    /// status.
    Ids(Vec<WorkItemId>),
    /// This item and its subtree, root first.
    Root(WorkItemId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusQuery {
    pub select: StatusSelector,
    /// Include closed items of a root's subtree. Items named by id are
    /// always returned.
    #[serde(default)]
    pub include_closed: bool,
}

/// One item as `work_status` shows it: the row plus the graph context a
/// reader needs to place it (plan section 14.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkStatusView {
    pub item: WorkItem,
    pub parent: Option<WorkItemId>,
    /// Edges touching the item in either direction.
    #[serde(default)]
    pub edges: Vec<Edge>,
    /// The rolled-up status of a parent (section 4.2); `None` for a leaf.
    pub rollup: Option<Status>,
    #[serde(default)]
    pub children_done: u32,
    #[serde(default)]
    pub children_total: u32,
}

/// Whether a tool a draft requires is in reach of the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolState {
    /// Loaded for the caller now.
    Loaded,
    /// Registered and loadable, but not loaded: the caller should load it
    /// and do the work rather than file it (plan section 7, scenario 11).
    RegisteredUnloaded,
    /// Not a registered tool.
    Unknown,
}

/// A typed question a worker asks through `ask_user` (plan sections 7 and
/// 14). The router classifies it; `class` is only the asker's claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskRequest {
    pub text: String,
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub kind: QuestionKind,
    #[serde(default)]
    pub options: Vec<String>,
    /// A recorded default, for a question that has one.
    #[serde(default)]
    pub default: Option<String>,
}

/// What a worker cannot proceed without, through `capability_request`: a
/// credential or consent only the user can give, or a tool, install,
/// compute or knowledge gap the ladder's order 2 acquires (plan sections 7
/// and 8). Generalises `credential_request`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityAsk {
    /// `credential | consent | tool | install | compute | knowledge`.
    pub kind: String,
    /// The credential, tool, resource or topic, by name.
    pub name: String,
    #[serde(default)]
    pub reason: String,
}

/// What the router did with a question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskOutcome {
    pub question: String,
    pub class: QuestionClass,
    /// The answer, when the router could give one now: a recorded default
    /// or a decision standing judgment covers.
    #[serde(default)]
    pub answer: Option<String>,
    /// `default`, `policy:<id>`.
    #[serde(default)]
    pub answered_by: Option<String>,
    /// The item parks and this run should end: it resumes with the answer.
    #[serde(default)]
    pub parked: bool,
    /// One line for the model.
    #[serde(default)]
    pub note: String,
}

fn unsupported(what: &str) -> Error {
    Error::ToolExecution(ToolError::internal(format!(
        "{what} is not supported by this host's control layer"
    )))
}

/// The control layer as the model-facing work tools see it.
#[async_trait]
pub trait WorkBackend: Send + Sync {
    /// File one draft. Accepted whole or rejected whole with every failed
    /// check (plan sections 4.4 and 14.1).
    async fn file(&self, draft: WorkItemDraft, provenance: Provenance) -> Result<PlanOutcome>;

    /// Read the items `principal` may see.
    async fn status(
        &self,
        query: StatusQuery,
        principal: &Principal,
    ) -> Result<Vec<WorkStatusView>>;

    /// Hand a worker's typed result to the controller. Its `discovered`
    /// drafts travel inside the report; the controller validates them as
    /// one graph (section 6.5).
    async fn report(
        &self,
        item: WorkItemId,
        report: ResultReport,
        provenance: Provenance,
    ) -> Result<()>;

    /// The host registry's view of a tool, for `work_file`'s draft checks
    /// when the runner's session does not settle it.
    fn tool_state(&self, name: &str) -> ToolState;

    /// Whether an MCP server is configured on this host.
    fn mcp_server_configured(&self, name: &str) -> bool;

    /// File a whole graph (`work_plan`, section 14.1): the planner's and the
    /// orchestration conversation's one call.
    async fn plan(&self, _plan: WorkPlan, _provenance: Provenance) -> Result<PlanOutcome> {
        Err(unsupported("work_plan"))
    }

    /// Route a worker's typed question (section 7).
    async fn ask(
        &self,
        _item: WorkItemId,
        _request: AskRequest,
        _provenance: Provenance,
    ) -> Result<AskOutcome> {
        Err(unsupported("ask_user"))
    }

    /// Park a worker on a capability it cannot proceed without (sections 7
    /// and 8).
    async fn request_capability(
        &self,
        _item: WorkItemId,
        _request: CapabilityAsk,
        _provenance: Provenance,
    ) -> Result<AskOutcome> {
        Err(unsupported("capability_request"))
    }
}

// ── the run binding ───────────────────────────────────────────────────────

/// The work item a worker run holds. The runner scopes it around a worker
/// run the way it scopes `SESSION_TOOL_CONTEXT`, including inside the
/// spawned task that executes each tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkRunContext {
    pub item: WorkItemId,
    /// Event actor for the run, e.g. `worker:pinch`.
    pub actor: String,
}

tokio::task_local! {
    pub static WORK_RUN_CONTEXT: WorkRunContext;
}

/// Run `f` with the current worker run's binding, if the runner set one.
pub fn with_work_run<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&WorkRunContext) -> R,
{
    WORK_RUN_CONTEXT.try_with(|ctx| f(ctx)).ok()
}

/// Provenance as the host sees it for the current call.
pub(crate) fn host_provenance() -> Provenance {
    let run = with_work_run(WorkRunContext::clone);
    Provenance {
        conversation_id: with_session_context(|ctx| ctx.conversation_id.to_string()),
        filed_by_item: run.as_ref().map(|r| r.item.clone()),
        actor: run
            .map(|r| r.actor)
            .unwrap_or_else(|| DEFAULT_ACTOR.to_string()),
    }
}

/// The principal a status read runs as.
pub(crate) fn host_principal() -> Principal {
    Principal {
        conversation_id: with_session_context(|ctx| ctx.conversation_id.to_string()),
        item: with_work_run(|r| r.item.clone()),
    }
}

// ── stub ──────────────────────────────────────────────────────────────────

/// One call a [`StubWorkBackend`] received.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkCall {
    File {
        draft: WorkItemDraft,
        provenance: Provenance,
    },
    Status {
        query: StatusQuery,
        principal: Principal,
    },
    Report {
        item: WorkItemId,
        report: ResultReport,
        provenance: Provenance,
    },
    Plan {
        plan: WorkPlan,
        provenance: Provenance,
    },
    Ask {
        item: WorkItemId,
        request: AskRequest,
        provenance: Provenance,
    },
    Capability {
        item: WorkItemId,
        request: CapabilityAsk,
        provenance: Provenance,
    },
}

/// A recording [`WorkBackend`] for tests and the e2e scripted daemon.
///
/// Files are accepted with ids `stub-1`, `stub-2`, ... and become readable
/// through `status` (`queued`, or `blocked(needs_tool)` when a required MCP
/// server is not configured), unless an outcome was scripted with
/// [`push_outcome`](Self::push_outcome). Tools are [`ToolState::Unknown`]
/// and MCP servers unconfigured unless declared. It validates nothing: the
/// controller's checks are not its job.
#[derive(Default)]
pub struct StubWorkBackend {
    state: Mutex<StubState>,
}

#[derive(Default)]
struct StubState {
    calls: Vec<WorkCall>,
    tools: HashMap<String, ToolState>,
    mcp_servers: HashSet<String>,
    outcomes: VecDeque<PlanOutcome>,
    items: Vec<WorkStatusView>,
    report_error: Option<String>,
    next_id: u64,
}

impl StubWorkBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare what `tool_state` answers for `name`.
    pub fn with_tool(mut self, name: impl Into<String>, state: ToolState) -> Self {
        self.inner().tools.insert(name.into(), state);
        self
    }

    /// Declare an MCP server configured.
    pub fn with_mcp_server(mut self, name: impl Into<String>) -> Self {
        self.inner().mcp_servers.insert(name.into());
        self
    }

    /// Seed an item `status` can return.
    pub fn with_item(mut self, view: WorkStatusView) -> Self {
        self.inner().items.push(view);
        self
    }

    /// Answer the next `file` with `outcome` instead of accepting it.
    pub fn push_outcome(&self, outcome: PlanOutcome) {
        self.lock().outcomes.push_back(outcome);
    }

    /// Make every later `report` fail with `message`.
    pub fn fail_reports(&self, message: impl Into<String>) {
        self.lock().report_error = Some(message.into());
    }

    /// Every call received, in order.
    pub fn calls(&self) -> Vec<WorkCall> {
        self.lock().calls.clone()
    }

    fn inner(&mut self) -> &mut StubState {
        self.state.get_mut().unwrap_or_else(|e| e.into_inner())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StubState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl WorkBackend for StubWorkBackend {
    async fn file(&self, draft: WorkItemDraft, provenance: Provenance) -> Result<PlanOutcome> {
        let mut state = self.lock();
        state.calls.push(WorkCall::File {
            draft: draft.clone(),
            provenance: provenance.clone(),
        });
        if let Some(outcome) = state.outcomes.pop_front() {
            return Ok(outcome);
        }
        state.next_id += 1;
        let id = format!("stub-{}", state.next_id);
        let waits_on_mcp = draft
            .required_mcp_servers
            .iter()
            .any(|s| !state.mcp_servers.contains(s));
        let status = if waits_on_mcp {
            Status::Blocked(BlockedReason::NeedsTool)
        } else {
            Status::Queued
        };
        let view = stub_view(&id, &draft, &provenance, status);
        state.items.push(view);
        Ok(PlanOutcome::Accepted(PlanAccepted {
            // Temp ids map to real ids; a lone `work_file` draft has none.
            ids: draft.tmp.into_iter().map(|t| (t, id.clone())).collect(),
            root: id,
            held: Vec::new(),
            policy: None,
            warnings: Vec::new(),
        }))
    }

    async fn status(
        &self,
        query: StatusQuery,
        principal: &Principal,
    ) -> Result<Vec<WorkStatusView>> {
        let mut state = self.lock();
        state.calls.push(WorkCall::Status {
            query: query.clone(),
            principal: principal.clone(),
        });
        let items = &state.items;
        let found = match &query.select {
            StatusSelector::Ids(ids) => ids
                .iter()
                .filter_map(|id| items.iter().find(|v| &v.item.id == id).cloned())
                .collect(),
            StatusSelector::Root(root) => {
                let mut out: Vec<WorkStatusView> = Vec::new();
                if let Some(v) = items.iter().find(|v| &v.item.id == root) {
                    out.push(v.clone());
                }
                // Breadth-first over parent links, root first.
                let mut next = 0;
                while next < out.len() {
                    let parent = out[next].item.id.clone();
                    next += 1;
                    for child in items.iter().filter(|v| v.parent.as_ref() == Some(&parent)) {
                        if query.include_closed || !child.item.status.is_closed() {
                            out.push(child.clone());
                        }
                    }
                }
                out
            }
        };
        Ok(found)
    }

    async fn report(
        &self,
        item: WorkItemId,
        report: ResultReport,
        provenance: Provenance,
    ) -> Result<()> {
        let mut state = self.lock();
        state.calls.push(WorkCall::Report {
            item,
            report,
            provenance,
        });
        match &state.report_error {
            Some(message) => Err(Error::Internal(message.clone())),
            None => Ok(()),
        }
    }

    fn tool_state(&self, name: &str) -> ToolState {
        self.lock()
            .tools
            .get(name)
            .copied()
            .unwrap_or(ToolState::Unknown)
    }

    fn mcp_server_configured(&self, name: &str) -> bool {
        self.lock().mcp_servers.contains(name)
    }

    async fn plan(&self, plan: WorkPlan, provenance: Provenance) -> Result<PlanOutcome> {
        let mut state = self.lock();
        state.calls.push(WorkCall::Plan {
            plan: plan.clone(),
            provenance,
        });
        if let Some(outcome) = state.outcomes.pop_front() {
            return Ok(outcome);
        }
        let mut ids = std::collections::BTreeMap::new();
        for draft in &plan.items {
            state.next_id += 1;
            if let Some(tmp) = &draft.tmp {
                ids.insert(tmp.clone(), format!("stub-{}", state.next_id));
            }
        }
        let root = match &plan.root {
            ItemRef::Id(id) => id.clone(),
            ItemRef::Tmp { tmp } => ids.get(tmp).cloned().unwrap_or_default(),
        };
        Ok(PlanOutcome::Accepted(PlanAccepted {
            root,
            ids,
            held: Vec::new(),
            policy: None,
            warnings: Vec::new(),
        }))
    }

    async fn ask(
        &self,
        item: WorkItemId,
        request: AskRequest,
        provenance: Provenance,
    ) -> Result<AskOutcome> {
        let mut state = self.lock();
        state.calls.push(WorkCall::Ask {
            item,
            request: request.clone(),
            provenance,
        });
        state.next_id += 1;
        Ok(AskOutcome {
            question: format!("q-{}", state.next_id),
            class: QuestionClass::BlockingNow,
            answer: None,
            answered_by: None,
            parked: true,
            note: "asked the user".to_string(),
        })
    }

    async fn request_capability(
        &self,
        item: WorkItemId,
        request: CapabilityAsk,
        provenance: Provenance,
    ) -> Result<AskOutcome> {
        let mut state = self.lock();
        state.calls.push(WorkCall::Capability {
            item,
            request,
            provenance,
        });
        state.next_id += 1;
        Ok(AskOutcome {
            question: format!("q-{}", state.next_id),
            class: QuestionClass::BlockingNow,
            answer: None,
            answered_by: None,
            parked: true,
            note: "parked on the capability".to_string(),
        })
    }
}

/// The row a stub filing would have produced, for `status` to return.
fn stub_view(
    id: &str,
    draft: &WorkItemDraft,
    provenance: &Provenance,
    status: Status,
) -> WorkStatusView {
    let now = Utc::now();
    let only_ids = |refs: &[ItemRef]| -> Vec<WorkItemId> {
        refs.iter()
            .filter_map(|r| match r {
                ItemRef::Id(id) => Some(id.clone()),
                ItemRef::Tmp { .. } => None,
            })
            .collect()
    };
    let parent = match &draft.parent {
        Some(ItemRef::Id(p)) => Some(p.clone()),
        _ => None,
    };
    let mut edges: Vec<Edge> = draft
        .edges
        .iter()
        .filter_map(|e| match &e.depends_on {
            ItemRef::Id(up) => Some(Edge {
                item: id.to_string(),
                depends_on: up.clone(),
                kind: e.kind,
            }),
            ItemRef::Tmp { .. } => None,
        })
        .collect();
    if let Some(old) = &draft.supersedes {
        edges.push(Edge {
            item: id.to_string(),
            depends_on: old.clone(),
            kind: EdgeKind::Supersedes,
        });
    }
    if let Some(from) = &provenance.filed_by_item {
        edges.push(Edge {
            item: id.to_string(),
            depends_on: from.clone(),
            kind: EdgeKind::DiscoveredFrom,
        });
    }
    WorkStatusView {
        item: WorkItem {
            id: id.to_string(),
            kind: draft.kind.unwrap_or(WorkKind::Personal),
            title: draft.title.clone(),
            objective: draft.objective.clone(),
            done_when: draft.done_when.clone(),
            constraints: draft.constraints.clone(),
            decisions_made: draft.decisions_made.clone(),
            artifact_refs: draft.artifact_refs.clone(),
            required_tools: draft.required_tools.clone(),
            required_mcp_servers: draft.required_mcp_servers.clone(),
            worker_kind: draft.worker_kind,
            writable_resources: draft.writable_resources.clone(),
            parent: parent.clone(),
            inputs_from: only_ids(&draft.inputs_from),
            origin_conversation_id: provenance.conversation_id.clone(),
            trigger: draft.trigger.clone(),
            preconditions: draft.preconditions.clone(),
            expires_at: draft.expires_at,
            budget: draft.budget.unwrap_or_default(),
            priority: draft.priority,
            status,
            status_origin: None,
            plan_id: None,
            held_by: None,
            created_at: now,
            updated_at: now,
            closed_at: None,
        },
        parent,
        edges,
        rollup: None,
        children_done: 0,
        children_total: 0,
    }
}

/// A plain item for tests and scripted fixtures.
pub fn fixture_view(id: &str, title: &str, status: Status) -> WorkStatusView {
    let draft = WorkItemDraft {
        title: title.to_string(),
        objective: format!("objective of {title}"),
        done_when: format!("{title} is done"),
        ..WorkItemDraft::default()
    };
    stub_view(id, &draft, &Provenance::default(), status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::work::CancelReason;

    fn child(id: &str, parent: &str, status: Status) -> WorkStatusView {
        let mut v = fixture_view(id, id, status);
        v.parent = Some(parent.to_string());
        v.item.parent = Some(parent.to_string());
        v
    }

    fn draft(title: &str) -> WorkItemDraft {
        WorkItemDraft {
            title: title.into(),
            objective: "o".into(),
            done_when: "d".into(),
            ..WorkItemDraft::default()
        }
    }

    #[tokio::test]
    async fn stub_records_every_call_in_order() {
        let stub = StubWorkBackend::new();
        let prov = Provenance {
            conversation_id: Some("c1".into()),
            filed_by_item: None,
            actor: DEFAULT_ACTOR.into(),
        };
        stub.file(draft("a"), prov.clone()).await.unwrap();
        let query = StatusQuery {
            select: StatusSelector::Ids(vec!["stub-1".into()]),
            include_closed: false,
        };
        let principal = Principal::default();
        let seen = stub.status(query.clone(), &principal).await.unwrap();
        assert_eq!(seen.len(), 1, "a filed item is readable");
        stub.report("stub-1".into(), ResultReport::default(), prov.clone())
            .await
            .unwrap();

        let calls = stub.calls();
        assert_eq!(calls.len(), 3);
        assert!(matches!(&calls[0], WorkCall::File { draft, provenance }
            if draft.title == "a" && provenance == &prov));
        assert_eq!(calls[1], WorkCall::Status { query, principal });
        assert!(matches!(&calls[2], WorkCall::Report { item, .. } if item == "stub-1"));
    }

    #[tokio::test]
    async fn stub_files_unconfigured_mcp_work_as_needs_tool() {
        let stub = StubWorkBackend::new().with_mcp_server("linear");
        let mut d = draft("sync");
        d.required_mcp_servers = vec!["jira".into()];
        stub.file(d, Provenance::default()).await.unwrap();
        let mut ok = draft("track");
        ok.required_mcp_servers = vec!["linear".into()];
        stub.file(ok, Provenance::default()).await.unwrap();

        let query = StatusQuery {
            select: StatusSelector::Ids(vec!["stub-1".into(), "stub-2".into()]),
            include_closed: false,
        };
        let seen = stub.status(query, &Principal::default()).await.unwrap();
        assert_eq!(
            seen[0].item.status,
            Status::Blocked(BlockedReason::NeedsTool)
        );
        assert_eq!(seen[1].item.status, Status::Queued);
    }

    #[tokio::test]
    async fn stub_scripted_outcome_wins_over_acceptance() {
        let stub = StubWorkBackend::new();
        let rejected = PlanOutcome::Rejected(rustykrab_core::work::PlanRejected { failed: vec![] });
        stub.push_outcome(rejected.clone());
        assert_eq!(
            stub.file(draft("a"), Provenance::default()).await.unwrap(),
            rejected
        );
        assert!(matches!(
            stub.file(draft("b"), Provenance::default()).await.unwrap(),
            PlanOutcome::Accepted(_)
        ));
    }

    #[tokio::test]
    async fn stub_root_query_walks_the_subtree_and_hides_closed() {
        let stub = StubWorkBackend::new()
            .with_item(fixture_view("p", "parent", Status::Running))
            .with_item(child("a", "p", Status::Done))
            .with_item(child("b", "p", Status::Ready))
            .with_item(child("b1", "b", Status::Queued))
            .with_item(child("c", "p", Status::Cancelled(CancelReason::Requested)))
            .with_item(fixture_view("other", "unrelated", Status::Ready));

        let open = StatusQuery {
            select: StatusSelector::Root("p".into()),
            include_closed: false,
        };
        let ids: Vec<String> = stub
            .status(open, &Principal::default())
            .await
            .unwrap()
            .into_iter()
            .map(|v| v.item.id)
            .collect();
        assert_eq!(ids, ["p", "b", "b1"]);

        let all = StatusQuery {
            select: StatusSelector::Root("p".into()),
            include_closed: true,
        };
        let n = stub.status(all, &Principal::default()).await.unwrap().len();
        assert_eq!(n, 5);
    }

    #[tokio::test]
    async fn stub_reports_can_be_made_to_fail() {
        let stub = StubWorkBackend::new();
        stub.fail_reports("lease lost");
        let err = stub
            .report("x".into(), ResultReport::default(), Provenance::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("lease lost"));
        assert_eq!(stub.calls().len(), 1, "a failed report is still recorded");
    }

    #[test]
    fn stub_tool_and_mcp_answers_default_to_unknown() {
        let stub = StubWorkBackend::new()
            .with_tool("browser", ToolState::RegisteredUnloaded)
            .with_mcp_server("linear");
        assert_eq!(stub.tool_state("browser"), ToolState::RegisteredUnloaded);
        assert_eq!(stub.tool_state("nope"), ToolState::Unknown);
        assert!(stub.mcp_server_configured("linear"));
        assert!(!stub.mcp_server_configured("jira"));
    }

    #[tokio::test]
    async fn run_binding_is_visible_only_inside_its_scope() {
        assert!(with_work_run(|r| r.item.clone()).is_none());
        let ctx = WorkRunContext {
            item: "item-7".into(),
            actor: "worker:pinch".into(),
        };
        let (prov, principal) = WORK_RUN_CONTEXT
            .scope(ctx, async { (host_provenance(), host_principal()) })
            .await;
        assert_eq!(prov.filed_by_item.as_deref(), Some("item-7"));
        assert_eq!(prov.actor, "worker:pinch");
        assert_eq!(
            prov.conversation_id, None,
            "no session scope, no conversation"
        );
        assert_eq!(principal.item.as_deref(), Some("item-7"));
        assert_eq!(host_provenance().actor, DEFAULT_ACTOR);
    }
}
