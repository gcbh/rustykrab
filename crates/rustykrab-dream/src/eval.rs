//! The eval protocol: how a property about the outer loop is run, scored
//! and reported so it can be tracked over time rather than merely passed.
//!
//! The unit tests and the invariant harness answer "does this commit hold
//! the property". An eval answers a second question the first cannot: *is
//! the property expected to hold yet*. The outer loop is built in phases,
//! and each phase's targets are known before its code is. Writing those
//! targets down as evals that are expected to fail lets the suite carry
//! them from the day they are understood, stay green while they fail, and
//! turn red the moment one passes unexpectedly -- which is the signal to
//! promote it. The e2e harness uses the same convention for scenarios;
//! this is the same idea for properties.
//!
//! Three things are configurable from the environment, so a nightly run
//! can be wider than a pull-request run without a code change:
//!
//! - `DREAM_EVAL_SEEDS` -- how many seeds a randomized eval runs (each
//!   eval has its own default for a PR-sized run).
//! - `DREAM_EVAL_REPORT` -- a path to append one JSON line per eval to.
//!   Absent, nothing is written and the outcome is only printed.
//! - `DREAM_EVAL_STRICT` -- when set, an expected failure is a failure.
//!   For finding out how far the code is from its targets, not for CI.
//!
//! An eval body returns `Err(reason)` rather than panicking where it can,
//! so the reason reaches the report; a panic inside the body is caught
//! and reported the same way.

use std::future::Future;
use std::io::Write;
use std::time::Instant;

/// Whether an eval is expected to hold on the current code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expected {
    /// Implemented behaviour. A failure fails the suite.
    Pass,
    /// A target the code does not meet yet. The suite stays green while
    /// it fails; an unexpected pass fails the suite so the eval gets
    /// promoted to `Pass` and the fix is pinned. The string names what is
    /// missing, so the report says why the failure was expected.
    XFail(&'static str),
}

impl Expected {
    fn as_str(self) -> &'static str {
        match self {
            Expected::Pass => "pass",
            Expected::XFail(_) => "xfail",
        }
    }
}

/// What one run of an eval produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalOutcome {
    pub name: String,
    pub expected: Expected,
    pub passed: bool,
    /// "pass" | "fail" | "xfail" | "xpass"
    pub outcome: &'static str,
    /// Seeds run, for a randomized eval; 1 for a deterministic one.
    pub seeds: u64,
    pub elapsed_ms: u128,
    /// The failure reason, when there is one.
    pub detail: Option<String>,
}

impl EvalOutcome {
    fn classify(expected: Expected, passed: bool) -> &'static str {
        match (expected, passed) {
            (Expected::Pass, true) => "pass",
            (Expected::Pass, false) => "fail",
            (Expected::XFail(_), false) => "xfail",
            (Expected::XFail(_), true) => "xpass",
        }
    }

    fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "name": self.name,
            "expected": self.expected.as_str(),
            "outcome": self.outcome,
            "passed": self.passed,
            "seeds": self.seeds,
            "elapsed_ms": self.elapsed_ms,
        });
        if let Expected::XFail(why) = self.expected {
            v["xfail_reason"] = serde_json::Value::String(why.to_string());
        }
        if let Some(d) = &self.detail {
            v["detail"] = serde_json::Value::String(d.clone());
        }
        v
    }
}

