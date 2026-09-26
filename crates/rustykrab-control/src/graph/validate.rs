//! Filing validation (plan sections 4.3, 4.4, 6.1, 6.5 and 14.1).
//!
//! Every filing path (`work_plan`, `work_file`, a worker's `discovered`
//! drafts, the ladder's `capability` items, an accepted proposal and the
//! delivery import) is wrapped into a [`WorkPlan`] and goes through
//! [`validate`]. A filing is accepted whole or rejected whole, and a
//! rejection names every failed check, so a planner can fix its graph in
//! one pass. Nothing is written here: an [`Accepted`] carries the rows to
//! insert and the effects on existing items for the controller to write in
//! one transaction.
//!
//! Checks run on the graph as it would stand after the filing, including
//! the edges `supersedes` re-points (4.4).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;

use chrono::{DateTime, TimeDelta, Utc};
use rustykrab_core::work::{
    BlockedReason, Budget, CancelReason, Edge, EdgeKind, FailedCheck, GraphCaps, ItemRef,
    PlanAccepted, PlanOutcome, PlanRejected, PlanWarning, RejectionReason, Status, Trigger,
    WarningCheck, WorkItem, WorkItemId, WorkKind, WorkPlan, WorkerKind,
};
use uuid::Uuid;

use super::order::Arcs;
use super::ready::{edge_summary, verdict, Verdict};
use super::supersede::{supersede_in, supersede_refusals};
use super::{Effects, Link, Snapshot, Transition};

// ── inputs ─────────────────────────────────────────────────────────────

/// Which path a filing came through. The source switches only the few
/// rules the plan scopes to one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilingSource {
    /// A `planner` run's `work_plan` call (6.1).
    Planner,
    /// One `work_file` draft, wrapped as a one-item plan. The only path on
    /// which a draft may set `plan: true`.
    WorkFile,
    /// A worker's `discovered` drafts, which the controller submits as one
    /// graph (6.5).
    Discovered,
    /// A `capability` item the ladder files (section 8).
    Ladder,
    /// An accepted proposal (section 10).
    Proposal,
    /// The delivery import of a code slice (4, 6.1). The controller never
    /// re-orders an imported graph, so `sequential_split` does not apply,
    /// and unordered writers are allowed (the compiler partitions scope by
    /// files and Select serialises them).
    DeliveryImport,
}

/// How `sequential_split` reports (14.1): a warning until Phase 1 has
/// measured its false-positive rate, a rejection after.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SplitMode {
    #[default]
    Warn,
    Reject,
}

/// The approval policy of 6.1: which triggers hold which items at
/// acceptance. Every threshold is optional; `None` never fires. Standing
/// judgment that covers a trigger is expressed by the caller leaving it
/// out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalPolicy {
    /// The policy id recorded on the plan when a trigger fires.
    pub id: Option<String>,
    /// More new items than this holds the whole graph.
    pub max_items: Option<u32>,
    /// More tokens across the new leaves' budgets than this holds the whole
    /// graph.
    pub max_total_tokens: Option<u64>,
    /// The writable resources the user has delegated. `Some`: an item
    /// writing any other resource is held with what depends on it. `None`:
    /// no resource trigger.
    pub delegated_resources: Option<BTreeSet<String>>,
    /// Items under which `code` work is authorised (a delivery slice).
    /// `Some`: a `code` item with no ancestor here is held with what
    /// depends on it. `None`: no code trigger.
    pub authorized_slices: Option<BTreeSet<WorkItemId>>,
}

/// An approval trigger that fired (6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalTrigger {
    ItemCount { items: u32, threshold: u32 },
    Budget { tokens: u64, threshold: u64 },
    UndelegatedResource { item: WorkItemId, resource: String },
    CodeOutsideSlice { item: WorkItemId },
}

/// Everything [`validate`] needs besides the snapshot and the filing.
#[derive(Debug, Clone)]
pub struct FilingContext {
    /// Stamped on new rows; also decides `at(time)` and `expires_at`.
    pub now: DateTime<Utc>,
    pub source: FilingSource,
    pub caps: GraphCaps,
    /// The subtree the caller may change: a worker's own parent, a
    /// planning item's parent. `None`: the filing's root when that is an
    /// existing item, else nothing (so no `supersedes` and no existing
    /// parent is in scope).
    pub scope: Option<WorkItemId>,
    /// Ids the caller may see. `None`: every item in the snapshot.
    pub visible: Option<BTreeSet<WorkItemId>>,
    /// Whether `code` items may be filed: the delivery import only, by
    /// default.
    pub allow_code: bool,
    pub sequential_split: SplitMode,
    /// Filings that superseded something under this root inside the rate
    /// window. A filing that supersedes anything counts one.
    pub supersedes_in_window: u32,
    /// The most such filings the window allows (`rate_limited`).
    pub supersede_limit: u32,
    /// This planning run already has an accepted graph
    /// (`already_planned`).
    pub already_planned: bool,
    pub approval: ApprovalPolicy,
    /// The budget of a new item that names none and sits under no parent,
    /// and the per-item ceiling of a share of a parent's remainder.
    pub default_budget: Budget,
    /// What existing parents have left, from the controller's spend
    /// records. A parent missing here has its budget less the budgets of
    /// its open children.
    pub remaining_budget: BTreeMap<WorkItemId, Budget>,
    /// Stamped on every new item.
    pub origin_conversation_id: Option<String>,
    /// For `discovered` drafts: the item they were found on. Every new item
    /// gets a `discovered_from` edge on it.
    pub discovered_from: Option<WorkItemId>,
}

impl FilingContext {
    /// Defaults: the plan's placeholder caps, no scope beyond the root,
    /// everything visible, `code` only for the delivery import,
    /// `sequential_split` as a warning, two supersede filings per window,
    /// no approval triggers.
    pub fn new(source: FilingSource, now: DateTime<Utc>) -> FilingContext {
        FilingContext {
            now,
            source,
            caps: GraphCaps::default(),
            scope: None,
            visible: None,
            allow_code: source == FilingSource::DeliveryImport,
            sequential_split: SplitMode::Warn,
            supersedes_in_window: 0,
            supersede_limit: 2,
            already_planned: false,
            approval: ApprovalPolicy::default(),
            default_budget: Budget::default(),
            remaining_budget: BTreeMap::new(),
            origin_conversation_id: None,
            discovered_from: None,
        }
    }
}

// ── outputs ────────────────────────────────────────────────────────────

/// A failed check: one of 14.1's rejection reasons, or `sequential_split`
/// in [`SplitMode::Reject`] (which `RejectionReason` cannot yet name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Check {
    Reason(RejectionReason),
    SequentialSplit,
}

/// One failed check, with the items it names (temp ids for new items, or
/// `#<index>` for a draft without one; real ids for existing items).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub check: Check,
    pub offending: Vec<ItemRef>,
    pub detail: String,
}

/// A rejected filing: every failed check. Nothing from it exists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rejection {
    pub failed: Vec<Failure>,
}

