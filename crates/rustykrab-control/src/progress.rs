//! The progress ledger (plan section 6, step 7; Magentic-One's pattern):
//! no new evidence across N steps triggers repair rather than more turns.
//!
//! A [`StepLedger`] watches one run. Each tool call is a step, fingerprinted
//! from the tool, its arguments and what came back; a step whose
//! fingerprint the run has not seen is progress, and one it has seen is
//! not. Once `threshold` steps in a row bring nothing new, the run has
//! stalled: the worker adapter says so in each repeating step's result and
//! gives the model another threshold of steps to change course. If those
//! also bring nothing new, [`StepLedger::recovery_due`] ends the local
//! attempt as `model/loop`, retaining [`Stall::detail`] for the next worker.
//! A new result resets both windows.
//!
//! Independently of what happens between them, the same call (tool and
//! arguments) failing with the same kind of error is counted across the
//! run: the [`REPEATED_FAILURE_WARNING`]th such failure carries the same
//! warning, and the [`REPEATED_FAILURE_LIMIT`]th ends the attempt the same
//! way. A loop that relaunches one failing command between polls whose
//! output changes brings something new every other step, so the first
//! count never sees it. A call that ran into its time limit while doing
//! work the next identical call continues (a Cargo build at the longest
//! exec limit) is recorded with [`StepLedger::record_resumable`] and counts
//! towards neither. Deterministic and pure: the same steps always give
//! the same verdict.
//!
//! The subtree half of step 7 (a parent with nothing moving beneath it) is
//! the controller's, over the graph: `controller::stalled_parents`.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use serde_json::Value;

/// Steps in a row without new evidence before a run has stalled. A
/// placeholder until Phase 0 measures how often a healthy run repeats
/// itself.
pub const DEFAULT_STALL_STEPS: u32 = 6;

/// Identical failures of one call in a run (same tool, arguments and error
/// kind, whatever came between) that raise the stall warning.
pub const REPEATED_FAILURE_WARNING: u32 = 3;

/// Identical failures of one call in a run that end the attempt.
pub const REPEATED_FAILURE_LIMIT: u32 = 6;

/// A run that stopped making progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stall {
    /// Steps the run took.
    pub steps: u32,
    /// Steps in a row that brought nothing new.
    pub without_progress: u32,
    /// The tool of the last step, or of the call that kept failing.
    pub last_tool: String,
    /// When the stall is one call failing again and again: how often it
    /// failed and the kind of error. `None` for a run without new evidence.
    pub repeated_failure: Option<(u32, String)>,
}

impl Stall {
    /// The failure detail the ladder records: it names the stall and the
    /// ledger, so the repair it triggers says why.
    pub fn detail(&self) -> String {
        if let Some((times, kind)) = &self.repeated_failure {
            return format!(
                "stalled: the same {} call failed {times} times with a {kind} error (progress \
                 ledger), {} steps in all",
                self.last_tool, self.steps
            );
        }
        format!(
            "stalled: {} steps without new evidence (progress ledger), {} steps in all; last \
             tool {}",
            self.without_progress, self.steps, self.last_tool
        )
    }
}

/// One run's ledger.
#[derive(Debug, Clone)]
pub struct StepLedger {
    threshold: u32,
    seen: HashSet<u64>,
    steps: u32,
    since_progress: u32,
    last_tool: String,
    /// How often each (tool, arguments, error kind) has failed in the run.
    failures: HashMap<u64, u32>,
    /// The count, error kind and tool of the last step's failure, when the
    /// last step failed.
    last_failure: Option<(u32, String, String)>,
    /// The most any one call has failed identically: count, kind and tool.
    most_failures: Option<(u32, String, String)>,
}

impl StepLedger {
    /// A ledger that calls a run stalled after `threshold` steps in a row
    /// with no new evidence (at least one).
    pub fn new(threshold: u32) -> StepLedger {
        StepLedger {
            threshold: threshold.max(1),
            seen: HashSet::new(),
            steps: 0,
            since_progress: 0,
            last_tool: String::new(),
            failures: HashMap::new(),
            last_failure: None,
            most_failures: None,
        }
    }

    /// Record one step: `tool` called with `args`, and what came back
    /// (`outcome`, the result or the error, as text). Returns whether the
    /// step was progress.
    pub fn record(&mut self, tool: &str, args: &Value, outcome: &str) -> bool {
        self.record_call(tool, args, outcome, None)
    }

