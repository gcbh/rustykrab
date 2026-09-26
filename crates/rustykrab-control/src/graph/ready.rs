//! Readiness (plan sections 4.1 and 4.2).
//!
//! An item is ready when every ordering edge is satisfied, its trigger has
//! fired, it has not expired, no approval holds it, and every ancestor's
//! gate lets it through. An item with children is never ready. Lease-time
//! preconditions are checked by the controller, not here.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use rustykrab_core::work::{BlockedReason, Edge, EdgeKind, Status, WorkItem, WorkItemId};

use super::Snapshot;

/// A cascade hold: what `blocked(upstream_failed | upstream_expired)` an
/// item's edges put on it, and on whose account (4.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    /// `UpstreamFailed` or `UpstreamExpired`.
    pub reason: BlockedReason,
    /// The root-cause item every held item in the chain names.
    pub origin: WorkItemId,
    /// The direct upstream.
    pub upstream: WorkItemId,
}

/// How an item's own ordering edges stand, per plan section 4.1's table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeSummary {
    /// Every ordering edge the item holds is satisfied.
    pub satisfied: bool,
    /// The first edge that holds the item: a `blocks` upstream failed or
    /// expired, or a `blocks` or `waits_for` upstream itself held.
    pub hold: Option<Hold>,
    /// The first edge whose upstream's outcome cancels the item by cascade:
    /// a `blocks` upstream cancelled, or a `conditional_on_failure`
    /// upstream done, expired or cancelled.
    pub cancel: Option<CascadeCancel>,
}

/// An upstream outcome that ends the item `cancelled(cascade)` (4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CascadeCancel {
    /// The root-cause item.
    pub origin: WorkItemId,
    /// The direct upstream.
    pub upstream: WorkItemId,
}

/// What one ordering edge says about its downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Satisfied,
    Waiting,
    Hold(Hold),
    Cancel(CascadeCancel),
}

fn root_cause(up: &WorkItem) -> WorkItemId {
    up.status_origin.clone().unwrap_or_else(|| up.id.clone())
}

fn hold(reason: BlockedReason, origin: WorkItemId, upstream: &str) -> Hold {
    Hold {
        reason,
        origin,
        upstream: upstream.to_string(),
    }
}

fn cancel(up: &WorkItem) -> CascadeCancel {
    CascadeCancel {
        origin: root_cause(up),
        upstream: up.id.clone(),
    }
}

/// Apply 4.1's table to one ordering edge. A non-ordering edge is
/// satisfied; an upstream missing from the snapshot never is.
pub(crate) fn verdict(snap: &Snapshot, edge: &Edge) -> Verdict {
    if !edge.kind.is_ordering() {
        return Verdict::Satisfied;
    }
    let Some(up) = snap.item(&edge.depends_on) else {
        return Verdict::Waiting;
    };
    let cascade_hold = match up.status {
        Status::Blocked(r) if r.is_cascade() => Some(r),
        _ => None,
    };
    match edge.kind {
        EdgeKind::Blocks => match up.status {
            Status::Done => Verdict::Satisfied,
            Status::Failed => {
                Verdict::Hold(hold(BlockedReason::UpstreamFailed, up.id.clone(), &up.id))
            }
            Status::Expired => {
                Verdict::Hold(hold(BlockedReason::UpstreamExpired, up.id.clone(), &up.id))
            }
            Status::Cancelled(_) => Verdict::Cancel(cancel(up)),
            _ => match cascade_hold {
                Some(r) => Verdict::Hold(hold(r, root_cause(up), &up.id)),
                None => Verdict::Waiting,
            },
        },
        EdgeKind::WaitsFor => {
            if up.status.is_closed() {
                Verdict::Satisfied
            } else if let Some(r) = cascade_hold {
                Verdict::Hold(hold(r, root_cause(up), &up.id))
            } else {
                Verdict::Waiting
            }
        }
        EdgeKind::ConditionalOnFailure => match up.status {
            Status::Failed => Verdict::Satisfied,
            Status::Done | Status::Expired | Status::Cancelled(_) => Verdict::Cancel(cancel(up)),
            _ => Verdict::Waiting,
        },
        EdgeKind::Supersedes | EdgeKind::DiscoveredFrom => Verdict::Satisfied,
    }
}

