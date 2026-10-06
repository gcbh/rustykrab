//! Questions, blocked states and standing judgment: the shared vocabulary of
//! `docs/plans/control-layer-and-worker-fleet.md`, section 7.
//!
//! A worker asks; the control layer's router classifies. The classes are the
//! planning plan's (blocking now, blocking later, researchable, defaultable,
//! delegated, obsolete), and only a blocking-now question outside delegated
//! authority reaches the user. Standing judgment is the user's delegation in
//! ordinary language, compiled to a checklist of [`JudgmentCheck`]s the
//! controller evaluates, so what the system may decide alone is data a
//! person can read back, never a model's inference.
//!
//! Pure data, like `work.rs`: the store persists it, `rustykrab-control`
//! routes and compiles, the tools, the gateway and the CLI render it.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::work::BlockedReason;

/// The router's class for a question (plan section 7, the planning plan's
/// question classes). A model may claim one; the router decides, in code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionClass {
    /// Nothing moves until the user answers: the only class that reaches
    /// the user, and only outside delegated authority.
    BlockingNow,
    /// Needed later, not by the step asking: recorded and carried in the
    /// parent's next message rather than sent at once.
    BlockingLater,
    /// A fact a `research` item can find: filed, never asked.
    Researchable,
    /// Has a recorded default: answered with it, and the default recorded.
    Defaultable,
    /// Inside standing judgment: decided by policy, with the decision
    /// recorded.
    Delegated,
    /// No longer needs an answer: closed.
    Obsolete,
}

impl QuestionClass {
    pub const ALL: [QuestionClass; 6] = [
        QuestionClass::BlockingNow,
        QuestionClass::BlockingLater,
        QuestionClass::Researchable,
        QuestionClass::Defaultable,
        QuestionClass::Delegated,
        QuestionClass::Obsolete,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            QuestionClass::BlockingNow => "blocking_now",
            QuestionClass::BlockingLater => "blocking_later",
            QuestionClass::Researchable => "researchable",
            QuestionClass::Defaultable => "defaultable",
            QuestionClass::Delegated => "delegated",
            QuestionClass::Obsolete => "obsolete",
        }
    }

    /// The class named `raw`, accepting spaces or dashes for underscores.
    pub fn parse(raw: &str) -> Option<QuestionClass> {
        let norm = raw.trim().to_ascii_lowercase().replace([' ', '-'], "_");
        QuestionClass::ALL
            .iter()
            .copied()
            .find(|c| c.as_str() == norm)
    }
}

impl fmt::Display for QuestionClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a question asks for, which decides the blocked state its item parks
/// in and how an answer is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    /// A choice: free text or one of the options (`needs_decision`).
    #[default]
    Decision,
    /// A fixed yes or no with an audit record (`needs_consent`).
    Consent,
    /// A credential only the user holds (`needs_credential`).
    Credential,
    /// A capability only the user can enable (`needs_tool`).
    Capability,
    /// A plan's approval at acceptance (section 6.1): approve or reject.
    Approval,
}

impl QuestionKind {
    pub const ALL: [QuestionKind; 5] = [
        QuestionKind::Decision,
        QuestionKind::Consent,
        QuestionKind::Credential,
        QuestionKind::Capability,
        QuestionKind::Approval,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            QuestionKind::Decision => "decision",
            QuestionKind::Consent => "consent",
            QuestionKind::Credential => "credential",
            QuestionKind::Capability => "capability",
            QuestionKind::Approval => "approval",
        }
    }

    pub fn parse(raw: &str) -> Option<QuestionKind> {
        QuestionKind::ALL
            .iter()
            .copied()
            .find(|k| k.as_str() == raw.trim())
    }

    /// The blocked state an item parks in while this kind of question is
    /// open (section 7).
    pub fn blocked_reason(&self) -> BlockedReason {
        match self {
            QuestionKind::Decision => BlockedReason::NeedsDecision,
            QuestionKind::Consent | QuestionKind::Approval => BlockedReason::NeedsConsent,
            QuestionKind::Credential => BlockedReason::NeedsCredential,
            QuestionKind::Capability => BlockedReason::NeedsTool,
        }
    }

    /// The kind a worker's typed block implies.
    pub fn for_blocked(reason: BlockedReason) -> QuestionKind {
        match reason {
            BlockedReason::NeedsConsent => QuestionKind::Consent,
            BlockedReason::NeedsCredential => QuestionKind::Credential,
            BlockedReason::NeedsTool => QuestionKind::Capability,
            _ => QuestionKind::Decision,
        }
    }
}

impl fmt::Display for QuestionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a question stands. `Open` and `Recorded` wait; the rest are
/// settled, and a settled question is never reopened (a new question is).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionStatus {
    /// Asked of the user; the item waits on the answer.
    Open,
    /// Kept, not sent: a blocking-later question for the next digest, or
    /// one held back while a plan B runs first (section 6.4).
    Recorded,
    /// A `research` item is finding the answer.
    Researching,
    /// The user answered.
    Answered,
    /// Answered with its recorded default.
    Defaulted,
    /// Decided by standing judgment.
    Delegated,
    /// Closed without an answer: the need went away.
    Obsolete,
}