impl Rejection {
    /// Whether any failure carries `reason`.
    pub fn has(&self, reason: RejectionReason) -> bool {
        self.failed.iter().any(|f| f.check == Check::Reason(reason))
    }

    /// The distinct checks that failed, in the order they were found.
    pub fn checks(&self) -> Vec<Check> {
        let mut out = Vec::new();
        for f in &self.failed {
            if !out.contains(&f.check) {
                out.push(f.check);
            }
        }
        out
    }

    /// The failures for one reason.
    pub fn of(&self, reason: RejectionReason) -> Vec<&Failure> {
        self.failed
            .iter()
            .filter(|f| f.check == Check::Reason(reason))
            .collect()
    }

    /// The tool-result shape. A `sequential_split` rejection, which only
    /// exists once policy has promoted the warning, carries its own reason.
    pub fn to_core(&self) -> PlanRejected {
        PlanRejected {
            failed: self
                .failed
                .iter()
                .map(|f| match f.check {
                    Check::Reason(reason) => FailedCheck {
                        reason,
                        offending: f.offending.clone(),
                        detail: f.detail.clone(),
                    },
                    Check::SequentialSplit => FailedCheck {
                        reason: RejectionReason::SequentialSplit,
                        offending: f.offending.clone(),
                        detail: f.detail.clone(),
                    },
                })
                .collect(),
        }
    }
}

/// An accepted filing: everything the controller writes in one
/// transaction. After writing it, [`super::settle`] over
/// [`Accepted::changed`] gives readiness and roll-ups; the controller runs
/// that once the filing's planning item is reconciled (6.1).
#[derive(Debug, Clone, PartialEq)]
pub struct Accepted {
    /// The `work_plans` row id, stamped as `plan_id` on every new item.
    pub plan_id: String,
    pub root: WorkItemId,
    /// Temp id to real id, for every new item that had one.
    pub ids: BTreeMap<String, WorkItemId>,
    /// New rows, in filing order: `queued`, or `blocked(needs_consent)`
    /// with `held_by` set when held for approval.
    pub items: Vec<WorkItem>,
    /// New edge rows, already re-pointed.
    pub edges: Vec<Edge>,
    /// What the filing does to existing items: supersedes with their
    /// re-points, dropped edges and cascades, holds cleared behind them,
    /// and ready items sent back to `queued` by a new unsatisfied edge.
    pub effects: Effects,
    /// Items waiting on the approval question.
    pub held: Vec<WorkItemId>,
    /// The approval question's id, written to `held_by`.
    pub question: Option<String>,
    pub triggers: Vec<ApprovalTrigger>,
    /// The policy that required approval, when one did.
    pub policy: Option<String>,
    pub warnings: Vec<PlanWarning>,
    /// Whether the filing superseded anything, so it counts against the
    /// supersede rate.
    pub supersedes: bool,
}

impl Accepted {
    /// The real id of a temp id.
    pub fn id(&self, tmp: &str) -> Option<&WorkItemId> {
        self.ids.get(tmp)
    }

    /// Every item the filing created or changed, for [`super::settle`].
    pub fn changed(&self) -> Vec<WorkItemId> {
        let mut out: Vec<WorkItemId> = self.items.iter().map(|i| i.id.clone()).collect();
        for id in self.effects.touched() {
            super::push_unique(&mut out, id);
        }
        for e in &self.edges {
            super::push_unique(&mut out, e.item.clone());
        }
        out
    }

    /// The tool-result shape.
    pub fn to_core(&self) -> PlanAccepted {
        PlanAccepted {
            root: self.root.clone(),
            ids: self.ids.clone(),
            held: self.held.clone(),
            policy: self.policy.clone(),
            warnings: self.warnings.clone(),
        }
    }
}

/// The result of [`validate`].
#[derive(Debug, Clone, PartialEq)]
pub enum Validation {
    Accepted(Box<Accepted>),
    Rejected(Rejection),
}

impl Validation {
    pub fn accepted(&self) -> Option<&Accepted> {
        match self {
            Validation::Accepted(a) => Some(a),
            Validation::Rejected(_) => None,
        }
    }

    pub fn rejected(&self) -> Option<&Rejection> {
        match self {
            Validation::Accepted(_) => None,
            Validation::Rejected(r) => Some(r),
        }
    }

    /// The tool-result shape.
    pub fn to_outcome(&self) -> PlanOutcome {
        match self {
            Validation::Accepted(a) => PlanOutcome::Accepted(a.to_core()),
            Validation::Rejected(r) => PlanOutcome::Rejected(r.to_core()),
        }
    }
}

// ── the validator ──────────────────────────────────────────────────────

