//! Work items: the control layer's unit of scheduling.
//!
//! These are the shared data types of `docs/plans/control-layer-and-worker-fleet.md`
//! (sections 4, 5, 8, 9 and 14.1). They are pure data: the store persists
//! them, `rustykrab-control` computes over them, the `work_*` tools file and
//! read them, and the CLI renders them. No behaviour beyond parsing,
//! rendering and small predicates lives here, so every crate agrees on the
//! vocabulary without depending on each other.
//!
//! Two rules from the plan are encoded in the types themselves:
//!
//! - **The model proposes, code transitions.** Nothing a model sends
//!   ([`WorkItemDraft`], [`WorkPlan`], [`ResultReport`]) carries a
//!   [`Status`]; only the controller writes one.
//! - **Unreadable values parse to the conservative case.** An unknown status
//!   reads as `failed` and an unknown edge kind as `blocks`, so a row the
//!   controller cannot interpret never becomes work that runs early.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Identifier of a work item. A UUID string, like every other store id.
pub type WorkItemId = String;

// ── kinds ──────────────────────────────────────────────────────────────

/// What a work item is for. Decides which workers qualify and which verifier
/// applies (plan section 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    Personal,
    Code,
    Research,
    /// Second-order work filed by the ladder: acquire, build or request a
    /// capability (section 8).
    Capability,
    /// An improvement to RustyKrab's own machinery (section 9).
    Internal,
    /// Dreaming's output (section 10).
    Proposal,
}

impl WorkKind {
    pub const ALL: [WorkKind; 6] = [
        WorkKind::Personal,
        WorkKind::Code,
        WorkKind::Research,
        WorkKind::Capability,
        WorkKind::Internal,
        WorkKind::Proposal,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            WorkKind::Personal => "personal",
            WorkKind::Code => "code",
            WorkKind::Research => "research",
            WorkKind::Capability => "capability",
            WorkKind::Internal => "internal",
            WorkKind::Proposal => "proposal",
        }
    }

    pub fn parse(raw: &str) -> Option<WorkKind> {
        WorkKind::ALL.iter().copied().find(|k| k.as_str() == raw)
    }
}

/// Which worker kinds may take an item. `Any` is the default; a narrower
/// value is a constraint the user or policy set, never one the controller
/// infers from the item's kind (section 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkerKind {
    #[default]
    Any,
    Local,
    Peer,
    ClaudeCode,
    Codex,
}

impl WorkerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkerKind::Any => "any",
            WorkerKind::Local => "local",
            WorkerKind::Peer => "peer",
            WorkerKind::ClaudeCode => "claude_code",
            WorkerKind::Codex => "codex",
        }
    }

    pub fn parse(raw: &str) -> Option<WorkerKind> {
        match raw {
            "any" => Some(WorkerKind::Any),
            "local" => Some(WorkerKind::Local),
            "peer" => Some(WorkerKind::Peer),
            "claude_code" => Some(WorkerKind::ClaudeCode),
            "codex" => Some(WorkerKind::Codex),
            _ => None,
        }
    }
}

// ── edges ──────────────────────────────────────────────────────────────

/// The five edge kinds of section 4.1. Three order work; two record history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Downstream is not ready until the upstream is `done`.
    Blocks,
    /// Downstream is not ready until the upstream is closed, whatever the
    /// outcome.
    WaitsFor,
    /// Downstream becomes ready only if the upstream fails; if the upstream
    /// succeeds the downstream is `cancelled(cascade)`.
    ConditionalOnFailure,
    /// The item holding the edge replaces the item it names; the old one
    /// becomes `cancelled(superseded)`.
    Supersedes,
    /// Non-blocking provenance: found during work on the named item.
    DiscoveredFrom,
}

