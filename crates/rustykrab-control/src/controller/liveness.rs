//! What keeps the controller observable and its runs' leases alive while a
//! tick is slow: the phase a tick is in (read from memory by
//! [`crate::handle::LoopStatus`]), and a heartbeat per run that renews the
//! run's lease from the worker's own activity without waiting for a tick.
//!
//! A tick is a long serial chain of store round-trips and Git subprocesses.
//! Under memory pressure each step slows by orders of magnitude, so a tick
//! can take minutes. Nothing here takes `loop_lock`: the phase record is
//! in-memory state the tick writes, and the heartbeat task reads only the
//! worker's activity counter and writes only the lease's `heartbeat_at`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rustykrab_core::work::WorkItemId;
use rustykrab_store::Store;
use tokio::task::AbortHandle;

use super::{Clock, Controller};
use crate::worker::Worker;

/// A phase slower than this is logged as a warning when it ends.
pub(super) const SLOW_PHASE: Duration = Duration::from_secs(10);
/// A whole tick slower than this is logged as a warning when it ends.
pub(super) const SLOW_TICK: Duration = Duration::from_secs(30);

/// The tick in progress: when it started, the phase it is in, and how long
/// each phase it has left took so far.
pub(super) struct TickProgress {
    /// Which tick owns this record: only it moves its phase or ends it.
    id: u64,
    /// On the controller's clock, as `last_tick` is.
    started: DateTime<Utc>,
    began: Instant,
    phase: &'static str,
    phase_began: Instant,
    /// Total time per phase name, in first-entered order.
    spent: Vec<(&'static str, Duration)>,
}

/// What the last tick that ended (completed or failed) took.
#[derive(Debug, Clone, Copy)]
pub(super) struct TickTiming {
    pub(super) total: Duration,
    pub(super) slowest: Option<(&'static str, Duration)>,
}

impl TickProgress {
    fn close_phase(&mut self) {
        let took = self.phase_began.elapsed();
        if took > SLOW_PHASE {
            tracing::warn!(
                phase = self.phase,
                duration_ms = millis(took),
                "controller tick phase was slow"
            );
        }
        match self.spent.iter_mut().find(|(name, _)| *name == self.phase) {
            Some((_, total)) => *total += took,
            None => self.spent.push((self.phase, took)),
        }
    }
}

pub(super) fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

tokio::task_local! {
    /// The id of the timed tick this task is running, if any.
    pub(super) static TICK: u64;
}

static NEXT_TICK: AtomicU64 = AtomicU64::new(1);

/// Times one tick from [`TickTimer::begin`] until it is dropped, so a tick
/// that fails, or whose future is dropped, still ends its record. A tick
/// that begins while another is being timed (a manual `work tick` beside
/// the loop's) is not timed at first: the record stays the first tick's,
/// and only the timer that created it moves or ends it. Once the later tick
/// holds `loop_lock` it begins a timer again, which claims the record if it
/// is free (`Controller::tick`).
pub(super) struct TickTimer<'a> {
    controller: &'a Controller,
    id: Option<u64>,
}

impl<'a> TickTimer<'a> {
    pub(super) fn begin(controller: &'a Controller, phase: &'static str) -> Self {
        let id = controller.begin_tick(phase);
        TickTimer { controller, id }
    }

    /// The record's id when this tick owns it: run the tick inside
    /// [`TICK`]`.scope(id, ..)` so its phases land on its own record.
    pub(super) fn id(&self) -> Option<u64> {
        self.id
    }
}

impl Drop for TickTimer<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.controller.end_tick(id);
        }
    }
}

impl Controller {
    /// A tick begins in `phase`; its record's id, or `None` when another
    /// tick is being timed.
    fn begin_tick(&self, phase: &'static str) -> Option<u64> {
        let mut state = self.state();
        if state.tick.is_some() {
            return None;
        }
        let now = Instant::now();
        let id = NEXT_TICK.fetch_add(1, Ordering::Relaxed);
        state.tick = Some(TickProgress {
            id,
            started: self.clock.now(),
            began: now,
            phase,
            phase_began: now,
            spent: Vec::new(),
        });
        Some(id)
    }

    /// The timed tick this task runs moves to `phase`. Nothing when this
    /// task runs no timed tick (an untimed concurrent tick, a test driving
    /// a pass directly).
    pub(super) fn phase(&self, phase: &'static str) {
        let Ok(id) = TICK.try_with(|id| *id) else {
            return;
        };
        let mut state = self.state();
        if let Some(tick) = state.tick.as_mut().filter(|t| t.id == id) {
            if tick.phase == phase {
                return;
            }
            tick.close_phase();
            tick.phase = phase;
            tick.phase_began = Instant::now();
        }
    }

    /// The tick in progress ended, completed or failed: its duration and
    /// slowest phase become the previous tick's.
    fn end_tick(&self, id: u64) {
        let mut state = self.state();
        if state.tick.as_ref().is_none_or(|t| t.id != id) {
            return;
        }
        let Some(mut tick) = state.tick.take() else {
            return;
        };
        tick.close_phase();
        let total = tick.began.elapsed();
        let slowest = tick.spent.iter().copied().max_by_key(|(_, took)| *took);
        if total > SLOW_TICK {
            tracing::warn!(
                duration_ms = millis(total),
                slowest_phase = slowest.map(|(name, _)| name),
                slowest_phase_ms = slowest.map(|(_, took)| millis(took)),
                "controller tick was slow"
            );
        }
        state.last_timing = Some(TickTiming { total, slowest });
    }

