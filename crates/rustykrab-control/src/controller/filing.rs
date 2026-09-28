//! Every filing path (plan sections 4.4, 6.1, 6.5, 8 and 14.1) and the
//! user's answers to a plan (approve, reject) and cancel, plus the read
//! paths the tools and the gateway use.
//!
//! Every filing goes through [`crate::graph::validate`] against the
//! snapshot as it stands inside the transaction that writes it: a caller's
//! `work_plan` or `work_file`, a worker's `discovered` drafts, the ladder's
//! `capability` and `internal` items, and the resume check's `internal`
//! item. Accepted whole or rejected whole; a rejection writes only a
//! `rejection` event on the filing item.

use std::collections::{BTreeSet, HashSet};

use chrono::{DateTime, Utc};
use rustykrab_core::work::{
    ArtifactRef, BlockedReason, DraftEdge, EdgeKind, EventKind, ItemRef, PlanEdge, PlanOutcome,
    Rung, Status, WorkError, WorkEvent, WorkItem, WorkItemDraft, WorkItemId, WorkKind, WorkPlan,
    WorkerKind,
};
use rustykrab_core::Error;
use rustykrab_store::{ArchivedItem, WorkOp, WorkPlanRow};
use rustykrab_tools::work_backend::{
    Principal, Provenance, StatusQuery, StatusSelector, WorkStatusView, DEFAULT_ACTOR,
};

use crate::errors::internal_item_draft;
use crate::graph::{
    self, validate, Accepted, ApprovalPolicy, ApprovalTrigger, Check, FilingContext, FilingSource,
    Rejection, Snapshot, Validation,
};
use crate::handle::{GraphNode, GraphView};
use crate::ladder::CapabilityNeed;

use super::batch::Batch;
use super::load::approval_marker;
use super::notice::{short, Cause};
use super::{Controller, Finished};