impl EdgeKind {
    pub const ALL: [EdgeKind; 5] = [
        EdgeKind::Blocks,
        EdgeKind::WaitsFor,
        EdgeKind::ConditionalOnFailure,
        EdgeKind::Supersedes,
        EdgeKind::DiscoveredFrom,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeKind::Blocks => "blocks",
            EdgeKind::WaitsFor => "waits_for",
            EdgeKind::ConditionalOnFailure => "conditional_on_failure",
            EdgeKind::Supersedes => "supersedes",
            EdgeKind::DiscoveredFrom => "discovered_from",
        }
    }

    /// Conservative parse: an unknown kind reads as `blocks`, the strictest
    /// ordering, so a row we cannot interpret never releases work early.
    pub fn parse(raw: &str) -> EdgeKind {
        EdgeKind::ALL
            .iter()
            .copied()
            .find(|k| k.as_str() == raw)
            .unwrap_or(EdgeKind::Blocks)
    }

    /// Whether the edge takes part in readiness (`blocks`, `waits_for`,
    /// `conditional_on_failure`).
    pub fn is_ordering(&self) -> bool {
        matches!(
            self,
            EdgeKind::Blocks | EdgeKind::WaitsFor | EdgeKind::ConditionalOnFailure
        )
    }
}

/// One row of `work_item_deps`: `item` is the downstream, `depends_on` the
/// upstream it names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Edge {
    pub item: WorkItemId,
    pub depends_on: WorkItemId,
    pub kind: EdgeKind,
}

// ── status ─────────────────────────────────────────────────────────────

/// Why an item is `blocked`. The two `upstream_*` reasons are set only by
/// cascade, never filed by a model (section 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedReason {
    NeedsCredential,
    NeedsDecision,
    NeedsConsent,
    NeedsTool,
    WorkerUnavailable,
    BudgetExhausted,
    VerificationFailed,
    PreconditionFailed,
    UpstreamFailed,
    UpstreamExpired,
}

impl BlockedReason {
    pub const ALL: [BlockedReason; 10] = [
        BlockedReason::NeedsCredential,
        BlockedReason::NeedsDecision,
        BlockedReason::NeedsConsent,
        BlockedReason::NeedsTool,
        BlockedReason::WorkerUnavailable,
        BlockedReason::BudgetExhausted,
        BlockedReason::VerificationFailed,
        BlockedReason::PreconditionFailed,
        BlockedReason::UpstreamFailed,
        BlockedReason::UpstreamExpired,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            BlockedReason::NeedsCredential => "needs_credential",
            BlockedReason::NeedsDecision => "needs_decision",
            BlockedReason::NeedsConsent => "needs_consent",
            BlockedReason::NeedsTool => "needs_tool",
            BlockedReason::WorkerUnavailable => "worker_unavailable",
            BlockedReason::BudgetExhausted => "budget_exhausted",
            BlockedReason::VerificationFailed => "verification_failed",
            BlockedReason::PreconditionFailed => "precondition_failed",
            BlockedReason::UpstreamFailed => "upstream_failed",
            BlockedReason::UpstreamExpired => "upstream_expired",
        }
    }

    pub fn parse(raw: &str) -> Option<BlockedReason> {
        BlockedReason::ALL
            .iter()
            .copied()
            .find(|r| r.as_str() == raw)
    }

    /// Set only by the controller's cascade (section 4.5); a model may not
    /// file it.
    pub fn is_cascade(&self) -> bool {
        matches!(
            self,
            BlockedReason::UpstreamFailed | BlockedReason::UpstreamExpired
        )
    }

    /// A reason that needs the user before anything else can move; the
    /// parent roll-up prefers it (section 4.2).
    pub fn needs_user(&self) -> bool {
        matches!(
            self,
            BlockedReason::NeedsCredential
                | BlockedReason::NeedsDecision
                | BlockedReason::NeedsConsent
        )
    }
}

/// Why an item is `cancelled`. `cascade` and `superseded` are set only by
/// code; `requested` is a user's or a policy's cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    Requested,
    Cascade,
    Superseded,
}

impl CancelReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            CancelReason::Requested => "requested",
            CancelReason::Cascade => "cascade",
            CancelReason::Superseded => "superseded",
        }
    }

    pub fn parse(raw: &str) -> Option<CancelReason> {
        match raw {
            "requested" => Some(CancelReason::Requested),
            "cascade" => Some(CancelReason::Cascade),
            "superseded" => Some(CancelReason::Superseded),
            _ => None,
        }
    }
}

