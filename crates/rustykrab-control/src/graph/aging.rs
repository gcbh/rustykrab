//! Aging (plan section 4.6).
//!
//! A closed item older than its kind's window is compacted into one
//! `work_item_archive` row, but only when no open item names it through an
//! ordering edge, `inputs_from` or `parent`, so readiness never reads an
//! archived row and a subtree ages together.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, TimeDelta, Utc};
use rustykrab_core::work::{WorkItemId, WorkKind};

use super::Snapshot;

/// The closed items that may be compacted now, in row order.
///
/// An item qualifies when it is closed, its `closed_at` plus its kind's
/// window is not after `now` (a kind with no window never ages), and no
/// open item names it through an ordering edge, `inputs_from` or `parent`.
/// Subtrees age together: an item waits for its parent (when the parent is
/// in the snapshot) and a parent waits for every child. A `discovered_from`
/// or `supersedes` edge from a live item does not hold anything back; it
/// resolves to the archive line.
pub fn aging_candidates(
    snap: &Snapshot,
    now: DateTime<Utc>,
    windows: &HashMap<WorkKind, TimeDelta>,
) -> Vec<WorkItemId> {
    let mut named: HashSet<&str> = HashSet::new();
    for item in snap.items().iter().filter(|i| !i.status.is_closed()) {
        for edge in snap.edges_held_by(&item.id) {
            if edge.kind.is_ordering() {
                named.insert(edge.depends_on.as_str());
            }
        }
        named.extend(item.inputs_from.iter().map(String::as_str));
        if let Some(p) = &item.parent {
            named.insert(p.as_str());
        }
    }

    let mut eligible: HashSet<&str> = snap
        .items()
        .iter()
        .filter(|i| {
            i.status.is_closed()
                && !named.contains(i.id.as_str())
                && match (i.closed_at, windows.get(&i.kind)) {
                    (Some(closed), Some(window)) => closed + *window <= now,
                    _ => false,
                }
        })
        .map(|i| i.id.as_str())
        .collect();

    loop {
        let drop: Vec<&str> = eligible
            .iter()
            .copied()
            .filter(|id| {
                let parent_blocks = snap
                    .item(id)
                    .and_then(|i| i.parent.as_deref())
                    .is_some_and(|p| snap.contains(p) && !eligible.contains(p));
                let child_blocks = snap
                    .children(id)
                    .iter()
                    .any(|c| !eligible.contains(c.as_str()));
                parent_blocks || child_blocks
            })
            .collect();
        if drop.is_empty() {
            break;
        }
        for id in drop {
            eligible.remove(id);
        }
    }

    snap.items()
        .iter()
        .filter(|i| eligible.contains(i.id.as_str()))
        .map(|i| i.id.clone())
        .collect()
}