    /// [`StepLedger::record`], with the kind of error when the call failed
    /// (`failure`): the same call failing with the same kind is counted
    /// across the run, whatever came between.
    pub fn record_call(
        &mut self,
        tool: &str,
        args: &Value,
        outcome: &str,
        failure: Option<&str>,
    ) -> bool {
        self.last_failure = failure.map(|kind| {
            let mut h = DefaultHasher::new();
            tool.hash(&mut h);
            args.to_string().hash(&mut h);
            kind.hash(&mut h);
            let count = self.failures.entry(h.finish()).or_insert(0);
            *count += 1;
            (*count, kind.to_string(), tool.to_string())
        });
        if let Some(failure) = &self.last_failure {
            if self
                .most_failures
                .as_ref()
                .is_none_or(|(most, _, _)| failure.0 > *most)
            {
                self.most_failures = Some(failure.clone());
            }
        }
        let mut h = DefaultHasher::new();
        tool.hash(&mut h);
        args.to_string().hash(&mut h);
        outcome.hash(&mut h);
        let fresh = self.seen.insert(h.finish());
        self.steps += 1;
        self.last_tool = tool.to_string();
        if fresh {
            self.since_progress = 0;
        } else {
            self.since_progress += 1;
        }
        fresh
    }

    /// Record a step that neither counts as a failure nor as a repeat: a
    /// call that ran into its time limit while doing work the next
    /// identical call continues (a Cargo build stopped at the longest exec
    /// limit keeps every step it finished, and running it again is what
    /// the run was told to do). It is a step, but adds nothing to the
    /// identical-failure count or the window without new evidence, and
    /// resets neither: whatever stalled the run before it still stands.
    pub fn record_resumable(&mut self, tool: &str) {
        self.last_failure = None;
        self.steps += 1;
        self.last_tool = tool.to_string();
    }

    /// The stall, once `threshold` steps in a row brought nothing new.
    /// Or, once the last step's call has failed the same way
    /// [`REPEATED_FAILURE_WARNING`] times in the run.
    pub fn stalled(&self) -> Option<Stall> {
        if let Some(failure) = &self.last_failure {
            if failure.0 >= REPEATED_FAILURE_WARNING {
                return Some(self.stall(Some(failure)));
            }
        }
        (self.since_progress >= self.threshold).then(|| self.stall(None))
    }

    /// The stall as it stands; for a repeated failure, naming the tool
    /// that failed rather than whichever ran last.
    fn stall(&self, repeated_failure: Option<&(u32, String, String)>) -> Stall {
        Stall {
            steps: self.steps,
            without_progress: self.since_progress,
            last_tool: repeated_failure
                .map_or_else(|| self.last_tool.clone(), |(_, _, tool)| tool.clone()),
            repeated_failure: repeated_failure.map(|(count, kind, _)| (*count, kind.clone())),
        }
    }

    /// The warning did not produce a change of approach: end this attempt
    /// after a second window without new evidence. Checked before model
    /// requests and tool effects, leaving the worker room to self-correct.
    /// Also due once any one call has failed the same way
    /// [`REPEATED_FAILURE_LIMIT`] times in the run.
    pub fn recovery_due(&self) -> Option<Stall> {
        if let Some(failure) = &self.most_failures {
            if failure.0 >= REPEATED_FAILURE_LIMIT {
                return Some(self.stall(Some(failure)));
            }
        }
        (self.since_progress >= self.threshold.saturating_mul(2)).then(|| self.stall(None))
    }