/// Lifecycle of a work item (section 4). Waiting: `queued | ready | blocked`.
/// Active: `leased | running | verifying`. Closed, and never rewritten:
/// `done | failed | cancelled | expired`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub enum Status {
    Queued,
    Ready,
    Leased,
    Running,
    Blocked(BlockedReason),
    Verifying,
    Done,
    Failed,
    Cancelled(CancelReason),
    Expired,
}

impl Status {
    /// The bare status column value, without its reason.
    pub fn name(&self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Ready => "ready",
            Status::Leased => "leased",
            Status::Running => "running",
            Status::Blocked(_) => "blocked",
            Status::Verifying => "verifying",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Cancelled(_) => "cancelled",
            Status::Expired => "expired",
        }
    }

    /// The reason column value, for `blocked` and `cancelled` only.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Status::Blocked(r) => Some(r.as_str()),
            Status::Cancelled(r) => Some(r.as_str()),
            _ => None,
        }
    }

    /// Conservative parse of the two columns: an unknown status, or a
    /// `blocked`/`cancelled` row with an unreadable reason, reads as `failed`.
    pub fn parse(status: &str, reason: Option<&str>) -> Status {
        match status {
            "queued" => Status::Queued,
            "ready" => Status::Ready,
            "leased" => Status::Leased,
            "running" => Status::Running,
            "blocked" => reason
                .and_then(BlockedReason::parse)
                .map(Status::Blocked)
                .unwrap_or(Status::Failed),
            "verifying" => Status::Verifying,
            "done" => Status::Done,
            "cancelled" => reason
                .and_then(CancelReason::parse)
                .map(Status::Cancelled)
                .unwrap_or(Status::Failed),
            "expired" => Status::Expired,
            _ => Status::Failed,
        }
    }

    pub fn is_waiting(&self) -> bool {
        matches!(self, Status::Queued | Status::Ready | Status::Blocked(_))
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Status::Leased | Status::Running | Status::Verifying)
    }

    /// Closed is final: `done | failed | cancelled | expired`.
    pub fn is_closed(&self) -> bool {
        matches!(
            self,
            Status::Done | Status::Failed | Status::Cancelled(_) | Status::Expired
        )
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason() {
            Some(r) => write!(f, "{}({})", self.name(), r),
            None => f.write_str(self.name()),
        }
    }
}

// ── item fields ────────────────────────────────────────────────────────

/// When an item may become ready (section 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Trigger {
    #[default]
    Now,
    At(DateTime<Utc>),
    OnCredential(String),
    OnMcp(String),
    OnAnswer(String),
}

/// A precondition re-checked at lease time. `name` selects a host check;
/// `args` are its parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Precondition {
    pub name: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// A pointer to evidence or an artifact: a message id, path, URL or commit
/// SHA. Pointers, never bodies (section 12).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// `message | path | url | commit | item | other`
    pub kind: String,
    pub value: String,
}

/// Per-rung budgets of the ladder (section 8), with the plan's typical
/// values as defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RungBudgets {
    pub retries: u32,
    pub repairs: u32,
    pub worker_switches: u32,
    pub acquisitions: u32,
    pub builds: u32,
    pub requests: u32,
    pub improvements: u32,
    pub replans: u32,
}

impl Default for RungBudgets {
    fn default() -> Self {
        RungBudgets {
            retries: 2,
            repairs: 2,
            worker_switches: 1,
            acquisitions: 1,
            builds: 1,
            requests: 1,
            improvements: 1,
            replans: 1,
        }
    }
}

/// What an item may spend. A parent's budget is an envelope over its
/// subtree (section 4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub iterations: u32,
    pub tokens: u64,
    pub wall_seconds: u64,
    pub repairs: u32,
    #[serde(default)]
    pub rungs: RungBudgets,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            iterations: 25,
            tokens: 200_000,
            wall_seconds: 3_600,
            repairs: 2,
            rungs: RungBudgets::default(),
        }
    }
}

