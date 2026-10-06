//! `worker_unavailable` (plan section 7): an item no healthy worker can
//! take waits for the registry, with a TTL.
//!
//! Selection parks a ready leaf that no healthy worker covers (whatever
//! their load) in `blocked(worker_unavailable)`, recorded as the ladder's
//! order 2c for a capacity gap: the wait is the item's capacity request,
//! routed to the registry. It spends the `requests` budget, not the
//! acquisition a credential or a tool would need later. The sweep requeues
//! it as soon as a healthy worker can take it, and once
//! [`super::ControllerConfig::worker_wait`] has passed climbs its ladder
//! with the same error. The request rung is spent then, so a plan B runs,
//! the parent re-plans, or the item surfaces; it never waits forever. A
//! surfaced item still resumes by itself when a worker appears, and its
//! question is then obsolete.

use std::collections::HashSet;

use rustykrab_core::questions::QuestionStatus;
use rustykrab_core::work::{
    BlockedReason, EdgeKind, Rung, RungEvent, Status, WorkError, WorkEvent, WorkItem, WorkItemId,
    WorkKind, WorkerKind,
};
use rustykrab_core::Error;
use rustykrab_store::{QuestionRow, QuestionWrite, WorkOp};

use super::batch::Batch;
use super::load::{history, ladder_from};
use super::Controller;
use crate::errors::{classify, Context, FailureInput, GapKind};
use crate::graph::{self, Snapshot};

/// How the wait's rung outcome starts: what the sweep looks for.
pub(super) const WORKER_WAIT: &str = "waiting for a worker";

/// What a worker for `item` has to be, as the gap's subject: stable for
/// an item, so the climb after the TTL finds the request rung spent.
fn worker_need(item: &WorkItem) -> String {
    let mut need = if graph::is_planning(item) {
        "a planner".to_string()
    } else if item.worker_kind == WorkerKind::Any {
        "a worker".to_string()
    } else {
        format!("a {} worker", item.worker_kind.as_str())
    };
    let mut with: Vec<String> = Vec::new();
    if !item.required_tools.is_empty() {
        with.push(format!("tools {}", item.required_tools.join(", ")));
    }
    if !item.required_mcp_servers.is_empty() {
        with.push(format!(
            "MCP servers {}",
            item.required_mcp_servers.join(", ")
        ));
    }
    if !item.writable_resources.is_empty() {
        with.push(format!(
            "write access to {}",
            item.writable_resources.join(", ")
        ));
    }
    if !with.is_empty() {
        need.push_str(" with ");
        need.push_str(&with.join("; "));
    }
    need
}

/// The typed error of an item no worker can take: a capacity gap.
fn worker_gap(item: &WorkItem) -> WorkError {
    classify(
        &FailureInput::CapabilityGap {
            gap: GapKind::Capacity,
            name: worker_need(item),
        },
        &Context {
            tool: None,
            worker_kind: Some(item.worker_kind),
        },
    )
}

/// Whether `item` waits behind a capability item the ladder filed (its
/// release is that item landing, not the registry).
fn behind_capability(snap: &Snapshot, item: &str) -> bool {
    snap.edges_held_by(item).any(|e| {
        e.kind == EdgeKind::Blocks
            && snap
                .item(&e.depends_on)
                .is_some_and(|u| u.kind == WorkKind::Capability)
    })
}

/// The wait an item's ladder records, if it is waiting for a worker.
fn the_wait(events: &[WorkEvent], item: &WorkItem) -> Option<(RungEvent, bool)> {
    let state = ladder_from(events, item);
    let is_wait = |e: &RungEvent| e.rung == Rung::Request && e.outcome.starts_with(WORKER_WAIT);
    let wait = state.history.iter().rev().find(|e| is_wait(e))?.clone();
    let last = state.history.last().is_some_and(is_wait);
    Some((wait, last))
}

impl Controller {
    /// Park a ready item no healthy worker can take (selection's step 2
    /// found none): `blocked(worker_unavailable)`, the wait recorded as
    /// its request rung. Returns the transitions written.
    pub(super) async fn await_worker(
        &self,
        item_id: &str,
        events: &[WorkEvent],
    ) -> Result<usize, Error> {
        let now = self.clock.now();
        let mut b = Batch::new(self.load().await?, now);
        let Some(item) = b.snap.item(item_id).cloned() else {
            return Ok(0);
        };
        if item.status != Status::Ready {
            return Ok(0);
        }
        let need = worker_need(&item);
        let until = now + self.config.worker_wait;
        let mut state = ladder_from(events, &item);
        b.rung(
            &item.id,
            &mut state,
            RungEvent {
                rung: Rung::Request,
                at: now,
                error: Some(worker_gap(&item)),
                outcome: format!(
                    "{WORKER_WAIT}: none healthy can take it; the ladder climbs at {}",
                    until.format("%Y-%m-%d %H:%M UTC")
                ),
            },
        );
        let changed = b.move_to(
            &item.id,
            Status::Blocked(BlockedReason::WorkerUnavailable),
            "controller",
            format!("no healthy worker can take it ({need}); waiting for the registry"),
        );
        b.settle(changed);
        let written = self.commit(b, &mut HashSet::new()).await?;
        Ok(written.map(|w| w.transitions).unwrap_or(0))
    }

    /// The sweep's half: an item waiting for a worker resumes when one can
    /// take it, and climbs its ladder once the wait is past its TTL.
    /// `waiting` is the questions still waiting, for the question a
    /// surfaced wait left open.
    pub(super) async fn worker_waits(
        &self,
        b: &mut Batch,
        waiting: &[QuestionRow],
    ) -> Result<Vec<WorkItemId>, Error> {
        let parked: Vec<WorkItem> = b
            .snap
            .items()
            .iter()
            .filter(|i| {
                i.status == Status::Blocked(BlockedReason::WorkerUnavailable)
                    && i.held_by.is_none()
                    && !b.snap.has_children(&i.id)
                    && !behind_capability(&b.snap, &i.id)
            })
            .cloned()
            .collect();
        let mut changed = Vec::new();
        for item in parked {
            let events = self.store.work_events(&item.id).await?;
            let Some((wait, still_waiting)) = the_wait(&events, &item) else {
                continue;
            };
            if self.has_alternative(&item, None, &history(&events).excluded, None) {
                for q in waiting
                    .iter()
                    .filter(|q| q.item == item.id && q.status == QuestionStatus::Open)
                {
                    b.ops.push(WorkOp::Question(QuestionWrite::Settle {
                        id: q.id.clone(),
                        status: QuestionStatus::Obsolete,
                        answer: None,
                        by: Some("controller".to_string()),
                        decision: None,
                        at: b.now,
                    }));
                }
                changed.extend(b.move_to(
                    &item.id,
                    Status::Queued,
                    "controller",
                    "a healthy worker can take it now",
                ));
                continue;
            }
            // Surfaced already: it waits for the user, or for a worker.
            if !still_waiting || b.now < wait.at + self.config.worker_wait {
                continue;
            }
            let Some(error) = wait.error.clone() else {
                continue;
            };
            changed.extend(self.climb(b, &item, "registry", error).await?);
        }
        Ok(changed)
    }
}
