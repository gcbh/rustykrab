//! One store transaction, built over a working snapshot.
//!
//! A [`Batch`] starts from the snapshot the store holds, and every write the
//! controller decides on is both queued as a [`WorkOp`] and applied to the
//! working snapshot, so each later decision in the same transaction (a
//! cascade, a filing's validation, a roll-up) reads the state the earlier
//! ones leave. The controller then writes the queued ops with one
//! `work_apply`, together with the outbox notices they cause
//! (`Controller::commit`).

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rustykrab_core::work::{
    CancelReason, EdgeKind, EventKind, Evidence, RungEvent, Status, WorkEvent, WorkItemId, WorkKind,
};
use rustykrab_store::{RepointSpec, TransitionSpec, WorkOp, WorkPlanRow};

use crate::graph::{self, Accepted, Effects, Link, Snapshot, Transition};
use crate::ladder::LadderState;

use super::load::encode_rung;
use super::notice::{label, Cause};

/// Parent verification rounds per batch; each round closes at least one
/// parent, so the tree's depth bounds it long before this.
const SETTLE_ROUNDS: usize = 64;

pub(super) struct Batch {
    pub snap: Snapshot,
    /// The controller's clock: what triggers, expiry and the engine read.
    pub now: DateTime<Utc>,
    /// The wall clock, for the note events this batch writes, so they sort
    /// with the events the store stamps itself.
    stamp: DateTime<Utc>,
    pub ops: Vec<WorkOp>,
    /// Written just before `ops`: the store's evidence rows are not part of
    /// a `work_apply` batch.
    pub evidence: Vec<Evidence>,
    /// Runs to stop at commit: their items left an active status.
    pub revoke: Vec<WorkItemId>,
    /// Ladders changed in this batch, for the notice's "not done" lines.
    pub ladders: HashMap<WorkItemId, LadderState>,
    /// Explicit notice causes; closes and expiries are read off `ops`.
    pub causes: Vec<(WorkItemId, Cause)>,
    /// Fingerprints to count in recurrence once the batch is written.
    pub observe: Vec<String>,
    /// Items whose approval hold this batch released.
    pub approved: Vec<WorkItemId>,
    /// Roots this batch superseded under, for the rate limit.
    pub superseded_under: Vec<WorkItemId>,
    /// Planning items this batch accepted a graph for.
    pub planned: Vec<WorkItemId>,
    /// Classifier rules this batch landed, consulted once it is written.
    pub learned: Vec<crate::errors::LearnedRule>,
    /// `(item, old, new)` of edge re-points already written: the store
    /// moves the matching `inputs_from` entry with the edge, so the
    /// engine's separate input re-point is not written again.
    moved_inputs: HashSet<(WorkItemId, WorkItemId, WorkItemId)>,
}

impl Batch {
    pub fn new(snap: Snapshot, now: DateTime<Utc>) -> Batch {
        Batch {
            snap,
            now,
            stamp: Utc::now(),
            ops: Vec::new(),
            evidence: Vec::new(),
            revoke: Vec::new(),
            ladders: HashMap::new(),
            causes: Vec::new(),
            observe: Vec::new(),
            approved: Vec::new(),
            superseded_under: Vec::new(),
            planned: Vec::new(),
            learned: Vec::new(),
            moved_inputs: HashSet::new(),
        }
    }

    pub fn status(&self, id: &str) -> Option<Status> {
        self.snap.status(id)
    }

    /// The top of `id`'s tree: whose message reports it (6.6). A single
    /// item is its own root.
    pub fn root_of(&self, id: &str) -> WorkItemId {
        self.snap
            .ancestors(id)
            .last()
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }

    /// The item's own transition, refused by the store if the item moved
    /// meanwhile. `false` when the item is unknown or closed.
    pub fn own(
        &mut self,
        item: &str,
        to: Status,
        actor: &str,
        reason: impl Into<String>,
        kind: Option<EventKind>,
    ) -> bool {
        let Some(from) = self.status(item) else {
            return false;
        };
        if from.is_closed() {
            return false;
        }
        let mut spec = TransitionSpec::new(item, to, actor);
        spec.expected_from = Some(from);
        spec.reason = Some(reason.into());
        if let Some(kind) = kind {
            spec.kind = kind;
        }
        let t = Transition {
            item: item.to_string(),
            from,
            to,
            kind: spec.kind,
            reason: spec.reason.clone(),
            upstream: None,
            origin: None,
        };
        self.ops.push(WorkOp::Transition(spec));
        if from.is_active() && !to.is_active() {
            graph_push(&mut self.revoke, item);
        }
        self.snap.apply(
            &Effects {
                transitions: vec![t],
                ..Effects::default()
            },
            self.now,
        );
        true
    }