/// Caps the validator applies to a filed graph (section 6.1). Placeholders
/// until Phase 0 measures them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphCaps {
    pub max_depth: u32,
    pub max_items: u32,
    pub max_inputs: u32,
    pub max_input_tokens: u32,
    pub max_replans_per_parent: u32,
}

impl Default for GraphCaps {
    fn default() -> Self {
        GraphCaps {
            max_depth: 3,
            max_items: 12,
            max_inputs: 8,
            max_input_tokens: 600,
            max_replans_per_parent: 1,
        }
    }
}

/// A stored work item: the `work_items` row (section 4). Edges, events,
/// evidence and the ladder live in their own tables and are fetched beside
/// it, not embedded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: WorkItemId,
    pub kind: WorkKind,
    pub title: String,
    pub objective: String,
    pub done_when: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub decisions_made: Vec<String>,
    #[serde(default)]
    pub artifact_refs: Vec<ArtifactRef>,
    #[serde(default)]
    pub required_tools: Vec<String>,
    #[serde(default)]
    pub required_mcp_servers: Vec<String>,
    #[serde(default)]
    pub worker_kind: WorkerKind,
    /// Writable resources the item touches (a calendar, a mailbox, a
    /// worktree, a device), for the single-writer rule.
    #[serde(default)]
    pub writable_resources: Vec<String>,
    pub parent: Option<WorkItemId>,
    /// Fan-in (section 4.3). Filled at file time; defaults to the `blocks`
    /// upstreams.
    #[serde(default)]
    pub inputs_from: Vec<WorkItemId>,
    pub origin_conversation_id: Option<String>,
    #[serde(default)]
    pub trigger: Trigger,
    #[serde(default)]
    pub preconditions: Vec<Precondition>,
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub priority: i32,
    pub status: Status,
    /// The root-cause item of a cascade status (section 4.5).
    pub status_origin: Option<WorkItemId>,
    /// The accepted `work_plan` that filed the item. Control column; no
    /// model sees it.
    pub plan_id: Option<String>,
    /// The approval question holding the item (section 6.1). Control
    /// column; no model sees it.
    pub held_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

/// One input copied into a brief at lease time (section 4.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRef {
    pub item: WorkItemId,
    pub title: String,
    pub status: Status,
    #[serde(default)]
    pub edge: Option<EdgeKind>,
    #[serde(default)]
    pub evidence: Vec<ArtifactRef>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub error: Option<WorkError>,
}

/// A lease on a leaf item (section 6, step 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub item: WorkItemId,
    pub worker: String,
    pub since: DateTime<Utc>,
    pub ttl_seconds: u64,
    pub heartbeat_at: DateTime<Utc>,
    /// The `inputs_from` set copied into the brief, so it can be rebuilt
    /// exactly and evaluation can see what the worker was given.
    #[serde(default)]
    pub inputs: Vec<InputRef>,
}

/// Evidence attached to an item (`work_item_evidence`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub item: WorkItemId,
    pub kind: String,
    pub reference: String,
    pub hash: Option<String>,
    pub verified_by: Option<String>,
    pub at: DateTime<Utc>,
}

/// What an event records (`work_item_events.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Transition,
    Cascade,
    Repoint,
    Rung,
    Lease,
    Resume,
    Rejection,
    Warning,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::Transition => "transition",
            EventKind::Cascade => "cascade",
            EventKind::Repoint => "repoint",
            EventKind::Rung => "rung",
            EventKind::Lease => "lease",
            EventKind::Resume => "resume",
            EventKind::Rejection => "rejection",
            EventKind::Warning => "warning",
        }
    }

    pub fn parse(raw: &str) -> Option<EventKind> {
        match raw {
            "transition" => Some(EventKind::Transition),
            "cascade" => Some(EventKind::Cascade),
            "repoint" => Some(EventKind::Repoint),
            "rung" => Some(EventKind::Rung),
            "lease" => Some(EventKind::Lease),
            "resume" => Some(EventKind::Resume),
            "rejection" => Some(EventKind::Rejection),
            "warning" => Some(EventKind::Warning),
            _ => None,
        }
    }
}

