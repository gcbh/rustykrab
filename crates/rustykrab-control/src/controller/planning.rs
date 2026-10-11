//! The planner (plan sections 6.1, 6.4 and 6.5), carried out.
//!
//! - **A planned request** (`work_file` with `plan: true`) becomes a parent
//!   with one planning child, which only the `planner` worker covers, and a
//!   fallback child that is the request as one item, `conditional_on_failure`
//!   on the planning child: when the planning item's ladder is spent without
//!   an accepted graph, the request runs as one item on one worker, as it
//!   would have without a planner.
//! - **The planner's filing** attaches beneath the request: a new root it
//!   files becomes the request's child, and every new item at the top of
//!   the graph gets a `blocks` edge onto the planning item, so no item of
//!   the graph is ready before its planning item is reconciled. The accepted
//!   plan ends the planning run with a report naming the graph.
//! - **A re-plan** is a planning item under the parent (6.4 step 3; the
//!   parent's order 1), briefed with the failed item's ladder and error class
//!   as pointers; one per parent by default. The same rung runs when a
//!   subtree stalls (step 7). A worker's `discovered` drafts that validation
//!   rejected become a planning item briefed with the drafts and the reasons
//!   (6.5).
//! - **A stalled subtree** (nothing leased, running or ready beneath a
//!   parent, and nothing it waits on still open) climbs the parent's ladder:
//!   the re-plan, then one message at the parent.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use rustykrab_core::questions::QuestionClass;
use rustykrab_core::work::{
    ArtifactRef, BlockedReason, Budget, DraftEdge, EdgeKind, ItemRef, PlanAccepted, PlanEdge,
    ResultReport, Rung, RungEvent, Status, Trigger, WorkError, WorkItem, WorkItemDraft, WorkItemId,
    WorkPlan,
};
use rustykrab_core::Error;
use rustykrab_store::QuestionRow;
use rustykrab_tools::work_backend::Provenance;

use crate::graph::{is_planning, Accepted, FilingSource, Rejection, Snapshot, PLAN_TOOL};
use crate::ladder::summary;
use crate::worker::Worker;

use super::batch::Batch;
use super::filing::describe_rejection;
use super::notice::{short, Cause};
use super::Controller;

/// Characters of a draft or a reason one constraint line of a re-plan
/// brief keeps.
const LINE_MAX: usize = 280;
/// Drafts a re-plan brief lists.
const DRAFTS_MAX: usize = 8;

