//! `supersedes` (plan sections 4.4 and 6.5).
//!
//! Re-planning supersedes; it never edits. Per target:
//!
//! | Target | Effect |
//! |---|---|
//! | `queued`, `ready`, `blocked` | `cancelled(superseded)` naming its replacement; its waiting dependents re-point to the replacement |
//! | `leased`, `running`, `verifying` | refused: `supersedes_active` |
//! | closed | refused: `supersedes_closed` |
//!
//! The target must sit under the filer's scope (`out_of_scope`). Re-pointing
//! moves every ordering edge and `inputs_from` entry naming the target onto
//! the replacement; the target's own ordering edges are dropped; history
//! edges (`supersedes`, `discovered_from`) keep naming it. Superseding a
//! parent supersedes its subtree: each waiting descendant is superseded in
//! the same filing or ends `cancelled(cascade)`, and an active descendant
//! refuses the filing. Holds behind a re-pointed item are re-derived, so a
//! re-plan that re-points a stalled chain clears it (4.5).

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use rustykrab_core::work::{CancelReason, Edge, RejectionReason, Status, WorkItemId};

use super::cascade::{hold_recompute_into, run};
use super::{push_unique, Effects, Link, Repoint, Snapshot, Transition};

/// Why one target may not be superseded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupersedeRefusal {
    /// `unknown_ref`, `out_of_scope`, `supersedes_active` or
    /// `supersedes_closed`.
    pub reason: RejectionReason,
    /// The target, and for an active descendant, that descendant.
    pub items: Vec<WorkItemId>,
    pub detail: String,
}

/// 4.4's per-target rules for superseding `target` from a filing scoped to
/// `scope` (`None`: nothing is in scope). Empty when the target may be
/// superseded.
pub fn supersede_refusals(
    snap: &Snapshot,
    target: &str,
    scope: Option<&str>,
) -> Vec<SupersedeRefusal> {
    let Some(row) = snap.item(target) else {
        return vec![SupersedeRefusal {
            reason: RejectionReason::UnknownRef,
            items: vec![target.to_string()],
            detail: "the supersede target does not exist".to_string(),
        }];
    };
    let mut out = Vec::new();
    if !scope.is_some_and(|s| snap.is_within(target, s)) {
        out.push(SupersedeRefusal {
            reason: RejectionReason::OutOfScope,
            items: vec![target.to_string()],
            detail: match scope {
                Some(s) => format!("{target} is not under the filer's root {s}"),
                None => "the filing has no scope to supersede in".to_string(),
            },
        });
    }
    if row.status.is_active() {
        out.push(SupersedeRefusal {
            reason: RejectionReason::SupersedesActive,
            items: vec![target.to_string()],
            detail: format!("{target} is {}: it must fail or finish first", row.status),
        });
    } else if row.status.is_closed() {
        out.push(SupersedeRefusal {
            reason: RejectionReason::SupersedesClosed,
            items: vec![target.to_string()],
            detail: format!(
                "{target} is {}: closed is final; file a retry as a new item",
                row.status
            ),
        });
    }
    for d in snap.descendants(target) {
        if let Some(s) = snap.status(&d).filter(|s| s.is_active()) {
            out.push(SupersedeRefusal {
                reason: RejectionReason::SupersedesActive,
                items: vec![target.to_string(), d.clone()],
                detail: format!("{d} under {target} is {s}"),
            });
        }
    }
    out
}

/// Apply supersedes to a snapshot where every replacement already exists.
/// `pairs` are `(replacement, target)`; each target must have passed
/// [`supersede_refusals`]. Returns the targets' `cancelled(superseded)`
/// transitions, the re-points, the dropped edges, the subtree cancels with
/// their cascades, and the holds re-derived behind re-pointed items.
pub fn supersede(
    snap: &Snapshot,
    pairs: &[(WorkItemId, WorkItemId)],
    now: DateTime<Utc>,
) -> Effects {
    let mut work = snap.clone();
    let mut out = Effects::default();
    supersede_in(
        &mut work,
        pairs,
        &HashSet::new(),
        &HashSet::new(),
        now,
        &mut out,
    );
    out
}