/// One append-only row of `work_item_events` (section 13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkEvent {
    pub item: WorkItemId,
    pub at: DateTime<Utc>,
    pub kind: EventKind,
    pub from: Option<Status>,
    pub to: Option<Status>,
    /// `controller | worker:<name> | user | policy | planner`
    pub actor: String,
    pub reason: Option<String>,
    /// The direct upstream, for cascades and re-points.
    pub upstream: Option<WorkItemId>,
    /// The root-cause item, for cascades and re-points.
    pub origin: Option<WorkItemId>,
    pub evidence_ref: Option<String>,
}

// ── errors (section 9) ─────────────────────────────────────────────────

/// Top-level error class. `unknown` is a defect in observability and files
/// an `internal` item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Tool,
    Model,
    CapabilityGap,
    Environment,
    Verification,
    Policy,
    Budget,
    Unknown,
}

impl ErrorClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorClass::Tool => "tool",
            ErrorClass::Model => "model",
            ErrorClass::CapabilityGap => "capability_gap",
            ErrorClass::Environment => "environment",
            ErrorClass::Verification => "verification",
            ErrorClass::Policy => "policy",
            ErrorClass::Budget => "budget",
            ErrorClass::Unknown => "unknown",
        }
    }

    pub fn parse(raw: &str) -> ErrorClass {
        match raw {
            "tool" => ErrorClass::Tool,
            "model" => ErrorClass::Model,
            "capability_gap" => ErrorClass::CapabilityGap,
            "environment" => ErrorClass::Environment,
            "verification" => ErrorClass::Verification,
            "policy" => ErrorClass::Policy,
            "budget" => ErrorClass::Budget,
            _ => ErrorClass::Unknown,
        }
    }
}

/// The subclass within a class. Flat so it serialises as one word; each
/// value belongs to exactly one [`ErrorClass`], see [`ErrorSubclass::class`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorSubclass {
    // tool
    InvalidArgs,
    NotFound,
    Timeout,
    UpstreamError,
    // model
    Format,
    Refusal,
    HallucinatedTool,
    Loop,
    Empty,
    // capability_gap
    ToolGap,
    Credential,
    Consent,
    Compute,
    Knowledge,
    // environment
    Network,
    Disk,
    Permission,
    Process,
    Dependency,
    // verification
    ClaimMismatch,
    CheckFailed,
    Incomplete,
    // policy
    Scope,
    SingleWriter,
    Ceiling,
    // budget
    Iterations,
    Tokens,
    Wall,
    Repairs,
    // unknown
    Unclassified,
}

impl ErrorSubclass {
    pub fn class(&self) -> ErrorClass {
        use ErrorSubclass::*;
        match self {
            InvalidArgs | NotFound | Timeout | UpstreamError => ErrorClass::Tool,
            Format | Refusal | HallucinatedTool | Loop | Empty => ErrorClass::Model,
            ToolGap | Credential | Consent | Compute | Knowledge => ErrorClass::CapabilityGap,
            Network | Disk | Permission | Process | Dependency => ErrorClass::Environment,
            ClaimMismatch | CheckFailed | Incomplete => ErrorClass::Verification,
            Scope | SingleWriter | Ceiling => ErrorClass::Policy,
            Iterations | Tokens | Wall | Repairs => ErrorClass::Budget,
            Unclassified => ErrorClass::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        use ErrorSubclass::*;
        match self {
            InvalidArgs => "invalid_args",
            NotFound => "not_found",
            Timeout => "timeout",
            UpstreamError => "upstream_error",
            Format => "format",
            Refusal => "refusal",
            HallucinatedTool => "hallucinated_tool",
            Loop => "loop",
            Empty => "empty",
            ToolGap => "tool",
            Credential => "credential",
            Consent => "consent",
            Compute => "compute",
            Knowledge => "knowledge",
            Network => "network",
            Disk => "disk",
            Permission => "permission",
            Process => "process",
            Dependency => "dependency",
            ClaimMismatch => "claim_mismatch",
            CheckFailed => "check_failed",
            Incomplete => "incomplete",
            Scope => "scope",
            SingleWriter => "single_writer",
            Ceiling => "ceiling",
            Iterations => "iterations",
            Tokens => "tokens",
            Wall => "wall",
            Repairs => "repairs",
            Unclassified => "unclassified",
        }
    }

    /// Transient errors are the only ones rung 0 retries (section 8).
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            ErrorSubclass::Timeout | ErrorSubclass::Network | ErrorSubclass::UpstreamError
        )
    }
}

