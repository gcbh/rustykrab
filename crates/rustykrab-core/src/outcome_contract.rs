//! Turning a skill's declared outcome into a checkable claim.
//!
//! `SKILL.md` may declare what success means (see the `[outcome]` block).
//! Until something *checks* that declaration, it is only a comment: the
//! loop still records `Implicit` evidence, the analysis still reports
//! `proxy_only`, and every mutating stage correctly refuses to act.
//!
//! This is the piece that closes that loop. A check names an effect the
//! run was supposed to have; verification asks whether that effect
//! actually occurred. That answer is ground truth in the only sense that
//! matters here — it is derived from what the system *did*, not from a
//! model's opinion about whether it went well.
//!
//! Deliberately narrow. A check is satisfied when a tool of that name
//! completed successfully during the run. That covers the honest case —
//! "the calendar event was created" is verifiable because creating it is
//! a tool call — and refuses to guess at anything else.

use serde::{Deserialize, Serialize};

use crate::outcome::{OutcomeVerdict, SignalClass};

/// What a run must have done for the skill that drove it to count as
/// having succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeContract {
    /// The skill that declared this.
    pub skill: String,
    /// Effects the run was supposed to produce, named as tools.
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
    /// Two ways to decide nothing, and both must fall through to the weaker
    /// signal rather than inventing ground truth: a contract with no checks
    /// states nothing, and a contract whose skill declared a non-verifiable
    /// signal has not asked for its effects to be treated as fact.
    pub fn is_checkable(&self) -> bool {
        !self.checks.is_empty() && self.signal == SignalClass::Verifiable
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

/// Evaluate a contract against what the run actually did.
///
/// `succeeded` answers, for a tool name, whether a call to it completed
/// successfully at least once during the run. `errored` says whether the
/// run itself terminated abnormally.
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
pub fn evaluate<F>(
    contract: &OutcomeContract,
    errored: bool,
    mut succeeded: F,
) -> Option<ContractVerdict>
where
    F: FnMut(&str) -> bool,
{
    if !contract.is_checkable() {
        return None;
    }

    let mut satisfied = Vec::new();
    let mut unsatisfied = Vec::new();
    for check in &contract.checks {
        if succeeded(check) {
            satisfied.push(check.clone());
        } else {
            unsatisfied.push(check.clone());
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
            "all {} declared check(s) satisfied for skill '{}'",
            satisfied.len(),
            contract.skill
        ),
        (true, true) => format!(
            "all {} declared check(s) satisfied for skill '{}', but the run errored",
            satisfied.len(),
            contract.skill
        ),
        (false, _) => format!(
            "{} of {} declared check(s) outstanding for skill '{}': {}",
            unsatisfied.len(),
            contract.checks.len(),
            contract.skill,
            unsatisfied.join(", ")
        ),
    };

    Some(ContractVerdict {
        verdict,
        signal: SignalClass::Verifiable,
        // High, but never 1.0. The check confirms the effect occurred; it
        // cannot confirm the effect was the one the user wanted. An
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

    fn contract(checks: &[&str]) -> OutcomeContract {
        OutcomeContract::new(
            "calendar-booking",
            checks.iter().map(|s| s.to_string()).collect(),
            SignalClass::Verifiable,
        )
    }

    #[test]
    fn a_contract_with_no_checks_decides_nothing() {
        // The critical case. An empty declaration must fall through to the
        // weaker signal, never be read as confirmation of success.
        assert!(evaluate(&contract(&[]), false, |_| true).is_none());
    }

    #[test]
    fn a_skill_that_did_not_ask_to_be_verified_decides_nothing() {
        // Checks alone do not buy ground truth. A skill judged by a model
        // stays a proxy however many effects it happens to name, or the
        // loop could promote its own opinion to fact.
        for declared in [
            SignalClass::Judge,
            SignalClass::Implicit,
            SignalClass::Explicit,
        ] {
            let c = OutcomeContract::new("x", vec!["calendar_create".into()], declared);
            assert!(
                evaluate(&c, false, |_| true).is_none(),
                "{declared:?} must not be promoted to Verifiable"
            );
        }
    }

    #[test]
    fn a_run_that_did_everything_declared_is_verifiable_success() {
        let v = evaluate(&contract(&["calendar_create", "email_send"]), false, |_| {
            true
        })
        .unwrap();
        assert_eq!(v.verdict, OutcomeVerdict::Success);
        assert_eq!(v.signal, SignalClass::Verifiable);
        assert!(v.signal.is_ground_truth(), "this is what unblocks mutation");
        assert!(v.unsatisfied.is_empty());
    }

    #[test]
    fn an_outstanding_check_is_ambiguous_not_failure() {
        // A skill's effects may be spread across turns. From one run we
        // cannot tell "did not do it" from "has not done it yet", and
        // manufacturing failures for a working skill would poison the
        // very analysis this evidence exists to feed.
        let v = evaluate(&contract(&["calendar_create", "email_send"]), false, |t| {
            t == "calendar_create"
        })
        .unwrap();
        assert_eq!(v.verdict, OutcomeVerdict::Ambiguous);
        assert_eq!(v.signal, SignalClass::Verifiable);
        assert_eq!(v.unsatisfied, vec!["email_send"]);
        assert!(v.detail.contains("email_send"));
    }

    #[test]
    fn an_ambiguous_verdict_cannot_count_against_the_skill() {
        // The guarantee that makes the choice above safe: outstanding
        // checks are recorded but excluded from success rates entirely.
        let mut tally = crate::outcome::OutcomeTally::default();
        for _ in 0..10 {
            tally.record(OutcomeVerdict::Ambiguous);
        }
        assert_eq!(tally.decisive(), 0, "ambiguity must not read as harm");
        assert_eq!(tally.success_rate(1), None);
    }

    #[test]
    fn producing_every_effect_and_then_dying_is_not_success() {
        let v = evaluate(&contract(&["calendar_create"]), true, |_| true).unwrap();
        assert_eq!(
            v.verdict,
            OutcomeVerdict::Failure,
            "a crashing skill must not accumulate a clean record"
        );
        assert!(v.detail.contains("errored"));
    }

    #[test]
    fn every_check_must_be_met_not_merely_most() {
        let v = evaluate(&contract(&["a", "b", "c"]), false, |t| t != "c").unwrap();
        assert_ne!(
            v.verdict,
            OutcomeVerdict::Success,
            "two out of three is not success"
        );
        assert_eq!(v.satisfied.len(), 2);
    }

    #[test]
    fn confidence_stays_below_certainty() {
        // The check confirms the effect happened; it cannot confirm the
        // effect was the one the user wanted.
        let v = evaluate(&contract(&["calendar_create"]), false, |_| true).unwrap();
        assert!(
            v.confidence < 1.0,
            "a verified effect is still not proof of intent"
        );
        assert!(v.confidence > 0.5);
    }

    #[test]
    fn a_verifiable_record_is_actionable_where_an_implicit_one_is_not() {
        // The whole point: this is the signal that lets a later phase act.
        let v = evaluate(&contract(&["calendar_create"]), false, |_| true).unwrap();
        assert!(v.signal.is_ground_truth());
        assert!(!SignalClass::Implicit.is_ground_truth());
    }
}
