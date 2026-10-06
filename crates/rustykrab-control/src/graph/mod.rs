//! The graph engine (plan sections 4, 6.1, 6.2, 6.4, 6.5 and 14.1).
//!
//! Pure functions over an in-memory [`Snapshot`] of work items and their
//! edges. Nothing here does I/O, touches the store, awaits or reads a
//! clock: every function that depends on time takes `now`. The store
//! supplies the rows; the controller writes back what these functions
//! return, inside the transaction that caused them (plan section 4.5).
//!
//! The pieces, and the plan sections they implement:
//!
//! - [`Snapshot`]: the rows plus the indexes every rule reads (children of
//!   a parent, the edges an item holds, the edges naming an item,
//!   ancestors).
//! - [`readiness`], [`is_ready`], [`stale_ready`], [`due_expiries`] and
//!   [`edge_summary`]: which waiting items may run (4.1, 4.2).
//! - [`cascade`], [`cancel_subtree`], [`hold_recompute`], [`step`] and
//!   [`settle`]: what a transition does to the item's dependents, its
//!   subtree and its ancestors (4.1, 4.2, 4.5, 6.2, 6.4).
//! - [`rollup`] and [`rollup_all`]: a parent's status from its subtree
//!   (4.2).
//! - [`validate`]: the one validator every filing path goes through, with
//!   the typed rejection reasons of 14.1, the approval holds of 6.1 and the
//!   `supersedes` rules of 4.4 and 6.5 ([`supersede`]).
//! - [`aging_candidates`]: which closed items may be compacted (4.6).
//!
//! Everything returns *effects* ([`Transition`], [`Repoint`], dropped
//! edges) rather than mutating, so the controller can write them and their
//! events in one transaction. [`Snapshot::apply`] replays effects onto a
//! snapshot, which is how the tests (and a controller that keeps a snapshot
//! warm) chain one step into the next.
//!
//! The usual order inside one controller transaction is: the item's own
//! transition, [`cascade`] (4.5), then [`settle`] (readiness, then the
//! roll-up of every touched ancestor). [`step`] does all three.

use rustykrab_core::work::{Edge, EdgeKind, EventKind, Status, WorkItemId};

mod aging;
mod cascade;
mod order;
mod ready;
mod rollup;
mod snapshot;
mod supersede;
mod validate;

#[cfg(test)]
mod tests;

pub use aging::aging_candidates;
pub use cascade::{
    cancel_subtree, cascade, cascade_from, hold_recompute, hold_recompute_all, settle, step,
};
pub use ready::{
    due_expiries, edge_summary, is_ready, readiness, stale_ready, CascadeCancel, EdgeSummary, Hold,
};
pub use rollup::{rollup, rollup_all, Rollup};
pub use snapshot::Snapshot;
pub use supersede::{supersede, supersede_refusals, SupersedeRefusal};
pub use validate::{
    review_scope_violation, validate, Accepted, ApprovalPolicy, ApprovalTrigger, Check, Failure,
    FilingContext, FilingSource, Rejection, SplitMode, Validation,
};

/// What a [`Repoint`] moves: an edge row, or an `inputs_from` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Link {
    /// The dependent's `work_item_deps` row of this kind.
    Edge(EdgeKind),
    /// An entry of the dependent's `inputs_from` (plan section 4.3).
    Input,
}

/// One status change the engine asks the controller to write, with the
/// fields of its `work_item_events` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub item: WorkItemId,
    pub from: Status,
    pub to: Status,
    /// `Cascade` when another item caused it (then `origin` is set),
    /// `Transition` otherwise (readiness, roll-up, a direct transition).
    pub kind: EventKind,
    /// A short cause, for the event row.
    pub reason: Option<String>,
    /// The direct upstream, for cascades.
    pub upstream: Option<WorkItemId>,
    /// The root-cause item. Written to `status_origin` as well; `None`
    /// clears it.
    pub origin: Option<WorkItemId>,
}

impl Transition {
    pub(crate) fn plain(item: &str, from: Status, to: Status, reason: &str) -> Transition {
        Transition {
            item: item.to_string(),
            from,
            to,
            kind: EventKind::Transition,
            reason: Some(reason.to_string()),
            upstream: None,
            origin: None,
        }
    }

    pub(crate) fn caused(
        item: &str,
        from: Status,
        to: Status,
        reason: &str,
        upstream: &str,
        origin: &str,
    ) -> Transition {
        Transition {
            item: item.to_string(),
            from,
            to,
            kind: EventKind::Cascade,
            reason: Some(reason.to_string()),
            upstream: Some(upstream.to_string()),
            origin: Some(origin.to_string()),
        }
    }
}

/// An edge or input moved from one upstream to another (plan sections 4.1,
/// "A plan B stands in for what it covers", and 4.4, "Re-pointing"). Each
/// is an event of kind `repoint` on `item`.
///
/// The store rewrites `depends_on` in place. When the rewritten row
/// already exists (the primary key covers all three columns), it deletes
/// the old row instead; [`Snapshot::apply`] does the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repoint {
    /// The dependent whose edge or input moves.
    pub item: WorkItemId,
    pub link: Link,
    pub old_upstream: WorkItemId,
    pub new_upstream: WorkItemId,
    /// What caused the move: the failed step whose plan B took over, or the
    /// replacement that superseded the old upstream.
    pub origin: WorkItemId,
}

/// Everything one engine call asks the controller to write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Effects {
    /// Status changes, in the order they were derived.
    pub transitions: Vec<Transition>,
    pub repoints: Vec<Repoint>,
    /// Edge rows to delete: a superseded item's own ordering edges (4.4).
    pub dropped_edges: Vec<Edge>,
    /// Leased or running items this cancels or expires: the controller
    /// revokes the lease and stops the worker at its next step, keeping its
    /// evidence (4.2).
    pub revoke: Vec<WorkItemId>,
    /// `verifying` items a parent's cancel or expiry reached. They finish
    /// verification and keep its verdict (4.2); the controller cascades
    /// from them when they close.
    pub verifying: Vec<WorkItemId>,
}

impl Effects {
    pub fn is_empty(&self) -> bool {
        self.transitions.is_empty()
            && self.repoints.is_empty()
            && self.dropped_edges.is_empty()
            && self.revoke.is_empty()
            && self.verifying.is_empty()
    }

    /// Append another call's effects after these.
    pub fn extend(&mut self, other: Effects) {
        self.transitions.extend(other.transitions);
        self.repoints.extend(other.repoints);
        self.dropped_edges.extend(other.dropped_edges);
        for id in other.revoke {
            push_unique(&mut self.revoke, id);
        }
        for id in other.verifying {
            push_unique(&mut self.verifying, id);
        }
    }

    /// The last transition these effects give `item`.
    pub fn transition(&self, item: &str) -> Option<&Transition> {
        self.transitions.iter().rev().find(|t| t.item == item)
    }

    /// The status these effects leave `item` in, if they change it.
    pub fn status_of(&self, item: &str) -> Option<Status> {
        self.transition(item).map(|t| t.to)
    }

    /// Every item a transition or re-point touches, first-seen order. The
    /// set to hand [`settle`] and [`rollup_all`].
    pub fn touched(&self) -> Vec<WorkItemId> {
        let mut out = Vec::new();
        for t in &self.transitions {
            push_unique(&mut out, t.item.clone());
        }
        for r in &self.repoints {
            push_unique(&mut out, r.item.clone());
        }
        out
    }
}

pub(crate) fn push_unique(list: &mut Vec<WorkItemId>, id: WorkItemId) {
    if !list.contains(&id) {
        list.push(id);
    }
}