impl QuestionStatus {
    pub const ALL: [QuestionStatus; 7] = [
        QuestionStatus::Open,
        QuestionStatus::Recorded,
        QuestionStatus::Researching,
        QuestionStatus::Answered,
        QuestionStatus::Defaulted,
        QuestionStatus::Delegated,
        QuestionStatus::Obsolete,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            QuestionStatus::Open => "open",
            QuestionStatus::Recorded => "recorded",
            QuestionStatus::Researching => "researching",
            QuestionStatus::Answered => "answered",
            QuestionStatus::Defaulted => "defaulted",
            QuestionStatus::Delegated => "delegated",
            QuestionStatus::Obsolete => "obsolete",
        }
    }

    /// Conservative parse: an unreadable status reads as `open`, so a row
    /// this build cannot interpret keeps its item waiting rather than
    /// letting it run on an answer nobody gave.
    pub fn parse(raw: &str) -> QuestionStatus {
        QuestionStatus::ALL
            .iter()
            .copied()
            .find(|s| s.as_str() == raw)
            .unwrap_or(QuestionStatus::Open)
    }

    /// Still waiting for something: the user, a digest, or research.
    pub fn is_waiting(&self) -> bool {
        matches!(
            self,
            QuestionStatus::Open | QuestionStatus::Recorded | QuestionStatus::Researching
        )
    }

    /// Settled with an answer a brief can carry.
    pub fn has_answer(&self) -> bool {
        matches!(
            self,
            QuestionStatus::Answered | QuestionStatus::Defaulted | QuestionStatus::Delegated
        )
    }
}

impl fmt::Display for QuestionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The record every delegated decision leaves (section 7): what was chosen,
/// what else was possible, why, why it fell inside authority, and how the
/// user can revisit it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegatedDecision {
    pub chosen: String,
    #[serde(default)]
    pub alternatives: Vec<String>,
    pub rationale: String,
    /// The policy and the check that covered it.
    pub authority: String,
    /// How to reverse or change it.
    pub revisit: String,
}

/// One compiled check of a standing-judgment grant: the deterministic,
/// inspectable form of the user's words. The controller evaluates these; a
/// sentence that compiles to none is reported back as not understood rather
/// than guessed at.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "check", rename_all = "snake_case")]
pub enum JudgmentCheck {
    /// A plan of at most this many new items needs no approval (6.1).
    PlanItemsAtMost { items: u32 },
    /// A plan whose leaves budget at most this many tokens needs no
    /// approval (6.1).
    PlanTokensAtMost { tokens: u64 },
    /// Writing a resource this names (exactly, or as a `name:` prefix)
    /// needs the user's consent: a named side effect.
    ConsentFor { resource: String },
    /// Writing a resource this names is delegated: no consent needed.
    Delegate { resource: String },
    /// Questions mentioning this topic are decided by policy (the first
    /// option, or the default), with the decision recorded.
    DecideOn { topic: String },
    /// Questions mentioning this topic always go to the user, even when
    /// they carry a default.
    ReserveOn { topic: String },
}

impl JudgmentCheck {
    /// One line a person can read back.
    pub fn describe(&self) -> String {
        match self {
            JudgmentCheck::PlanItemsAtMost { items } => {
                format!("plans of up to {items} items run without asking")
            }
            JudgmentCheck::PlanTokensAtMost { tokens } => {
                format!("plans budgeted up to {tokens} tokens run without asking")
            }
            JudgmentCheck::ConsentFor { resource } => {
                format!("ask before anything writes {resource}")
            }
            JudgmentCheck::Delegate { resource } => {
                format!("writing {resource} is delegated; no need to ask")
            }
            JudgmentCheck::DecideOn { topic } => {
                format!("decide questions about {topic} without asking, and record why")
            }
            JudgmentCheck::ReserveOn { topic } => {
                format!("always ask about {topic}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_round_trip_and_share_serde_and_as_str() {
        for c in QuestionClass::ALL {
            assert_eq!(QuestionClass::parse(c.as_str()), Some(c));
            assert_eq!(serde_json::to_value(c).unwrap(), c.as_str());
        }
        assert_eq!(
            QuestionClass::parse("Blocking now"),
            Some(QuestionClass::BlockingNow)
        );
        assert_eq!(QuestionClass::parse("urgent"), None);
    }

    #[test]
    fn an_unreadable_status_keeps_its_item_waiting() {
        for s in QuestionStatus::ALL {
            assert_eq!(QuestionStatus::parse(s.as_str()), s);
        }
        assert_eq!(QuestionStatus::parse("bogus"), QuestionStatus::Open);
        assert!(QuestionStatus::Open.is_waiting());
        assert!(!QuestionStatus::Defaulted.is_waiting());
        assert!(QuestionStatus::Delegated.has_answer());
        assert!(!QuestionStatus::Obsolete.has_answer());
    }

    #[test]
    fn kinds_map_to_the_blocked_state_their_item_parks_in() {
        assert_eq!(
            QuestionKind::Decision.blocked_reason(),
            BlockedReason::NeedsDecision
        );
        assert_eq!(
            QuestionKind::Approval.blocked_reason(),
            BlockedReason::NeedsConsent
        );
        assert_eq!(
            QuestionKind::for_blocked(BlockedReason::NeedsCredential),
            QuestionKind::Credential
        );
        for k in QuestionKind::ALL {
            assert_eq!(QuestionKind::parse(k.as_str()), Some(k));
        }
    }

    #[test]
    fn checks_serialise_tagged_and_describe_themselves() {
        let c = JudgmentCheck::ConsentFor {
            resource: "payment".into(),
        };
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["check"], "consent_for");
        assert_eq!(json["resource"], "payment");
        assert!(c.describe().contains("payment"));
    }
}
