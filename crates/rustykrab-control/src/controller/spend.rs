//! Observed execution usage, retained across attempts and restarts.
//! No estimated token, iteration or task wall-time allowance gates work.
use super::{batch::Batch, Controller, Run};
use crate::worker::RUN_LOG;
use chrono::Utc;
use rustykrab_core::questions::{QuestionClass, QuestionKind, QuestionStatus};
use rustykrab_core::work::{BlockedReason, EventKind, Evidence, Status, WorkEvent};
use rustykrab_core::Error;
use rustykrab_store::{AnswerSubmissionStatus, QuestionWrite, RunSpend, Spend, WorkOp};
use std::collections::HashSet;

/// Reserved historical evidence; never accepted as a worker-authored artifact.
pub(super) const BUDGET_GRANT: &str = "budget_grant";
/// Historical failure provenance retained for recurrence analysis.
pub(super) const CUMULATIVE_GUARD: &str = "cumulative_budget_guard";

impl Controller {
    /// Keep durable usage available for observations and outcome analysis.
    pub(super) async fn ensure_spent(&self) -> Result<(), Error> {
        if self.state().spent.is_some() {
            return Ok(());
        }
        let totals = self.store.work_spend_totals().await?;
        let mut state = self.state();
        if state.spent.is_none() {
            state.spent = Some(totals);
        }
        Ok(())
    }

    /// Retire only the removed execution-budget gate. Independent consent,
    /// questions, dependencies, cancellation and historical spend stay intact.
    pub(super) async fn retire_execution_budget_holds(&self) -> Result<(), Error> {
        let mut b = Batch::new(self.load().await?, self.clock.now());
        let mut changed = self.retire_budget_approval_holds(&mut b).await?;
        let released: HashSet<_> = changed.iter().cloned().collect();
        // The working transaction already retires these questions, even though
        // store reads below still see their pre-commit open status.
        let obsolete: HashSet<_> = b
            .ops
            .iter()
            .filter_map(|op| match op {
                WorkOp::Question(QuestionWrite::Settle {
                    id,
                    status: QuestionStatus::Obsolete,
                    ..
                }) => Some(id.clone()),
                _ => None,
            })
            .collect();
        let items: Vec<_> = b
            .snap
            .items()
            .iter()
            .filter(|i| {
                (i.status == Status::Blocked(BlockedReason::BudgetExhausted)
                    || released.contains(&i.id))
                    && !b.snap.has_children(&i.id)
            })
            .cloned()
            .collect();
        for item in items {
            let questions = self.questions_of(&item.id).await?;
            // An accepted cancellation must retain its original stopped-state
            // semantics until applied. A rejected receipt is history, not a hold.
            if questions.iter().any(|q| {
                q.rule == "surfaced"
                    && q.text.starts_with("Its budget is spent (budget/")
                    && q.answer_submission
                        .as_ref()
                        .is_some_and(|a| a.status == AnswerSubmissionStatus::Pending)
            }) {
                continue;
            }
            let retired: HashSet<_> = questions
                .iter()
                .filter(|q| {
                    q.status == QuestionStatus::Open
                        && q.asked_by == "controller"
                        && q.rule == "surfaced"
                        && q.text.starts_with("Its budget is spent (budget/")
                        && q.answer_submission
                            .as_ref()
                            .is_none_or(|a| a.status == AnswerSubmissionStatus::Rejected)
                })
                .map(|q| q.id.clone())
                .collect();
            for q in questions.iter().filter(|q| retired.contains(&q.id)) {
                b.ops.push(WorkOp::Question(QuestionWrite::Settle {
                    id: q.id.clone(),
                    status: QuestionStatus::Obsolete,
                    answer: None,
                    by: None,
                    decision: None,
                    at: b.now,
                }));
                b.note(&item.id, EventKind::Question, "controller",
                    format!("Retired execution-budget question {}; usage history retained, no owner answer supplied.", q.id));
            }
            if item.held_by.is_some() {
                changed.extend(b.move_to(
                    &item.id,
                    Status::Blocked(BlockedReason::NeedsConsent),
                    "controller",
                    "Execution budget retired; independent owner approval remains",
                ));
                continue;
            }
            let other = questions.iter().any(|q| {
                !retired.contains(&q.id)
                    && !obsolete.contains(&q.id)
                    && q.class == QuestionClass::BlockingNow
                    && matches!(q.status, QuestionStatus::Open | QuestionStatus::Researching)
            });
            let status = if other {
                Status::Blocked(BlockedReason::NeedsDecision)
            } else {
                Status::Queued
            };
            changed.extend(b.move_to(
                &item.id,
                status,
                "controller",
                "Execution budgets retired; observed usage retained; independent holds still apply",
            ));
        }
        b.settle(changed);
        self.commit(b, &mut HashSet::new()).await?;
        Ok(())
    }