/// [`supersede`] on a working snapshot. Items in `fresh_items` and edges in
/// `fresh_edges` are being inserted by the same filing: their rows are
/// written already re-pointed, so they get no re-point event and no
/// dropped-edge row.
pub(crate) fn supersede_in(
    work: &mut Snapshot,
    pairs: &[(WorkItemId, WorkItemId)],
    fresh_items: &HashSet<WorkItemId>,
    fresh_edges: &HashSet<Edge>,
    now: DateTime<Utc>,
    out: &mut Effects,
) {
    let targets: HashSet<&str> = pairs.iter().map(|(_, t)| t.as_str()).collect();
    let mut moved: Vec<WorkItemId> = Vec::new();

    // 1. Re-point what names each target onto its replacement.
    for (replacement, target) in pairs {
        let dependents: Vec<Edge> = work
            .edges_naming(target)
            .filter(|e| e.kind.is_ordering())
            .cloned()
            .collect();
        for edge in dependents {
            if targets.contains(edge.item.as_str()) || edge.item == *replacement {
                continue;
            }
            if !work.status(&edge.item).is_some_and(|s| s.is_waiting()) {
                continue;
            }
            if !fresh_items.contains(&edge.item) {
                out.repoints.push(Repoint {
                    item: edge.item.clone(),
                    link: Link::Edge(edge.kind),
                    old_upstream: target.clone(),
                    new_upstream: replacement.clone(),
                    origin: replacement.clone(),
                });
            }
            work.repoint(&edge.item, Link::Edge(edge.kind), target, replacement);
            push_unique(&mut moved, edge.item.clone());
        }
        let readers: Vec<WorkItemId> = work
            .items()
            .iter()
            .filter(|i| {
                i.status.is_waiting()
                    && !targets.contains(i.id.as_str())
                    && i.id != *replacement
                    && i.inputs_from.iter().any(|x| x == target)
            })
            .map(|i| i.id.clone())
            .collect();
        for id in readers {
            if !fresh_items.contains(&id) {
                out.repoints.push(Repoint {
                    item: id.clone(),
                    link: Link::Input,
                    old_upstream: target.clone(),
                    new_upstream: replacement.clone(),
                    origin: replacement.clone(),
                });
            }
            work.repoint(&id, Link::Input, target, replacement);
        }
    }

    // 2. Drop each target's own ordering edges; the replacement declares
    //    its own.
    let mut ordered_targets: Vec<&WorkItemId> = Vec::new();
    for (_, target) in pairs {
        if !ordered_targets.contains(&target) {
            ordered_targets.push(target);
        }
    }
    for target in ordered_targets {
        let own: Vec<Edge> = work
            .edges_held_by(target)
            .filter(|e| e.kind.is_ordering())
            .cloned()
            .collect();
        for edge in own {
            if !fresh_edges.contains(&edge) {
                out.dropped_edges.push(edge.clone());
            }
            work.remove_edge(&edge);
        }
    }

    // 3. Each target becomes `cancelled(superseded)` naming its
    //    replacement.
    let mut closed: Vec<WorkItemId> = Vec::new();
    for (replacement, target) in pairs {
        if closed.contains(target) {
            continue;
        }
        let Some(from) = work.status(target) else {
            continue;
        };
        let to = Status::Cancelled(CancelReason::Superseded);
        work.set_status(target, to, Some(replacement.clone()), now);
        out.transitions.push(Transition::caused(
            target,
            from,
            to,
            "superseded",
            replacement,
            replacement,
        ));
        closed.push(target.clone());
    }

    // 4. Superseding a parent supersedes its subtree: what the filing did
    //    not supersede ends `cancelled(cascade)`, and that cascades on.
    for target in &closed {
        run(work, target, target, now, out);
    }

    // 5. Holds behind a re-pointed item are re-derived.
    hold_recompute_into(work, &moved, now, &mut out.transitions);
}
