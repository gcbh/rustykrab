//! Turning a skill's declared outcome into a checkable claim.
//!
//! `SKILL.md` may declare what success means (see the `[outcome]` block).
//! Until something *checks* that declaration, it is only a comment: the
//! loop still records `Implicit` evidence, the analysis still reports
//! `proxy_only`, and every mutating stage correctly refuses to act.
//!
//! This is the piece that closes that loop. A check names an effect the
//! run was supposed to have; verification asks whether that effect
//! actually occurred, by going and looking — see
//! [`crate::post_condition`], which explains at length why "the agent
//! called a tool of that name and it returned Ok" is not an acceptable
//! answer to that question.
//!
//! The short version: a check is satisfied when a **probe** observes the
//! effect appear across the run. Not when the agent reports having done
//! it.

use serde::{Deserialize, Serialize};

use crate::outcome::{OutcomeVerdict, SignalClass};
use crate::post_condition::{ProbeRegistry, ProbeWindow};

/// What a run must have done for the skill that drove it to count as
/// having succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeContract {
    /// The skill that declared this.
    pub skill: String,
    /// Effects the run was supposed to produce, each naming a registered
    /// post-condition probe.
    pub checks: Vec<String>,
    /// The evidence class the skill *declared*. Only [`SignalClass::Verifiable`]
    /// buys ground truth: a skill that asked to be judged by a model, or
    /// said nothing, must not be promoted to fact merely because it also
    /// happened to list some checks.
    pub signal: SignalClass,
}

impl OutcomeContract {
    pub fn new(skill: impl Into<String>, checks: Vec<String>, signal: SignalClass) -> Self {
        Self {
            skill: skill.into(),
            checks,
            signal,
        }
    }

    /// Whether this contract can actually decide anything.
    ///
    /// Three ways to decide nothing, and all of them must fall through to
    /// the weaker signal rather than inventing ground truth:
    ///
    /// - A contract with **no checks** states nothing.
    /// - A contract whose skill declared a **non-verifiable signal** has
    ///   not asked for its effects to be treated as fact. Checks alone do
    ///   not buy that, or a skill asking to be judged by a model could
    ///   launder the model's opinion into evidence.
    /// - A check naming **no registered probe** cannot be looked at. This
    ///   is the one that matters most in practice: it is what a typo in a
    ///   `SKILL.md`, or a check written against a deployment that has no
    ///   probe for it, actually is. Treating it as unmet would score a
    ///   working skill as having done nothing, forever, and treating it as
    ///   met would be pure invention.
    pub fn is_checkable(&self, probes: &ProbeRegistry) -> bool {
        !self.checks.is_empty()
            && self.signal == SignalClass::Verifiable
            && self.checks.iter().all(|c| probes.contains(c))
    }

    /// Checks this contract names that nothing knows how to observe.
    ///
    /// Worth surfacing rather than silently degrading: a skill declaring
    /// `signal = "verifiable"` against a probe that does not exist has a
    /// broken declaration, and the operator is the only one who can fix
    /// it.
    pub fn unprobed(&self, probes: &ProbeRegistry) -> Vec<&str> {
        self.checks
            .iter()
            .filter(|c| !probes.contains(c.as_str()))
            .map(|c| c.as_str())
            .collect()
    }
}

/// The result of holding a run up against its contract.
#[derive(Debug, Clone, PartialEq)]
pub struct ContractVerdict {
    pub verdict: OutcomeVerdict,
    pub signal: SignalClass,
    pub confidence: f64,
    pub detail: String,
    /// Checks the run satisfied.
    pub satisfied: Vec<String>,
    /// Checks it did not.
    pub unsatisfied: Vec<String>,
}

