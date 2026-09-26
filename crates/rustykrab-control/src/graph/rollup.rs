//! Parent roll-up (plan section 4.2).
//!
//! A parent's status is computed from its subtree, first match wins:
//!
//! | Subtree | Parent |
//! |---|---|
//! | the parent's own gate is shut | `queued`, `blocked(needs_consent)` for an approval, or the cascade hold its own edges put on it |
//! | any descendant ready, leased, running or verifying | `running` |
//! | otherwise any descendant blocked | `blocked(reason)`: a reason that needs the user first, else the earliest, with its origin |
//! | otherwise open descendants waiting | `queued` |
//! | every child closed | `verifying`, for the controller's verifier |
//!
//! A parent's own `cancelled`, `expired`, `done` or `failed` overrides the
//! table: closed is final.

use chrono::{DateTime, Utc};
use rustykrab_core::work::{BlockedReason, Status, WorkItemId};

use super::ready::edge_summary;
use super::{Snapshot, Transition};

/// A parent's rolled-up status and, for a `blocked` one, the item it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rollup {
    pub status: Status,
    /// For `blocked`: the root cause (the blocked descendant's
    /// `status_origin`, or the descendant itself when it is blocked for its
    /// own reason). `None` otherwise.
    pub origin: Option<WorkItemId>,
}

/// Roll `parent` up from its subtree per 4.2's table. `None` when `parent`
/// has no children (a leaf is never rolled up) or is not in the snapshot.
pub fn rollup(snap: &Snapshot, parent: &str, now: DateTime<Utc>) -> Option<Rollup> {
    let p = snap.item(parent)?;
    if !snap.has_children(parent) {
        return None;
    }
    if p.status.is_closed() {
        return Some(Rollup {
            status: p.status,
            origin: p.status_origin.clone(),
        });
    }

    // Row 1: the parent's own gate.
    if p.held_by.is_some() {
        return Some(Rollup {
            status: Status::Blocked(BlockedReason::NeedsConsent),
            origin: Some(parent.to_string()),
        });
    }
    let edges = edge_summary(snap, parent);
    if let Some(h) = edges.hold {
        return Some(Rollup {
            status: Status::Blocked(h.reason),
            origin: Some(h.origin),
        });
    }
    if !edges.satisfied || !snap.trigger_fired(parent, now) {
        return Some(queued());
    }

    let descendants: Vec<_> = snap
        .descendants(parent)
        .into_iter()
        .filter_map(|d| snap.item(&d))
        .collect();

    // Row 2: anything moving.
    if descendants.iter().any(|d| {
        matches!(
            d.status,
            Status::Ready | Status::Leased | Status::Running | Status::Verifying
        )
    }) {
        return Some(Rollup {
            status: Status::Running,
            origin: None,
        });
    }

    // Row 3: blocked, a reason that needs the user first, else the
    // earliest.
    let blocked: Vec<(usize, BlockedReason, &rustykrab_core::work::WorkItem)> = descendants
        .iter()
        .enumerate()
        .filter_map(|(i, d)| match d.status {
            Status::Blocked(r) => Some((i, r, *d)),
            _ => None,
        })
        .collect();
    let earliest = |needs_user: bool| {
        blocked
            .iter()
            .filter(|(_, r, _)| !needs_user || r.needs_user())
            .min_by_key(|(i, _, d)| (d.updated_at, *i))
    };
    if let Some((_, reason, d)) = earliest(true).or_else(|| earliest(false)) {
        return Some(Rollup {
            status: Status::Blocked(*reason),
            origin: Some(d.status_origin.clone().unwrap_or_else(|| d.id.clone())),
        });
    }

    // Row 4: open descendants waiting on edges or triggers.
    if descendants.iter().any(|d| !d.status.is_closed()) {
        return Some(queued());
    }

    // Row 5: every child closed.
    Some(Rollup {
        status: Status::Verifying,
        origin: None,
    })
}

fn queued() -> Rollup {
    Rollup {
        status: Status::Queued,
        origin: None,
    }
}

/// Roll up every ancestor of `changed` (and each changed item that is
/// itself a parent), deepest first, so each parent reads its children's new
/// roll-ups. Returns the transitions to write.
///
/// Statuses the roll-up never overwrites: a closed parent; a parent parked
/// `blocked(budget_exhausted)` by Select (6, step 1), which the controller
/// clears; and a parent `blocked(verification_failed)` while its children
/// are all still closed, which waits for its ladder's re-plan (6.4).
pub fn rollup_all(snap: &Snapshot, changed: &[WorkItemId], now: DateTime<Utc>) -> Vec<Transition> {
    let mut work = snap.clone();
    let mut out = Vec::new();
    rollup_all_into(&mut work, changed, now, &mut out);
    out
}

pub(crate) fn rollup_all_into(
    work: &mut Snapshot,
    changed: &[WorkItemId],
    now: DateTime<Utc>,
    out: &mut Vec<Transition>,
) {
    let mut parents: Vec<(usize, usize, WorkItemId)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in changed {
        let mut chain = Vec::new();
        if work.has_children(id) {
            chain.push(id.clone());
        }
        chain.extend(work.ancestors(id));
        for p in chain {
            if seen.insert(p.clone()) {
                let depth = work.ancestors(&p).len();
                parents.push((depth, parents.len(), p));
            }
        }
    }
    // Deepest first; ties in first-seen order.
    parents.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

    for (_, _, p) in parents {
        let Some(r) = rollup(work, &p, now) else {
            continue;
        };
        let Some(row) = work.item(&p) else {
            continue;
        };
        let current = row.status;
        if current.is_closed()
            || current == Status::Blocked(BlockedReason::BudgetExhausted)
            || (current == Status::Blocked(BlockedReason::VerificationFailed)
                && r.status == Status::Verifying)
        {
            continue;
        }
        if current == r.status && row.status_origin == r.origin {
            continue;
        }
        out.push(Transition {
            item: p.clone(),
            from: current,
            to: r.status,
            kind: rustykrab_core::work::EventKind::Transition,
            reason: Some("roll-up".to_string()),
            upstream: None,
            origin: r.origin.clone(),
        });
        work.set_status(&p, r.status, r.origin, now);
    }
}