    /// A non-closing move: the item's own transition, nothing cascades.
    /// Returns the ids to settle.
    pub fn move_to(
        &mut self,
        item: &str,
        to: Status,
        actor: &str,
        reason: impl Into<String>,
    ) -> Vec<WorkItemId> {
        if self.own(item, to, actor, reason, None) {
            vec![item.to_string()]
        } else {
            Vec::new()
        }
    }

    /// A `resume` move (6.7): a lease that did not survive.
    pub fn resume_to(&mut self, item: &str, to: Status, reason: String) -> Vec<WorkItemId> {
        if self.own(item, to, "controller", reason, Some(EventKind::Resume)) {
            vec![item.to_string()]
        } else {
            Vec::new()
        }
    }

    /// Record the same status again with a new reason, keeping its origin:
    /// how an approval marks an item a cascade holds.
    pub fn reassert(&mut self, item: &str, actor: &str, reason: String) {
        let Some(row) = self.snap.item(item) else {
            return;
        };
        let mut spec = TransitionSpec::new(item, row.status, actor);
        spec.expected_from = Some(row.status);
        spec.reason = Some(reason);
        spec.upstream = row.status_origin.clone();
        spec.origin = row.status_origin.clone();
        self.ops.push(WorkOp::Transition(spec));
    }

    /// The item's own transition and, for a closing one, its cascade (4.5),
    /// not yet settled. Returns the ids to settle.
    pub fn close(
        &mut self,
        item: &str,
        to: Status,
        actor: &str,
        reason: impl Into<String>,
    ) -> Vec<WorkItemId> {
        if !self.own(item, to, actor, reason, None) {
            return Vec::new();
        }
        let mut changed = vec![item.to_string()];
        if to.is_closed() {
            let fx = graph::cascade(&self.snap, item, to, self.now);
            for id in fx.touched() {
                graph_push(&mut changed, &id);
            }
            self.effects(&fx, "controller");
        }
        changed
    }

    /// Cancel a capability item nothing needs any more (6.2):
    /// `cancelled(cascade)` naming `origin`, the dependent whose close left
    /// it unneeded, then its own cascade. Returns the ids to settle.
    pub fn cancel_unneeded(&mut self, item: &str, origin: &str) -> Vec<WorkItemId> {
        let Some(from) = self.status(item).filter(|s| !s.is_closed()) else {
            return Vec::new();
        };
        let to = Status::Cancelled(CancelReason::Cascade);
        let own = Effects {
            transitions: vec![Transition {
                item: item.to_string(),
                from,
                to,
                kind: EventKind::Cascade,
                reason: Some("no open item needs this capability any more".to_string()),
                upstream: Some(origin.to_string()),
                origin: Some(origin.to_string()),
            }],
            ..Effects::default()
        };
        self.effects(&own, "controller");
        let mut changed = vec![item.to_string()];
        let fx = graph::cascade(&self.snap, item, to, self.now);
        for id in fx.touched() {
            graph_push(&mut changed, &id);
        }
        self.effects(&fx, "controller");
        changed
    }

    /// Cancel `item` and its open subtree for a user or policy (4.2), with
    /// its own transition under `actor`. Returns the ids to settle and the
    /// ids cancelled.
    pub fn cancel_tree(
        &mut self,
        item: &str,
        actor: &str,
        reason: Option<String>,
    ) -> (Vec<WorkItemId>, Vec<WorkItemId>) {
        let fx = graph::cancel_subtree(&self.snap, item, CancelReason::Requested, item, self.now);
        let cancelled: Vec<WorkItemId> = fx
            .transitions
            .iter()
            .filter(|t| matches!(t.to, Status::Cancelled(_)))
            .map(|t| t.item.clone())
            .collect();
        let mut rest = fx.clone();
        let mut own = Effects::default();
        if rest.transitions.first().is_some_and(|t| t.item == item) {
            let mut t = rest.transitions.remove(0);
            if let Some(r) = reason {
                t.reason = Some(r);
            }
            own.transitions.push(t);
        }
        self.push_effects(&own, actor, None);
        self.push_effects(&rest, "controller", None);
        self.snap.apply(&fx, self.now);
        (fx.touched(), cancelled)
    }

    /// Write an engine call's effects under `actor` and apply them.
    pub fn effects(&mut self, fx: &Effects, actor: &str) {
        self.push_effects(fx, actor, None);
        self.snap.apply(fx, self.now);
    }

    /// Write effects as `resume` corrections (6.7).
    pub fn corrections(&mut self, fx: &Effects) {
        self.push_effects(fx, "controller", Some(EventKind::Resume));
        self.snap.apply(fx, self.now);
    }