/// Validate one filing against the snapshot (14.1). Returns every failed
/// check, or the rows and effects of the accepted filing.
///
/// The checks, and the reason each rejects with:
///
/// | Check | Reason |
/// |---|---|
/// | every ref is a temp id of this call or an item the caller may see; temp ids unique | `unknown_ref`, `duplicate_tmp` |
/// | each draft has a title, objective and `done_when`, sets no field its path may not (`plan: true` outside `work_file`), names a usable budget, and a target has one replacement | `invalid_item` |
/// | no `code` item unless the path allows it | `kind_not_allowed` |
/// | the root, existing parents and supersede targets sit under the caller's scope | `out_of_scope` |
/// | no cycle over every edge kind and parent link, a parent depending on each child and each child inheriting its parent's ordering edges; no ordering edge between an item and its own ancestor or descendant | `cycle` |
/// | parent-tree depth and open items under the root within the caps | `depth_exceeded`, `too_many_items` |
/// | children's budgets within the parent's (remaining) budget | `over_budget` |
/// | a plan B has no other ordering edge, and no `blocks` or `waits_for` edge names an unreleased plan B | `plan_b_edges` |
/// | an ordering edge onto an existing item only while it waits; no new child under a verifying parent or an active leaf | `edge_onto_active` |
/// | every `inputs_from` entry is an item the downstream is ordered after | `input_unordered` |
/// | no item filed already cancelled, held or expired (by its edges, its ancestors or its `expires_at`) | `dead_filing` |
/// | supersede targets are waiting, as is their subtree | `supersedes_active`, `supersedes_closed` |
/// | no two unordered items write the same resource | `single_writer_conflict` |
/// | one accepted graph per planning run | `already_planned` |
/// | supersede filings within the policy rate | `rate_limited` |
/// | no `blocks` link joins steps that belong in one worker | `sequential_split`, a warning by default |
pub fn validate(snap: &Snapshot, plan: &WorkPlan, ctx: &FilingContext) -> Validation {
    let now = ctx.now;
    let mut run = Run::new(snap, plan, ctx);

    // References.
    let root = run.resolve(&plan.root, "the root");
    let root_index = match &plan.root {
        ItemRef::Tmp { tmp } => run.by_tmp.get(tmp).copied(),
        ItemRef::Id(_) => None,
    };
    let mut parents: Vec<Option<WorkItemId>> = Vec::with_capacity(plan.items.len());
    let mut inputs: Vec<Vec<WorkItemId>> = Vec::with_capacity(plan.items.len());
    for (i, draft) in plan.items.iter().enumerate() {
        let whose = run.name(i);
        let parent = match &draft.parent {
            Some(r) => run.resolve(r, &format!("the parent of {whose}")),
            None if Some(i) == root_index => None,
            None => root.clone(),
        };
        parents.push(parent);
        let mut list: Vec<WorkItemId> = Vec::new();
        for r in &draft.inputs_from {
            if let Some(id) = run.resolve(r, &format!("the inputs of {whose}")) {
                if !list.contains(&id) {
                    list.push(id);
                }
            }
        }
        inputs.push(list);
    }
    let new_edges = run.resolve_edges();

    // Drafts.
    let kinds = run.resolve_kinds(&parents);
    run.check_drafts(&kinds);

    // Scope.
    let scope: Option<WorkItemId> = ctx
        .scope
        .clone()
        .or_else(|| root.clone().filter(|r| snap.contains(r)));
    run.check_scope(root.as_deref(), &parents, scope.as_deref());

    // The graph as it would stand.
    let plan_id = Uuid::new_v4().to_string();
    let mut work = snap.clone();
    for i in 0..plan.items.len() {
        work.add_item(run.build_item(i, parents[i].clone(), kinds[i], inputs[i].clone(), &plan_id));
    }
    for edge in &new_edges {
        work.add_edge(edge.clone());
    }

    // Supersedes, applied before the structural checks.
    let pairs: Vec<(WorkItemId, WorkItemId)> = new_edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Supersedes)
        .map(|e| (e.item.clone(), e.depends_on.clone()))
        .collect();
    let valid_pairs = run.check_supersedes(&pairs, scope.as_deref());
    let mut effects = Effects::default();
    let fresh_edges: HashSet<Edge> = new_edges.iter().cloned().collect();
    supersede_in(
        &mut work,
        &valid_pairs,
        &run.fresh,
        &fresh_edges,
        now,
        &mut effects,
    );

    let mut touched: HashSet<WorkItemId> = run.fresh.clone();
    touched.extend(new_edges.iter().map(|e| e.item.clone()));
    touched.extend(effects.repoints.iter().map(|r| r.item.clone()));

    // Structure.
    run.check_cycles(&work, &touched);
    run.check_depth(&work);
    run.check_count(&work, root.as_deref());
    run.fill_budgets(&mut work, &parents);
    run.check_plan_b(&work, &touched);
    let mut dead = run.check_onto_active(&new_edges, &parents);
    run.default_inputs(&mut work);
    run.check_inputs(&work);
    dead.extend(run.cap_expiry(&mut work));
    run.check_dead(&work, &touched, dead);
    run.check_writers(&work, root.as_deref());
    if ctx.already_planned {
        run.fail(
            RejectionReason::AlreadyPlanned,
            root.iter().map(|r| run.label(r)).collect(),
            "this planning run already has an accepted graph",
        );
    }
    if !pairs.is_empty() && ctx.supersedes_in_window >= ctx.supersede_limit {
        let targets: Vec<ItemRef> = pairs.iter().map(|(_, t)| run.label(t)).collect();
        run.fail(
            RejectionReason::RateLimited,
            targets,
            format!(
                "{} supersede filings under this root in the window; the policy allows {}",
                ctx.supersedes_in_window, ctx.supersede_limit
            ),
        );
    }

    // Approval, then the one check that reads it.
    let (held, triggers) = approve(&work, &run.ids, &run.fresh, ctx);
    let held_set: HashSet<WorkItemId> = held.iter().cloned().collect();
    let warnings = run.check_split(&work, &held_set);

    // An unresolved root has already failed with `unknown_ref`.
    let (Some(root), true) = (root, run.failed.is_empty()) else {
        return Validation::Rejected(Rejection { failed: run.failed });
    };

    // Accepted: approval holds on the new rows.
    let question = (!held.is_empty()).then(|| Uuid::new_v4().to_string());
    for id in &held {
        if let Some(row) = work.item_mut(id) {
            row.status = Status::Blocked(BlockedReason::NeedsConsent);
            row.held_by = question.clone();
        }
    }
    // A ready item that gains an unsatisfied edge returns to `queued` (4.4).
    let gained: Vec<WorkItemId> = work
        .items()
        .iter()
        .filter(|i| !run.fresh.contains(&i.id) && touched.contains(&i.id))
        .map(|i| i.id.clone())
        .collect();
    for id in gained {
        if work.status(&id) == Some(Status::Ready) && !edge_summary(&work, &id).satisfied {
            effects.transitions.push(Transition::plain(
                &id,
                Status::Ready,
                Status::Queued,
                "gained an unsatisfied edge",
            ));
            work.set_status(&id, Status::Queued, None, now);
        }
    }

    let items: Vec<WorkItem> = run
        .ids
        .iter()
        .filter_map(|id| work.item(id).cloned())
        .collect();
    let stored: HashSet<&Edge> = snap.edges().iter().collect();
    let repointed: HashSet<Edge> = effects
        .repoints
        .iter()
        .filter_map(|r| match r.link {
            Link::Edge(kind) => Some(Edge {
                item: r.item.clone(),
                depends_on: r.new_upstream.clone(),
                kind,
            }),
            Link::Input => None,
        })
        .collect();
    let edges: Vec<Edge> = work
        .edges()
        .iter()
        .filter(|e| !stored.contains(e) && !repointed.contains(*e))
        .cloned()
        .collect();
    let ids: BTreeMap<String, WorkItemId> = run
        .by_tmp
        .iter()
        .map(|(tmp, &i)| (tmp.clone(), run.ids[i].clone()))
        .collect();
    let policy = question.as_ref().and(ctx.approval.id.clone());

    Validation::Accepted(Box::new(Accepted {
        plan_id,
        root,
        ids,
        items,
        edges,
        effects,
        held,
        question,
        triggers,
        policy,
        warnings,
        supersedes: !valid_pairs.is_empty(),
    }))
}

struct Run<'a> {
    snap: &'a Snapshot,
    plan: &'a WorkPlan,
    ctx: &'a FilingContext,
    /// The new id of each draft, by index.
    ids: Vec<WorkItemId>,
    labels: HashMap<WorkItemId, ItemRef>,
    by_tmp: HashMap<String, usize>,
    fresh: HashSet<WorkItemId>,
    failed: Vec<Failure>,
}

