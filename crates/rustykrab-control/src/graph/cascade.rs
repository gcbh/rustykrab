//! Cascade (plan sections 4.1, 4.2, 4.5, 6.2 and 6.4).
//!
//! When an item closes, 4.1's table applies to its dependents and 4.2 to
//! its subtree, in the same transaction:
//!
//! - **Cancellation cascades through closed items.** A `blocks` dependent of
//!   a cancelled item, and a plan B whose step did anything but fail, end
//!   `cancelled(cascade)`, and their own dependents take the table in turn.
//!   A cancelled or expired parent ends every open descendant
//!   `cancelled(cascade)`: waiting ones at once, leased or running ones
//!   through a lease revoke, while `verifying` ones finish.
//! - **Holds spread through open items.** A `blocks` dependent of a failed
//!   or expired item is held (`blocked(upstream_failed | upstream_expired)`),
//!   and the hold spreads through `blocks` and `waits_for` edges to every
//!   open item behind it, all naming the same origin.
//! - **A plan B stands in for its step.** When a step with an open
//!   `conditional_on_failure` dependent fails, its `blocks` and `waits_for`
//!   dependents and the `inputs_from` entries naming it re-point to the
//!   plan B instead of taking the `failed` column.
//!
//! Nothing reaches an active item except a parent's cancel or expiry.

use std::collections::{HashMap, HashSet, VecDeque};

use chrono::{DateTime, Utc};
use rustykrab_core::work::{
    BlockedReason, CancelReason, Edge, EdgeKind, EventKind, Status, WorkItemId,
};

use super::ready::{edge_summary, readiness, stale_ready};
use super::rollup::rollup_all_into;
use super::{push_unique, Effects, Link, Repoint, Snapshot, Transition};

/// The consequences of `item` closing as `to`, with `item` as the origin:
/// 4.1 applied to its dependents and 4.2 to its subtree. The closing
/// transition itself is not included (the controller writes it), nor are
/// readiness and roll-up ([`settle`]).
pub fn cascade(snap: &Snapshot, item: &str, to: Status, now: DateTime<Utc>) -> Effects {
    cascade_from(snap, item, to, item, now)
}

/// [`cascade`] naming another root cause: for an item that closed because
/// of `origin` (a parent's expiry, an upstream's cancel).
pub fn cascade_from(
    snap: &Snapshot,
    item: &str,
    to: Status,
    origin: &str,
    now: DateTime<Utc>,
) -> Effects {
    let mut work = snap.clone();
    let mut out = Effects::default();
    let own_origin = (origin != item).then(|| origin.to_string());
    work.set_status(item, to, own_origin, now);
    run(&mut work, item, origin, now, &mut out);
    out
}

/// Cancel `parent` for `reason` and everything open beneath it (4.2), then
/// cascade from each cancelled item through its dependents (4.5).
///
/// - Waiting descendants end `cancelled(cascade)` at once.
/// - Leased or running descendants end `cancelled(cascade)` and are listed
///   in [`Effects::revoke`]: the controller revokes the lease, stops the
///   worker at its next step and keeps its partial evidence.
/// - `verifying` descendants are left to finish and listed in
///   [`Effects::verifying`].
/// - Closed descendants keep their status.
///
/// `origin` is the root cause: `parent` itself for a user's or policy's
/// cancel. A closed `parent` yields nothing; a `verifying` one is listed in
/// `verifying` and left to finish.
pub fn cancel_subtree(
    snap: &Snapshot,
    parent: &str,
    reason: CancelReason,
    origin: &str,
    now: DateTime<Utc>,
) -> Effects {
    let mut work = snap.clone();
    let mut out = Effects::default();
    let Some(from) = work.status(parent) else {
        return out;
    };
    if from.is_closed() {
        return out;
    }
    if from == Status::Verifying {
        push_unique(&mut out.verifying, parent.to_string());
        return out;
    }
    let to = Status::Cancelled(reason);
    let own_origin = (origin != parent).then(|| origin.to_string());
    out.transitions.push(Transition {
        item: parent.to_string(),
        from,
        to,
        kind: if own_origin.is_some() {
            EventKind::Cascade
        } else {
            EventKind::Transition
        },
        reason: Some(format!("cancelled ({})", reason.as_str())),
        upstream: own_origin.clone(),
        origin: own_origin.clone(),
    });
    if holds_lease(&work, parent, from) {
        push_unique(&mut out.revoke, parent.to_string());
    }
    work.set_status(parent, to, own_origin, now);
    run(&mut work, parent, origin, now, &mut out);
    out
}

