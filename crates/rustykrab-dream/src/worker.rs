//! The downtime worker that runs analysis when nothing else needs the
//! machine (see `DREAMING.md`).
//!
//! Three properties define it, in priority order:
//!
//! 1. **Off-cycle.** It runs only after the system has been quiet for a
//!    while. It is never invoked on the session-end path, where it would
//!    add latency exactly when a follow-up is most likely.
//! 2. **Yields immediately.** If activity arrives mid-pass, the pass is
//!    abandoned rather than finished. The work is cheap and idempotent, so
//!    throwing it away and redoing it later costs less than making a user
//!    wait behind it.
//! 3. **Cannot affect correctness.** It reads; it never writes. A failed
//!    pass is logged and forgotten.
//!
//! Yielding is *abort-and-retry*, not pause-and-resume. A pass is a
//! handful of aggregate queries, so there is no partial progress worth the
//! machinery of persisting it. That trade is revisited only if a pass ever
//! grows long enough that discarding it hurts.

use std::sync::Arc;
use std::time::Duration;

use rustykrab_core::activity::ActivityTracker;
use rustykrab_core::Result;
use uuid::Uuid;

use crate::report::{analyze, AnalysisReport, OutcomeSource};

/// How the worker paces itself.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// How quiet the system must be before a pass may start.
    pub idle_threshold: Duration,
    /// How often to reconsider whether it is quiet enough.
    pub poll_interval: Duration,
    /// Agent whose idleness gates the work.
    pub agent_id: Uuid,
}

impl WorkerConfig {
    /// Defaults chosen to be unobtrusive rather than prompt: analysis has
    /// no deadline, so waiting longer costs nothing and reduces the chance
    /// of colliding with a user.
    pub fn new(agent_id: Uuid) -> Self {
        Self {
            idle_threshold: Duration::from_secs(600),
            poll_interval: Duration::from_secs(120),
            agent_id,
        }
    }

    pub fn with_idle_threshold(mut self, threshold: Duration) -> Self {
        self.idle_threshold = threshold;
        self
    }

    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }
}

/// Why a pass produced no report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassOutcome {
    /// The system was not quiet enough to start.
    NotIdle,
    /// Activity arrived while the pass was running, so it was abandoned.
    Preempted,
    /// The pass ran and produced a report.
    Completed,
    /// The pass failed; the error was logged and discarded.
    Failed,
}

/// Where a completed report is kept.
///
/// The phase gate in `DREAMING.md` is "reports show real, actionable
/// patterns". A report that exists only as a log line cannot answer that
/// — it is gone at the next rotation, and nothing but a human with `grep`
/// can tell whether the loop has been running at all. Persisting the pass
/// turns the gate into something a person can look up.
///
/// Best-effort by contract, like the rest of this crate: failing to record
/// an observation must never be worse than not observing.
#[async_trait::async_trait]
pub trait ReportSink: Send + Sync {
    async fn record_report(&self, report: &AnalysisReport) -> Result<()>;
}

/// Runs read-only analysis during downtime.
pub struct DreamWorker {
    source: Arc<dyn OutcomeSource>,
    activity: ActivityTracker,
    config: WorkerConfig,
    /// Where completed passes are kept. `None` logs and forgets, which is
    /// the old behaviour and fine for a test.
    sink: Option<Arc<dyn ReportSink>>,
}

impl DreamWorker {
    pub fn new(
        source: Arc<dyn OutcomeSource>,
        activity: ActivityTracker,
        config: WorkerConfig,
    ) -> Self {
        Self {
            source,
            activity,
            config,
            sink: None,
        }
    }