    pub fn steps(&self) -> u32 {
        self.steps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repeating_one_call_with_one_answer_stalls_after_the_threshold() {
        let mut l = StepLedger::new(3);
        assert!(l.record("todo_read", &json!({}), "[]"));
        for _ in 0..2 {
            assert!(!l.record("todo_read", &json!({}), "[]"));
            assert!(l.stalled().is_none());
        }
        assert!(!l.record("todo_read", &json!({}), "[]"));
        let stall = l.stalled().unwrap();
        assert_eq!(stall.without_progress, 3);
        assert_eq!(stall.steps, 4);
        assert!(stall.detail().contains("stalled") && stall.detail().contains("progress"));
    }

    #[test]
    fn a_new_result_or_new_arguments_is_progress_and_resets_the_count() {
        let mut l = StepLedger::new(2);
        l.record("read", &json!({"path": "a"}), "x");
        l.record("read", &json!({"path": "a"}), "x");
        assert!(
            l.record("read", &json!({"path": "b"}), "x"),
            "new arguments"
        );
        l.record("read", &json!({"path": "b"}), "x");
        assert!(l.record("read", &json!({"path": "b"}), "y"), "a new answer");
        assert!(l.stalled().is_none());
    }

    #[test]
    fn worker_autonomy_warns_then_allows_a_full_window_before_recovery() {
        let mut ledger = StepLedger::new(2);
        ledger.record("read", &serde_json::json!({}), "same");
        for _ in 0..2 {
            ledger.record("read", &serde_json::json!({}), "same");
        }
        assert!(ledger.stalled().is_some());
        assert!(ledger.recovery_due().is_none());
        ledger.record("read", &serde_json::json!({}), "same");
        assert!(ledger.recovery_due().is_none());
        ledger.record("read", &serde_json::json!({}), "same");
        assert_eq!(ledger.recovery_due().unwrap().without_progress, 4);
        ledger.record("read", &serde_json::json!({}), "changed result");
        assert!(ledger.stalled().is_none());
        assert!(ledger.recovery_due().is_none());
    }

    /// The live run's loop: a cargo command launched and timing out, then a
    /// poll of its log whose output differs every time. Every poll is new
    /// evidence, so the window count never stalls; the failing launch does.
    #[test]
    fn a_failing_launch_between_varying_polls_warns_then_ends_the_run() {
        let mut l = StepLedger::new(DEFAULT_STALL_STEPS);
        let launch = json!({"command": "cargo test", "timeout_secs": 15});
        for round in 1..=REPEATED_FAILURE_LIMIT {
            assert!(l.recovery_due().is_none(), "round {round}");
            l.record_call("exec", &launch, "error: command timed out", Some("timeout"));
            let warned = l.stalled();
            assert_eq!(
                warned.is_some(),
                round >= REPEATED_FAILURE_WARNING,
                "round {round}: {warned:?}"
            );
            if let Some(stall) = warned {
                assert_eq!(stall.repeated_failure, Some((round, "timeout".into())));
                assert!(stall.detail().contains("failed"), "{}", stall.detail());
            }
            if round < REPEATED_FAILURE_LIMIT {
                l.record_call(
                    "exec",
                    &json!({"command": "tail -5 test.log"}),
                    &format!("{{\"stdout\": \"Compiling crate {round}\"}}"),
                    None,
                );
                // A successful step in between carries no warning.
                assert!(l.stalled().is_none(), "round {round}");
            }
        }
        // A different tool running afterwards does not change who failed.
        l.record_call("read", &json!({"path": "x"}), "contents", None);
        let stall = l
            .recovery_due()
            .expect("six identical failures end the run");
        assert_eq!(stall.last_tool, "exec");
        assert_eq!(stall.repeated_failure, Some((6, "timeout".into())));
        assert!(
            stall.detail().contains("progress ledger"),
            "{}",
            stall.detail()
        );
        assert!(
            stall.detail().contains("failed 6 times"),
            "{}",
            stall.detail()
        );
    }

    #[test]
    fn three_identical_failures_raise_the_warning_and_others_do_not_count() {
        let mut l = StepLedger::new(DEFAULT_STALL_STEPS);
        let call = json!({"command": "cargo build"});
        l.record_call("exec", &call, "error: a", Some("timeout"));
        // Other arguments, another tool, or another kind of error are other calls.
        l.record_call(
            "exec",
            &json!({"command": "cargo check"}),
            "error: b",
            Some("timeout"),
        );
        l.record_call("read", &call, "error: c", Some("timeout"));
        l.record_call("exec", &call, "error: d", Some("invalid_input"));
        assert!(l.stalled().is_none());
        l.record_call("exec", &call, "error: e", Some("timeout"));
        assert!(l.stalled().is_none());
        l.record_call("exec", &call, "error: f", Some("timeout"));
        let stall = l.stalled().expect("the third identical failure warns");
        assert_eq!(stall.repeated_failure, Some((3, "timeout".into())));
        assert!(l.recovery_due().is_none(), "a warning, not the end");
    }
}
