//! The progress ledger (plan section 6, step 7; Magentic-One's pattern):
//! no new evidence across N steps triggers repair rather than more turns.
//!
//! A [`StepLedger`] watches one run. Each tool call is a step, fingerprinted
//! from the tool, its arguments and what came back; a step whose
//! fingerprint the run has not seen is progress, and one it has seen is
//! not. Once `threshold` steps in a row bring nothing new, the run has
//! stalled: the worker adapter stops it, and the failure reaches the
//! ladder as `model/loop` with [`Stall::detail`], so the repair run's brief
//! carries this attempt's evidence. Deterministic and pure: the same steps
//! always give the same verdict.
//!
//! The subtree half of step 7 (a parent with nothing moving beneath it) is
//! the controller's, over the graph: `controller::stalled_parents`.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use serde_json::Value;

/// Steps in a row without new evidence before a run has stalled. A
/// placeholder until Phase 0 measures how often a healthy run repeats
/// itself.
pub const DEFAULT_STALL_STEPS: u32 = 6;

/// A run that stopped making progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stall {
    /// Steps the run took.
    pub steps: u32,
    /// Steps in a row that brought nothing new.
    pub without_progress: u32,
    /// The tool of the last step.
    pub last_tool: String,
}

impl Stall {
    /// The failure detail the ladder records: it names the stall and the
    /// ledger, so the repair it triggers says why.
    pub fn detail(&self) -> String {
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
        }
    }

    /// Record one step: `tool` called with `args`, and what came back
    /// (`outcome`, the result or the error, as text). Returns whether the
    /// step was progress.
    pub fn record(&mut self, tool: &str, args: &Value, outcome: &str) -> bool {
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

    /// The stall, once `threshold` steps in a row brought nothing new.
    pub fn stalled(&self) -> Option<Stall> {
        (self.since_progress >= self.threshold).then(|| Stall {
            steps: self.steps,
            without_progress: self.since_progress,
            last_tool: self.last_tool.clone(),
        })
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
}
