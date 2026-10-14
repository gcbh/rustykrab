//! What runs spend (plan sections 4.2, 4.6 and 13): each run's tokens and
//! wall time go to `work_spend` when it ends, its self-counts (iterations,
//! completion reminders) to a `run` event on its item, and a parent's
//! remaining budget is its budget less what its subtree actually spent.
//!
//! A leaf's budget is cumulative across every attempt, restart and revival
//! ([`Controller::cumulative_budget`]): a run is briefed with what is left of
//! `budget × (1 + grants) − spent` in iterations, tokens and wall time, where
//! a grant is one `budget_grant` row an owner's answer recorded
//! ([`BUDGET_GRANT`]). An item with less left than [`exhausted`]'s floors is
//! not leased; it fails `budget/<dimension>` observed by
//! [`CUMULATIVE_GUARD`] and surfaces as `blocked(budget_exhausted)`.
//!
//! The totals are read once, on the first load, and kept current here as
//! runs end: the controller is the only writer of `work_spend`.

use std::collections::BTreeMap;

use chrono::Utc;
use rustykrab_core::work::{Budget, EventKind, Evidence, WorkEvent, WorkItem, WorkItemId};
use rustykrab_core::Error;
use rustykrab_store::{RunSpend, Spend};

use crate::errors::BudgetKind;
use crate::graph::Snapshot;
use crate::worker::RUN_LOG;

use super::{Controller, Run};

/// The evidence kind of one more allotment of an item's budget: recorded
/// when an owner's answer releases the item from `blocked(budget_exhausted)`
/// (`questions.rs`), at most one per question. Its reference is the
/// question's id and `verified_by` the actor who answered.
pub(super) const BUDGET_GRANT: &str = "budget_grant";

/// Who observed a cumulative budget running out: the controller, at lease
/// time, before anything ran. The `observed_by` of its `budget/*` error.
pub(super) const CUMULATIVE_GUARD: &str = "cumulative_budget_guard";

/// The least wall time worth starting a run with, in seconds: below it an
/// agent cannot read its brief and act. Capped at a tenth of one allotment,
/// so an item budgeted under ten minutes still gets most of its allotment.
const MIN_WALL_SECONDS: u64 = 60;

/// The least token budget worth starting a run with: below it a model
/// cannot read a brief. Capped at a tenth of one allotment, like the wall.
const MIN_TOKENS: u64 = 2_000;

/// Milliseconds of wall time, rounded up to whole seconds.
fn seconds(wall_ms: u64) -> u64 {
    wall_ms.div_ceil(1000)
}

/// `budget`'s iterations, tokens and wall time `allotments` times over.
/// Repairs and rung budgets are per item and pass through.
pub(super) fn scaled(budget: &Budget, allotments: u32) -> Budget {
    let n = u64::from(allotments.max(1));
    Budget {
        iterations: u32::try_from(u64::from(budget.iterations).saturating_mul(n))
            .unwrap_or(u32::MAX),
        tokens: budget.tokens.saturating_mul(n),
        wall_seconds: budget.wall_seconds.saturating_mul(n),
        ..*budget
    }
}

/// The floor below which `left` of one `allotment` is too little to start a
/// run with: a tenth of the allotment, at most `cap`, at least 1. A zero
/// allotment is unbounded in that dimension and has no floor.
fn floor(allotment: u64, cap: u64) -> Option<u64> {
    (allotment > 0).then(|| (allotment / 10).clamp(1, cap))
}