/// A classified failure (section 9). The `fingerprint` is a stable hash of
/// class, subclass, tool, worker kind and the normalised message, for
/// recurrence counting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkError {
    pub class: ErrorClass,
    pub subclass: ErrorSubclass,
    pub fingerprint: String,
    pub detail: String,
    #[serde(default)]
    pub artifact_refs: Vec<ArtifactRef>,
    /// Which probe, check or diagnosis classified it.
    #[serde(default)]
    pub observed_by: String,
}

// ── the ladder (section 8) ─────────────────────────────────────────────

/// A rung of the resolution ladder. Orders 0 to 4 are section 8's; `PlanB`
/// and `Replan` are the two graph moves between order 3 and order 4
/// (section 6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rung {
    Retry,
    Repair,
    SwitchWorker,
    Acquire,
    Build,
    Request,
    Improve,
    PlanB,
    Replan,
    Surface,
}

impl Rung {
    /// The order label from the plan's table.
    pub fn order(&self) -> &'static str {
        match self {
            Rung::Retry => "0",
            Rung::Repair | Rung::SwitchWorker => "1",
            Rung::Acquire => "2a",
            Rung::Build => "2b",
            Rung::Request => "2c",
            Rung::Improve => "3",
            Rung::PlanB | Rung::Replan => "3+",
            Rung::Surface => "4",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Rung::Retry => "retry",
            Rung::Repair => "repair",
            Rung::SwitchWorker => "switch_worker",
            Rung::Acquire => "acquire",
            Rung::Build => "build",
            Rung::Request => "request",
            Rung::Improve => "improve",
            Rung::PlanB => "plan_b",
            Rung::Replan => "replan",
            Rung::Surface => "surface",
        }
    }
}

/// One rung climbed on an item, with its error and outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RungEvent {
    pub rung: Rung,
    pub at: DateTime<Utc>,
    pub error: Option<WorkError>,
    /// What was tried and what changed.
    pub outcome: String,
}

// ── what models send (sections 5 and 14.1) ─────────────────────────────

/// A reference to an item inside a filing: an existing item by id, or a new
/// item by the temp id the caller gave it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ItemRef {
    Id(WorkItemId),
    Tmp { tmp: String },
}

/// An edge a draft declares on itself: the draft is the downstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftEdge {
    pub kind: EdgeKind,
    pub depends_on: ItemRef,
}

/// The fields a model may file (`work_file`, a `work_plan` item, or a
/// `discovered` draft in a result). No status, no provenance: the host fills
/// those.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WorkItemDraft {
    /// Client-side id, unique within a `work_plan` call.
    #[serde(default)]
    pub tmp: Option<String>,
    pub kind: Option<WorkKind>,
    pub title: String,
    pub objective: String,
    pub done_when: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub decisions_made: Vec<String>,
    #[serde(default)]
    pub artifact_refs: Vec<ArtifactRef>,
    #[serde(default)]
    pub required_tools: Vec<String>,
    #[serde(default)]
    pub required_mcp_servers: Vec<String>,
    #[serde(default)]
    pub worker_kind: WorkerKind,
    #[serde(default)]
    pub writable_resources: Vec<String>,
    #[serde(default)]
    pub parent: Option<ItemRef>,
    #[serde(default)]
    pub inputs_from: Vec<ItemRef>,
    #[serde(default)]
    pub trigger: Trigger,
    #[serde(default)]
    pub preconditions: Vec<Precondition>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub budget: Option<Budget>,
    #[serde(default)]
    pub priority: i32,
    /// Edges this draft declares on itself.
    #[serde(default)]
    pub edges: Vec<DraftEdge>,
    /// The one item this draft replaces (section 6.5).
    #[serde(default)]
    pub supersedes: Option<WorkItemId>,
    /// The filing model's request for a planning step (section 6.1).
    #[serde(default)]
    pub plan: bool,
}