    fn push_effects(&mut self, fx: &Effects, actor: &str, kind: Option<EventKind>) {
        for t in &fx.transitions {
            self.ops.push(WorkOp::Transition(TransitionSpec {
                item: t.item.clone(),
                expected_from: Some(t.from),
                to: t.to,
                kind: kind.unwrap_or(t.kind),
                actor: actor.to_string(),
                reason: t.reason.clone(),
                upstream: t.upstream.clone(),
                origin: t.origin.clone(),
                evidence_ref: None,
            }));
        }
        for r in &fx.repoints {
            let key = (
                r.item.clone(),
                r.old_upstream.clone(),
                r.new_upstream.clone(),
            );
            let kind = match r.link {
                Link::Edge(k) => {
                    self.moved_inputs.insert(key);
                    Some(k)
                }
                Link::Input if self.moved_inputs.contains(&key) => continue,
                Link::Input => None,
            };
            self.ops.push(WorkOp::Repoint(RepointSpec {
                item: r.item.clone(),
                kind,
                old_upstream: r.old_upstream.clone(),
                new_upstream: r.new_upstream.clone(),
                origin: Some(r.origin.clone()),
                actor: actor.to_string(),
            }));
        }
        let mut dropped: Vec<WorkItemId> = Vec::new();
        for e in &fx.dropped_edges {
            graph_push(&mut dropped, &e.item);
        }
        for item in dropped {
            self.ops.push(WorkOp::RemoveOrderingEdgesOf(item));
        }
        for id in &fx.revoke {
            graph_push(&mut self.revoke, id);
        }
    }

    /// Readiness and roll-up (6.2) for `changed`, then verify every parent
    /// the roll-up moves into `verifying` and settle what that closes, until
    /// nothing moves.
    pub fn settle(&mut self, changed: Vec<WorkItemId>) {
        let mut changed = changed;
        for _ in 0..SETTLE_ROUNDS {
            let fx = graph::settle(&self.snap, &changed, self.now);
            self.effects(&fx, "controller");
            let verifying: Vec<WorkItemId> = self
                .snap
                .items()
                .iter()
                .filter(|i| i.status == Status::Verifying && self.snap.has_children(&i.id))
                .map(|i| i.id.clone())
                .collect();
            if verifying.is_empty() {
                return;
            }
            changed = Vec::new();
            for parent in verifying {
                let touched = match parent_verdict(&self.snap, &parent) {
                    Ok(record) => self.close(&parent, Status::Done, "controller", record),
                    Err(why) => {
                        let to = Status::Blocked(
                            rustykrab_core::work::BlockedReason::VerificationFailed,
                        );
                        let touched = self.move_to(&parent, to, "controller", why.clone());
                        self.notify(
                            &parent,
                            Cause::Asked {
                                item: parent.clone(),
                                text: format!("its done_when did not verify: {why}"),
                            },
                        );
                        touched
                    }
                };
                changed.extend(touched);
            }
        }
    }

    /// A note event: no status change.
    pub fn note(&mut self, item: &str, kind: EventKind, actor: &str, reason: String) {
        self.ops.push(WorkOp::Note(WorkEvent {
            item: item.to_string(),
            at: self.stamp,
            kind,
            from: None,
            to: None,
            actor: actor.to_string(),
            reason: Some(reason),
            upstream: None,
            origin: None,
            evidence_ref: None,
        }));
    }

    /// Record a rung climbed on `item` (section 8: every rung is an event).
    pub fn rung(&mut self, item: &str, state: &mut LadderState, event: RungEvent) {
        self.note(item, EventKind::Rung, "controller", encode_rung(&event));
        state.history.push(event);
        self.ladders.insert(item.to_string(), state.clone());
    }

    /// Ask for a notice on `item`'s root.
    pub fn notify(&mut self, item: &str, cause: Cause) {
        self.causes.push((item.to_string(), cause));
    }

    /// Insert an accepted filing: its rows, its plan row, its edges and its
    /// effects on existing items, then its warnings as events on the root.
    pub fn file(&mut self, accepted: &Accepted, row: WorkPlanRow, actor: &str) {
        for item in &accepted.items {
            self.ops.push(WorkOp::Insert(Box::new(item.clone())));
        }
        self.ops.push(WorkOp::Plan(row));
        for edge in &accepted.edges {
            self.ops.push(WorkOp::AddEdge(edge.clone()));
        }
        self.push_effects(&accepted.effects, actor, None);
        self.snap.insert(accepted, self.now);
        for w in &accepted.warnings {
            let ids: Vec<String> = w.items.iter().map(|i| super::notice::short(i)).collect();
            self.note(
                &accepted.root,
                EventKind::Warning,
                "controller",
                format!(
                    "sequential_split: {} could be one item on one worker",
                    ids.join(" -> ")
                ),
            );
        }
    }