/// The first dimension whose cumulative budget is spent or too small to be
/// useful, with a sentence saying so, or `None` when a run may start.
/// `remaining` is what [`Controller::cumulative_budget`] left, `allotment` the
/// item's own budget, `allotments` how many it has had (1 + grants).
/// Floors: under one iteration, under a tenth of an allotment's tokens
/// (at most [`MIN_TOKENS`]) or wall time (at most [`MIN_WALL_SECONDS`]).
pub(super) fn exhausted(
    remaining: &Budget,
    allotment: &Budget,
    allotments: u32,
    spent: Spend,
) -> Option<(BudgetKind, String)> {
    let allotments = allotments.max(1);
    // (kind, unit, one allotment, what is left, what was spent)
    let dimensions = [
        (
            BudgetKind::Iterations,
            "iterations",
            u64::from(allotment.iterations),
            u64::from(remaining.iterations),
            spent.iterations,
            1,
        ),
        (
            BudgetKind::Tokens,
            "tokens",
            allotment.tokens,
            remaining.tokens,
            spent.tokens,
            MIN_TOKENS,
        ),
        (
            BudgetKind::Wall,
            "wall seconds",
            allotment.wall_seconds,
            remaining.wall_seconds,
            seconds(spent.wall_ms),
            MIN_WALL_SECONDS,
        ),
    ];
    dimensions
        .into_iter()
        .find_map(|(kind, unit, one, left, used, cap)| {
            let floor = floor(one, cap)?;
            (left < floor).then(|| {
                (
                    kind,
                    // The classifier puts `<dimension> budget spent: ` in
                    // front of this.
                    format!(
                        "{used} of {} {unit} used over {} run(s) ({allotments} allotment(s) of \
                         {one}, counted across every attempt); {left} left, under the {floor} a \
                         run needs",
                        one.saturating_mul(u64::from(allotments)),
                        spent.runs,
                    ),
                )
            })
        })
}

/// `budget` less `spent`, never below zero. Repairs and rung budgets are
/// per item and pass through.
pub(super) fn left(budget: &Budget, spent: Spend) -> Budget {
    Budget {
        iterations: u32::try_from(u64::from(budget.iterations).saturating_sub(spent.iterations))
            .unwrap_or(0),
        tokens: budget.tokens.saturating_sub(spent.tokens),
        wall_seconds: budget.wall_seconds.saturating_sub(seconds(spent.wall_ms)),
        ..*budget
    }
}

impl Controller {
    /// Read the spend totals once, on the first load.
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

    /// How many allotments of its budget `item` has had: one, and one per
    /// [`BUDGET_GRANT`] row an answer recorded.
    pub(super) async fn allotments(&self, item: &str) -> Result<u32, Error> {
        let grants = self
            .store
            .work_evidence_list(item)
            .await?
            .iter()
            .filter(|e| e.kind == BUDGET_GRANT)
            .count();
        Ok(u32::try_from(grants).unwrap_or(u32::MAX).saturating_add(1))
    }

    /// What `item` has left to spend over all its attempts (4.2 for a
    /// leaf): its budget `allotments` times over, less what every run of
    /// it spent, from the durable totals, so a restart or a revival does
    /// not refill it. Returned with what was spent.
    pub(super) fn cumulative_budget(&self, item: &WorkItem, allotments: u32) -> (Budget, Spend) {
        let spent = self
            .state()
            .spent
            .as_ref()
            .and_then(|s| s.get(&item.id).copied())
            .unwrap_or_default();
        (left(&scaled(&item.budget, allotments), spent), spent)
    }