fn clip(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

/// Whether a worker is reserved for planning and holds the plan tool.
/// A peer's broad tool advertisement does not reserve it for planning.
pub(super) fn plans(worker: &dyn Worker) -> bool {
    worker.planning_only() && worker.capabilities().tools.iter().any(|t| t == PLAN_TOOL)
}

impl Controller {
    /// Whether a `planner` worker is registered, which is what makes a
    /// planning item runnable.
    pub(super) fn has_planner(&self) -> bool {
        self.registry.workers().iter().any(|w| plans(w.as_ref()))
    }

    /// The budget of a planned request that named none: an envelope for a
    /// graph of the caps' size, since a parent's budget bounds its
    /// children's (4.2).
    fn plan_envelope(&self) -> Budget {
        let d = self.config.default_budget;
        let n = self.config.caps.max_items.max(1);
        Budget {
            iterations: d.iterations.saturating_mul(n),
            tokens: d.tokens.saturating_mul(u64::from(n)),
            wall_seconds: d.wall_seconds.saturating_mul(u64::from(n)),
            ..d
        }
    }

    /// Shape a filing before validation: a planned request gets an envelope
    /// budget, and a planner's graph attaches beneath its request behind
    /// the planning item (6.1).
    pub(super) fn shape_filing(
        &self,
        snap: &Snapshot,
        mut plan: WorkPlan,
        provenance: &Provenance,
        source: FilingSource,
    ) -> WorkPlan {
        if source == FilingSource::WorkFile {
            for draft in plan.items.iter_mut().filter(|d| d.plan) {
                if draft.budget.is_none() {
                    draft.budget = Some(self.plan_envelope());
                }
            }
            return plan;
        }
        if source != FilingSource::Planner {
            return plan;
        }
        let Some(filer) = provenance
            .filed_by_item
            .as_deref()
            .and_then(|id| snap.item(id))
            .filter(|i| is_planning(i))
        else {
            return plan;
        };
        let Some(request) = filer.parent.clone() else {
            return plan;
        };
        let tmps: HashSet<String> = plan.items.iter().filter_map(|d| d.tmp.clone()).collect();
        let root_tmp = match &plan.root {
            ItemRef::Tmp { tmp } => Some(tmp.clone()),
            ItemRef::Id(_) => None,
        };
        for draft in plan.items.iter_mut() {
            let is_root = root_tmp.is_some() && draft.tmp == root_tmp;
            if is_root && draft.parent.is_none() {
                draft.parent = Some(ItemRef::Id(request.clone()));
            }
            // The top of the new graph: an item whose parent is not another
            // new item of this call.
            let top = match &draft.parent {
                Some(ItemRef::Tmp { tmp }) => !tmps.contains(tmp),
                Some(ItemRef::Id(_)) => true,
                None => root_tmp.is_none(),
            };
            let gate = DraftEdge {
                kind: EdgeKind::Blocks,
                depends_on: ItemRef::Id(filer.id.clone()),
            };
            if top && !draft.edges.contains(&gate) {
                draft.edges.push(gate);
            }
        }
        plan
    }

    /// A planned request (6.1): under the new parent, one planning item and
    /// the request as one item, which runs only if planning fails. Returns
    /// the ids to settle.
    pub(super) fn plan_request(
        &self,
        b: &mut Batch,
        accepted: &Accepted,
        provenance: &Provenance,
    ) -> Result<Vec<WorkItemId>, Rejection> {
        let Some(request) = b.snap.item(&accepted.root).cloned() else {
            return Ok(Vec::new());
        };
        let caps = self.config.caps;
        let planning = WorkItemDraft {
            tmp: Some("planning".to_string()),
            title: format!("Plan: {}", clip(&request.title, 110)),
            objective: format!(
                "Build the work graph for {} and file it with one work_plan call. The \
                 request: {}",
                short(&request.id),
                request.objective
            ),
            done_when: format!(
                "One work_plan call under {} is accepted.",
                short(&request.id)
            ),
            constraints: [
                format!(
                    "File one graph with work_plan: its root is {} or one new item with no \
                     parent, which goes under it.",
                    request.id
                ),
                format!(
                    "At most {} items and {} levels; budgets sum within the root's.",
                    caps.max_items, caps.max_depth
                ),
                "Break large work into small, verifiable execution slices that fit one run. \
                 A context, token or turn budget is a reason to split even when the same \
                 worker could do every step. Give each slice a precise done_when and \
                 inspection pointers; order dependent slices with blocks and inputs_from, \
                 leave independent work parallel, and keep shared writable resources ordered."
                    .to_string(),
            ]
            .into_iter()
            .chain(request.constraints.iter().cloned())
            .collect(),
            decisions_made: request.decisions_made.clone(),
            artifact_refs: std::iter::once(ArtifactRef {
                kind: "item".to_string(),
                value: request.id.clone(),
            })
            .chain(request.artifact_refs.iter().cloned())
            .collect(),
            required_tools: vec![PLAN_TOOL.to_string()],
            budget: Some(self.config.plan_budget),
            ..WorkItemDraft::default()
        };
        let whole = WorkItemDraft {
            tmp: Some("whole".to_string()),
            title: request.title.clone(),
            objective: request.objective.clone(),
            done_when: request.done_when.clone(),
            constraints: request.constraints.clone(),
            decisions_made: request.decisions_made.clone(),
            artifact_refs: request.artifact_refs.clone(),
            required_tools: request.required_tools.clone(),
            required_mcp_servers: request.required_mcp_servers.clone(),
            worker_kind: request.worker_kind,
            writable_resources: request.writable_resources.clone(),
            ..WorkItemDraft::default()
        };
        let plan = WorkPlan {
            root: ItemRef::Id(request.id.clone()),
            items: vec![planning, whole],
            edges: vec![PlanEdge {
                item: ItemRef::Tmp {
                    tmp: "whole".to_string(),
                },
                kind: EdgeKind::ConditionalOnFailure,
                depends_on: ItemRef::Tmp {
                    tmp: "planning".to_string(),
                },
            }],
            rationale: "a planned request: one planning item, and the request as one item if \
                        planning fails (section 6.1)"
                .to_string(),
        };
        let system = Provenance {
            conversation_id: provenance.conversation_id.clone(),
            filed_by_item: None,
            actor: "controller".to_string(),
        };
        let accepted = self.file_into(b, &plan, &system, FilingSource::Ladder)?;
        Ok(accepted.changed())
    }

    /// The planner's accepted graph ends its run: the planning item is
    /// reconciled with a report naming the graph, and only then does the
    /// gate onto it let the graph's items become ready (6.1).
    pub(super) fn planned_by_run(&self, filer: &str, accepted: &PlanAccepted) {
        let worker = {
            let state = self.state();
            match state.runs.get(filer) {
                Some(run) => run.worker.clone(),
                None => return,
            }
        };
        let mut summary = format!(
            "Filed the plan: {} items under {}",
            accepted.ids.len(),
            short(&accepted.root)
        );
        if !accepted.held.is_empty() {
            summary.push_str(&format!(
                "; {} held for the user's approval",
                accepted.held.len()
            ));
        }
        self.end_run_with(
            filer,
            &worker,
            ResultReport {
                summary,
                artifacts: vec![ArtifactRef {
                    kind: "item".to_string(),
                    value: accepted.root.clone(),
                }],
                ..ResultReport::default()
            },
        );
    }

    /// The parent's re-plan (6.4, order 1 of its ladder): a planning item
    /// under `parent`, briefed with `why` and pointers to what failed and
    /// what is held, which may supersede held and queued items. Records the
    /// `replan` rung on the parent. Returns the planning item's id.
    pub(super) async fn file_replan(
        &self,
        b: &mut Batch,
        parent: &WorkItem,
        origin: Option<&WorkItem>,
        why: String,
        error: Option<&WorkError>,
    ) -> Result<WorkItemId, String> {
        let held: Vec<WorkItemId> = b
            .snap
            .descendants(&parent.id)
            .into_iter()
            .filter(|d| {
                b.status(d).is_some_and(|s| {
                    matches!(
                        s,
                        Status::Blocked(r) if r.is_cascade()
                    ) || s == Status::Queued
                })
            })
            .collect();
        let mut constraints = vec![
            format!(
                "File one graph with work_plan under {}: new items, and supersedes edges onto \
                 the held or queued items they replace. Closed items stay closed; a retry is a \
                 new item.",
                parent.id
            ),
            clip(&why, LINE_MAX),
        ];
        if let Some(o) = origin {
            let ladder = self
                .ladder_of(o)
                .await
                .map(|l| summary(&l))
                .unwrap_or_default();
            if !ladder.is_empty() {
                constraints.push(clip(&format!("{} tried: {ladder}", short(&o.id)), LINE_MAX));
            }
        }
        if let Some(e) = error {
            constraints.push(format!(
                "Error class: {}/{}.",
                e.class.as_str(),
                e.subclass.as_str()
            ));
        }
        if !held.is_empty() {
            constraints.push(clip(
                &format!(
                    "Held or waiting: {}",
                    held.iter()
                        .map(|h| h.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                LINE_MAX,
            ));
        }
        let mut refs = vec![ArtifactRef {
            kind: "item".to_string(),
            value: parent.id.clone(),
        }];
        if let Some(o) = origin {
            refs.push(ArtifactRef {
                kind: "item".to_string(),
                value: o.id.clone(),
            });
        }
        let replan = self
            .file_planning_item(
                b,
                parent,
                format!("Re-plan: {}", clip(&parent.title, 100)),
                format!(
                    "Re-plan the rest of {} so it can still meet its done_when.",
                    short(&parent.id)
                ),
                constraints,
                refs,
            )
            .map_err(|r| describe_rejection(&r))?;
        let mut state = self.ladder_of(parent).await.map_err(|e| e.to_string())?;
        b.rung(
            &parent.id,
            &mut state,
            RungEvent {
                rung: Rung::Replan,
                at: b.now,
                error: error.cloned(),
                outcome: format!("filed re-plan item {replan}"),
            },
        );
        Ok(replan)
    }

    /// A planning item under `parent`, filed by the controller.
    fn file_planning_item(
        &self,
        b: &mut Batch,
        parent: &WorkItem,
        title: String,
        objective: String,
        constraints: Vec<String>,
        artifact_refs: Vec<ArtifactRef>,
    ) -> Result<WorkItemId, Rejection> {
        // Re-planning shares the parent's remaining envelope with earlier runs.
        // A small planning run is preferable to exceeding that envelope.
        let available = self
            .remaining_budgets(&b.snap)
            .get(&parent.id)
            .copied()
            .unwrap_or(parent.budget);
        let mut budget = self.config.plan_budget;
        budget.iterations = budget.iterations.min(available.iterations);
        budget.tokens = budget.tokens.min(available.tokens);
        budget.wall_seconds = budget.wall_seconds.min(available.wall_seconds);
        if budget.iterations == 0 || budget.tokens == 0 || budget.wall_seconds == 0 {
            return Err(Rejection {
                failed: vec![crate::graph::Failure {
                    check: crate::graph::Check::Reason(
                        rustykrab_core::work::RejectionReason::OverBudget,
                    ),
                    offending: vec![ItemRef::Id(parent.id.clone())],
                    detail: "the parent has no budget left for a planning run".into(),
                }],
            });
        }
        let tmp = "replan".to_string();
        let plan = WorkPlan {
            root: ItemRef::Id(parent.id.clone()),
            items: vec![WorkItemDraft {
                tmp: Some(tmp.clone()),
                title,
                objective,
                done_when: format!(
                    "One work_plan call under {} is accepted.",
                    short(&parent.id)
                ),
                constraints,
                artifact_refs,
                required_tools: vec![PLAN_TOOL.to_string()],
                budget: Some(budget),
                ..WorkItemDraft::default()
            }],
            edges: Vec::new(),
            rationale: "the parent's re-plan (section 6.4)".to_string(),
        };
        let provenance = Provenance {
            conversation_id: parent.origin_conversation_id.clone(),
            filed_by_item: None,
            actor: "controller".to_string(),
        };
        let accepted = self.file_into(b, &plan, &provenance, FilingSource::Ladder)?;
        Ok(accepted
            .ids
            .get(&tmp)
            .cloned()
            .unwrap_or_else(|| accepted.root.clone()))
    }

    /// A worker's `discovered` drafts that validation rejected become a
    /// planning item under its parent, briefed with the drafts and the
    /// reasons (6.5), within the parent's re-plan budget. `None` when no
    /// re-plan was filed.
    pub(super) async fn replan_rejected_drafts(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        drafts: &[WorkItemDraft],
        rejection: &Rejection,
    ) -> Option<WorkItemId> {
        if !self.config.replan || !self.has_planner() {
            return None;
        }
        let parent = item
            .parent
            .as_deref()
            .and_then(|p| b.snap.item(p))
            .cloned()?;
        if parent.status.is_closed() || parent.status == Status::Verifying {
            return None;
        }
        let state = self.ladder_of(&parent).await.ok()?;
        if state.left(Rung::Replan) == 0 {
            return None;
        }
        let mut constraints = vec![format!(
            "A worker on {} found this follow-up work, but its drafts were rejected: {}. File \
             it as one valid graph under {}.",
            short(&item.id),
            clip(&describe_rejection(rejection), LINE_MAX),
            parent.id
        )];
        for d in drafts.iter().take(DRAFTS_MAX) {
            constraints.push(clip(
                &format!(
                    "Draft {}: {}; done when {}",
                    d.title.trim(),
                    d.objective.trim(),
                    d.done_when.trim()
                ),
                LINE_MAX,
            ));
        }
        let refs = vec![
            ArtifactRef {
                kind: "item".to_string(),
                value: parent.id.clone(),
            },
            ArtifactRef {
                kind: "item".to_string(),
                value: item.id.clone(),
            },
        ];
        let filed = self
            .file_planning_item(
                b,
                &parent,
                format!("Re-plan discovered work: {}", clip(&parent.title, 90)),
                format!(
                    "File the follow-up work a worker found on {} as one valid graph.",
                    short(&item.id)
                ),
                constraints,
                refs,
            )
            .ok()?;
        let mut state = state;
        b.rung(
            &parent.id,
            &mut state,
            RungEvent {
                rung: Rung::Replan,
                at: b.now,
                error: None,
                outcome: format!("filed re-plan item {filed} for rejected drafts"),
            },
        );
        Some(filed)
    }

    /// Step 7 for subtrees: each stalled parent climbs its own ladder, the
    /// re-plan first and then one message at the parent. Only with a
    /// planner registered: without one, a failure already surfaced at the
    /// parent when it happened. Returns the ids to settle.
    pub(super) async fn stalled_subtrees(
        &self,
        b: &mut Batch,
        now: DateTime<Utc>,
        waiting: &[QuestionRow],
    ) -> Result<Vec<WorkItemId>, Error> {
        if !self.config.replan || !self.has_planner() {
            return Ok(Vec::new());
        }
        let asked: HashSet<WorkItemId> = waiting
            .iter()
            .filter(|q| q.class != QuestionClass::BlockingLater)
            .map(|q| q.item.clone())
            .collect();
        let mut changed = Vec::new();
        for parent in stalled_parents(&b.snap, now, &asked) {
            let Some(row) = b.snap.item(&parent).cloned() else {
                continue;
            };
            let state = self.ladder_of(&row).await?;
            let origin = b
                .snap
                .descendants(&parent)
                .into_iter()
                .filter_map(|d| b.snap.item(&d).cloned())
                .find_map(|d| d.status_origin.clone())
                .and_then(|o| b.snap.item(&o).cloned());
            // The origin by id only: the parent's message already names it
            // where it failed, and says it once.
            let why = format!(
                "The subtree under {} stalled: nothing is running or ready beneath it{}.",
                short(&parent),
                origin
                    .as_ref()
                    .map(|o| format!(", and what waits is held behind {}", short(&o.id)))
                    .unwrap_or_default()
            );
            if state.left(Rung::Replan) > 0 {
                match self
                    .file_replan(b, &row, origin.as_ref(), why.clone(), None)
                    .await
                {
                    Ok(id) => {
                        changed.push(id);
                        continue;
                    }
                    Err(e) => tracing::warn!(parent = %parent, error = %e, "re-plan not filed"),
                }
            }
            let surfaced = state
                .history
                .iter()
                .any(|e| e.rung == Rung::Surface && e.outcome.starts_with("stalled"));
            if surfaced {
                continue;
            }
            let mut state = state;
            b.rung(
                &parent,
                &mut state,
                RungEvent {
                    rung: Rung::Surface,
                    at: b.now,
                    error: None,
                    outcome: format!("stalled: {why}"),
                },
            );
            b.notify(
                &parent,
                Cause::Asked {
                    item: parent.clone(),
                    text: format!("{why} How should it go on, or should it stop?"),
                },
            );
        }
        Ok(changed)
    }
}

/// Parents whose subtree is stalled (plan section 6, step 7): open, not
/// held for approval, their own gate open, with open descendants of which
/// none is ready, leased, running or verifying, none waits on the user (an
/// open question), none waits on a trigger that has not fired, and none
/// waits on an open item outside the subtree (a capability item, another
/// tree), and at least one is held or blocked for a reason no one else will
/// clear. Pure.
pub fn stalled_parents(
    snap: &Snapshot,
    now: DateTime<Utc>,
    asked: &HashSet<WorkItemId>,
) -> Vec<WorkItemId> {
    let mut out = Vec::new();
    for p in snap.items() {
        if p.status.is_closed() || p.held_by.is_some() || !snap.has_children(&p.id) {
            continue;
        }
        if !snap.trigger_fired(&p.id, now) || p.status == Status::Verifying {
            continue;
        }
        // A parent under another parent stalls with its ancestor.
        if p.parent
            .as_deref()
            .is_some_and(|a| snap.status(a).is_some_and(|s| !s.is_closed()))
        {
            continue;
        }
        let subtree: HashSet<WorkItemId> = snap.descendants(&p.id).into_iter().collect();
        let open: Vec<&WorkItem> = subtree
            .iter()
            .filter_map(|d| snap.item(d))
            .filter(|d| !d.status.is_closed() && !snap.has_children(&d.id))
            .collect();
        if open.is_empty() {
            continue;
        }
        // Moving: running, or ready, or about to be (an item the settle
        // would make ready, whose edges and triggers already allow it).
        let moving = open.iter().any(|d| {
            matches!(
                d.status,
                Status::Ready | Status::Leased | Status::Running | Status::Verifying
            ) || (d.status == Status::Queued && crate::graph::is_ready(snap, &d.id, now))
        });
        if moving {
            continue;
        }
        let waits_on_the_world = open.iter().any(|d| {
            asked.contains(&d.id)
                || d.held_by.is_some()
                || matches!(d.status, Status::Blocked(r) if r.needs_user())
                || match &d.trigger {
                    Trigger::Now => false,
                    Trigger::At(t) => *t > now,
                    _ => !snap.trigger_fired(&d.id, now),
                }
                || snap.edges_held_by(&d.id).any(|e| {
                    e.kind.is_ordering()
                        && !subtree.contains(&e.depends_on)
                        && snap.status(&e.depends_on).is_some_and(|s| !s.is_closed())
                })
        });
        if waits_on_the_world {
            continue;
        }
        let stuck = open.iter().any(|d| {
            matches!(
                d.status,
                Status::Blocked(
                    BlockedReason::UpstreamFailed
                        | BlockedReason::UpstreamExpired
                        | BlockedReason::VerificationFailed
                        | BlockedReason::BudgetExhausted
                        | BlockedReason::PreconditionFailed
                )
            )
        });
        if stuck {
            out.push(p.id.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::work::{Edge, WorkKind, WorkerKind};

    fn item(id: &str, parent: Option<&str>, status: Status) -> WorkItem {
        let now = Utc::now();
        WorkItem {
            id: id.into(),
            kind: WorkKind::Personal,
            title: id.into(),
            objective: "o".into(),
            done_when: "d".into(),
            constraints: vec![],
            decisions_made: vec![],
            artifact_refs: vec![],
            required_tools: vec![],
            required_mcp_servers: vec![],
            worker_kind: WorkerKind::Any,
            writable_resources: vec![],
            parent: parent.map(str::to_string),
            inputs_from: vec![],
            origin_conversation_id: None,
            trigger: Trigger::Now,
            preconditions: vec![],
            expires_at: None,
            budget: Budget::default(),
            priority: 0,
            status,
            status_origin: None,
            plan_id: None,
            held_by: None,
            created_at: now,
            updated_at: now,
            closed_at: None,
        }
    }

    fn edge(item: &str, depends_on: &str) -> Edge {
        Edge {
            item: item.into(),
            depends_on: depends_on.into(),
            kind: EdgeKind::Blocks,
        }
    }

    #[test]
    fn a_chain_held_behind_a_failure_with_nothing_moving_is_stalled() {
        let mut c = item(
            "c",
            Some("p"),
            Status::Blocked(BlockedReason::UpstreamFailed),
        );
        c.status_origin = Some("b".into());
        let snap = Snapshot::new(
            vec![
                item("p", None, Status::Blocked(BlockedReason::UpstreamFailed)),
                item("a", Some("p"), Status::Done),
                item("b", Some("p"), Status::Failed),
                c,
            ],
            vec![edge("b", "a"), edge("c", "b")],
        );
        assert_eq!(
            stalled_parents(&snap, Utc::now(), &HashSet::new()),
            vec!["p".to_string()]
        );
    }

    #[test]
    fn a_sibling_the_settle_would_make_ready_is_not_a_stall() {
        // a expired: b, blocked by it, is held; c, waiting for it, is still
        // queued in the sweep that applied the expiry but may run.
        let mut b = item(
            "b",
            Some("p"),
            Status::Blocked(BlockedReason::UpstreamExpired),
        );
        b.status_origin = Some("a".into());
        let snap = Snapshot::new(
            vec![
                item("p", None, Status::Running),
                item("a", Some("p"), Status::Expired),
                b,
                item("c", Some("p"), Status::Queued),
            ],
            vec![
                edge("b", "a"),
                Edge {
                    item: "c".into(),
                    depends_on: "a".into(),
                    kind: EdgeKind::WaitsFor,
                },
            ],
        );
        assert!(stalled_parents(&snap, Utc::now(), &HashSet::new()).is_empty());
    }

    #[test]
    fn anything_moving_or_waiting_on_the_world_is_not_a_stall() {
        let now = Utc::now();
        let held = || {
            let mut c = item(
                "c",
                Some("p"),
                Status::Blocked(BlockedReason::UpstreamFailed),
            );
            c.status_origin = Some("b".into());
            c
        };
        // A sibling still ready.
        let snap = Snapshot::new(
            vec![
                item("p", None, Status::Running),
                item("b", Some("p"), Status::Failed),
                held(),
                item("r", Some("p"), Status::Ready),
            ],
            vec![edge("c", "b")],
        );
        assert!(stalled_parents(&snap, now, &HashSet::new()).is_empty());
        // A sibling waiting on the user.
        let snap = Snapshot::new(
            vec![
                item("p", None, Status::Blocked(BlockedReason::NeedsDecision)),
                item("b", Some("p"), Status::Failed),
                held(),
                item(
                    "q",
                    Some("p"),
                    Status::Blocked(BlockedReason::NeedsDecision),
                ),
            ],
            vec![edge("c", "b")],
        );
        assert!(stalled_parents(&snap, now, &HashSet::new()).is_empty());
        // A sibling waiting on a future time.
        let mut later = item("t", Some("p"), Status::Queued);
        later.trigger = Trigger::At(now + chrono::TimeDelta::hours(1));
        let snap = Snapshot::new(
            vec![
                item("p", None, Status::Queued),
                item("b", Some("p"), Status::Failed),
                held(),
                later,
            ],
            vec![edge("c", "b")],
        );
        assert!(stalled_parents(&snap, now, &HashSet::new()).is_empty());
        // Waiting on an open item outside the subtree (a capability).
        let snap = Snapshot::new(
            vec![
                item("p", None, Status::Blocked(BlockedReason::NeedsTool)),
                item("x", Some("p"), Status::Blocked(BlockedReason::NeedsTool)),
                item("cap", None, Status::Running),
            ],
            vec![edge("x", "cap")],
        );
        assert!(stalled_parents(&snap, now, &HashSet::new()).is_empty());
    }

    #[test]
    fn planner_role_requires_explicit_reservation_and_the_plan_tool() {
        use crate::worker::{Brief, WorkerCapabilities};
        struct W(Vec<&'static str>, bool);
        #[async_trait::async_trait]
        impl Worker for W {
            fn name(&self) -> &str {
                "w"
            }
            fn kind(&self) -> WorkerKind {
                WorkerKind::Local
            }
            fn planning_only(&self) -> bool {
                self.1
            }
            fn capabilities(&self) -> WorkerCapabilities {
                WorkerCapabilities {
                    tools: self.0.iter().map(|t| t.to_string()).collect(),
                    ..WorkerCapabilities::default()
                }
            }
            async fn run(&self, _: Brief) -> Result<ResultReport, Error> {
                Ok(ResultReport::default())
            }
        }
        assert!(plans(&W(vec!["work_plan", "work_status"], true)));
        assert!(!plans(&W(vec!["work_plan", "work_status"], false)));
        assert!(!plans(&W(vec!["*"], true)));
        assert!(!plans(&W(vec!["work_status"], true)));
    }
}