    /// The roots this batch owes a notice, each with its causes, in the
    /// order first seen: explicit causes, every expiry, and every root
    /// that closes (except system work that simply finished, and a failed
    /// step whose plan B now runs in its place).
    pub fn notice_roots(&self) -> Vec<(WorkItemId, Vec<Cause>)> {
        let mut out: Vec<(WorkItemId, Vec<Cause>)> = Vec::new();
        let mut add =
            |root: WorkItemId, cause: Cause| match out.iter_mut().find(|(r, _)| *r == root) {
                Some((_, causes)) => {
                    if !causes.contains(&cause) {
                        causes.push(cause);
                    }
                }
                None => out.push((root, vec![cause])),
            };
        for op in &self.ops {
            let WorkOp::Transition(t) = op else {
                continue;
            };
            if t.to == Status::Expired {
                add(self.root_of(&t.item), Cause::Expired(t.item.clone()));
            }
            let is_root = self.snap.item(&t.item).is_some_and(|i| i.parent.is_none());
            if t.to.is_closed() && is_root && self.close_is_news(&t.item, t.to) {
                add(t.item.clone(), Cause::Closed(t.item.clone()));
            }
        }
        for (item, cause) in &self.causes {
            add(self.root_of(item), cause.clone());
        }
        out
    }

    fn close_is_news(&self, item: &str, to: Status) -> bool {
        let Some(row) = self.snap.item(item) else {
            return false;
        };
        if to == Status::Done && matches!(row.kind, WorkKind::Internal | WorkKind::Capability) {
            return false;
        }
        // A capability item cancelled because nothing needs it any more is
        // housekeeping: the user heard about the chain that needed it.
        if to == Status::Cancelled(CancelReason::Cascade) && row.kind == WorkKind::Capability {
            return false;
        }
        !(to == Status::Failed && has_open_plan_b(&self.snap, item))
    }

    /// Wrap an item's result claims as evidence rows.
    pub fn evidence(
        &mut self,
        item: &str,
        kind: &str,
        reference: impl Into<String>,
        verified_by: Option<&str>,
    ) {
        self.evidence.push(Evidence {
            item: item.to_string(),
            kind: kind.to_string(),
            reference: reference.into(),
            hash: None,
            verified_by: verified_by.map(str::to_string),
            at: self.now,
        });
    }
}

fn graph_push(list: &mut Vec<WorkItemId>, id: &str) {
    if !list.iter().any(|x| x == id) {
        list.push(id.to_string());
    }
}

/// Whether `id` has a plan B its failure would release: a waiting
/// `conditional_on_failure` dependent.
pub(super) fn has_open_plan_b(snap: &Snapshot, id: &str) -> bool {
    snap.edges_naming(id).any(|e| {
        e.kind == EdgeKind::ConditionalOnFailure
            && snap.status(&e.item).is_some_and(|s| s.is_waiting())
    })
}

/// A parent's verifier in Phase 1 (4.2: `done_when` decides, from the
/// children's evidence). No model reads `done_when` yet, so code checks
/// what it can: every child closed, at least one done, and every child
/// that did not finish accounted for: cancelled (superseded, a plan B that
/// was not needed, or a step the user declined) or a failed step whose
/// plan B is done. A failed or expired step fails verification, which
/// surfaces.
pub(super) fn parent_verdict(snap: &Snapshot, parent: &str) -> Result<String, String> {
    let kids = snap.children(parent);
    let mut done = 0usize;
    let mut covered: Vec<String> = Vec::new();
    for kid in kids {
        let Some(row) = snap.item(kid) else {
            continue;
        };
        match row.status {
            Status::Done => done += 1,
            Status::Cancelled(_) => {}
            Status::Failed if covered_by_plan_b(snap, kid, 0) => covered.push(label(row)),
            other if !other.is_closed() => {
                return Err(format!("{} is still {}", label(row), other));
            }
            other => return Err(format!("{} ended {}", label(row), other)),
        }
    }
    if done == 0 {
        return Err("no child is done".to_string());
    }
    let mut record = format!("verified from its children: {done} of {} done", kids.len());
    if !covered.is_empty() {
        record.push_str(&format!(
            "; covered by their plan B: {}",
            covered.join(", ")
        ));
    }
    Ok(record)
}

fn covered_by_plan_b(snap: &Snapshot, id: &str, depth: usize) -> bool {
    depth < 8
        && snap
            .edges_naming(id)
            .filter(|e| e.kind == EdgeKind::ConditionalOnFailure)
            .any(|e| match snap.status(&e.item) {
                Some(Status::Done) => true,
                Some(Status::Failed) => covered_by_plan_b(snap, &e.item, depth + 1),
                _ => false,
            })
}
