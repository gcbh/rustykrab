//! Tracking when the system was last busy, so background work can run only
//! when nothing else needs the machine (see `DREAMING.md`).
//!
//! The outer loop is the lowest-priority activity in the process. It must
//! run only in downtime and must get out of the way the instant real work
//! arrives. Both halves of that need the same fact — when was the last
//! inbound message — so both are served from here.
//!
//! Preemption is expressed as a **generation counter** rather than a
//! channel. A worker snapshots the counter before a unit of work and
//! compares afterwards; a change means activity arrived and the work should
//! be abandoned. That keeps the signal pull-based and cheap: no
//! subscription to leak, no receiver to keep alive, and a worker that is
//! not running costs nothing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

/// Agents tracked at once. Bounds the map for a process that may serve many
/// agents over a long life.
const MAX_TRACKED_AGENTS: usize = 256;

struct Inner {
    /// Last inbound activity per agent.
    last: HashMap<Uuid, Instant>,
    /// When tracking began, used as the baseline for an agent that has not
    /// been seen at all.
    started_at: Instant,
}

/// Records when each agent last saw inbound activity.
///
/// Cheap to clone; all clones share one table and one generation counter.
#[derive(Clone)]
pub struct ActivityTracker {
    inner: Arc<Mutex<Inner>>,
    /// Bumped on every recorded activity. Workers compare snapshots of this
    /// to decide whether to yield.
    generation: Arc<AtomicU64>,
}

impl ActivityTracker {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                last: HashMap::new(),
                started_at: Instant::now(),
            })),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Note that `agent_id` just saw inbound activity.
    ///
    /// Called on the inbound path of every channel, so it must stay cheap:
    /// one lock, one insert, one atomic increment.
    pub fn record(&self, agent_id: Uuid) {
        {
            let mut inner = self.lock();
            if inner.last.len() >= MAX_TRACKED_AGENTS && !inner.last.contains_key(&agent_id) {
                // Evict the least recently active agent — the one whose
                // entry matters least to an idleness question.
                if let Some(stalest) = inner
                    .last
                    .iter()
                    .min_by_key(|(_, seen)| **seen)
                    .map(|(id, _)| *id)
                {
                    inner.last.remove(&stalest);
                }
            }
            inner.last.insert(agent_id, Instant::now());
        }
        // Bumped after the table is updated and the lock released, so a
        // worker that observes the new generation also observes the new
        // timestamp.
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// How long `agent_id` has been quiet.
    ///
    /// An agent that has never been seen reports the time since tracking
    /// began rather than `None`: a daemon that has served no traffic at all
    /// is the *most* idle it can be, not an unknown.
    pub fn idle_for(&self, agent_id: Uuid) -> Duration {
        let inner = self.lock();
        let since = inner
            .last
            .get(&agent_id)
            .copied()
            .unwrap_or(inner.started_at);
        since.elapsed()
    }

    /// Whether `agent_id` has been quiet for at least `threshold`.
    pub fn is_idle(&self, agent_id: Uuid, threshold: Duration) -> bool {
        self.idle_for(agent_id) >= threshold
    }

    /// Current activity generation.
    ///
    /// Snapshot this before a unit of background work and compare after;
    /// a difference means activity arrived while the work was running.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Whether activity has been recorded since `generation` was taken.
    pub fn changed_since(&self, generation: u64) -> bool {
        self.generation() != generation
    }

    /// Number of agents currently tracked.
    pub fn tracked_agents(&self) -> usize {
        self.lock().last.len()
    }
}

impl Default for ActivityTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ActivityTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivityTracker")
            .field("agents", &self.tracked_agents())
            .field("generation", &self.generation())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unseen_agent_is_idle_rather_than_unknown() {
        // A daemon that has served nothing is maximally idle. Reporting
        // "unknown" would make the worker's gate undecidable at startup,
        // which is exactly when there is most opportunity to do work.
        let tracker = ActivityTracker::new();
        assert!(tracker.is_idle(Uuid::new_v4(), Duration::ZERO));
    }

    #[test]
    fn recording_activity_resets_idleness() {
        let tracker = ActivityTracker::new();
        let agent = Uuid::new_v4();

        tracker.record(agent);
        assert!(!tracker.is_idle(agent, Duration::from_secs(60)));
        assert!(tracker.idle_for(agent) < Duration::from_secs(1));
    }

    #[test]
    fn agents_are_tracked_independently() {
        let tracker = ActivityTracker::new();
        let busy = Uuid::new_v4();
        let quiet = Uuid::new_v4();

        tracker.record(busy);

        assert!(!tracker.is_idle(busy, Duration::from_secs(60)));
        // The quiet agent falls back to the tracking baseline, so a busy
        // neighbour does not make it look busy.
        assert!(tracker.is_idle(quiet, Duration::ZERO));
    }

    #[test]
    fn generation_advances_on_every_activity() {
        let tracker = ActivityTracker::new();
        let agent = Uuid::new_v4();

        let before = tracker.generation();
        assert!(!tracker.changed_since(before));

        tracker.record(agent);
        assert!(tracker.changed_since(before));

        let mid = tracker.generation();
        tracker.record(agent);
        assert!(tracker.changed_since(mid));
    }

    #[test]
    fn generation_is_shared_across_clones() {
        // The worker holds a clone; the channels hold another. A bump on
        // one must be visible to the other or preemption silently stops
        // working.
        let tracker = ActivityTracker::new();
        let worker_view = tracker.clone();

        let snapshot = worker_view.generation();
        tracker.record(Uuid::new_v4());

        assert!(worker_view.changed_since(snapshot));
    }

    #[test]
    fn any_agent_activity_preempts() {
        // Preemption is deliberately not per-agent: the resources a
        // background job competes for (the connection, model quota) are
        // shared, so traffic for any agent is reason enough to yield.
        let tracker = ActivityTracker::new();
        let snapshot = tracker.generation();

        tracker.record(Uuid::new_v4());

        assert!(tracker.changed_since(snapshot));
    }

    #[test]
    fn tracked_agents_are_bounded() {
        let tracker = ActivityTracker::new();
        for _ in 0..MAX_TRACKED_AGENTS + 32 {
            tracker.record(Uuid::new_v4());
        }
        assert!(tracker.tracked_agents() <= MAX_TRACKED_AGENTS);
    }
}