/// One controller step: `item` moves to `to`, and everything that follows
/// in the same transaction does too. For a closing status that is the
/// cascade (4.5); for every status it is then readiness and the roll-up of
/// each touched ancestor ([`settle`]).
///
/// The first transition is the item's own. A closed `item` yields nothing:
/// closed is final. A leased or running item that is cancelled or expires
/// is listed in [`Effects::revoke`].
pub fn step(snap: &Snapshot, item: &str, to: Status, now: DateTime<Utc>) -> Effects {
    let mut work = snap.clone();
    let mut out = Effects::default();
    let Some(from) = work.status(item) else {
        return out;
    };
    if from.is_closed() {
        return out;
    }
    out.transitions.push(Transition {
        item: item.to_string(),
        from,
        to,
        kind: EventKind::Transition,
        reason: None,
        upstream: None,
        origin: None,
    });
    if holds_lease(&work, item, from) && matches!(to, Status::Cancelled(_) | Status::Expired) {
        push_unique(&mut out.revoke, item.to_string());
    }
    work.set_status(item, to, None, now);
    if to.is_closed() {
        run(&mut work, item, item, now, &mut out);
    }
    let changed = out.touched();
    settle_into(&mut work, &changed, now, &mut out);
    out
}

/// Re-derive what a change implies without a closing cascade: ready items
/// that no longer are return to `queued`, `queued` items whose gates are
/// open become `ready`, and every ancestor of `changed` (and `changed`
/// itself when it is a parent) is rolled up, bottom-up (6.2).
///
/// Call it after inserting an accepted filing, after a trigger fires or an
/// approval is answered, and at resume (6.7).
pub fn settle(snap: &Snapshot, changed: &[WorkItemId], now: DateTime<Utc>) -> Effects {
    let mut work = snap.clone();
    let mut out = Effects::default();
    settle_into(&mut work, changed, now, &mut out);
    out
}

pub(crate) fn settle_into(
    work: &mut Snapshot,
    changed: &[WorkItemId],
    now: DateTime<Utc>,
    out: &mut Effects,
) {
    let mut changed: Vec<WorkItemId> = changed.to_vec();
    for id in stale_ready(work, now) {
        out.transitions.push(Transition::plain(
            &id,
            Status::Ready,
            Status::Queued,
            "no longer ready",
        ));
        work.set_status(&id, Status::Queued, None, now);
        push_unique(&mut changed, id);
    }
    for id in readiness(work, now) {
        out.transitions.push(Transition::plain(
            &id,
            Status::Queued,
            Status::Ready,
            "ready",
        ));
        work.set_status(&id, Status::Ready, None, now);
        push_unique(&mut changed, id);
    }
    rollup_all_into(work, &changed, now, &mut out.transitions);
}

/// Re-derive one item's cascade hold from its upstream edges (4.5, "Holds
/// clear only by change"): it stays held only while a `blocks` upstream is
/// failed or expired, or a `blocks` or `waits_for` upstream is itself held.
///
/// Returns the transition to write: into a hold (or onto a different
/// origin), or out of one to `queued`. Items blocked for their own reasons
/// (`needs_*`, `budget_exhausted`, ...) and items not waiting are left
/// alone.
pub fn hold_recompute(snap: &Snapshot, item: &str) -> Option<Transition> {
    let row = snap.item(item)?;
    let held = matches!(row.status, Status::Blocked(r) if r.is_cascade());
    if !held && !matches!(row.status, Status::Queued | Status::Ready) {
        return None;
    }
    match edge_summary(snap, item).hold {
        Some(h) => {
            let to = Status::Blocked(h.reason);
            if row.status == to && row.status_origin.as_deref() == Some(h.origin.as_str()) {
                return None;
            }
            Some(Transition::caused(
                item,
                row.status,
                to,
                "held behind upstream",
                &h.upstream,
                &h.origin,
            ))
        }
        None if held => Some(Transition::plain(
            item,
            row.status,
            Status::Queued,
            "hold cleared",
        )),
        None => None,
    }
}

/// [`hold_recompute`] for `items` and, whenever one changes, for every item
/// behind it through `blocks` and `waits_for` edges, so a re-point that
/// clears one hold clears the chain behind it.
pub fn hold_recompute_all(
    snap: &Snapshot,
    items: &[WorkItemId],
    now: DateTime<Utc>,
) -> Vec<Transition> {
    let mut work = snap.clone();
    let mut out = Vec::new();
    hold_recompute_into(&mut work, items, now, &mut out);
    out
}