    /// What each existing parent in `snap` has left: its budget less what
    /// every item under it spent. A parent that has spent nothing has its
    /// whole budget, however much its open children were allocated, so a
    /// follow-up under a busy parent is judged against actual spend (4.2).
    /// Empty until the totals are loaded, which leaves the validator's own
    /// fallback (the budget less its open children's).
    pub(super) fn remaining_budgets(&self, snap: &Snapshot) -> BTreeMap<WorkItemId, Budget> {
        let state = self.state();
        let Some(spent) = state.spent.as_ref() else {
            return BTreeMap::new();
        };
        snap.items()
            .iter()
            .filter(|i| snap.has_children(&i.id))
            .map(|parent| {
                let total = snap
                    .descendants(&parent.id)
                    .iter()
                    .chain(std::iter::once(&parent.id))
                    .filter_map(|id| spent.get(id).copied())
                    .fold(Spend::default(), Spend::plus);
                (parent.id.clone(), left(&parent.budget, total))
            })
            .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn allotment() -> Budget {
        Budget {
            iterations: 25,
            tokens: 200_000,
            wall_seconds: 3_600,
            ..Budget::default()
        }
    }

    fn spend(iterations: u64, tokens: u64, wall_seconds: u64) -> Spend {
        Spend {
            runs: 4,
            tokens,
            wall_ms: wall_seconds * 1_000,
            iterations,
        }
    }

    /// What is left of `allotments` of [`allotment`] after `spent`, and
    /// whether that stops the lease.
    fn check(allotments: u32, spent: Spend) -> Option<(BudgetKind, String)> {
        let remaining = left(&scaled(&allotment(), allotments), spent);
        exhausted(&remaining, &allotment(), allotments, spent)
    }

    #[test]
    fn a_grant_adds_one_allotment_in_every_dimension() {
        let two = scaled(&allotment(), 2);
        assert_eq!(
            (two.iterations, two.tokens, two.wall_seconds),
            (50, 400_000, 7_200)
        );
        assert_eq!(two.rungs, allotment().rungs, "rung budgets are per item");
        assert_eq!(scaled(&allotment(), 0), allotment(), "never less than one");
    }

    #[test]
    fn a_budget_with_room_left_leases() {
        assert_eq!(check(1, spend(24, 150_000, 3_000)), None);
    }

    #[test]
    fn a_spent_dimension_stops_the_lease_and_says_how() {
        let (kind, why) = check(1, spend(25, 1_000, 10)).expect("iterations spent");
        assert_eq!(kind, BudgetKind::Iterations);
        assert_eq!(
            why,
            "25 of 25 iterations used over 4 run(s) (1 allotment(s) of 25, counted across \
             every attempt); 0 left, under the 1 a run needs"
        );
        // Overspent by a run that ran past what it was given: still spent.
        assert_eq!(check(1, spend(31, 0, 0)).unwrap().0, BudgetKind::Iterations);
    }

    #[test]
    fn too_little_left_to_be_useful_stops_the_lease() {
        // Under a minute of wall time left of an hour.
        assert_eq!(
            check(1, spend(1, 0, 3_541)).unwrap().0,
            BudgetKind::Wall,
            "59 s left"
        );
        assert_eq!(check(1, spend(1, 0, 3_540)), None, "60 s left");
        // Under 2,000 tokens left.
        assert_eq!(
            check(1, spend(1, 198_001, 0)).unwrap().0,
            BudgetKind::Tokens
        );
        // A grant lifts every floor.
        assert_eq!(check(2, spend(25, 198_001, 3_541)), None);
    }

    #[test]
    fn a_small_allotment_has_a_floor_in_proportion() {
        let small = Budget {
            iterations: 3,
            tokens: 5_000,
            wall_seconds: 120,
            ..Budget::default()
        };
        let at = |spent: Spend| exhausted(&left(&small, spent), &small, 1, spent);
        // A tenth of 120 s is 12 s, a tenth of 5,000 tokens is 500.
        assert_eq!(at(spend(0, 0, 108)), None);
        assert_eq!(at(spend(0, 0, 109)).unwrap().0, BudgetKind::Wall);
        assert_eq!(at(spend(0, 4_500, 0)), None);
        assert_eq!(at(spend(0, 4_501, 0)).unwrap().0, BudgetKind::Tokens);
    }

    #[test]
    fn a_zero_dimension_is_unbounded() {
        let open = Budget {
            iterations: 0,
            tokens: 0,
            wall_seconds: 0,
            ..Budget::default()
        };
        let spent = spend(100, 1_000_000, 100_000);
        assert_eq!(exhausted(&left(&open, spent), &open, 1, spent), None);
    }
}