/// How the ordering edges `id` holds stand (4.1). Edges its ancestors hold
/// are not included; they gate it through [`is_ready`].
pub fn edge_summary(snap: &Snapshot, id: &str) -> EdgeSummary {
    let mut out = EdgeSummary {
        satisfied: true,
        hold: None,
        cancel: None,
    };
    for edge in snap.edges_held_by(id) {
        match verdict(snap, edge) {
            Verdict::Satisfied => {}
            Verdict::Waiting => out.satisfied = false,
            Verdict::Hold(h) => {
                out.satisfied = false;
                out.hold.get_or_insert(h);
            }
            Verdict::Cancel(c) => {
                out.satisfied = false;
                out.cancel.get_or_insert(c);
            }
        }
    }
    out
}

/// An item's own gate: open, not held for approval, trigger fired, not
/// past `expires_at`, every ordering edge satisfied (4.2).
fn gate_open(snap: &Snapshot, item: &WorkItem, now: DateTime<Utc>) -> bool {
    !item.status.is_closed()
        && item.held_by.is_none()
        && snap.trigger_fired(&item.id, now)
        && item.expires_at.is_none_or(|t| t > now)
        && edge_summary(snap, &item.id).satisfied
}

/// Whether `id` may run now: it is `queued` or `ready`, has no children,
/// and its own gate and every ancestor's gate are open (4.1, 4.2).
pub fn is_ready(snap: &Snapshot, id: &str, now: DateTime<Utc>) -> bool {
    let Some(item) = snap.item(id) else {
        return false;
    };
    matches!(item.status, Status::Queued | Status::Ready)
        && !snap.has_children(id)
        && gate_open(snap, item, now)
        && snap
            .ancestors(id)
            .iter()
            .all(|a| snap.item(a).is_some_and(|p| gate_open(snap, p, now)))
}

/// The `queued` items that should become `ready`, in row order.
pub fn readiness(snap: &Snapshot, now: DateTime<Utc>) -> Vec<WorkItemId> {
    snap.items()
        .iter()
        .filter(|i| i.status == Status::Queued && is_ready(snap, &i.id, now))
        .map(|i| i.id.clone())
        .collect()
}

/// The `ready` items that no longer are (an edge was added, an approval
/// hold was placed, an ancestor's gate shut): they return to `queued`.
/// Resume (6.7) and the lease transaction re-read this.
pub fn stale_ready(snap: &Snapshot, now: DateTime<Utc>) -> Vec<WorkItemId> {
    snap.items()
        .iter()
        .filter(|i| i.status == Status::Ready && !is_ready(snap, &i.id, now))
        .map(|i| i.id.clone())
        .collect()
}

/// Open items whose `expires_at` has passed, for the timer sweep and for
/// resume (6.7). An item under an ancestor that is itself due is left out:
/// the ancestor's expiry cancels it by cascade (4.2).
pub fn due_expiries(snap: &Snapshot, now: DateTime<Utc>) -> Vec<WorkItemId> {
    let due: Vec<&WorkItem> = snap
        .items()
        .iter()
        .filter(|i| !i.status.is_closed() && i.expires_at.is_some_and(|t| t <= now))
        .collect();
    let ids: HashSet<&str> = due.iter().map(|i| i.id.as_str()).collect();
    due.iter()
        .filter(|i| {
            !snap
                .ancestors(&i.id)
                .iter()
                .any(|a| ids.contains(a.as_str()))
        })
        .map(|i| i.id.clone())
        .collect()
}
