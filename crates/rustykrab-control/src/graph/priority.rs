//! Blocker impact and temporary urgency through prerequisites and parent gates.
//! Derived on every scheduling pass; only a human reorder changes base priority.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rustykrab_core::work::{BlockedReason, EdgeKind, Status, WorkItem, WorkItemId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::Snapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchedulingPriority {
    pub priority: i32,
    /// Distinct open items that need this work; diamonds count once.
    pub waiters: usize,
}

impl SchedulingPriority {
    /// Explicit/inherited priority first; blocker impact breaks ties.
    pub fn compare(&self, other: &Self) -> std::cmp::Ordering {
        other
            .priority
            .cmp(&self.priority)
            .then_with(|| other.waiters.cmp(&self.waiters))
    }
}

/// The controller's ordered pending leaves. Dispatch and every UI consume
/// this projection; no browser recomputes the scheduling order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityList {
    pub revision: String,
    pub entries: Vec<PriorityEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityDependency {
    pub item: WorkItemId,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityEntry {
    pub position: usize,
    pub item: WorkItemId,
    pub title: String,
    /// Bounded description for every entry, including those outside telemetry.
    #[serde(default)]
    pub objective: String,
    #[serde(default)]
    pub done_when: String,
    #[serde(default)]
    pub description_truncated: bool,
    pub status: Status,
    pub base_priority: i32,
    pub effective_priority: i32,
    pub waiters: usize,
    pub goal: Option<PriorityDependency>,
    pub waiting_on: Vec<PriorityDependency>,
    pub wait_reason: String,
}

pub fn priority_list(snap: &Snapshot, now: DateTime<Utc>) -> PriorityList {
    let ranks = scheduling_priorities(snap, now);
    let mut items: Vec<_> = snap
        .items()
        .iter()
        .filter(|i| !i.status.is_closed() && !snap.has_children(&i.id))
        .collect();
    items.sort_by(|a, b| {
        ranks[&a.id]
            .compare(&ranks[&b.id])
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    let dependency = |id: &str| PriorityDependency {
        item: id.to_string(),
        title: snap
            .item(id)
            .map_or_else(|| "Missing prerequisite".into(), |i| i.title.clone()),
    };
    let entries: Vec<_> = items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let mut waiting = std::collections::BTreeSet::new();
            for owner in std::iter::once(item.id.clone()).chain(snap.ancestors(&item.id)) {
                for edge in snap.edges_held_by(&owner) {
                    if edge.kind.is_ordering()
                        && !matches!(
                            super::ready::verdict(snap, edge),
                            super::ready::Verdict::Satisfied
                        )
                    {
                        waiting.insert(edge.depends_on.clone());
                    }
                }
                if owner != item.id && snap.item(&owner).is_some_and(|i| i.held_by.is_some()) {
                    waiting.insert(owner);
                }
            }
            let waiting_on: Vec<_> = waiting.iter().map(|id| dependency(id)).collect();
            let wait_reason = if item.held_by.is_some() {
                "Waiting for your approval".to_string()
            } else if !waiting_on.is_empty() {
                "Waiting for prerequisite work".to_string()
            } else if !snap.trigger_fired(&item.id, now) {
                "Waiting for its trigger".to_string()
            } else {
                match item.status {
                    Status::Ready => "Ready for dispatch checks",
                    Status::Queued => "Waiting for readiness checks",
                    Status::Leased => "Assigned to a worker",
                    Status::Running => "Running",
                    Status::Verifying => "Checking the result",
                    Status::Blocked(BlockedReason::NeedsConsent) => "Needs your permission",
                    Status::Blocked(BlockedReason::NeedsDecision) => "Needs your decision",
                    Status::Blocked(BlockedReason::NeedsCredential) => "Needs a credential",
                    Status::Blocked(BlockedReason::BudgetExhausted) => {
                        "Stopped at its execution budget"
                    }
                    Status::Blocked(BlockedReason::NeedsTool) => "Waiting for a required tool",
                    Status::Blocked(_) => "Waiting for recovery",
                    _ => "Closed",
                }
                .to_string()
            };
            PriorityEntry {
                position: index + 1,
                item: item.id.clone(),
                title: item.title.clone(),
                objective: item.objective.chars().take(800).collect(),
                done_when: item.done_when.chars().take(400).collect(),
                description_truncated: item.objective.chars().count() > 800
                    || item.done_when.chars().count() > 400,
                status: item.status,
                base_priority: item.priority,
                effective_priority: ranks[&item.id].priority,
                waiters: ranks[&item.id].waiters,
                goal: snap.ancestors(&item.id).last().map(|id| dependency(id)),
                waiting_on,
                wait_reason,
            }
        })
        .collect();
    let revision = hex::encode(Sha256::digest(
        serde_json::to_vec(&entries).expect("priority entries serialize"),
    ));
    PriorityList { revision, entries }
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
    let unreleased_alternative = |item: &WorkItem| {
        snap.edges_held_by(&item.id).any(|edge| {
            edge.kind == EdgeKind::ConditionalOnFailure
                && !matches!(
                    super::ready::verdict(snap, edge),
                    super::ready::Verdict::Satisfied
                )
        })
    };
    let interested = |item: &WorkItem| {
        !item.status.is_closed()
            && item.held_by.is_none()
            && item.expires_at.is_none_or(|t| t > now)
            && snap.trigger_fired(&item.id, now)
            && !unreleased_alternative(item)
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
            if unreleased_alternative(item) {
                continue;
            }
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