/// An edge in a `work_plan` call, in `work_item_deps` row shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEdge {
    pub item: ItemRef,
    pub kind: EdgeKind,
    pub depends_on: ItemRef,
}

/// A whole graph filed in one call (section 14.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkPlan {
    /// An existing item (new items without a parent become its children),
    /// or the temp id of the one new item with no parent.
    pub root: ItemRef,
    pub items: Vec<WorkItemDraft>,
    #[serde(default)]
    pub edges: Vec<PlanEdge>,
    /// One line, shown in the plan preview.
    #[serde(default)]
    pub rationale: String,
}

/// Why a filing was rejected (section 14.1). Every filing path uses the
/// same list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionReason {
    UnknownRef,
    DuplicateTmp,
    InvalidItem,
    Cycle,
    DepthExceeded,
    TooManyItems,
    OverBudget,
    SingleWriterConflict,
    PlanBEdges,
    EdgeOntoActive,
    InputUnordered,
    DeadFiling,
    SupersedesActive,
    SupersedesClosed,
    OutOfScope,
    KindNotAllowed,
    AlreadyPlanned,
    RateLimited,
}

impl RejectionReason {
    pub fn as_str(&self) -> &'static str {
        use RejectionReason::*;
        match self {
            UnknownRef => "unknown_ref",
            DuplicateTmp => "duplicate_tmp",
            InvalidItem => "invalid_item",
            Cycle => "cycle",
            DepthExceeded => "depth_exceeded",
            TooManyItems => "too_many_items",
            OverBudget => "over_budget",
            SingleWriterConflict => "single_writer_conflict",
            PlanBEdges => "plan_b_edges",
            EdgeOntoActive => "edge_onto_active",
            InputUnordered => "input_unordered",
            DeadFiling => "dead_filing",
            SupersedesActive => "supersedes_active",
            SupersedesClosed => "supersedes_closed",
            OutOfScope => "out_of_scope",
            KindNotAllowed => "kind_not_allowed",
            AlreadyPlanned => "already_planned",
            RateLimited => "rate_limited",
        }
    }
}

/// A check that warns instead of rejecting, until it is measured
/// (section 14.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningCheck {
    SequentialSplit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedCheck {
    pub reason: RejectionReason,
    #[serde(default)]
    pub offending: Vec<ItemRef>,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanWarning {
    pub check: WarningCheck,
    #[serde(default)]
    pub items: Vec<WorkItemId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanAccepted {
    pub root: WorkItemId,
    /// Temp id to real id, for every new item.
    #[serde(default)]
    pub ids: BTreeMap<String, WorkItemId>,
    /// Items waiting on the approval question (section 6.1).
    #[serde(default)]
    pub held: Vec<WorkItemId>,
    pub policy: Option<String>,
    #[serde(default)]
    pub warnings: Vec<PlanWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRejected {
    /// Every failed check, so a planner can fix the graph in one pass.
    pub failed: Vec<FailedCheck>,
}

/// The result of any filing: accepted whole or rejected whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PlanOutcome {
    Accepted(PlanAccepted),
    Rejected(PlanRejected),
}

/// A typed block a worker reports (section 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedReport {
    pub reason: BlockedReason,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub needs: Vec<String>,
}

/// A question a worker files for the user, classified by the router
/// (section 7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub text: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub options: Vec<String>,
}

