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
}

impl OutcomeContract {
    pub fn new(skill: impl Into<String>, checks: Vec<String>) -> Self {
        Self {
            skill: skill.into(),
            checks,
        }
    }

    /// Whether this contract can actually decide anything.
    ///
    /// A contract with no checks states nothing, and must not be mistaken
    /// for evidence that the run succeeded.
    pub fn is_checkable(&self) -> bool {
        !self.checks.is_empty()
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
/// successfully at least once during the run.
///
/// Returns `None` when the contract cannot decide — no checks declared —
/// so the caller falls back to the weaker implicit signal rather than
/// inventing ground truth from nothing.
pub fn evaluate<F>(contract: &OutcomeContract, mut succeeded: F) -> Option<ContractVerdict>
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
    let verdict = if all_met {
        OutcomeVerdict::Success
    } else {
        // Not ambiguous. The skill said what success required and the run
        // did not do it, which is a fact about the run rather than a gap
        // in the evidence.
        OutcomeVerdict::Failure
    };

    let detail = if all_met {
        format!(
            "all {} declared check(s) satisfied for skill '{}'",
            satisfied.len(),
            contract.skill
        )
    } else {
        format!(
            "{} of {} declared check(s) unmet for skill '{}': {}",
            unsatisfied.len(),
            contract.checks.len(),
            contract.skill,
            unsatisfied.join(", ")
        )
    };

    Some(ContractVerdict {
        verdict,
        signal: SignalClass::Verifiable,
        // High, but never 1.0. The check confirms the effect occurred; it
        // cannot confirm the effect was the one the user wanted.
        confidence: 0.9,
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
        )
    }

    #[test]
    fn a_contract_with_no_checks_decides_nothing() {
        // The critical case. An empty declaration must fall through to the
        // weaker signal, never be read as confirmation of success.
        assert!(evaluate(&contract(&[]), |_| true).is_none());
    }

    #[test]
    fn a_run_that_did_everything_declared_is_verifiable_success() {
        let v = evaluate(&contract(&["calendar_create", "email_send"]), |_| true).unwrap();
        assert_eq!(v.verdict, OutcomeVerdict::Success);
        assert_eq!(v.signal, SignalClass::Verifiable);
        assert!(v.signal.is_ground_truth(), "this is what unblocks mutation");
        assert!(v.unsatisfied.is_empty());
    }

    #[test]
    fn a_missed_check_is_failure_not_ambiguity() {
        // The skill said what success required and the run did not do it.
        // That is a fact about the run, not a gap in the evidence.
        let v = evaluate(&contract(&["calendar_create", "email_send"]), |t| {
            t == "calendar_create"
        })
        .unwrap();
        assert_eq!(v.verdict, OutcomeVerdict::Failure);
        assert_eq!(v.signal, SignalClass::Verifiable);
        assert_eq!(v.unsatisfied, vec!["email_send"]);
        assert!(v.detail.contains("email_send"));
    }

    #[test]
    fn every_check_must_be_met_not_merely_most() {
        let v = evaluate(&contract(&["a", "b", "c"]), |t| t != "c").unwrap();
        assert_eq!(
            v.verdict,
            OutcomeVerdict::Failure,
            "two out of three is not success"
        );
        assert_eq!(v.satisfied.len(), 2);
    }

    #[test]
    fn confidence_stays_below_certainty() {
        // The check confirms the effect happened; it cannot confirm the
        // effect was the one the user wanted.
        let v = evaluate(&contract(&["calendar_create"]), |_| true).unwrap();
        assert!(
            v.confidence < 1.0,
            "a verified effect is still not proof of intent"
        );
        assert!(v.confidence > 0.5);
    }

    #[test]
    fn a_verifiable_record_is_actionable_where_an_implicit_one_is_not() {
        // The whole point: this is the signal that lets a later phase act.
        let v = evaluate(&contract(&["calendar_create"]), |_| true).unwrap();
        assert!(v.signal.is_ground_truth());
        assert!(!SignalClass::Implicit.is_ground_truth());
    }
}
