//! Temporary urgency inherited through unmet prerequisites and parent gates.
//! Derived on every scheduling pass; the stored priority never changes.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rustykrab_core::work::{EdgeKind, Status, WorkItem, WorkItemId};

use super::Snapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulingPriority {
    pub priority: i32,
    /// Distinct open items that need this work; diamonds count once.
    pub waiters: usize,
}

/// A prerequisite inherits the highest base priority of interested open work.
/// Closed, expired, future-triggered and approval-held work donate no urgency.
/// Metadata and unreleased plan-B edges never donate. Corrupt cycles terminate.
pub fn scheduling_priorities(
    snap: &Snapshot,
    now: DateTime<Utc>,
) -> HashMap<WorkItemId, SchedulingPriority> {
    let mut ranks: HashMap<_, _> = snap
        .items()
        .iter()
        .map(|i| {
            (
                i.id.clone(),
                SchedulingPriority {
                    priority: i.priority,
                    waiters: 0,
                },
            )
        })
        .collect();
    let interested = |item: &WorkItem| {
        !item.status.is_closed()
            && item.held_by.is_none()
            && item.expires_at.is_none_or(|t| t > now)
            && snap.trigger_fired(&item.id, now)
    };
    for source in snap.items().iter().filter(|i| interested(i)) {
        if snap
            .ancestors(&source.id)
            .iter()
            .filter_map(|id| snap.item(id))
            .any(|parent| !interested(parent))
        {
            continue;
        }
        let mut seen = HashSet::new();
        let mut pending = vec![source.id.clone()];
        while let Some(id) = pending.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let Some(item) = snap.item(&id).filter(|i| !i.status.is_closed()) else {
                continue;
            };
            if id != source.id {
                let rank = ranks.get_mut(&id).expect("snapshot item has rank");
                rank.priority = rank.priority.max(source.priority);
                rank.waiters += 1;
            }
            pending.extend(snap.children(&item.id).iter().cloned());
            // A child cannot run before its parent's prerequisites either.
            for owner in std::iter::once(id.clone()).chain(snap.ancestors(&id)) {
                for edge in snap.edges_held_by(&owner) {
                    let Some(up) = snap.item(&edge.depends_on) else {
                        continue;
                    };
                    let unmet = match edge.kind {
                        EdgeKind::Blocks => up.status != Status::Done,
                        EdgeKind::WaitsFor => !up.status.is_closed(),
                        _ => false,
                    };
                    if unmet {
                        pending.push(up.id.clone());
                    }
                }
            }
        }
    }
    ranks
}