/// The one result contract for every worker kind (section 5). Every field
/// is a claim the controller verifies before anything counts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ResultReport {
    pub summary: String,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
    #[serde(default)]
    pub changed_paths: Vec<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub checks_run: Vec<String>,
    #[serde(default)]
    pub known_limits: Vec<String>,
    #[serde(default)]
    pub blocked: Option<BlockedReport>,
    #[serde(default)]
    pub error: Option<WorkError>,
    #[serde(default)]
    pub questions: Vec<Question>,
    /// Drafts the controller files through `work_plan` validation; never
    /// filed as they stand (section 6.5).
    #[serde(default)]
    pub discovered: Vec<WorkItemDraft>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips_through_its_two_columns() {
        let all = [
            Status::Queued,
            Status::Ready,
            Status::Leased,
            Status::Running,
            Status::Blocked(BlockedReason::NeedsTool),
            Status::Blocked(BlockedReason::UpstreamFailed),
            Status::Verifying,
            Status::Done,
            Status::Failed,
            Status::Cancelled(CancelReason::Superseded),
            Status::Expired,
        ];
        for s in all {
            assert_eq!(Status::parse(s.name(), s.reason()), s, "{s}");
        }
    }

    #[test]
    fn unreadable_values_parse_to_the_conservative_case() {
        assert_eq!(Status::parse("nonsense", None), Status::Failed);
        assert_eq!(Status::parse("blocked", None), Status::Failed);
        assert_eq!(Status::parse("blocked", Some("bogus")), Status::Failed);
        assert_eq!(Status::parse("cancelled", Some("bogus")), Status::Failed);
        assert_eq!(EdgeKind::parse("bogus"), EdgeKind::Blocks);
        assert_eq!(ErrorClass::parse("bogus"), ErrorClass::Unknown);
    }

    #[test]
    fn status_groups_are_disjoint_and_complete() {
        let all = [
            Status::Queued,
            Status::Ready,
            Status::Leased,
            Status::Running,
            Status::Blocked(BlockedReason::NeedsTool),
            Status::Verifying,
            Status::Done,
            Status::Failed,
            Status::Cancelled(CancelReason::Requested),
            Status::Expired,
        ];
        for s in all {
            let groups = [s.is_waiting(), s.is_active(), s.is_closed()];
            assert_eq!(groups.iter().filter(|g| **g).count(), 1, "{s}");
        }
    }

    #[test]
    fn every_subclass_maps_to_one_class_and_serialises_flat() {
        let e = WorkError {
            class: ErrorClass::Tool,
            subclass: ErrorSubclass::Timeout,
            fingerprint: "abc".into(),
            detail: "slow".into(),
            artifact_refs: vec![],
            observed_by: "tool_result".into(),
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["class"], "tool");
        assert_eq!(json["subclass"], "timeout");
        assert_eq!(ErrorSubclass::Timeout.class(), ErrorClass::Tool);
        assert!(ErrorSubclass::Timeout.is_transient());
        assert!(!ErrorSubclass::Scope.is_transient());
    }

    #[test]
    fn item_refs_accept_ids_and_temp_ids() {
        let by_id: ItemRef = serde_json::from_str("\"abc-123\"").unwrap();
        assert_eq!(by_id, ItemRef::Id("abc-123".into()));
        let by_tmp: ItemRef = serde_json::from_str(r#"{"tmp":"a"}"#).unwrap();
        assert_eq!(by_tmp, ItemRef::Tmp { tmp: "a".into() });
    }

    #[test]
    fn a_draft_needs_only_the_three_text_fields() {
        let d: WorkItemDraft =
            serde_json::from_str(r#"{"title":"t","objective":"o","done_when":"d"}"#).unwrap();
        assert_eq!(d.trigger, Trigger::Now);
        assert_eq!(d.worker_kind, WorkerKind::Any);
        assert!(!d.plan);
        assert!(d.edges.is_empty());
    }

    #[test]
    fn plan_outcome_is_tagged() {
        let o = PlanOutcome::Rejected(PlanRejected {
            failed: vec![FailedCheck {
                reason: RejectionReason::Cycle,
                offending: vec![ItemRef::Tmp { tmp: "a".into() }],
                detail: String::new(),
            }],
        });
        let json = serde_json::to_value(&o).unwrap();
        assert_eq!(json["outcome"], "rejected");
        assert_eq!(json["failed"][0]["reason"], "cycle");
    }
}