impl<'a> Run<'a> {
    fn new(snap: &'a Snapshot, plan: &'a WorkPlan, ctx: &'a FilingContext) -> Run<'a> {
        let mut run = Run {
            snap,
            plan,
            ctx,
            ids: Vec::new(),
            labels: HashMap::new(),
            by_tmp: HashMap::new(),
            fresh: HashSet::new(),
            failed: Vec::new(),
        };
        for (i, draft) in plan.items.iter().enumerate() {
            let id = Uuid::new_v4().to_string();
            let label = ItemRef::Tmp {
                tmp: draft.tmp.clone().unwrap_or_else(|| format!("#{i}")),
            };
            if let Some(tmp) = &draft.tmp {
                if run.by_tmp.contains_key(tmp) {
                    run.fail(
                        RejectionReason::DuplicateTmp,
                        vec![label.clone()],
                        format!("temp id {tmp:?} is used by more than one item"),
                    );
                } else {
                    run.by_tmp.insert(tmp.clone(), i);
                }
            }
            run.labels.insert(id.clone(), label);
            run.fresh.insert(id.clone());
            run.ids.push(id);
        }
        run
    }

    fn fail(
        &mut self,
        reason: RejectionReason,
        offending: Vec<ItemRef>,
        detail: impl Into<String>,
    ) {
        let failure = Failure {
            check: Check::Reason(reason),
            offending,
            detail: detail.into(),
        };
        if !self.failed.contains(&failure) {
            self.failed.push(failure);
        }
    }

    /// How a rejection names an item: its temp id when new, else its id.
    fn label(&self, id: &str) -> ItemRef {
        self.labels
            .get(id)
            .cloned()
            .unwrap_or_else(|| ItemRef::Id(id.to_string()))
    }

    /// A draft's name for detail strings.
    fn name(&self, i: usize) -> String {
        match &self.plan.items[i].tmp {
            Some(tmp) => format!("{tmp:?}"),
            None => format!("item #{i}"),
        }
    }

    fn visible(&self, id: &str) -> bool {
        self.snap.contains(id) && self.ctx.visible.as_ref().is_none_or(|v| v.contains(id))
    }

    fn resolve(&mut self, r: &ItemRef, whose: &str) -> Option<WorkItemId> {
        match r {
            ItemRef::Tmp { tmp } => match self.by_tmp.get(tmp) {
                Some(&i) => Some(self.ids[i].clone()),
                None => {
                    self.fail(
                        RejectionReason::UnknownRef,
                        vec![r.clone()],
                        format!("no item in this call has temp id {tmp:?} ({whose})"),
                    );
                    None
                }
            },
            ItemRef::Id(id) => {
                if self.visible(id) {
                    Some(id.clone())
                } else {
                    self.fail(
                        RejectionReason::UnknownRef,
                        vec![r.clone()],
                        format!("no item {id} the caller may see ({whose})"),
                    );
                    None
                }
            }
        }
    }

    /// Every edge the filing declares, resolved and deduplicated, without
    /// the ones the store already holds.
    fn resolve_edges(&mut self) -> Vec<Edge> {
        let mut out: Vec<Edge> = Vec::new();
        let push = |out: &mut Vec<Edge>, snap: &Snapshot, edge: Edge| {
            if !out.contains(&edge) && !snap.edges().contains(&edge) {
                out.push(edge);
            }
        };
        let plan = self.plan;
        for (i, draft) in plan.items.iter().enumerate() {
            let me = self.ids[i].clone();
            let whose = format!("an edge of {}", self.name(i));
            for de in &draft.edges {
                if let Some(up) = self.resolve(&de.depends_on, &whose) {
                    push(
                        &mut out,
                        self.snap,
                        Edge {
                            item: me.clone(),
                            depends_on: up,
                            kind: de.kind,
                        },
                    );
                }
            }
            if let Some(target) = &draft.supersedes {
                if let Some(up) = self.resolve(&ItemRef::Id(target.clone()), &whose) {
                    push(
                        &mut out,
                        self.snap,
                        Edge {
                            item: me.clone(),
                            depends_on: up,
                            kind: EdgeKind::Supersedes,
                        },
                    );
                }
            }
            if let Some(source) = &self.ctx.discovered_from {
                push(
                    &mut out,
                    self.snap,
                    Edge {
                        item: me.clone(),
                        depends_on: source.clone(),
                        kind: EdgeKind::DiscoveredFrom,
                    },
                );
            }
        }
        for pe in &plan.edges {
            let item = self.resolve(&pe.item, "a plan edge");
            let up = self.resolve(&pe.depends_on, "a plan edge");
            if let (Some(item), Some(up)) = (item, up) {
                push(
                    &mut out,
                    self.snap,
                    Edge {
                        item,
                        depends_on: up,
                        kind: pe.kind,
                    },
                );
            }
        }
        out
    }

    /// A draft without a kind takes its parent's; a top-level one is
    /// `personal`.
    fn resolve_kinds(&self, parents: &[Option<WorkItemId>]) -> Vec<WorkKind> {
        let position: HashMap<&str, usize> = self
            .ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();
        let n = self.plan.items.len();
        (0..n)
            .map(|i| {
                let mut at = i;
                for _ in 0..=n {
                    if let Some(kind) = self.plan.items[at].kind {
                        return kind;
                    }
                    match &parents[at] {
                        Some(p) => match position.get(p.as_str()) {
                            Some(&j) => at = j,
                            None => {
                                return self
                                    .snap
                                    .item(p)
                                    .map(|x| x.kind)
                                    .unwrap_or(WorkKind::Personal)
                            }
                        },
                        None => return WorkKind::Personal,
                    }
                }
                WorkKind::Personal
            })
            .collect()
    }

    fn check_drafts(&mut self, kinds: &[WorkKind]) {
        let plan = self.plan;
        for (i, draft) in plan.items.iter().enumerate() {
            let label = self.label(&self.ids[i]);
            let missing: Vec<&str> = [
                ("title", &draft.title),
                ("objective", &draft.objective),
                ("done_when", &draft.done_when),
            ]
            .iter()
            .filter(|(_, v)| v.trim().is_empty())
            .map(|(k, _)| *k)
            .collect();
            if !missing.is_empty() {
                self.fail(
                    RejectionReason::InvalidItem,
                    vec![label.clone()],
                    format!("missing {}", missing.join(", ")),
                );
            }
            if draft.plan && self.ctx.source != FilingSource::WorkFile {
                self.fail(
                    RejectionReason::InvalidItem,
                    vec![label.clone()],
                    "`plan: true` belongs to work_file; an item of a graph is already planned",
                );
            }
            if let Some(b) = &draft.budget {
                if b.iterations == 0 || b.tokens == 0 || b.wall_seconds == 0 {
                    self.fail(
                        RejectionReason::InvalidItem,
                        vec![label.clone()],
                        "a budget needs iterations, tokens and wall seconds",
                    );
                }
            }
            if kinds[i] == WorkKind::Code && !self.ctx.allow_code {
                self.fail(
                    RejectionReason::KindNotAllowed,
                    vec![label],
                    "code graphs come only from the delivery compiler",
                );
            }
        }
    }

    fn check_scope(
        &mut self,
        root: Option<&str>,
        parents: &[Option<WorkItemId>],
        scope: Option<&str>,
    ) {
        if let (ItemRef::Id(r), Some(s)) = (&self.plan.root, &self.ctx.scope) {
            if self.snap.contains(r) && !self.snap.is_within(r, s) {
                self.fail(
                    RejectionReason::OutOfScope,
                    vec![ItemRef::Id(r.clone())],
                    format!("the root {r} is outside the caller's scope {s}"),
                );
            }
        }
        for (i, parent) in parents.iter().enumerate() {
            let Some(p) = parent else {
                continue;
            };
            if self.fresh.contains(p) || !self.snap.contains(p) || Some(p.as_str()) == root {
                continue;
            }
            if !scope.is_some_and(|s| self.snap.is_within(p, s)) {
                let label = self.label(&self.ids[i]);
                self.fail(
                    RejectionReason::OutOfScope,
                    vec![label, ItemRef::Id(p.clone())],
                    format!("the parent {p} is outside the caller's scope"),
                );
            }
        }
    }

    fn build_item(
        &self,
        i: usize,
        parent: Option<WorkItemId>,
        kind: WorkKind,
        inputs_from: Vec<WorkItemId>,
        plan_id: &str,
    ) -> WorkItem {
        let d = &self.plan.items[i];
        WorkItem {
            id: self.ids[i].clone(),
            kind,
            title: d.title.clone(),
            objective: d.objective.clone(),
            done_when: d.done_when.clone(),
            constraints: d.constraints.clone(),
            decisions_made: d.decisions_made.clone(),
            artifact_refs: d.artifact_refs.clone(),
            required_tools: d.required_tools.clone(),
            required_mcp_servers: d.required_mcp_servers.clone(),
            worker_kind: d.worker_kind,
            writable_resources: d.writable_resources.clone(),
            parent,
            inputs_from,
            origin_conversation_id: self.ctx.origin_conversation_id.clone(),
            trigger: d.trigger.clone(),
            preconditions: d.preconditions.clone(),
            expires_at: d.expires_at,
            budget: d.budget.unwrap_or(self.ctx.default_budget),
            priority: d.priority,
            status: Status::Queued,
            status_origin: None,
            plan_id: Some(plan_id.to_string()),
            held_by: None,
            // Items of one filing keep the order the caller listed them in:
            // each is stamped a microsecond after the one before, and the
            // store and the controller's select break ties by `created_at`.
            created_at: self.ctx.now + TimeDelta::microseconds(i64::try_from(i).unwrap_or(0)),
            updated_at: self.ctx.now,
            closed_at: None,
        }
    }

    /// 4.4's per-target rules. Returns the pairs that may be applied.
    fn check_supersedes(
        &mut self,
        pairs: &[(WorkItemId, WorkItemId)],
        scope: Option<&str>,
    ) -> Vec<(WorkItemId, WorkItemId)> {
        let mut replacements: HashMap<&str, Vec<&str>> = HashMap::new();
        for (r, t) in pairs {
            replacements.entry(t.as_str()).or_default().push(r.as_str());
        }
        let mut valid = Vec::new();
        for (r, t) in pairs {
            let many = replacements.get(t.as_str()).is_some_and(|rs| rs.len() > 1);
            if many {
                let offending: Vec<ItemRef> = replacements[t.as_str()]
                    .iter()
                    .map(|x| self.label(x))
                    .collect();
                self.fail(
                    RejectionReason::InvalidItem,
                    offending,
                    format!("{t} is superseded by more than one item; one replaces it"),
                );
                continue;
            }
            if self.fresh.contains(t) {
                let label = self.label(t);
                self.fail(
                    RejectionReason::InvalidItem,
                    vec![self.label(r), label],
                    "a supersede target must already exist",
                );
                continue;
            }
            let refusals = supersede_refusals(self.snap, t, scope);
            if refusals.is_empty() {
                valid.push((r.clone(), t.clone()));
            }
            for refusal in refusals {
                let offending = refusal.items.iter().map(|x| self.label(x)).collect();
                self.fail(refusal.reason, offending, refusal.detail);
            }
        }
        valid
    }

    fn check_cycles(&mut self, work: &Snapshot, touched: &HashSet<WorkItemId>) {
        let mut reported: HashSet<Edge> = HashSet::new();
        for edge in work.edges() {
            if !edge.kind.is_ordering() || !touched.contains(&edge.item) {
                continue;
            }
            let ancestral = edge.item == edge.depends_on
                || work.ancestors(&edge.item).contains(&edge.depends_on)
                || work.ancestors(&edge.depends_on).contains(&edge.item);
            if ancestral {
                let mut offending = vec![self.label(&edge.item)];
                if edge.depends_on != edge.item {
                    offending.push(self.label(&edge.depends_on));
                }
                self.fail(
                    RejectionReason::Cycle,
                    offending,
                    format!(
                        "a {} edge between an item and its own ancestor or descendant",
                        edge.kind.as_str()
                    ),
                );
                reported.insert(edge.clone());
            }
        }
        let arcs = Arcs::build(work, true, &reported);
        for component in arcs.cycles() {
            if !component.iter().any(|&n| touched.contains(&arcs.nodes[n])) {
                continue;
            }
            let offending = component
                .iter()
                .map(|&n| self.label(&arcs.nodes[n]))
                .collect();
            let path: Vec<String> = arcs
                .cycle_path(&component)
                .iter()
                .map(|id| describe(&self.label(id)))
                .collect();
            self.fail(
                RejectionReason::Cycle,
                offending,
                format!("cycle: {}", path.join(" -> ")),
            );
        }
    }

    fn check_depth(&mut self, work: &Snapshot) {
        let max = self.ctx.caps.max_depth as usize;
        let deep: Vec<ItemRef> = self
            .ids
            .iter()
            .filter(|id| work.ancestors(id).len() + 1 > max)
            .map(|id| self.label(id))
            .collect();
        if !deep.is_empty() {
            self.fail(
                RejectionReason::DepthExceeded,
                deep,
                format!("the parent tree may be at most {max} levels deep"),
            );
        }
    }

    /// The open tree the filing leaves: the root and the open items under
    /// it (so the cap is on the tree's size, 12 items meaning a root and
    /// eleven others), plus new items filed elsewhere.
    fn check_count(&mut self, work: &Snapshot, root: Option<&str>) {
        let max = self.ctx.caps.max_items as usize;
        let under: HashSet<WorkItemId> = root
            .map(|r| work.descendants(r))
            .unwrap_or_default()
            .into_iter()
            .collect();
        let open = |id: &str| work.status(id).is_some_and(|s| !s.is_closed());
        let open_under = under.iter().filter(|id| open(id)).count();
        let root_open = usize::from(root.is_some_and(open));
        let elsewhere = self
            .ids
            .iter()
            .filter(|id| !under.contains(*id) && Some(id.as_str()) != root)
            .count();
        let count = root_open + open_under + elsewhere;
        if count > max {
            let offending = root.map(|r| vec![self.label(r)]).unwrap_or_default();
            self.fail(
                RejectionReason::TooManyItems,
                offending,
                format!("{count} open items in the tree; the cap is {max}"),
            );
        }
    }

    /// Check each parent's envelope (4.2) and fill the budgets of drafts
    /// that named none with an equal share of what is left, capped at the
    /// default budget.
    fn fill_budgets(&mut self, work: &mut Snapshot, parents: &[Option<WorkItemId>]) {
        let mut groups: Vec<(WorkItemId, Vec<usize>)> = Vec::new();
        for (i, parent) in parents.iter().enumerate() {
            let Some(p) = parent else {
                continue;
            };
            match groups.iter_mut().find(|(q, _)| q == p) {
                Some((_, kids)) => kids.push(i),
                None => groups.push((p.clone(), vec![i])),
            }
        }
        groups.sort_by_key(|(p, _)| work.ancestors(p).len());

        for (p, kids) in groups {
            let Some(row) = work.item(&p) else {
                continue;
            };
            let remaining = if self.fresh.contains(&p) {
                Envelope::of(&row.budget)
            } else if let Some(b) = self.ctx.remaining_budget.get(&p) {
                Envelope::of(b)
            } else {
                let committed = work
                    .children(&p)
                    .iter()
                    .filter(|c| !self.fresh.contains(*c))
                    .filter_map(|c| work.item(c))
                    .filter(|c| !c.status.is_closed())
                    .fold(Envelope::default(), |acc, c| {
                        acc.plus(Envelope::of(&c.budget))
                    });
                Envelope::of(&row.budget).minus(committed)
            };
            let (explicit, unbudgeted): (Vec<usize>, Vec<usize>) = kids
                .iter()
                .partition(|&&i| self.plan.items[i].budget.is_some());
            let asked = explicit
                .iter()
                .filter_map(|&i| self.plan.items[i].budget.as_ref())
                .fold(Envelope::default(), |acc, b| acc.plus(Envelope::of(b)));
            if !asked.fits_in(remaining) {
                let offending = explicit.iter().map(|&i| self.label(&self.ids[i])).collect();
                self.fail(
                    RejectionReason::OverBudget,
                    offending,
                    format!("the children ask for {asked}; {p} has {remaining} left"),
                );
                continue;
            }
            if unbudgeted.is_empty() {
                continue;
            }
            let share = remaining
                .minus(asked)
                .share(unbudgeted.len())
                .min(Envelope::of(&self.ctx.default_budget));
            if share.any_zero() {
                let offending = unbudgeted
                    .iter()
                    .map(|&i| self.label(&self.ids[i]))
                    .collect();
                self.fail(
                    RejectionReason::OverBudget,
                    offending,
                    format!("nothing is left of {p}'s budget for the items that name none"),
                );
                continue;
            }
            for &i in &unbudgeted {
                if let Some(item) = work.item_mut(&self.ids[i]) {
                    item.budget = share.budget(self.ctx.default_budget);
                }
            }
        }
    }

    fn check_plan_b(&mut self, work: &Snapshot, touched: &HashSet<WorkItemId>) {
        let stored: HashSet<&Edge> = self.snap.edges().iter().collect();
        for item in work.items() {
            if !touched.contains(&item.id) {
                continue;
            }
            let ordering: Vec<&Edge> = work
                .edges_held_by(&item.id)
                .filter(|e| e.kind.is_ordering())
                .collect();
            let conditional = ordering
                .iter()
                .any(|e| e.kind == EdgeKind::ConditionalOnFailure);
            if conditional && ordering.len() > 1 {
                self.fail(
                    RejectionReason::PlanBEdges,
                    vec![self.label(&item.id)],
                    "a plan B has exactly one upstream edge: the step it covers",
                );
            }
            for edge in ordering {
                if stored.contains(edge)
                    || !matches!(edge.kind, EdgeKind::Blocks | EdgeKind::WaitsFor)
                {
                    continue;
                }
                if unreleased_plan_b(work, &edge.depends_on) {
                    self.fail(
                        RejectionReason::PlanBEdges,
                        vec![self.label(&item.id), self.label(&edge.depends_on)],
                        "downstream items name the step, never its plan B",
                    );
                }
            }
        }
    }

    /// Ordering edges onto existing items only while they wait; new
    /// children only under a parent that can take them. Returns the new
    /// items already reported dead (under a closed parent).
    fn check_onto_active(
        &mut self,
        new_edges: &[Edge],
        parents: &[Option<WorkItemId>],
    ) -> HashSet<WorkItemId> {
        let mut dead = HashSet::new();
        for edge in new_edges {
            if !edge.kind.is_ordering() || self.fresh.contains(&edge.item) {
                continue;
            }
            if let Some(status) = self.snap.status(&edge.item).filter(|s| !s.is_waiting()) {
                self.fail(
                    RejectionReason::EdgeOntoActive,
                    vec![ItemRef::Id(edge.item.clone())],
                    format!(
                        "{} is {status}: an edge may be added only while it is queued, ready or blocked",
                        edge.item
                    ),
                );
            }
        }
        for (i, parent) in parents.iter().enumerate() {
            let Some(p) = parent else {
                continue;
            };
            if self.fresh.contains(p) {
                continue;
            }
            let Some(row) = self.snap.item(p) else {
                continue;
            };
            let label = self.label(&self.ids[i]);
            if row.status.is_closed() {
                self.fail(
                    RejectionReason::DeadFiling,
                    vec![label],
                    format!("the parent {p} is {}: it takes no new children", row.status),
                );
                dead.insert(self.ids[i].clone());
            } else if row.status == Status::Verifying {
                self.fail(
                    RejectionReason::EdgeOntoActive,
                    vec![label, ItemRef::Id(p.clone())],
                    format!("the parent {p} is verifying: it takes no new children"),
                );
            } else if row.status.is_active() && !self.snap.has_children(p) {
                self.fail(
                    RejectionReason::EdgeOntoActive,
                    vec![label, ItemRef::Id(p.clone())],
                    format!(
                        "{p} is {}: an active item cannot become a parent",
                        row.status
                    ),
                );
            }
        }
        dead
    }

    /// 4.3: an item that names no inputs takes its `blocks` upstreams.
    fn default_inputs(&self, work: &mut Snapshot) {
        for (i, id) in self.ids.iter().enumerate() {
            if !self.plan.items[i].inputs_from.is_empty() {
                continue;
            }
            let mut upstreams: Vec<WorkItemId> = Vec::new();
            for e in work.edges_held_by(id) {
                if e.kind == EdgeKind::Blocks && !upstreams.contains(&e.depends_on) {
                    upstreams.push(e.depends_on.clone());
                }
            }
            if let Some(item) = work.item_mut(id) {
                item.inputs_from = upstreams;
            }
        }
    }

    fn check_inputs(&mut self, work: &Snapshot) {
        let arcs = Arcs::build(work, false, &HashSet::new());
        for id in self.ids.clone() {
            let Some(item) = work.item(&id) else {
                continue;
            };
            let reach = arcs.reach(&id);
            for input in &item.inputs_from {
                if input == &id || !reach.contains(input) {
                    self.fail(
                        RejectionReason::InputUnordered,
                        vec![self.label(&id), self.label(input)],
                        format!(
                            "{} is not ordered after {}, so its results may not exist at lease time",
                            describe(&self.label(&id)),
                            describe(&self.label(input))
                        ),
                    );
                }
            }
        }
    }

    /// Cap each new item's `expires_at` at its parent's (4.2), top-down.
    /// Returns the items that would be filed already expired.
    fn cap_expiry(&mut self, work: &mut Snapshot) -> HashSet<WorkItemId> {
        let mut order: Vec<WorkItemId> = self.ids.clone();
        order.sort_by_key(|id| work.ancestors(id).len());
        let mut dead = HashSet::new();
        for id in order {
            let parent_expiry = work
                .item(&id)
                .and_then(|x| x.parent.clone())
                .and_then(|p| work.item(&p))
                .and_then(|p| p.expires_at);
            let Some(row) = work.item_mut(&id) else {
                continue;
            };
            if let Some(cap) = parent_expiry {
                row.expires_at = Some(row.expires_at.map_or(cap, |e| e.min(cap)));
            }
            if row.expires_at.is_some_and(|e| e <= self.ctx.now) {
                let label = self.label(&id);
                self.fail(
                    RejectionReason::DeadFiling,
                    vec![label],
                    "it would be filed already expired",
                );
                dead.insert(id);
            }
        }
        dead
    }

    /// No item is filed already cancelled or held (4.4): 4.1's table on
    /// each new item's edges and its ancestors' edges, and on the new edges
    /// of existing items, run to a fixpoint so an item behind a dead one is
    /// dead too.
    fn check_dead(
        &mut self,
        work: &Snapshot,
        touched: &HashSet<WorkItemId>,
        mut dead: HashSet<WorkItemId>,
    ) {
        let stored: HashSet<&Edge> = self.snap.edges().iter().collect();
        let mut scratch = work.clone();
        // A new item already closed in the working graph was cancelled by
        // a supersede's subtree cascade: it sits under a superseded parent.
        for id in self.ids.clone() {
            if dead.contains(&id) {
                continue;
            }
            if let Some(status) = work.status(&id).filter(|s| s.is_closed()) {
                let label = self.label(&id);
                self.fail(
                    RejectionReason::DeadFiling,
                    vec![label],
                    format!("it would be filed {status}: its parent is superseded"),
                );
                dead.insert(id);
            }
        }
        let candidates: Vec<WorkItemId> = work
            .items()
            .iter()
            .filter(|i| touched.contains(&i.id) && i.status.is_waiting())
            .map(|i| i.id.clone())
            .collect();
        loop {
            let mut changed = false;
            for id in &candidates {
                if dead.contains(id) {
                    continue;
                }
                let fresh = self.fresh.contains(id);
                let mut cause: Option<(Status, WorkItemId, String)> = None;
                for edge in scratch.edges_held_by(id) {
                    if !fresh && stored.contains(edge) {
                        continue;
                    }
                    match verdict(&scratch, edge) {
                        Verdict::Cancel(c) => {
                            cause = Some((
                                Status::Cancelled(CancelReason::Cascade),
                                c.origin,
                                format!(
                                    "{} {} would cancel it at once",
                                    edge.kind.as_str(),
                                    c.upstream
                                ),
                            ));
                            break;
                        }
                        Verdict::Hold(h) if cause.is_none() => {
                            cause = Some((
                                Status::Blocked(h.reason),
                                h.origin,
                                format!(
                                    "{} {} would hold it {}",
                                    edge.kind.as_str(),
                                    h.upstream,
                                    Status::Blocked(h.reason)
                                ),
                            ));
                        }
                        _ => {}
                    }
                }
                if cause.is_none() && fresh {
                    for a in scratch.ancestors(id) {
                        if dead.contains(&a) {
                            cause = Some((
                                Status::Cancelled(CancelReason::Cascade),
                                a.clone(),
                                format!(
                                    "its parent {} is itself a dead filing",
                                    describe(&self.label(&a))
                                ),
                            ));
                            break;
                        }
                        let gate = edge_summary(&scratch, &a);
                        if let Some(c) = gate.cancel {
                            cause = Some((
                                Status::Cancelled(CancelReason::Cascade),
                                c.origin,
                                format!("its ancestor {a} would be cancelled by {}", c.upstream),
                            ));
                            break;
                        }
                        if let Some(h) = gate.hold {
                            cause = Some((
                                Status::Blocked(h.reason),
                                h.origin,
                                format!("its ancestor {a} is held behind {}", h.upstream),
                            ));
                            break;
                        }
                    }
                }
                if let Some((status, origin, detail)) = cause {
                    let label = self.label(id);
                    self.fail(RejectionReason::DeadFiling, vec![label], detail);
                    dead.insert(id.clone());
                    scratch.set_status(id, status, Some(origin), self.ctx.now);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// No two unordered items write the same resource (6.1, 14.1). Pairs
    /// involving a new item, among open leaves under the root.
    fn check_writers(&mut self, work: &Snapshot, root: Option<&str>) {
        if self.ctx.source == FilingSource::DeliveryImport {
            return;
        }
        let mut pool: Vec<WorkItemId> = Vec::new();
        if let Some(r) = root {
            pool.push(r.to_string());
            pool.extend(work.descendants(r));
        }
        for id in &self.ids {
            super::push_unique(&mut pool, id.clone());
        }
        let writers: Vec<&WorkItem> = pool
            .iter()
            .filter_map(|id| work.item(id))
            .filter(|i| {
                !i.status.is_closed()
                    && !work.has_children(&i.id)
                    && !i.writable_resources.is_empty()
            })
            .collect();
        if writers.len() < 2 {
            return;
        }
        let arcs = Arcs::build(work, false, &HashSet::new());
        let reach: HashMap<&str, HashSet<WorkItemId>> = writers
            .iter()
            .map(|w| (w.id.as_str(), arcs.reach(&w.id)))
            .collect();
        for (i, a) in writers.iter().enumerate() {
            for b in &writers[i + 1..] {
                if !self.fresh.contains(&a.id) && !self.fresh.contains(&b.id) {
                    continue;
                }
                let shared: Vec<&str> = a
                    .writable_resources
                    .iter()
                    .filter(|r| b.writable_resources.contains(r))
                    .map(String::as_str)
                    .collect();
                if shared.is_empty() {
                    continue;
                }
                let ordered =
                    reach[a.id.as_str()].contains(&b.id) || reach[b.id.as_str()].contains(&a.id);
                if !ordered {
                    self.fail(
                        RejectionReason::SingleWriterConflict,
                        vec![self.label(&a.id), self.label(&b.id)],
                        format!(
                            "both write {} and neither is ordered after the other",
                            shared.join(", ")
                        ),
                    );
                }
            }
        }
    }

    /// 14.1's `sequential_split`: a `blocks` link whose downstream has no
    /// other upstream, whose upstream has no other downstream, and whose
    /// downstream adds no kind, trigger, precondition, worker constraint,
    /// writable resource or approval point the upstream lacks.
    fn check_split(&mut self, work: &Snapshot, held: &HashSet<WorkItemId>) -> Vec<PlanWarning> {
        let mut warnings = Vec::new();
        if self.ctx.source == FilingSource::DeliveryImport {
            return warnings;
        }
        for edge in work.edges() {
            if edge.kind != EdgeKind::Blocks
                || !(self.fresh.contains(&edge.item) || self.fresh.contains(&edge.depends_on))
            {
                continue;
            }
            let (Some(up), Some(down)) = (work.item(&edge.depends_on), work.item(&edge.item))
            else {
                continue;
            };
            if work.has_children(&up.id) || work.has_children(&down.id) {
                continue;
            }
            let sole_upstream = work
                .edges_held_by(&down.id)
                .filter(|e| e.kind.is_ordering())
                .count()
                == 1;
            let sole_downstream = work
                .edges_naming(&up.id)
                .filter(|e| e.kind.is_ordering())
                .count()
                == 1;
            let adds_nothing = down.kind == up.kind
                && (down.trigger == Trigger::Now || down.trigger == up.trigger)
                && down
                    .preconditions
                    .iter()
                    .all(|p| up.preconditions.contains(p))
                && (down.worker_kind == WorkerKind::Any || down.worker_kind == up.worker_kind)
                && down
                    .writable_resources
                    .iter()
                    .all(|r| up.writable_resources.contains(r))
                && !(held.contains(&down.id) && !held.contains(&up.id));
            if !(sole_upstream && sole_downstream && adds_nothing) {
                continue;
            }
            match self.ctx.sequential_split {
                SplitMode::Warn => warnings.push(PlanWarning {
                    check: WarningCheck::SequentialSplit,
                    items: vec![up.id.clone(), down.id.clone()],
                }),
                SplitMode::Reject => {
                    let failure = Failure {
                        check: Check::SequentialSplit,
                        offending: vec![self.label(&up.id), self.label(&down.id)],
                        detail:
                            "a blocks link that adds nothing: one worker can do both in one item"
                                .to_string(),
                    };
                    if !self.failed.contains(&failure) {
                        self.failed.push(failure);
                    }
                }
            }
        }
        warnings
    }
}

/// Whether `id` is a plan B its step has not released: it holds a
/// `conditional_on_failure` edge on an item that has not failed.
fn unreleased_plan_b(work: &Snapshot, id: &str) -> bool {
    work.edges_held_by(id).any(|e| {
        e.kind == EdgeKind::ConditionalOnFailure
            && work.status(&e.depends_on) != Some(Status::Failed)
    })
}

/// The approval holds of 6.1 over the new items: the whole graph for the
/// item-count and budget triggers, else each triggering item with what
/// depends on it (through ordering edges and parent links).
fn approve(
    work: &Snapshot,
    ids: &[WorkItemId],
    fresh: &HashSet<WorkItemId>,
    ctx: &FilingContext,
) -> (Vec<WorkItemId>, Vec<ApprovalTrigger>) {
    let policy = &ctx.approval;
    let mut triggers = Vec::new();
    let mut whole = false;
    let count = ids.len() as u32;
    if let Some(threshold) = policy.max_items {
        if count > threshold {
            triggers.push(ApprovalTrigger::ItemCount {
                items: count,
                threshold,
            });
            whole = true;
        }
    }
    let leaves: Vec<&WorkItem> = ids
        .iter()
        .filter(|id| !work.has_children(id))
        .filter_map(|id| work.item(id))
        .collect();
    let tokens: u64 = leaves.iter().map(|i| i.budget.tokens).sum();
    if let Some(threshold) = policy.max_total_tokens {
        if tokens > threshold {
            triggers.push(ApprovalTrigger::Budget { tokens, threshold });
            whole = true;
        }
    }
    let mut seeds: Vec<WorkItemId> = Vec::new();
    for item in &leaves {
        if let Some(delegated) = &policy.delegated_resources {
            for resource in &item.writable_resources {
                if !delegated.contains(resource) {
                    triggers.push(ApprovalTrigger::UndelegatedResource {
                        item: item.id.clone(),
                        resource: resource.clone(),
                    });
                    super::push_unique(&mut seeds, item.id.clone());
                }
            }
        }
        if let Some(slices) = &policy.authorized_slices {
            let inside = work.ancestors(&item.id).iter().any(|a| slices.contains(a));
            if item.kind == WorkKind::Code && !inside {
                triggers.push(ApprovalTrigger::CodeOutsideSlice {
                    item: item.id.clone(),
                });
                super::push_unique(&mut seeds, item.id.clone());
            }
        }
    }
    if whole {
        return (ids.to_vec(), triggers);
    }
    let mut held: HashSet<WorkItemId> = HashSet::new();
    let mut queue: Vec<WorkItemId> = seeds;
    while let Some(h) = queue.pop() {
        if !held.insert(h.clone()) {
            continue;
        }
        for e in work.edges_naming(&h) {
            if e.kind.is_ordering() && fresh.contains(&e.item) {
                queue.push(e.item.clone());
            }
        }
        for d in work.descendants(&h) {
            if fresh.contains(&d) {
                queue.push(d);
            }
        }
    }
    let held = ids
        .iter()
        .filter(|id| held.contains(*id))
        .cloned()
        .collect();
    (held, triggers)
}

fn describe(r: &ItemRef) -> String {
    match r {
        ItemRef::Id(id) => id.clone(),
        ItemRef::Tmp { tmp } => tmp.clone(),
    }
}

/// The parts of a [`Budget`] a parent's envelope bounds (4.2): iterations,
/// tokens and wall time. Repairs and rung budgets are per item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Envelope {
    iterations: u64,
    tokens: u64,
    wall_seconds: u64,
}

impl Envelope {
    fn of(b: &Budget) -> Envelope {
        Envelope {
            iterations: u64::from(b.iterations),
            tokens: b.tokens,
            wall_seconds: b.wall_seconds,
        }
    }

    fn plus(self, o: Envelope) -> Envelope {
        Envelope {
            iterations: self.iterations.saturating_add(o.iterations),
            tokens: self.tokens.saturating_add(o.tokens),
            wall_seconds: self.wall_seconds.saturating_add(o.wall_seconds),
        }
    }

    fn minus(self, o: Envelope) -> Envelope {
        Envelope {
            iterations: self.iterations.saturating_sub(o.iterations),
            tokens: self.tokens.saturating_sub(o.tokens),
            wall_seconds: self.wall_seconds.saturating_sub(o.wall_seconds),
        }
    }

    fn fits_in(self, o: Envelope) -> bool {
        self.iterations <= o.iterations
            && self.tokens <= o.tokens
            && self.wall_seconds <= o.wall_seconds
    }

    fn share(self, n: usize) -> Envelope {
        let n = n.max(1) as u64;
        Envelope {
            iterations: self.iterations / n,
            tokens: self.tokens / n,
            wall_seconds: self.wall_seconds / n,
        }
    }

    fn min(self, o: Envelope) -> Envelope {
        Envelope {
            iterations: self.iterations.min(o.iterations),
            tokens: self.tokens.min(o.tokens),
            wall_seconds: self.wall_seconds.min(o.wall_seconds),
        }
    }

    fn any_zero(self) -> bool {
        self.iterations == 0 || self.tokens == 0 || self.wall_seconds == 0
    }

    fn budget(self, base: Budget) -> Budget {
        Budget {
            iterations: u32::try_from(self.iterations).unwrap_or(u32::MAX),
            tokens: self.tokens,
            wall_seconds: self.wall_seconds,
            ..base
        }
    }
}

impl fmt::Display for Envelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} iterations, {} tokens, {}s",
            self.iterations, self.tokens, self.wall_seconds
        )
    }
}