/// Evaluate a contract against what the world looks like after the run.
///
/// `window` holds the probe samples taken either side of the run;
/// `errored` says whether the run itself terminated abnormally.
///
/// Returns `None` when the contract cannot decide — see
/// [`OutcomeContract::is_checkable`] — so the caller falls back to the
/// weaker implicit signal rather than inventing ground truth from nothing.
///
/// # What an unmet check does and does not mean
///
/// A single run is one turn of a conversation, and a skill's declared
/// effects may legitimately be spread across several. A booking that
/// clarifies on turn one, creates on turn two and confirms on turn three
/// leaves the first two turns with checks outstanding while nothing has
/// gone wrong. So an unmet check yields [`OutcomeVerdict::Ambiguous`]:
/// observed, recorded, and deliberately excluded from success rates. It is
/// not evidence of failure, because from one run we cannot distinguish
/// "did not do it" from "has not done it yet" — and manufacturing failures
/// for a working skill is far more corrosive to the loop than staying
/// silent.
pub fn evaluate(
    contract: &OutcomeContract,
    probes: &ProbeRegistry,
    window: &ProbeWindow,
    errored: bool,
) -> Option<ContractVerdict> {
    if !contract.is_checkable(probes) {
        return None;
    }

    let mut satisfied = Vec::new();
    let mut unsatisfied = Vec::new();
    for check in &contract.checks {
        match window.produced(check) {
            Some(true) => satisfied.push(check.clone()),
            Some(false) => unsatisfied.push(check.clone()),
            // Sampled but unanswerable -- a probe that errored on one side
            // of the window. The contract was checkable when it was built,
            // so this is a transient fault, not a declaration problem, and
            // guessing either way would attribute a server outage to the
            // skill.
            None => return None,
        }
    }

    let all_met = unsatisfied.is_empty();

    // A run that produced every declared effect and then died is not a
    // success. The effects are real and observed, so this stays ground
    // truth -- but the run did not complete, and reporting it as success
    // would let a crashing skill accumulate a clean record.
    let verdict = match (all_met, errored) {
        (true, false) => OutcomeVerdict::Success,
        (true, true) => OutcomeVerdict::Failure,
        (false, _) => OutcomeVerdict::Ambiguous,
    };

    let detail = match (all_met, errored) {
        (true, false) => format!(
            "all {} declared effect(s) observed for skill '{}'",
            satisfied.len(),
            contract.skill
        ),
        (true, true) => format!(
            "all {} declared effect(s) observed for skill '{}', but the run errored",
            satisfied.len(),
            contract.skill
        ),
        (false, _) => format!(
            "{} of {} declared effect(s) not observed for skill '{}': {}",
            unsatisfied.len(),
            contract.checks.len(),
            contract.skill,
            unsatisfied.join(", ")
        ),
    };

    Some(ContractVerdict {
        verdict,
        signal: SignalClass::Verifiable,
        // High, but never 1.0. The probe confirms the effect appeared; it
        // cannot confirm the effect was the one the user wanted -- a
        // calendar event created on the wrong day still registers. An
        // outstanding check is weaker still: it may only mean the
        // conversation is not finished.
        confidence: if all_met { 0.9 } else { 0.5 },
        detail,
        satisfied,
        unsatisfied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post_condition::{Observation, PostCondition};
    use std::sync::Arc;

    /// A probe whose answer the test dictates, so the contract can be
    /// exercised without a calendar server.
    struct Fixed {
        name: &'static str,
        before: Observation,
        after: Observation,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl Fixed {
        fn new(name: &'static str, before: Observation, after: Observation) -> Arc<Self> {
            Arc::new(Self {
                name,
                before,
                after,
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl PostCondition for Fixed {
        fn name(&self) -> &str {
            self.name
        }
        async fn observe(&self) -> crate::Result<Observation> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(if n == 0 {
                self.before.clone()
            } else {
                self.after.clone()
            })
        }
    }

    fn contract(checks: &[&str], signal: SignalClass) -> OutcomeContract {
        OutcomeContract::new(
            "calendar-booking",
            checks.iter().map(|s| s.to_string()).collect(),
            signal,
        )
    }

    /// Registry with one probe that goes from absent to present.
    fn produced_registry() -> ProbeRegistry {
        ProbeRegistry::new().with(Fixed::new("event_exists", None, Some("ev-1".into())))
    }

    /// Registry with one probe whose state never changes.
    fn unproduced_registry() -> ProbeRegistry {
        ProbeRegistry::new().with(Fixed::new("event_exists", None, None))
    }

    async fn window(probes: &ProbeRegistry, checks: &[&str]) -> ProbeWindow {
        let checks: Vec<String> = checks.iter().map(|s| s.to_string()).collect();
        ProbeWindow {
            before: probes.sample(&checks).await,
            after: probes.sample(&checks).await,
        }
    }

    #[tokio::test]
    async fn an_observed_effect_is_ground_truth_success() {
        let probes = produced_registry();
        let w = window(&probes, &["event_exists"]).await;
        let v = evaluate(
            &contract(&["event_exists"], SignalClass::Verifiable),
            &probes,
            &w,
            false,
        )
        .expect("a probed, verifiable contract decides");

        assert_eq!(v.verdict, OutcomeVerdict::Success);
        assert_eq!(v.signal, SignalClass::Verifiable);
        assert!(v.signal.is_ground_truth());
        assert!(v.confidence < 1.0, "a probe cannot confirm intent");
    }

    #[tokio::test]
    async fn an_unobserved_effect_is_ambiguous_not_failure() {
        // A skill's effects may span turns. From one run there is no way
        // to tell "did not do it" from "has not done it yet", and
        // manufacturing failures for a working skill corrupts the analysis
        // worse than staying quiet.
        let probes = unproduced_registry();
        let w = window(&probes, &["event_exists"]).await;
        let v = evaluate(
            &contract(&["event_exists"], SignalClass::Verifiable),
            &probes,
            &w,
            false,
        )
        .unwrap();

        assert_eq!(v.verdict, OutcomeVerdict::Ambiguous);
        assert_eq!(v.unsatisfied, vec!["event_exists".to_string()]);
    }

    #[tokio::test]
    async fn producing_every_effect_then_erroring_is_not_success() {
        // Otherwise a skill that reliably crashes after its side effects
        // accumulates a spotless record.
        let probes = produced_registry();
        let w = window(&probes, &["event_exists"]).await;
        let v = evaluate(
            &contract(&["event_exists"], SignalClass::Verifiable),
            &probes,
            &w,
            true,
        )
        .unwrap();

        assert_eq!(v.verdict, OutcomeVerdict::Failure);
        assert_eq!(v.signal, SignalClass::Verifiable);
    }

    #[tokio::test]
    async fn a_check_with_no_probe_yields_no_contract() {
        // The rule that keeps a typo from becoming a permanent verdict.
        // Neither "satisfied" (invention) nor "unsatisfied" (a working
        // skill scored as doing nothing forever) is acceptable, so the run
        // falls back to the implicit signal instead.
        let probes = produced_registry();
        let c = contract(&["event_exists", "user_confirmed"], SignalClass::Verifiable);

        assert!(!c.is_checkable(&probes));
        assert_eq!(c.unprobed(&probes), vec!["user_confirmed"]);

        let w = window(&probes, &["event_exists", "user_confirmed"]).await;
        assert!(evaluate(&c, &probes, &w, false).is_none());
    }

    #[tokio::test]
    async fn a_non_verifiable_signal_never_buys_ground_truth() {
        // A skill asking to be judged by a model must not have its runs
        // promoted to fact merely because it also listed some checks --
        // that is the loop laundering its own opinion into evidence.
        let probes = produced_registry();
        for signal in [
            SignalClass::Judge,
            SignalClass::Implicit,
            SignalClass::Explicit,
        ] {
            let c = contract(&["event_exists"], signal);
            assert!(!c.is_checkable(&probes), "{signal:?} must not be checkable");
            let w = window(&probes, &["event_exists"]).await;
            assert!(evaluate(&c, &probes, &w, false).is_none());
        }
    }

    #[tokio::test]
    async fn a_contract_with_no_checks_decides_nothing() {
        let probes = produced_registry();
        let c = contract(&[], SignalClass::Verifiable);
        assert!(!c.is_checkable(&probes));
        assert!(evaluate(&c, &probes, &ProbeWindow::default(), false).is_none());
    }

    #[tokio::test]
    async fn an_effect_that_was_already_present_is_not_credited_to_this_run() {
        // The property that separates a post-condition from a state
        // assertion, and the reason tool-call matching was not good
        // enough: a calendar that already held the event says nothing
        // about the turn that just ran.
        let probes = ProbeRegistry::new().with(Fixed::new(
            "event_exists",
            Some("ev".into()),
            Some("ev".into()),
        ));
        let w = window(&probes, &["event_exists"]).await;
        let v = evaluate(
            &contract(&["event_exists"], SignalClass::Verifiable),
            &probes,
            &w,
            false,
        )
        .unwrap();

        assert_eq!(
            v.verdict,
            OutcomeVerdict::Ambiguous,
            "a pre-existing effect must not be credited to this run"
        );
    }

    #[tokio::test]
    async fn a_probe_that_fails_mid_window_decides_nothing() {
        // A server outage is not evidence about the skill.
        struct Flaky(std::sync::atomic::AtomicUsize);

        #[async_trait::async_trait]
        impl PostCondition for Flaky {
            fn name(&self) -> &str {
                "event_exists"
            }
            async fn observe(&self) -> crate::Result<Observation> {
                if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    Ok(None)
                } else {
                    Err(crate::Error::Internal("calendar unreachable".into()))
                }
            }
        }

        let probes =
            ProbeRegistry::new().with(Arc::new(Flaky(std::sync::atomic::AtomicUsize::new(0))));
        let c = contract(&["event_exists"], SignalClass::Verifiable);
        assert!(c.is_checkable(&probes), "the probe is registered");

        let w = window(&probes, &["event_exists"]).await;
        assert!(
            evaluate(&c, &probes, &w, false).is_none(),
            "an unanswerable probe must fall back, not guess"
        );
    }
}