pub(crate) fn hold_recompute_into(
    work: &mut Snapshot,
    items: &[WorkItemId],
    now: DateTime<Utc>,
    out: &mut Vec<Transition>,
) {
    let mut queue: VecDeque<WorkItemId> = items.iter().cloned().collect();
    let mut visits: HashMap<WorkItemId, usize> = HashMap::new();
    let limit = 2 * work.items().len() + 2;
    while let Some(x) = queue.pop_front() {
        let seen = visits.entry(x.clone()).or_default();
        *seen += 1;
        if *seen > limit {
            continue;
        }
        let Some(t) = hold_recompute(work, &x) else {
            continue;
        };
        work.set_status(&x, t.to, t.origin.clone(), now);
        out.push(t);
        let behind: Vec<WorkItemId> = work
            .edges_naming(&x)
            .filter(|e| matches!(e.kind, EdgeKind::Blocks | EdgeKind::WaitsFor))
            .map(|e| e.item.clone())
            .collect();
        queue.extend(behind);
    }
}

/// The cascade proper, on a working snapshot whose `start` already carries
/// its closing status. `origin` is the root cause every cascaded item
/// names.
pub(crate) fn run(
    work: &mut Snapshot,
    start: &str,
    origin: &str,
    now: DateTime<Utc>,
    out: &mut Effects,
) {
    let mut queue: VecDeque<WorkItemId> = VecDeque::from([start.to_string()]);
    let mut done: HashSet<WorkItemId> = HashSet::new();
    while let Some(closed) = queue.pop_front() {
        if !done.insert(closed.clone()) {
            continue;
        }
        let Some(status) = work.status(&closed) else {
            continue;
        };
        if !status.is_closed() {
            continue;
        }
        if matches!(status, Status::Cancelled(_) | Status::Expired) {
            cancel_open_descendants(work, &closed, status, origin, now, out, &mut queue);
        }
        // Superseding re-points every waiting dependent first (4.4), so a
        // superseded item never reaches 4.1's table.
        if status == Status::Cancelled(CancelReason::Superseded) {
            continue;
        }
        let plan_b = if status == Status::Failed {
            open_plan_b(work, &closed)
        } else {
            None
        };
        let dependents: Vec<Edge> = work
            .edges_naming(&closed)
            .filter(|e| e.kind.is_ordering())
            .cloned()
            .collect();
        for edge in dependents {
            let Some(dep) = work.status(&edge.item) else {
                continue;
            };
            if !dep.is_waiting() {
                continue;
            }
            match (edge.kind, status) {
                (EdgeKind::Blocks | EdgeKind::WaitsFor, Status::Failed) => {
                    match plan_b.as_deref().filter(|p| *p != edge.item) {
                        Some(p) => repoint(work, &edge, p, &closed, out),
                        None if edge.kind == EdgeKind::Blocks => hold(
                            work,
                            &edge.item,
                            BlockedReason::UpstreamFailed,
                            origin,
                            &closed,
                            now,
                            out,
                        ),
                        None => {}
                    }
                }
                (EdgeKind::Blocks, Status::Expired) => hold(
                    work,
                    &edge.item,
                    BlockedReason::UpstreamExpired,
                    origin,
                    &closed,
                    now,
                    out,
                ),
                (EdgeKind::Blocks, Status::Cancelled(_)) => {
                    cancel(
                        work,
                        &edge.item,
                        origin,
                        &closed,
                        "blocks upstream cancelled",
                        now,
                        out,
                    );
                    queue.push_back(edge.item.clone());
                }
                (EdgeKind::ConditionalOnFailure, Status::Failed) => {}
                (EdgeKind::ConditionalOnFailure, _) => {
                    cancel(
                        work,
                        &edge.item,
                        origin,
                        &closed,
                        "plan B not needed: its step did not fail",
                        now,
                        out,
                    );
                    queue.push_back(edge.item.clone());
                }
                // `blocks` on a done upstream and `waits_for` on any closed
                // one are satisfied; readiness picks them up.
                _ => {}
            }
        }
        if let Some(p) = plan_b {
            repoint_inputs(work, &closed, &p, out);
        }
    }
}

/// Whether an item in `status` holds a lease: a leased or running leaf. A
/// parent's `running` is a roll-up; parents hold no lease (4.2).
fn holds_lease(work: &Snapshot, id: &str, status: Status) -> bool {
    matches!(status, Status::Leased | Status::Running) && !work.has_children(id)
}

