//! What runs spend (plan sections 4.2, 4.6 and 13): each run's tokens and
//! wall time go to `work_spend` when it ends, its self-counts (iterations,
//! completion reminders) to a `run` event on its item, and a parent's
//! remaining budget is its budget less what its subtree actually spent.
//!
//! The totals are read once, on the first load, and kept current here as
//! runs end: the controller is the only writer of `work_spend`.

use std::collections::BTreeMap;

use chrono::Utc;
use rustykrab_core::work::{Budget, EventKind, WorkEvent, WorkItemId};
use rustykrab_core::Error;
use rustykrab_store::{RunSpend, Spend};

use crate::graph::Snapshot;

use super::{Controller, Run};

/// Milliseconds of wall time, rounded up to whole seconds.
fn seconds(wall_ms: u64) -> u64 {
    wall_ms.div_ceil(1000)
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