/// How many seeds a randomized eval should run.
///
/// `DREAM_EVAL_SEEDS` overrides the eval's own default. Never below one:
/// an eval that ran zero seeds proves nothing and must not report a pass.
pub fn seeds(default: u64) -> u64 {
    std::env::var("DREAM_EVAL_SEEDS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
        .max(1)
}

fn strict() -> bool {
    std::env::var("DREAM_EVAL_STRICT")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Run one eval body under the protocol and judge it against `expected`.
///
/// The body runs on its own task so a panic inside it is contained and
/// reported rather than tearing down the test. Returns the outcome after
/// recording it; panics only when the outcome is one the suite must not
/// accept -- an unexpected failure, or an unexpected pass.
pub async fn run<F>(name: &str, expected: Expected, seeds: u64, body: F) -> EvalOutcome
where
    F: Future<Output = Result<(), String>> + Send + 'static,
{
    let started = Instant::now();
    let result = match tokio::spawn(body).await {
        Ok(r) => r,
        Err(join) => match join.try_into_panic() {
            Ok(payload) => Err(format!("panicked: {}", panic_message(&payload))),
            Err(e) => Err(format!("eval task failed: {e}")),
        },
    };
    let passed = result.is_ok();
    let outcome = EvalOutcome {
        name: name.to_string(),
        expected,
        passed,
        outcome: EvalOutcome::classify(expected, passed),
        seeds,
        elapsed_ms: started.elapsed().as_millis(),
        detail: result.err(),
    };

    record(&outcome);

    match outcome.outcome {
        "pass" => {}
        "xfail" => {
            if strict() {
                panic!(
                    "eval {name}: expected failure, and DREAM_EVAL_STRICT is set: {}",
                    outcome.detail.as_deref().unwrap_or("no detail")
                );
            }
        }
        "fail" => panic!(
            "eval {name} failed: {}",
            outcome.detail.as_deref().unwrap_or("no detail")
        ),
        "xpass" => {
            let why = match expected {
                Expected::XFail(why) => why,
                Expected::Pass => unreachable!("xpass requires an XFail expectation"),
            };
            panic!(
                "eval {name} passed but was expected to fail ({why}). \
                 The code now meets this target: promote the eval to Expected::Pass \
                 so the property is pinned."
            )
        }
        other => unreachable!("unknown eval outcome {other:?}"),
    }

    outcome
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Print the outcome and, when `DREAM_EVAL_REPORT` is set, append it as
/// one JSON line. Appending a single line per write keeps concurrent
/// evals from interleaving inside a record.
fn record(outcome: &EvalOutcome) {
    let line = outcome.to_json().to_string();
    eprintln!(
        "eval {} {} ({} seeds)",
        outcome.outcome, outcome.name, outcome.seeds
    );
    if let Some(detail) = &outcome.detail {
        eprintln!("  {detail}");
    }

    let Ok(path) = std::env::var("DREAM_EVAL_REPORT") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => panic!("DREAM_EVAL_REPORT={path}: cannot open for append: {e}"),
    };
    if let Err(e) = writeln!(file, "{line}") {
        panic!("DREAM_EVAL_REPORT={path}: write failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_are_classified_by_expectation_and_result() {
        assert_eq!(EvalOutcome::classify(Expected::Pass, true), "pass");
        assert_eq!(EvalOutcome::classify(Expected::Pass, false), "fail");
        assert_eq!(EvalOutcome::classify(Expected::XFail("x"), false), "xfail");
        assert_eq!(EvalOutcome::classify(Expected::XFail("x"), true), "xpass");
    }

    #[tokio::test]
    async fn an_expected_failure_is_reported_and_does_not_panic() {
        let out = run("xfail-sample", Expected::XFail("not built"), 1, async {
            Err("still missing".to_string())
        })
        .await;
        assert_eq!(out.outcome, "xfail");
        assert_eq!(out.detail.as_deref(), Some("still missing"));
    }

    #[tokio::test]
    async fn a_panic_inside_the_body_is_contained_and_reported() {
        let out = run("panic-sample", Expected::XFail("not built"), 1, async {
            panic!("boom");
        })
        .await;
        assert_eq!(out.outcome, "xfail");
        assert_eq!(out.detail.as_deref(), Some("panicked: boom"));
    }

    #[tokio::test]
    #[should_panic(expected = "expected to fail")]
    async fn an_unexpected_pass_fails_so_the_eval_gets_promoted() {
        run("xpass-sample", Expected::XFail("not built"), 1, async {
            Ok(())
        })
        .await;
    }

    #[tokio::test]
    #[should_panic(expected = "eval fail-sample failed")]
    async fn an_unexpected_failure_fails() {
        run("fail-sample", Expected::Pass, 1, async {
            Err("broke".to_string())
        })
        .await;
    }

    #[test]
    fn seeds_never_drop_below_one() {
        // Reads the environment, so only the floor is asserted here; the
        // override itself is exercised by the nightly workflow.
        assert!(seeds(0) >= 1);
        assert!(seeds(5) >= 1);
    }
}