/// The plan B a failed step releases: its first open
/// `conditional_on_failure` dependent.
fn open_plan_b(work: &Snapshot, step: &str) -> Option<WorkItemId> {
    work.edges_naming(step)
        .filter(|e| e.kind == EdgeKind::ConditionalOnFailure)
        .find(|e| work.status(&e.item).is_some_and(|s| s.is_waiting()))
        .map(|e| e.item.clone())
}

fn cancel_open_descendants(
    work: &mut Snapshot,
    parent: &str,
    parent_status: Status,
    origin: &str,
    now: DateTime<Utc>,
    out: &mut Effects,
    queue: &mut VecDeque<WorkItemId>,
) {
    let reason = if parent_status == Status::Expired {
        "ancestor expired"
    } else {
        "ancestor cancelled"
    };
    for d in work.descendants(parent) {
        let Some(row) = work.item(&d) else {
            continue;
        };
        let status = row.status;
        if status.is_closed() {
            continue;
        }
        if status == Status::Verifying {
            push_unique(&mut out.verifying, d);
            continue;
        }
        if holds_lease(work, &d, status) {
            push_unique(&mut out.revoke, d.clone());
        }
        let upstream = row.parent.clone().unwrap_or_else(|| parent.to_string());
        cancel(work, &d, origin, &upstream, reason, now, out);
        queue.push_back(d);
    }
}

fn cancel(
    work: &mut Snapshot,
    id: &str,
    origin: &str,
    upstream: &str,
    reason: &str,
    now: DateTime<Utc>,
    out: &mut Effects,
) {
    let Some(from) = work.status(id) else {
        return;
    };
    let to = Status::Cancelled(CancelReason::Cascade);
    work.set_status(id, to, Some(origin.to_string()), now);
    out.transitions
        .push(Transition::caused(id, from, to, reason, upstream, origin));
}

/// Hold `id` behind `upstream` and spread the hold through `blocks` and
/// `waits_for` edges, every held item naming `origin`. An item already held
/// by a cascade keeps its first cause.
fn hold(
    work: &mut Snapshot,
    id: &str,
    reason: BlockedReason,
    origin: &str,
    upstream: &str,
    now: DateTime<Utc>,
    out: &mut Effects,
) {
    let mut queue: VecDeque<(WorkItemId, WorkItemId)> =
        VecDeque::from([(id.to_string(), upstream.to_string())]);
    while let Some((x, up)) = queue.pop_front() {
        let Some(from) = work.status(&x) else {
            continue;
        };
        if !from.is_waiting() || matches!(from, Status::Blocked(r) if r.is_cascade()) {
            continue;
        }
        let to = Status::Blocked(reason);
        work.set_status(&x, to, Some(origin.to_string()), now);
        out.transitions.push(Transition::caused(
            &x,
            from,
            to,
            "held behind upstream",
            &up,
            origin,
        ));
        let behind: Vec<(WorkItemId, WorkItemId)> = work
            .edges_naming(&x)
            .filter(|e| matches!(e.kind, EdgeKind::Blocks | EdgeKind::WaitsFor))
            .map(|e| (e.item.clone(), x.clone()))
            .collect();
        queue.extend(behind);
    }
}

fn repoint(work: &mut Snapshot, edge: &Edge, to: &str, origin: &str, out: &mut Effects) {
    out.repoints.push(Repoint {
        item: edge.item.clone(),
        link: Link::Edge(edge.kind),
        old_upstream: edge.depends_on.clone(),
        new_upstream: to.to_string(),
        origin: origin.to_string(),
    });
    work.repoint(&edge.item, Link::Edge(edge.kind), &edge.depends_on, to);
}

/// Re-point waiting items' `inputs_from` entries naming a failed `step` to
/// its plan B (4.3). The step's plan Bs keep it as an input: they run
/// because it failed, and may report why.
fn repoint_inputs(work: &mut Snapshot, step: &str, plan_b: &str, out: &mut Effects) {
    let plan_bs: HashSet<WorkItemId> = work
        .edges_naming(step)
        .filter(|e| e.kind == EdgeKind::ConditionalOnFailure)
        .map(|e| e.item.clone())
        .collect();
    let movers: Vec<WorkItemId> = work
        .items()
        .iter()
        .filter(|i| {
            i.status.is_waiting()
                && !plan_bs.contains(&i.id)
                && i.inputs_from.iter().any(|x| x == step)
        })
        .map(|i| i.id.clone())
        .collect();
    for id in movers {
        out.repoints.push(Repoint {
            item: id.clone(),
            link: Link::Input,
            old_upstream: step.to_string(),
            new_upstream: plan_b.to_string(),
            origin: step.to_string(),
        });
        work.repoint(&id, Link::Input, step, plan_b);
    }
}