    /// Keep completed passes, so the phase gate can be read rather than
    /// inferred from logs.
    pub fn with_report_sink(mut self, sink: Arc<dyn ReportSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Attempt one pass.
    ///
    /// Returns the report only when the system stayed quiet from start to
    /// finish. A report computed across a burst of activity is discarded
    /// rather than returned: it competed with a user for the store's single
    /// connection, and finishing it would extend that contention.
    pub async fn run_once(&self) -> (PassOutcome, Option<AnalysisReport>) {
        if !self
            .activity
            .is_idle(self.config.agent_id, self.config.idle_threshold)
        {
            return (PassOutcome::NotIdle, None);
        }

        let generation = self.activity.generation();

        let report = match analyze(self.source.as_ref()).await {
            Ok(report) => report,
            Err(e) => {
                // Analysis is advisory, so a failure is worth a line in the
                // log and nothing more.
                tracing::warn!(error = %e, "outcome analysis failed");
                return (PassOutcome::Failed, None);
            }
        };

        if self.activity.changed_since(generation) {
            tracing::debug!("outcome analysis preempted by activity; discarding pass");
            return (PassOutcome::Preempted, None);
        }

        // Persisted before it is logged: the log line is the convenience
        // copy, the row is the record.
        if let Some(sink) = self.sink.as_ref() {
            if let Err(e) = sink.record_report(&report).await {
                // Analysis is advisory and so is keeping it. Losing a pass
                // must not take down the worker that produced it.
                tracing::warn!(error = %e, "could not persist outcome analysis");
            }
        }

        tracing::info!("\n{}", report.summary());
        (PassOutcome::Completed, Some(report))
    }

    /// Poll forever, running a pass whenever the system is quiet enough.
    ///
    /// Intended to be spawned and later aborted, matching how the daemon's
    /// other background tasks are shut down.
    pub async fn run(self) {
        tracing::info!(
            idle_threshold_secs = self.config.idle_threshold.as_secs(),
            poll_interval_secs = self.config.poll_interval.as_secs(),
            "outcome analysis worker started"
        );
        loop {
            tokio::time::sleep(self.config.poll_interval).await;
            let (outcome, _) = self.run_once().await;
            tracing::trace!(?outcome, "outcome analysis pass");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::OutcomeSource;
    use rustykrab_core::outcome::{AttributionKind, OutcomeTally};
    use rustykrab_core::Result;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    /// Source that counts how often it was queried, and can bump an
    /// activity tracker mid-query to simulate a user arriving.
    struct ProbeSource {
        queries: AtomicU32,
        interrupt: Option<(ActivityTracker, Uuid)>,
    }

    impl ProbeSource {
        fn new() -> Self {
            Self {
                queries: AtomicU32::new(0),
                interrupt: None,
            }
        }

        fn interrupting(activity: ActivityTracker, agent: Uuid) -> Self {
            Self {
                queries: AtomicU32::new(0),
                interrupt: Some((activity, agent)),
            }
        }
    }

    #[async_trait::async_trait]
    impl OutcomeSource for ProbeSource {
        async fn tallies(
            &self,
            _kind: AttributionKind,
            _ground_truth_only: bool,
        ) -> Result<Vec<(String, OutcomeTally)>> {
            self.queries.fetch_add(1, Ordering::SeqCst);
            if let Some((activity, agent)) = &self.interrupt {
                activity.record(*agent);
            }
            Ok(Vec::new())
        }

        async fn total_records(&self) -> Result<u32> {
            Ok(0)
        }

        async fn verdict_totals(&self, _ground_truth_only: bool) -> Result<OutcomeTally> {
            Ok(OutcomeTally::default())
        }
    }

    fn config(agent: Uuid) -> WorkerConfig {
        WorkerConfig::new(agent)
            .with_idle_threshold(Duration::ZERO)
            .with_poll_interval(Duration::from_millis(1))
    }

    #[tokio::test]
    async fn runs_when_the_system_is_quiet() {
        let agent = Uuid::new_v4();
        let activity = ActivityTracker::new();
        let source = Arc::new(ProbeSource::new());

        let worker = DreamWorker::new(source.clone(), activity, config(agent));
        let (outcome, report) = worker.run_once().await;

        assert_eq!(outcome, PassOutcome::Completed);
        assert!(report.is_some());
        assert!(source.queries.load(Ordering::SeqCst) > 0);
    }

    #[tokio::test]
    async fn does_not_start_while_the_system_is_busy() {
        let agent = Uuid::new_v4();
        let activity = ActivityTracker::new();
        activity.record(agent);

        let source = Arc::new(ProbeSource::new());
        let worker = DreamWorker::new(
            source.clone(),
            activity,
            WorkerConfig::new(agent).with_idle_threshold(Duration::from_secs(300)),
        );

        let (outcome, report) = worker.run_once().await;

        assert_eq!(outcome, PassOutcome::NotIdle);
        assert!(report.is_none());
        assert_eq!(
            source.queries.load(Ordering::SeqCst),
            0,
            "a busy system must not even be queried"
        );
    }

    #[tokio::test]
    async fn abandons_a_pass_when_activity_arrives_mid_flight() {
        // The user is what matters; a finished report is not.
        let agent = Uuid::new_v4();
        let activity = ActivityTracker::new();
        let source = Arc::new(ProbeSource::interrupting(activity.clone(), agent));

        let worker = DreamWorker::new(source, activity, config(agent));
        let (outcome, report) = worker.run_once().await;

        assert_eq!(outcome, PassOutcome::Preempted);
        assert!(
            report.is_none(),
            "a pass that raced a user must be discarded, not returned"
        );
    }

    #[tokio::test]
    async fn a_preempted_pass_is_retried_later() {
        // Abort-and-retry: discarding costs one cheap pass, and the next
        // quiet moment picks the work up again from scratch.
        let agent = Uuid::new_v4();
        let activity = ActivityTracker::new();
        let source = Arc::new(ProbeSource::interrupting(activity.clone(), agent));

        let worker = DreamWorker::new(source, activity.clone(), config(agent));
        assert_eq!(worker.run_once().await.0, PassOutcome::Preempted);

        // A later pass over a now-quiet system succeeds, with no state
        // carried over from the abandoned one.
        let quiet_source = Arc::new(ProbeSource::new());
        let quiet_worker = DreamWorker::new(quiet_source, activity, config(agent));
        assert_eq!(quiet_worker.run_once().await.0, PassOutcome::Completed);
    }

    /// Captures what the worker handed it, so "the pass was persisted"
    /// can be asserted rather than assumed.
    #[derive(Default)]
    struct RecordingSink {
        reports: Mutex<Vec<AnalysisReport>>,
    }

    #[async_trait::async_trait]
    impl ReportSink for RecordingSink {
        async fn record_report(&self, report: &AnalysisReport) -> Result<()> {
            self.reports.lock().unwrap().push(report.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_completed_pass_is_persisted() {
        // Without this the report exists only as a log line, and the phase
        // gate -- "reports show real, actionable patterns" -- cannot be
        // evaluated by anything but a human reading rotated logs.
        let agent = Uuid::new_v4();
        let sink = Arc::new(RecordingSink::default());
        let worker = DreamWorker::new(
            Arc::new(ProbeSource::new()),
            ActivityTracker::new(),
            config(agent),
        )
        .with_report_sink(sink.clone());

        assert_eq!(worker.run_once().await.0, PassOutcome::Completed);
        assert_eq!(sink.reports.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_preempted_pass_is_not_persisted() {
        // A pass that raced a user is discarded, so it must not be written
        // down either -- a stored report is a claim the system was quiet
        // enough to trust the numbers.
        let agent = Uuid::new_v4();
        let activity = ActivityTracker::new();
        let sink = Arc::new(RecordingSink::default());
        let worker = DreamWorker::new(
            Arc::new(ProbeSource::interrupting(activity.clone(), agent)),
            activity,
            config(agent),
        )
        .with_report_sink(sink.clone());

        assert_eq!(worker.run_once().await.0, PassOutcome::Preempted);
        assert!(sink.reports.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failing_report_sink_does_not_fail_the_pass() {
        // Keeping the observation is best-effort, like making it. Losing a
        // pass must not take down the worker that produced it.
        struct BrokenSink;

        #[async_trait::async_trait]
        impl ReportSink for BrokenSink {
            async fn record_report(&self, _: &AnalysisReport) -> Result<()> {
                Err(rustykrab_core::Error::Storage("disk on fire".into()))
            }
        }

        let agent = Uuid::new_v4();
        let worker = DreamWorker::new(
            Arc::new(ProbeSource::new()),
            ActivityTracker::new(),
            config(agent),
        )
        .with_report_sink(Arc::new(BrokenSink));

        let (outcome, report) = worker.run_once().await;
        assert_eq!(outcome, PassOutcome::Completed);
        assert!(report.is_some());
    }

    #[tokio::test]
    async fn analysis_failure_is_swallowed() {
        struct Broken;

        #[async_trait::async_trait]
        impl OutcomeSource for Broken {
            async fn tallies(
                &self,
                _: AttributionKind,
                _: bool,
            ) -> Result<Vec<(String, OutcomeTally)>> {
                Err(rustykrab_core::Error::Storage("disk on fire".into()))
            }
            async fn total_records(&self) -> Result<u32> {
                Ok(0)
            }
            async fn verdict_totals(&self, _: bool) -> Result<OutcomeTally> {
                Ok(OutcomeTally::default())
            }
        }

        let agent = Uuid::new_v4();
        let worker = DreamWorker::new(Arc::new(Broken), ActivityTracker::new(), config(agent));
        let (outcome, report) = worker.run_once().await;

        assert_eq!(outcome, PassOutcome::Failed);
        assert!(report.is_none());
    }
}