/// One line per failed check, for events and ladder outcomes.
pub(super) fn describe_rejection(r: &Rejection) -> String {
    r.failed
        .iter()
        .map(|f| {
            let reason = match f.check {
                Check::Reason(reason) => reason.as_str(),
                Check::SequentialSplit => "sequential_split",
            };
            format!("{reason}: {}", f.detail)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn describe_trigger(t: &ApprovalTrigger) -> String {
    match t {
        ApprovalTrigger::ItemCount { items, threshold } => {
            format!("{items} items, over the {threshold} the policy allows")
        }
        ApprovalTrigger::Budget { tokens, threshold } => {
            format!("{tokens} tokens, over the {threshold} the policy allows")
        }
        ApprovalTrigger::UndelegatedResource { item, resource } => {
            format!("{} writes {resource}, which is not delegated", short(item))
        }
        ApprovalTrigger::CodeOutsideSlice { item } => {
            format!("{} is code outside an authorised slice", short(item))
        }
        ApprovalTrigger::Discovered { items } => {
            format!(
                "{items} follow-up items a worker discovered, which the policy holds for a person"
            )
        }
    }
}

fn actor_of(provenance: &Provenance) -> String {
    if provenance.actor.is_empty() {
        DEFAULT_ACTOR.to_string()
    } else {
        provenance.actor.clone()
    }
}

fn plan_row(
    accepted: &Accepted,
    plan: &WorkPlan,
    provenance: &Provenance,
    now: DateTime<Utc>,
) -> WorkPlanRow {
    WorkPlanRow {
        id: accepted.plan_id.clone(),
        root: accepted.root.clone(),
        filed_by: provenance.filed_by_item.clone(),
        rationale: plan.rationale.clone(),
        approval_question: accepted.question.clone(),
        policy: accepted.policy.clone(),
        created_at: now,
    }
}

/// The blocked state an item parks in while a capability item it needs is
/// worked on (section 8, orders 2a to 2c).
pub(super) fn parked_reason(gap: crate::errors::GapKind) -> BlockedReason {
    use crate::errors::GapKind;
    match gap {
        GapKind::Tool | GapKind::Install => BlockedReason::NeedsTool,
        GapKind::Credential => BlockedReason::NeedsCredential,
        GapKind::Consent => BlockedReason::NeedsConsent,
        GapKind::Compute | GapKind::Capacity => BlockedReason::WorkerUnavailable,
        GapKind::Knowledge => BlockedReason::PreconditionFailed,
    }
}

/// The blocked reasons an item parks in behind a capability item, which
/// the sweep clears once every capability it waits on is `done`.
pub(super) const CAPABILITY_PARKED: [BlockedReason; 5] = [
    BlockedReason::NeedsTool,
    BlockedReason::NeedsCredential,
    BlockedReason::NeedsConsent,
    BlockedReason::WorkerUnavailable,
    BlockedReason::PreconditionFailed,
];

/// Items accepted as `blocked(needs_tool)` for an unconfigured MCP server
/// (section 7) whose servers are all configured now. An item parked behind
/// a capability item waits for that item instead.
pub(super) fn waiting_on_mcp(
    snap: &Snapshot,
    configured: impl Fn(&str) -> bool,
) -> Vec<WorkItemId> {
    snap.items()
        .iter()
        .filter(|i| {
            i.held_by.is_none()
                && i.status == Status::Blocked(BlockedReason::NeedsTool)
                && !i.required_mcp_servers.is_empty()
                && i.required_mcp_servers.iter().all(|s| configured(s))
        })
        .filter(|i| {
            !snap.edges_held_by(&i.id).any(|e| {
                e.kind == EdgeKind::Blocks
                    && snap
                        .item(&e.depends_on)
                        .is_some_and(|u| u.kind == WorkKind::Capability)
            })
        })
        .map(|i| i.id.clone())
        .collect()
}

/// Items parked on capability items that have all landed.
pub(super) fn parked_on_landed_capability(snap: &Snapshot) -> Vec<WorkItemId> {
    snap.items()
        .iter()
        .filter(|i| {
            i.held_by.is_none()
                && matches!(i.status, Status::Blocked(r) if CAPABILITY_PARKED.contains(&r))
        })
        .filter(|i| {
            let caps: Vec<&str> = snap
                .edges_held_by(&i.id)
                .filter(|e| e.kind == EdgeKind::Blocks)
                .filter(|e| {
                    snap.item(&e.depends_on)
                        .is_some_and(|u| u.kind == WorkKind::Capability)
                })
                .map(|e| e.depends_on.as_str())
                .collect();
            !caps.is_empty() && caps.iter().all(|c| snap.status(c) == Some(Status::Done))
        })
        .map(|i| i.id.clone())
        .collect()
}

/// Capability items nothing needs any more (plan 6.2 and section 8): not
/// leased or running, held by no approval, and every item that waited on
/// them through an ordering edge has closed. Each comes with the dependent
/// that closed last, the origin of its `cancelled(cascade)`. A capability
/// item nothing ever waited on (one filed on its own) is left alone.
pub(super) fn unneeded_capabilities(snap: &Snapshot) -> Vec<(WorkItemId, WorkItemId)> {
    snap.items()
        .iter()
        .filter(|i| i.kind == WorkKind::Capability && i.status.is_waiting() && i.held_by.is_none())
        .filter_map(|i| {
            let dependents: Vec<&WorkItem> = snap
                .edges_naming(&i.id)
                .filter(|e| e.kind.is_ordering())
                .filter_map(|e| snap.item(&e.item))
                .collect();
            if dependents.is_empty() || dependents.iter().any(|d| !d.status.is_closed()) {
                return None;
            }
            let last = dependents
                .iter()
                .max_by_key(|d| (d.closed_at.unwrap_or(d.updated_at), d.id.clone()))?;
            Some((i.id.clone(), last.id.clone()))
        })
        .collect()
}

impl Controller {
    /// The filing context for `source` from the configuration and the
    /// caller's provenance: a worker's scope is its own parent's subtree
    /// and its items carry `discovered_from` on its item (6.5).
    pub(super) fn filing_context(
        &self,
        source: FilingSource,
        provenance: &Provenance,
        snap: &Snapshot,
        plan: &WorkPlan,
        now: DateTime<Utc>,
    ) -> FilingContext {
        let mut ctx = FilingContext::new(source, now);
        ctx.caps = self.config.caps;
        ctx.sequential_split = self.config.split_mode;
        ctx.default_budget = self.config.default_budget;
        ctx.supersede_limit = self.config.supersede_limit;
        ctx.remaining_budget = self.remaining_budgets(snap);
        ctx.origin_conversation_id = provenance.conversation_id.clone();
        // The ladder's own filings are system work under policy, not a plan
        // the user approves (section 8); an accepted proposal's work was
        // approved on the review surface (section 10).
        ctx.approval = if matches!(source, FilingSource::Ladder | FilingSource::Proposal) {
            ApprovalPolicy::default()
        } else {
            self.config.approval.clone()
        };
        let caller = provenance
            .filed_by_item
            .as_deref()
            .and_then(|id| snap.item(id));
        match (source, caller) {
            (FilingSource::Discovered | FilingSource::WorkFile, Some(c)) => {
                ctx.discovered_from = Some(c.id.clone());
                ctx.scope = c.parent.clone();
            }
            (FilingSource::Planner, Some(c)) => {
                ctx.scope = c.parent.clone();
                ctx.already_planned = self.state().planned.contains(&c.id);
            }
            _ => {}
        }
        if let Some(root) = scope_root(&ctx, plan) {
            let since = now - self.config.supersede_window;
            let count = self
                .state()
                .supersedes
                .iter()
                .filter(|(r, at)| *r == root && *at > since)
                .count();
            ctx.supersedes_in_window = u32::try_from(count).unwrap_or(u32::MAX);
        }
        ctx
    }

    /// Validate `plan` against the batch's working snapshot and, when
    /// accepted, add it to the batch.
    pub(super) fn file_into(
        &self,
        b: &mut Batch,
        plan: &WorkPlan,
        provenance: &Provenance,
        source: FilingSource,
    ) -> Result<Box<Accepted>, Rejection> {
        let ctx = self.filing_context(source, provenance, &b.snap, plan, b.now);
        match validate(&b.snap, plan, &ctx) {
            Validation::Accepted(accepted) => {
                let row = plan_row(&accepted, plan, provenance, b.now);
                b.file(&accepted, row, &actor_of(provenance));
                // Phase 6 (sections 10 and 11): the review facets land with
                // the rows, and a new proposal waits for its review.
                self.record_facets(b, plan, &accepted);
                self.hold_for_review(b, &accepted);
                if accepted.supersedes {
                    if let Some(root) = scope_root(&ctx, plan) {
                        b.superseded_under.push(root);
                    }
                }
                if source == FilingSource::Planner {
                    if let Some(filer) = &provenance.filed_by_item {
                        b.planned.push(filer.clone());
                    }
                }
                self.hold_for_mcp(b, &accepted);
                if !accepted.held.is_empty() {
                    b.notify(
                        &accepted.root,
                        Cause::Approval {
                            held: accepted.held.clone(),
                            triggers: accepted.triggers.iter().map(describe_trigger).collect(),
                        },
                    );
                }
                Ok(accepted)
            }
            Validation::Rejected(rejection) => Err(rejection),
        }
    }

    /// Section 7: a new item naming an MCP server this host has not
    /// configured is accepted as `blocked(needs_tool)`, and the sweep
    /// releases it once the server is configured ([`waiting_on_mcp`]).
    fn hold_for_mcp(&self, b: &mut Batch, accepted: &Accepted) {
        for item in &accepted.items {
            let missing: Vec<&str> = item
                .required_mcp_servers
                .iter()
                .filter(|s| !self.catalog.mcp_server_configured(s))
                .map(String::as_str)
                .collect();
            if missing.is_empty() || b.status(&item.id) != Some(Status::Queued) {
                continue;
            }
            b.move_to(
                &item.id,
                Status::Blocked(BlockedReason::NeedsTool),
                "controller",
                format!(
                    "waiting for MCP server {}: not configured on this host",
                    missing.join(", ")
                ),
            );
        }
    }

    pub(super) async fn file_plan_locked(
        &self,
        plan: WorkPlan,
        provenance: Provenance,
        source: FilingSource,
    ) -> Result<PlanOutcome, Error> {
        let now = self.clock.now();
        let mut b = Batch::new(self.load().await?, now);
        match self.file_into(&mut b, &plan, &provenance, source) {
            Ok(accepted) => {
                let outcome = PlanOutcome::Accepted(accepted.to_core());
                b.settle(accepted.changed());
                self.commit(b, &mut HashSet::new()).await?;
                Ok(outcome)
            }
            Err(rejection) => {
                // The rejection is an event dreaming can count (6.1): on the
                // item that filed, else on the existing root the filing was
                // under (a REST or CLI caller has no item of its own).
                let root = match &plan.root {
                    ItemRef::Id(id) => Some(id.as_str()),
                    ItemRef::Tmp { .. } => None,
                };
                let filer = provenance
                    .filed_by_item
                    .as_deref()
                    .or(root)
                    .filter(|id| b.snap.contains(id));
                if let Some(filer) = filer {
                    self.store
                        .work_event_append(&WorkEvent {
                            item: filer.to_string(),
                            at: Utc::now(),
                            kind: EventKind::Rejection,
                            from: None,
                            to: None,
                            actor: actor_of(&provenance),
                            reason: Some(describe_rejection(&rejection)),
                            upstream: None,
                            origin: None,
                            evidence_ref: None,
                        })
                        .await?;
                }
                Ok(PlanOutcome::Rejected(rejection.to_core()))
            }
        }
    }

    /// `work_file`: one draft as a one-item plan (14.1). A worker's draft
    /// files under its own item's parent, since its leased leaf cannot take
    /// children, with `discovered_from` on its item; any other draft is its
    /// own root.
    pub(super) async fn wrap_draft(
        &self,
        mut draft: WorkItemDraft,
        provenance: &Provenance,
    ) -> Result<WorkPlan, Error> {
        let tmp = draft.tmp.get_or_insert_with(|| "draft".to_string()).clone();
        let parent = match &provenance.filed_by_item {
            Some(item) => self.store.work_get(item).await?.and_then(|i| i.parent),
            None => None,
        };
        let root = match (parent, &draft.parent) {
            (Some(p), None) => ItemRef::Id(p),
            _ => ItemRef::Tmp { tmp },
        };
        Ok(WorkPlan {
            root,
            items: vec![draft],
            edges: Vec::new(),
            rationale: String::new(),
        })
    }

    /// A worker's `discovered` drafts (section 5 and 6.5): under the item's
    /// parent they file as one graph, scoped to that subtree; an item with
    /// no parent files each draft as its own root. Each draft inherits the
    /// item's `repo:` resources and worker constraint unless it sets its own
    /// ([`inherit_from`]), drafts sharing a repository are ordered among
    /// themselves ([`order_repo_writers`]), and every draft runs after the
    /// open writers of its repositories under the parent
    /// ([`order_after_open_writers`]), whether it inherited the repository or
    /// named it. A rejection is recorded on the item. Returns the ids to
    /// settle.
    pub(super) fn file_discovered(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        worker: &str,
        drafts: &[WorkItemDraft],
    ) -> Vec<WorkItemId> {
        if drafts.is_empty() {
            return Vec::new();
        }
        let provenance = Provenance {
            conversation_id: item.origin_conversation_id.clone(),
            filed_by_item: Some(item.id.clone()),
            actor: format!("worker:{worker}"),
        };
        let rationale = format!("discovered while working on {}", short(&item.id));
        let mut drafts: Vec<WorkItemDraft> = drafts.iter().map(|d| inherit_from(item, d)).collect();
        for (i, d) in drafts.iter_mut().enumerate() {
            d.tmp.get_or_insert_with(|| format!("d{i}"));
        }
        let plans: Vec<WorkPlan> = match &item.parent {
            Some(parent) => {
                order_repo_writers(&mut drafts);
                order_after_open_writers(&b.snap, parent, &mut drafts);
                vec![WorkPlan {
                    root: ItemRef::Id(parent.clone()),
                    items: drafts,
                    edges: Vec::new(),
                    rationale,
                }]
            }
            None => drafts
                .into_iter()
                .map(|d| {
                    let tmp = d.tmp.clone().unwrap_or_default();
                    WorkPlan {
                        root: ItemRef::Tmp { tmp },
                        items: vec![d],
                        edges: Vec::new(),
                        rationale: rationale.clone(),
                    }
                })
                .collect(),
        };
        let mut changed = Vec::new();
        for plan in plans {
            match self.file_into(b, &plan, &provenance, FilingSource::Discovered) {
                Ok(accepted) => changed.extend(accepted.changed()),
                Err(rejection) => b.note(
                    &item.id,
                    EventKind::Rejection,
                    "controller",
                    format!(
                        "discovered drafts rejected: {}",
                        describe_rejection(&rejection)
                    ),
                ),
            }
        }
        changed
    }

    /// Order 2 (section 8): a `capability` item with the need's trigger and
    /// a `blocks` edge from `item` onto it. An open capability item for the
    /// same need is reused, so one build serves every item that needs it.
    /// `item` must already be parked (waiting), so the edge is allowed.
    pub(super) fn file_capability(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        rung: Rung,
        need: &CapabilityNeed,
    ) -> Result<WorkItemId, String> {
        let verb = match rung {
            Rung::Build => "Build",
            Rung::Request => "Request",
            _ => "Acquire",
        };
        let title = format!("{verb} {}: {}", need.gap.as_str(), need.subject);
        let existing = b
            .snap
            .items()
            .iter()
            .find(|i| i.kind == WorkKind::Capability && !i.status.is_closed() && i.title == title)
            .map(|i| i.id.clone());
        let tmp = "capability".to_string();
        let upstream = match &existing {
            Some(id) => ItemRef::Id(id.clone()),
            None => ItemRef::Tmp { tmp: tmp.clone() },
        };
        let items = match existing {
            Some(_) => Vec::new(),
            None => vec![WorkItemDraft {
                tmp: Some(tmp.clone()),
                kind: Some(WorkKind::Capability),
                title: title.clone(),
                objective: format!(
                    "{verb} the {} `{}` that {} needs, so it can run again.",
                    need.gap.as_str(),
                    need.subject,
                    short(&item.id)
                ),
                done_when: format!(
                    "The {} `{}` is available to workers and {} can use it.",
                    need.gap.as_str(),
                    need.subject,
                    short(&item.id)
                ),
                artifact_refs: vec![
                    ArtifactRef {
                        kind: "item".to_string(),
                        value: item.id.clone(),
                    },
                    // The need it answers. Whether it builds, acquires or
                    // requests is its facet, below: the one source of
                    // truth the review projection, routing and the
                    // verifier all read.
                    crate::routing::CapabilityRef {
                        gap: need.gap,
                        subject: need.subject.clone(),
                    }
                    .to_ref(),
                ],
                trigger: need.trigger.clone(),
                capability: Some(match rung {
                    Rung::Build => rustykrab_core::work::CapabilityMode::Build,
                    Rung::Request => rustykrab_core::work::CapabilityMode::Request,
                    _ => rustykrab_core::work::CapabilityMode::Acquire,
                }),
                ..WorkItemDraft::default()
            }],
        };
        let plan = WorkPlan {
            root: upstream.clone(),
            items,
            edges: vec![PlanEdge {
                item: ItemRef::Id(item.id.clone()),
                kind: EdgeKind::Blocks,
                depends_on: upstream,
            }],
            rationale: format!(
                "the ladder's order {} for {}",
                rung.order(),
                short(&item.id)
            ),
        };
        let provenance = Provenance {
            conversation_id: item.origin_conversation_id.clone(),
            filed_by_item: Some(item.id.clone()),
            actor: "controller".to_string(),
        };
        let accepted = self
            .file_into(b, &plan, &provenance, FilingSource::Ladder)
            .map_err(|r| describe_rejection(&r))?;
        Ok(accepted
            .ids
            .get(&tmp)
            .cloned()
            .unwrap_or_else(|| accepted.root.clone()))
    }

    /// Order 3 (sections 8 and 9): the `internal` item for `error`, with
    /// the failing item as evidence. An open one for the same fingerprint
    /// is reused.
    pub(super) fn file_internal(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        error: &WorkError,
    ) -> Result<WorkItemId, String> {
        let evidence = vec![ArtifactRef {
            kind: "item".to_string(),
            value: item.id.clone(),
        }];
        let draft = internal_item_draft(error, evidence);
        self.file_system_item(b, draft, item.origin_conversation_id.clone())
    }

    /// A controller defect found at resume (6.7 and section 9): stored
    /// state disagreed with what its edges and children derive.
    pub(super) fn file_defect(
        &self,
        b: &mut Batch,
        found: &[String],
    ) -> Result<WorkItemId, String> {
        let draft = WorkItemDraft {
            kind: Some(WorkKind::Internal),
            title: "Controller state disagreed with its own derivation".to_string(),
            objective: format!(
                "The controller re-derived cascade holds and roll-ups from edges and statuses \
                 and found stored state that did not match, which means a transition was \
                 written without its consequences. Find the path that wrote it. Found: {}",
                found.join("; ")
            ),
            done_when: "The path that wrote a status without its cascade or roll-up is fixed \
                        and a test pins it."
                .to_string(),
            artifact_refs: found
                .iter()
                .filter_map(|f| f.split_whitespace().next())
                .map(|id| ArtifactRef {
                    kind: "item".to_string(),
                    value: id.to_string(),
                })
                .collect(),
            ..WorkItemDraft::default()
        };
        self.file_system_item(b, draft, None)
    }

    fn file_system_item(
        &self,
        b: &mut Batch,
        mut draft: WorkItemDraft,
        conversation: Option<String>,
    ) -> Result<WorkItemId, String> {
        if let Some(open) = b.snap.items().iter().find(|i| {
            i.kind == WorkKind::Internal && !i.status.is_closed() && i.title == draft.title
        }) {
            return Ok(open.id.clone());
        }
        let tmp = "internal".to_string();
        draft.tmp = Some(tmp.clone());
        let plan = WorkPlan {
            root: ItemRef::Tmp { tmp: tmp.clone() },
            items: vec![draft],
            edges: Vec::new(),
            rationale: "filed by the controller (section 9)".to_string(),
        };
        let provenance = Provenance {
            conversation_id: conversation,
            filed_by_item: None,
            actor: "controller".to_string(),
        };
        let accepted = self
            .file_into(b, &plan, &provenance, FilingSource::Ladder)
            .map_err(|r| describe_rejection(&r))?;
        Ok(accepted
            .ids
            .get(&tmp)
            .cloned()
            .unwrap_or(accepted.root.clone()))
    }

    /// The items under `root` held on an approval question that a plan row
    /// really asked, not yet answered.
    async fn held_under(&self, snap: &Snapshot, root: &str) -> Result<Vec<WorkItem>, Error> {
        let mut out = Vec::new();
        for item in snap.items() {
            let Some(question) = &item.held_by else {
                continue;
            };
            if item.status.is_closed() || !snap.is_within(&item.id, root) {
                continue;
            }
            let Some(plan) = &item.plan_id else {
                continue;
            };
            let asked = self
                .store
                .work_plan_get(plan)
                .await?
                .and_then(|p| p.approval_question);
            if asked.as_deref() == Some(question.as_str()) {
                out.push(item.clone());
            }
        }
        out.sort_by_key(|i| snap.ancestors(&i.id).len());
        Ok(out)
    }

    pub(super) async fn approve_locked(
        &self,
        root: &str,
        actor: &str,
    ) -> Result<Vec<WorkItemId>, Error> {
        let now = self.clock.now();
        let mut b = Batch::new(self.load().await?, now);
        if !b.snap.contains(root) {
            return Err(Error::NotFound(format!("work item {root}")));
        }
        let held = self.held_under(&b.snap, root).await?;
        let mut released = Vec::new();
        for item in held {
            let question = item.held_by.clone().unwrap_or_default();
            let marker = approval_marker(&question);
            if item.status == Status::Blocked(BlockedReason::NeedsConsent) {
                b.own(&item.id, Status::Queued, actor, marker, None);
            } else {
                b.reassert(&item.id, actor, marker);
            }
            if let Some(row) = b.snap.item_mut(&item.id) {
                row.held_by = None;
            }
            b.ops.push(WorkOp::ReleaseHold(item.id.clone()));
            b.approved.push(item.id.clone());
            released.push(item.id);
        }
        b.settle(released.clone());
        self.commit(b, &mut HashSet::new()).await?;
        Ok(released)
    }

    pub(super) async fn reject_locked(
        &self,
        root: &str,
        reason: Option<String>,
        actor: &str,
    ) -> Result<Vec<WorkItemId>, Error> {
        let now = self.clock.now();
        let mut b = Batch::new(self.load().await?, now);
        if !b.snap.contains(root) {
            return Err(Error::NotFound(format!("work item {root}")));
        }
        let held = self.held_under(&b.snap, root).await?;
        let reason = reason.unwrap_or_else(|| "the plan was declined".to_string());
        let mut changed = Vec::new();
        let mut cancelled = Vec::new();
        for item in held {
            if b.status(&item.id).is_none_or(|s| s.is_closed()) {
                continue;
            }
            let (touched, _) = b.cancel_tree(&item.id, actor, Some(reason.clone()));
            changed.extend(touched);
            cancelled.push(item.id);
        }
        b.settle(changed);
        self.commit(b, &mut HashSet::new()).await?;
        Ok(cancelled)
    }

    pub(super) async fn cancel_locked(
        &self,
        item: &str,
        reason: Option<String>,
        actor: &str,
    ) -> Result<Vec<WorkItemId>, Error> {
        let now = self.clock.now();
        let mut b = Batch::new(self.load().await?, now);
        if !b.snap.contains(item) {
            return Err(Error::NotFound(format!("work item {item}")));
        }
        let (changed, cancelled) = b.cancel_tree(item, actor, reason);
        b.settle(changed);
        self.commit(b, &mut HashSet::new()).await?;
        Ok(cancelled)
    }

    /// Hand a worker's result to the reconcile step (section 5), only from
    /// the run that holds the item's lease.
    pub(super) async fn accept_report(
        &self,
        item: WorkItemId,
        report: rustykrab_core::work::ResultReport,
        provenance: Provenance,
    ) -> Result<(), Error> {
        if provenance.filed_by_item.as_deref() != Some(item.as_str()) {
            return Err(Error::Auth(format!(
                "a result for {item} is accepted only from the run holding its lease"
            )));
        }
        let lease = self
            .store
            .work_lease_get(&item)
            .await?
            .ok_or_else(|| Error::Auth(format!("{item} has no live lease")))?;
        if let Some(name) = provenance.actor.strip_prefix("worker:") {
            if name != lease.worker {
                return Err(Error::Auth(format!(
                    "{item} is leased to {}, not {name}",
                    lease.worker
                )));
            }
        }
        let mut state = self.state();
        if let Some(run) = state.runs.get_mut(&item) {
            run.reported = true;
        }
        state.finished.insert(
            item,
            Finished {
                worker: lease.worker,
                outcome: Ok(report),
            },
        );
        Ok(())
    }

    /// `work_status` (14): the named items, or a root and its subtree, with
    /// edges both ways and roll-ups. Phase 1 has one principal, so every
    /// item is visible.
    pub(super) async fn status_views(
        &self,
        query: StatusQuery,
        _principal: &Principal,
    ) -> Result<Vec<WorkStatusView>, Error> {
        let now = self.clock.now();
        let snap = self.load().await?;
        let ids: Vec<WorkItemId> = match &query.select {
            StatusSelector::Ids(ids) => ids.clone(),
            StatusSelector::Root(root) => {
                let mut ids = vec![root.clone()];
                ids.extend(snap.descendants(root).into_iter().filter(|d| {
                    query.include_closed || snap.status(d).is_some_and(|s| !s.is_closed())
                }));
                ids
            }
        };
        let mut views = Vec::new();
        for id in ids {
            let item = match snap.item(&id) {
                Some(item) => item.clone(),
                None => match self.store.work_archive_get(&id).await? {
                    Some(a) => archived_item(&a),
                    None => continue,
                },
            };
            let mut edges = self.store.work_edges_of(&id).await?;
            for e in self.store.work_dependents_of(&id).await? {
                if !edges.contains(&e) {
                    edges.push(e);
                }
            }
            let kids = snap.children(&id);
            let done = kids
                .iter()
                .filter(|k| snap.status(k) == Some(Status::Done))
                .count();
            views.push(WorkStatusView {
                parent: item.parent.clone(),
                rollup: graph::rollup(&snap, &id, now).map(|r| r.status),
                children_done: u32::try_from(done).unwrap_or(u32::MAX),
                children_total: u32::try_from(kids.len()).unwrap_or(u32::MAX),
                edges,
                item,
            });
        }
        Ok(views)
    }

    /// The tree under `root`, depth first, with roll-ups; aged children
    /// appear as their archive line (4.6, 14.2).
    pub(super) async fn graph_view(&self, root: &str) -> Result<GraphView, Error> {
        let now = self.clock.now();
        let snap = self.load().await?;
        if !snap.contains(root) {
            let archived = self
                .store
                .work_archive_get(root)
                .await?
                .ok_or_else(|| Error::NotFound(format!("work item {root}")))?;
            return Ok(GraphView {
                root: root.to_string(),
                nodes: vec![archived_node(&archived, 0)],
            });
        }
        let archive = self.store.work_archive_list(None, None).await?;
        let mut nodes = Vec::new();
        let mut stack: Vec<(WorkItemId, u32)> = vec![(root.to_string(), 0)];
        let mut seen: BTreeSet<WorkItemId> = BTreeSet::new();
        while let Some((id, depth)) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let Some(item) = snap.item(&id).cloned() else {
                continue;
            };
            let live: Vec<WorkItemId> = snap.children(&id).to_vec();
            let aged: Vec<&ArchivedItem> = archive
                .iter()
                .filter(|a| a.parent.as_deref() == Some(id.as_str()))
                .collect();
            let done = live
                .iter()
                .filter(|k| snap.status(k) == Some(Status::Done))
                .count()
                + aged.iter().filter(|a| a.status == Status::Done).count();
            nodes.push(GraphNode {
                edges: self.store.work_edges_of(&id).await?,
                rollup: graph::rollup(&snap, &id, now).map(|r| r.status),
                children_done: u32::try_from(done).unwrap_or(u32::MAX),
                children_total: u32::try_from(live.len() + aged.len()).unwrap_or(u32::MAX),
                archived_summary: None,
                depth,
                item,
            });
            for a in aged {
                nodes.push(archived_node(a, depth + 1));
            }
            for kid in live.iter().rev() {
                stack.push((kid.clone(), depth + 1));
            }
        }
        Ok(GraphView {
            root: root.to_string(),
            nodes,
        })
    }
}

/// A discovered draft as filed under `filer`: one naming no `repo:`
/// resource takes the filer's, and one with no worker constraint takes the
/// filer's, so a code follow-up keeps its repository and its worker. A
/// draft that sets either keeps its own.
fn inherit_from(filer: &WorkItem, draft: &WorkItemDraft) -> WorkItemDraft {
    let mut d = draft.clone();
    if !d.writable_resources.iter().any(|r| is_repo(r)) {
        d.writable_resources.extend(
            filer
                .writable_resources
                .iter()
                .filter(|r| is_repo(r))
                .cloned(),
        );
    }
    if d.worker_kind == WorkerKind::Any {
        d.worker_kind = filer.worker_kind;
    }
    d
}

fn is_repo(resource: &str) -> bool {
    resource.starts_with(crate::workspace::REPO_PREFIX)
}

/// Drafts filed as one graph that write the same repository, inherited or
/// named themselves, run in the order the worker listed them: each gets a
/// `blocks` edge on the last earlier draft writing that repository, unless
/// one already depends on the other through the drafts' own edges (an edge
/// then would be redundant or close a cycle). Two unordered writers of one
/// resource are a `single_writer_conflict`, and a worker's list is the only
/// order it gives, so without this a second follow-up in the same
/// repository would reject the whole filing. Every draft carries a `tmp`.
fn order_repo_writers(drafts: &mut [WorkItemDraft]) {
    for i in 1..drafts.len() {
        let mut upstreams: Vec<usize> = Vec::new();
        for r in drafts[i].writable_resources.iter().filter(|r| is_repo(r)) {
            let Some(j) = (0..i)
                .rev()
                .find(|&j| drafts[j].writable_resources.contains(r))
            else {
                continue;
            };
            if !upstreams.contains(&j) {
                upstreams.push(j);
            }
        }
        for j in upstreams {
            if !depends(drafts, i, j) && !depends(drafts, j, i) {
                let depends_on = ItemRef::Tmp {
                    tmp: drafts[j].tmp.clone().unwrap_or_default(),
                };
                drafts[i].edges.push(DraftEdge {
                    kind: EdgeKind::Blocks,
                    depends_on,
                });
            }
        }
    }
}

/// Whether draft `from` reaches draft `to` over the drafts' own edges.
fn depends(drafts: &[WorkItemDraft], from: usize, to: usize) -> bool {
    let index_of = |r: &ItemRef| match r {
        ItemRef::Tmp { tmp } => drafts.iter().position(|d| d.tmp.as_ref() == Some(tmp)),
        ItemRef::Id(_) => None,
    };
    let mut seen = vec![false; drafts.len()];
    let mut stack = vec![from];
    while let Some(k) = stack.pop() {
        if k == to {
            return true;
        }
        if std::mem::replace(&mut seen[k], true) {
            continue;
        }
        stack.extend(
            drafts[k]
                .edges
                .iter()
                .filter_map(|e| index_of(&e.depends_on)),
        );
    }
    false
}

/// Discovered drafts filed under `root` run after the open items already
/// there that write one of their repositories: each gets a `blocks` edge
/// on every such open leaf, unless it already names one or files under it.
/// A discovered draft is follow-up work, so it queues behind the writers in
/// flight rather than rejecting the filing as a `single_writer_conflict`;
/// this holds for a repository the draft named as for one it inherited.
fn order_after_open_writers(snap: &Snapshot, root: &str, drafts: &mut [WorkItemDraft]) {
    let writers: Vec<&WorkItem> = snap
        .descendants(root)
        .iter()
        .filter_map(|id| snap.item(id))
        .filter(|i| !i.status.is_closed() && !snap.has_children(&i.id))
        .collect();
    for d in drafts.iter_mut() {
        for w in &writers {
            let writes = d
                .writable_resources
                .iter()
                .any(|r| is_repo(r) && w.writable_resources.contains(r));
            let upstream = ItemRef::Id(w.id.clone());
            let linked = d.parent.as_ref() == Some(&upstream)
                || d.edges.iter().any(|e| e.depends_on == upstream);
            if writes && !linked {
                d.edges.push(DraftEdge {
                    kind: EdgeKind::Blocks,
                    depends_on: upstream,
                });
            }
        }
    }
}

/// The scope a supersede counts against: the caller's subtree, else the
/// plan's existing root.
fn scope_root(ctx: &FilingContext, plan: &WorkPlan) -> Option<WorkItemId> {
    ctx.scope.clone().or(match &plan.root {
        ItemRef::Id(id) => Some(id.clone()),
        ItemRef::Tmp { .. } => None,
    })
}

/// What is left of an aged item, as a row.
fn archived_item(a: &ArchivedItem) -> WorkItem {
    WorkItem {
        id: a.id.clone(),
        kind: a.kind,
        title: a.title.clone(),
        objective: String::new(),
        done_when: String::new(),
        constraints: Vec::new(),
        decisions_made: Vec::new(),
        artifact_refs: Vec::new(),
        required_tools: Vec::new(),
        required_mcp_servers: Vec::new(),
        worker_kind: Default::default(),
        writable_resources: Vec::new(),
        parent: a.parent.clone(),
        inputs_from: Vec::new(),
        origin_conversation_id: None,
        trigger: Default::default(),
        preconditions: Vec::new(),
        expires_at: None,
        budget: Default::default(),
        priority: 0,
        status: a.status,
        status_origin: None,
        plan_id: None,
        held_by: None,
        created_at: a.closed_at,
        updated_at: a.closed_at,
        closed_at: Some(a.closed_at),
    }
}

fn archived_node(a: &ArchivedItem, depth: u32) -> GraphNode {
    GraphNode {
        item: archived_item(a),
        depth,
        edges: a.edges.clone(),
        rollup: None,
        children_done: 0,
        children_total: 0,
        archived_summary: Some(a.summary.clone()),
    }
}
