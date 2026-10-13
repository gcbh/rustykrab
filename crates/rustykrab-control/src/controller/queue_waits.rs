//! Recover legacy availability holds. New unavailable work stays in the
//! ready queue: no lease, failure, ladder rung, question or timeout.

use crate::errors::{FailureInput, GapKind, ProviderProblem};
use crate::worker::{run_failure_input, Worker};
use chrono::TimeDelta;
use rustykrab_core::questions::QuestionStatus;
use rustykrab_core::work::{BlockedReason, Status, WorkItemId};
use rustykrab_store::{QuestionRow, QuestionWrite, WorkOp};

use super::batch::Batch;
use super::Controller;

impl Controller {
    pub(super) fn dispatch_available(&self, worker: &dyn Worker) -> bool {
        worker.healthy()
            && self
                .state()
                .availability_backoff
                .get(worker.name())
                .is_none_or(|until| self.clock.now() >= *until)
    }

    pub(super) fn availability_error(&self, worker: &str, error: &rustykrab_core::Error) -> bool {
        matches!(
            run_failure_input(error),
            FailureInput::Provider {
                problem: ProviderProblem::Unavailable,
                ..
            }
        ) || matches!(
            run_failure_input(error),
            FailureInput::CapabilityGap {
                gap: GapKind::Capacity,
                ..
            }
        ) || self.worker(worker).is_some_and(|worker| !worker.healthy())
    }

    pub(super) fn queue_after_unavailable(
        &self,
        b: &mut Batch,
        item: &str,
        worker: &str,
    ) -> Vec<WorkItemId> {
        // Avoid repeatedly contacting an overloaded/offline provider. Other
        // eligible resources may still take the queued work immediately.
        self.state()
            .availability_backoff
            .insert(worker.into(), b.now + TimeDelta::seconds(30));
        b.resume_to(
            item,
            Status::Ready,
            format!(
                "{worker} is unavailable; retained in the queue without a failure or retry charge"
            ),
        )
    }

    /// Older releases parked unavailable work and eventually asked the user.
    /// Move those open leaves back into the queue even while capacity is absent.
    /// Approval holds and other kinds of user decisions remain independent.
    pub(super) fn recover_worker_waits(
        &self,
        b: &mut Batch,
        waiting: &[QuestionRow],
    ) -> Vec<WorkItemId> {
        let parked: Vec<_> = b
            .snap
            .items()
            .iter()
            .filter(|item| {
                item.status == Status::Blocked(BlockedReason::WorkerUnavailable)
                    && item.held_by.is_none()
                    && !b.snap.has_children(&item.id)
            })
            .map(|item| item.id.clone())
            .collect();
        let mut changed = Vec::new();
        for id in parked {
            for question in waiting.iter().filter(|question| {
                question.item == id
                    && question.status == QuestionStatus::Open
                    && question.rule == "surfaced"
            }) {
                b.ops.push(WorkOp::Question(QuestionWrite::Settle {
                    id: question.id.clone(),
                    status: QuestionStatus::Obsolete,
                    answer: None,
                    by: Some("controller".into()),
                    decision: None,
                    at: b.now,
                }));
            }
            changed.extend(b.move_to(
                &id,
                Status::Queued,
                "controller",
                "availability waits belong in the queue; no user decision is required",
            ));
        }
        changed
    }
}