    /// Old plan approvals are released only when the durable host-authored plan
    /// links this question and its exact sole trigger was an execution estimate.
    /// Mixed approval triggers, edited scopes and review proposals remain held.
    async fn retire_budget_approval_holds(&self, b: &mut Batch) -> Result<Vec<String>, Error> {
        let held: Vec<_> = b
            .snap
            .items()
            .iter()
            .filter(|i| i.held_by.is_some() && !i.status.is_closed())
            .cloned()
            .collect();
        let ids: HashSet<_> = held.iter().filter_map(|i| i.held_by.clone()).collect();
        let mut changed = Vec::new();
        for id in ids {
            let Some(q) = self.store.question_get(&id).await? else {
                continue;
            };
            if q.status != QuestionStatus::Open
                || q.kind != QuestionKind::Approval
                || q.rule != "approval"
                || q.answer_submission
                    .as_ref()
                    .is_some_and(|a| a.status != AnswerSubmissionStatus::Rejected)
            {
                continue;
            }
            let Some(root) = b.snap.item(&q.item) else {
                continue;
            };
            let prefix = format!("Approve the plan \"{}\" (", root.title.trim());
            let Some(rest) = q.text.strip_prefix(&prefix) else {
                continue;
            };
            let Some((trigger, count)) = rest.split_once(")? ") else {
                continue;
            };
            let Some((tokens, cap)) = trigger.split_once(" tokens, over the ") else {
                continue;
            };
            let Some(cap) = cap.strip_suffix(" the policy allows") else {
                continue;
            };
            let Some(count) = count.strip_suffix(" item(s) held.") else {
                continue;
            };
            if tokens.parse::<u64>().is_err()
                || cap.parse::<u64>().is_err()
                || count.parse::<usize>().is_err()
            {
                continue;
            }
            let members: Vec<_> = held
                .iter()
                .filter(|i| i.held_by.as_deref() == Some(&id))
                .collect();
            let mut proven = false;
            for item in &members {
                if let Some(plan) = &item.plan_id {
                    proven |= self.store.work_plan_get(plan).await?.is_some_and(|p| {
                        p.approval_question.as_deref() == Some(&id) && p.root == q.item
                    });
                }
            }
            if !proven {
                continue;
            }
            b.ops.push(WorkOp::Question(QuestionWrite::Settle {
                id: id.clone(),
                status: QuestionStatus::Obsolete,
                answer: None,
                by: None,
                decision: None,
                at: b.now,
            }));
            for item in members {
                if let Some(row) = b.snap.item_mut(&item.id) {
                    row.held_by = None;
                }
                b.ops.push(WorkOp::ReleaseHold(item.id.clone()));
                let independent = self.questions_of(&item.id).await?.iter().any(|other| {
                    other.id != id
                        && other.class == QuestionClass::BlockingNow
                        && matches!(
                            other.status,
                            QuestionStatus::Open | QuestionStatus::Researching
                        )
                });
                if item.status == Status::Blocked(BlockedReason::NeedsConsent) {
                    let status = if independent {
                        Status::Blocked(BlockedReason::NeedsDecision)
                    } else {
                        Status::Queued
                    };
                    b.own(
                        &item.id,
                        status,
                        "controller",
                        "Retired sole execution-budget approval; no owner decision supplied",
                        None,
                    );
                }
                changed.push(item.id.clone());
            }
        }
        Ok(changed)
    }

    /// Record a run that has ended or been stopped: its spend, from the
    /// worker's own numbers when it keeps them, else the controller's wall
    /// time; and a `run` event on the item with the worker's counts. Best
    /// effort: a failed write is logged, never fails the tick.
    pub(super) async fn record_run(&self, item: &str, run: &Run, how: &str) {
        // Where the run's whole session is, for whoever looks into it
        // later: the next attempt, or an investigation of its failure.
        if let Some(path) = self
            .worker(&run.worker)
            .and_then(|w| w.run_log(&run.run_id))
        {
            let log = Evidence {
                item: item.to_string(),
                kind: RUN_LOG.to_string(),
                reference: path.display().to_string(),
                hash: Some(run.run_id.clone()),
                verified_by: Some(format!("worker:{}", run.worker)),
                at: Utc::now(),
            };
            if let Err(e) = self.store.work_evidence_add(log).await {
                tracing::warn!(item = %item, error = %e, "run log not recorded");
            }
        }
        let usage = self.worker(&run.worker).and_then(|w| w.usage(&run.run_id));
        let wall_ms = match usage {
            Some(u) if u.wall_ms > 0 => u.wall_ms,
            _ => u64::try_from(run.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        let spend = RunSpend {
            item: item.to_string(),
            run: Some(run.run_id.clone()),
            worker: run.worker.clone(),
            tokens: usage.map_or(0, |u| u.tokens),
            wall_ms,
            iterations: usage.map_or(0, |u| u.iterations),
            at: Utc::now(),
        };
        // A rejected/unavailable provider with no token usage did no work.
        // Preserve its diagnostic event, but do not consume execution budgets.
        let charge = how != "resource_wait" || spend.tokens > 0;
        if charge {
            if let Err(e) = self.store.work_spend_record(spend.clone()).await {
                tracing::warn!(item = %item, error = %e, "run spend not recorded");
                return;
            }
            {
                let mut state = self.state();
                if let Some(spent) = state.spent.as_mut() {
                    let total = spent.entry(item.to_string()).or_default();
                    *total = total.plus(Spend {
                        runs: 1,
                        tokens: spend.tokens,
                        wall_ms: spend.wall_ms,
                        iterations: u64::from(spend.iterations),
                    });
                }
            }
        }
        let counts = match usage {
            Some(u) => format!(
                "{} iterations, {} completion reminders, {} tokens, {} ms",
                u.iterations, u.reminders, spend.tokens, spend.wall_ms
            ),
            None => format!("{} ms; the worker keeps no counts", spend.wall_ms),
        };
        let event = WorkEvent {
            item: item.to_string(),
            at: Utc::now(),
            kind: EventKind::Run,
            from: None,
            to: None,
            actor: format!("worker:{}", run.worker),
            reason: Some(format!("run {how}: {counts}")),
            upstream: None,
            origin: None,
            evidence_ref: Some(run.run_id.clone()),
        };
        if let Err(e) = self.store.work_event_append(&event).await {
            tracing::debug!(item = %item, error = %e, "run event not recorded");
        }
    }

    /// The spend totals as the controller holds them, for tests and views.
    #[cfg(test)]
    pub(super) fn spent_of(&self, item: &str) -> Spend {
        self.state()
            .spent
            .as_ref()
            .and_then(|s| s.get(item).copied())
            .unwrap_or_default()
    }
}