    /// The tick in progress, for [`crate::handle::LoopStatus`]: its start
    /// and current phase.
    pub(super) fn tick_in_progress(
        state: &super::State,
    ) -> (Option<DateTime<Utc>>, Option<&'static str>) {
        match &state.tick {
            Some(tick) => (Some(tick.started), Some(tick.phase)),
            None => (None, None),
        }
    }
}

/// A run's last observed activity, shared by the run's heartbeat task and
/// the tick's `observe_execution`. Whichever sees a new value first records
/// it and writes the one heartbeat for it; the other sees no change. The
/// stall judgment stays the tick's: it reads `changed` as the run's last
/// sign of life.
#[derive(Debug, Default)]
pub(super) struct Liveness {
    inner: Mutex<Seen>,
    /// The run handed in its result: as `observe_execution` does, the
    /// heartbeat task renews nothing more for it.
    reported: AtomicBool,
}

#[derive(Debug, Default, Clone, Copy)]
struct Seen {
    activity: Option<u64>,
    changed: Option<DateTime<Utc>>,
}

impl Liveness {
    /// Record `activity` read at `now`. True when it is new adapter
    /// activity (a positive count other than the last one seen): the caller
    /// then renews the lease.
    pub(super) fn observe(&self, activity: Option<u64>, now: DateTime<Utc>) -> bool {
        let mut seen = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if activity.is_some_and(|n| n > 0) && activity != seen.activity {
            seen.activity = activity;
            seen.changed = Some(now);
            true
        } else {
            false
        }
    }

    /// The run handed in its result (`accept_report`).
    pub(super) fn reported(&self) {
        self.reported.store(true, Ordering::SeqCst);
    }

    fn is_reported(&self) -> bool {
        self.reported.load(Ordering::SeqCst)
    }

    /// When activity last changed, on the controller's clock.
    pub(super) fn changed(&self) -> Option<DateTime<Utc>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).changed
    }
}

/// Aborts the heartbeat task when the run it belongs to is dropped: the run
/// was reconciled, stopped or cancelled, or the controller went away.
pub(super) struct HeartbeatGuard(Option<AbortHandle>);

impl HeartbeatGuard {
    /// No heartbeat task (yet).
    pub(super) fn none() -> Self {
        HeartbeatGuard(None)
    }
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

/// How often a run's heartbeat task reads its worker's activity: a quarter
/// of the lease TTL, between 100 ms and 15 s. Reading activity is an
/// in-memory counter; the store is written only when it moved.
pub(super) fn heartbeat_interval(lease_ttl_seconds: u64) -> Duration {
    Duration::from_millis(lease_ttl_seconds.saturating_mul(250))
        .clamp(Duration::from_millis(100), Duration::from_secs(15))
}

/// What a run's heartbeat task needs, none of it behind `loop_lock`.
pub(super) struct Heartbeat {
    pub(super) item: WorkItemId,
    pub(super) run_id: String,
    pub(super) worker: Arc<dyn Worker>,
    pub(super) store: Store,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) liveness: Arc<Liveness>,
    pub(super) every: Duration,
    /// The run's own task: the heartbeat stops when it finishes.
    pub(super) run: AbortHandle,
}

impl Heartbeat {
    /// Renew the lease whenever the worker's activity moves, until the run
    /// ends or its lease is gone.
    ///
    /// The write goes through the shared store connection, in a task of its
    /// own, and the loop never waits for it: while one write is still queued
    /// behind other store work, the loop keeps watching the run (so it ends
    /// with it) and queues no second write, then picks up the result and
    /// renews again on the next interval with activity. A heartbeat is late
    /// at worst, never stuck.
    ///
    /// It deliberately has no connection of its own, although the store is
    /// in WAL mode. A second connection that writes would make the shared
    /// connection's deferred transactions (each reads, then writes: every
    /// `work_apply`) fail with `SQLITE_BUSY` at once whenever a heartbeat
    /// committed between their read and their write; SQLite's busy timeout
    /// does not apply to that case. Taking the long readers off the shared
    /// connection ([`rustykrab_store::Store::observer`]) is what keeps the
    /// wait short.
    pub(super) fn spawn(self) -> HeartbeatGuard {
        let task = tokio::spawn(async move {
            let mut every = tokio::time::interval(self.every);
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            every.tick().await;
            let mut pending: Option<tokio::task::JoinHandle<_>> = None;
            loop {
                every.tick().await;
                if self.run.is_finished() || self.liveness.is_reported() {
                    if let Some(write) = pending {
                        write.abort();
                    }
                    return;
                }
                if let Some(write) = pending.take() {
                    if !write.is_finished() {
                        // Still waiting for the connection: observe the
                        // activity once it has landed, not now.
                        pending = Some(write);
                        continue;
                    }
                    match write.await {
                        Ok(Err(rustykrab_store::WorkStoreError::NotFound(_))) => {
                            // Released: the run was reconciled, stopped or
                            // cancelled. Nothing left to renew.
                            return;
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(item = %self.item, error = %e, "run heartbeat not recorded");
                        }
                        Ok(Ok(_)) | Err(_) => {}
                    }
                }
                let activity = self.worker.activity(&self.run_id);
                if !self.liveness.observe(activity, self.clock.now()) {
                    continue;
                }
                let (store, item) = (self.store.clone(), self.item.clone());
                pending = Some(tokio::spawn(async move {
                    store.work_lease_heartbeat(&item).await
                }));
            }
        });
        HeartbeatGuard(Some(task.abort_handle()))
    }
}

#[cfg(test)]
#[path = "tests/liveness.rs"]
mod tests;
