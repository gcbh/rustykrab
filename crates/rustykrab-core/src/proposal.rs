//! Evaluation and proposals: the vocabulary of plan sections 1.1, 10 and 11
//! (`docs/plans/control-layer-and-worker-fleet.md`).
//!
//! The dreaming pass (`rustykrab-dream`) computes the expectation metrics of
//! section 1.1, looks where the system could improve by the criteria of
//! section 10, and files what it finds as `proposal`, `internal` or
//! `capability` work items. The review surface (`rustykrab-control`'s
//! `review` module) projects engineering items to issues and syncs the
//! human's decisions back. The store keeps the metrics and the proposal
//! records; the gateway serves them. These are the shapes all of them
//! share, and nothing here computes anything.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::outcome::SignalClass;
use crate::work::{ArtifactRef, Status, WorkItemId, WorkKind};

// ── expectation metrics (section 1.1) ─────────────────────────────────

/// Which way an expectation metric should move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
    /// The target is zero: an unknown-error rate, a verification gap, a
    /// bound violated.
    ToZero,
}

impl Direction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Direction::Up => "up",
            Direction::Down => "down",
            Direction::ToZero => "to_zero",
        }
    }
}

/// One metric of section 1.1's table: the expectation it measures, which
/// way is better, and the class of evidence it rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricSpec {
    pub name: &'static str,
    pub expectation: &'static str,
    pub direction: Direction,
    pub signal: SignalClass,
    /// What the value is, for a reader: "rate", "count" or "mean".
    pub unit: &'static str,
}

pub const DONE_WITHOUT_INTERVENTION: &str = "done_without_intervention_rate";
pub const VERIFICATION_GAP: &str = "verification_gap_rate";
pub const ESCAPED_DEFECTS: &str = "escaped_defects";
pub const RUNGS_PER_ESCALATION: &str = "rungs_per_escalation";
pub const ESCALATIONS_PER_COMPLETED: &str = "escalations_per_completed_item";
pub const AVOIDABLE_ESCALATIONS: &str = "avoidable_escalations";
pub const TYPED_ERROR_RATE: &str = "typed_error_rate";
pub const UNKNOWN_ERROR_RATE: &str = "unknown_error_rate";
pub const CAPABILITIES_USED: &str = "capabilities_used_rate";
pub const PROPOSALS_FILED: &str = "proposals_filed";
pub const PROPOSALS_MOVED_METRIC: &str = "proposals_moved_metric_rate";
pub const BOUND_VIOLATIONS: &str = "bound_violations";

/// The artifact kind a routing proposal names its move with: `<tier>
/// <class>`, the cost tier the class's default tier moves to (plan section
/// 10). Dreaming writes it on a `routing:<class>` proposal; accepting the
/// proposal applies it to the worker registry's `routing_defaults`, and
/// nothing else moves a default.
pub const ROUTING_DEFAULT: &str = "routing_default";

/// The move a `routing_default` artifact names: the tier and the class.
pub fn routing_move(value: &str) -> Option<(u32, String)> {
    let (tier, class) = value.trim().split_once(' ')?;
    let class = class.trim();
    if class.is_empty() {
        return None;
    }
    Some((tier.parse().ok()?, class.to_string()))
}

/// Section 1.1, one row per metric, in the table's order. Every
/// expectation has at least one.
pub const METRICS: &[MetricSpec] = &[
    MetricSpec {
        name: DONE_WITHOUT_INTERVENTION,
        expectation: "Finish what it is given",
        direction: Direction::Up,
        signal: SignalClass::Verifiable,
        unit: "rate",
    },
    MetricSpec {
        name: VERIFICATION_GAP,
        expectation: "Finish it correctly",
        direction: Direction::ToZero,
        signal: SignalClass::Verifiable,
        unit: "rate",
    },
    MetricSpec {
        name: ESCAPED_DEFECTS,
        expectation: "Finish it correctly",
        direction: Direction::ToZero,
        signal: SignalClass::Verifiable,
        unit: "count",
    },
    MetricSpec {
        name: RUNGS_PER_ESCALATION,
        expectation: "Persist before surfacing",
        direction: Direction::Up,
        signal: SignalClass::Verifiable,
        unit: "mean",
    },
    MetricSpec {
        name: ESCALATIONS_PER_COMPLETED,
        expectation: "Surface rarely, and only when stuck",
        direction: Direction::Down,
        signal: SignalClass::Verifiable,
        unit: "rate",
    },
    MetricSpec {
        name: AVOIDABLE_ESCALATIONS,
        expectation: "Surface rarely, and only when stuck",
        direction: Direction::Down,
        signal: SignalClass::Explicit,
        unit: "count",
    },
    MetricSpec {
        name: TYPED_ERROR_RATE,
        expectation: "Know what went wrong",
        direction: Direction::Up,
        signal: SignalClass::Verifiable,
        unit: "rate",
    },
    MetricSpec {
        name: UNKNOWN_ERROR_RATE,
        expectation: "Know what went wrong",
        direction: Direction::ToZero,
        signal: SignalClass::Verifiable,
        unit: "rate",
    },
    MetricSpec {
        name: CAPABILITIES_USED,
        expectation: "Grow its own capability",
        direction: Direction::Up,
        signal: SignalClass::Verifiable,
        unit: "rate",
    },
    MetricSpec {
        name: PROPOSALS_FILED,
        expectation: "Improve on evidence",
        direction: Direction::Up,
        signal: SignalClass::Verifiable,
        unit: "count",
    },
    MetricSpec {
        name: PROPOSALS_MOVED_METRIC,
        expectation: "Improve on evidence",
        direction: Direction::Up,
        signal: SignalClass::Verifiable,
        unit: "rate",
    },
    MetricSpec {
        name: BOUND_VIOLATIONS,
        expectation: "Stay inside bounds",
        direction: Direction::ToZero,
        signal: SignalClass::Verifiable,
        unit: "count",
    },
];

/// The spec of a metric by name.
pub fn metric_spec(name: &str) -> Option<&'static MetricSpec> {
    METRICS.iter().find(|m| m.name == name)
}

/// A metric restricted to one kind of item or one worker ("by kind or
/// worker", section 10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricSlice {
    /// `kind:<work kind>` or `worker:<name>`.
    pub scope: String,
    pub value: f64,
    pub numerator: f64,
    pub denominator: f64,
}

/// One computed expectation metric. `value` is always a finite number:
/// with nothing to measure it is `0` and `sample` says so, because a
/// missing value would read as a metric nobody computed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricValue {
    pub name: String,
    pub expectation: String,
    pub direction: Direction,
    pub unit: String,
    pub signal: SignalClass,
    pub value: f64,
    /// For a rate, what was counted over what it was counted among; for a
    /// count, the count over 1; for a mean, the total over the count.
    pub numerator: f64,
    pub denominator: f64,
    /// Observations the value rests on.
    pub sample: u32,
    pub computed_at: DateTime<Utc>,
    /// The trailing window the value covers.
    pub window_days: u32,
    #[serde(default)]
    pub breakdown: Vec<MetricSlice>,
}

// ── criteria and proposals (section 10) ───────────────────────────────

/// The places section 10 looks for improvement, one per row of its table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Criterion {
    ExpectationRegression,
    AvoidableEscalation,
    RecurringFingerprint,
    UnknownError,
    CapabilityGap,
    WastedRungs,
    VerificationMiss,
    CostLatency,
    CodingQuality,
    SkillOutcome,
}

impl Criterion {
    pub const ALL: [Criterion; 10] = [
        Criterion::ExpectationRegression,
        Criterion::AvoidableEscalation,
        Criterion::RecurringFingerprint,
        Criterion::UnknownError,
        Criterion::CapabilityGap,
        Criterion::WastedRungs,
        Criterion::VerificationMiss,
        Criterion::CostLatency,
        Criterion::CodingQuality,
        Criterion::SkillOutcome,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Criterion::ExpectationRegression => "expectation_regression",
            Criterion::AvoidableEscalation => "avoidable_escalation",
            Criterion::RecurringFingerprint => "recurring_fingerprint",
            Criterion::UnknownError => "unknown_error",
            Criterion::CapabilityGap => "capability_gap",
            Criterion::WastedRungs => "wasted_rungs",
            Criterion::VerificationMiss => "verification_miss",
            Criterion::CostLatency => "cost_latency",
            Criterion::CodingQuality => "coding_quality",
            Criterion::SkillOutcome => "skill_outcome",
        }
    }

    pub fn parse(raw: &str) -> Option<Criterion> {
        Criterion::ALL.iter().copied().find(|c| c.as_str() == raw)
    }
}

/// Section 10's proposal shape: the observed failure or opportunity, the
/// expectation it serves, what it would change, evidence and
/// counterexamples, the metric it expects to move and how, the risk and
/// the rollback condition, and the evaluation that could falsify it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposalBody {
    pub criterion: Criterion,
    pub observed: String,
    pub expectation: String,
    /// The affected skill, prompt, tool, adapter, policy or code path, as
    /// `<area>` or `<area>:<name>`.
    pub subject: String,
    /// The metric expected to move: a section 1.1 name, or
    /// `skill:<name>` for a skill's ground-truth success rate.
    pub metric: String,
    pub expected_movement: String,
    #[serde(default)]
    pub evidence: Vec<ArtifactRef>,
    #[serde(default)]
    pub counterexamples: Vec<ArtifactRef>,
    pub risk: String,
    pub rollback: String,
    pub falsified_by: String,
    /// The class of evidence the proposal rests on; only ground truth
    /// files one.
    pub signal: SignalClass,
}

/// A decision on a proposal, taken on the review surface (section 11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ReviewDecision {
    /// The proposal becomes a `code` item under verification.
    Accept,
    Decline {
        #[serde(default)]
        reason: Option<String>,
    },
    /// A change of scope the human wants carried into the work.
    Amend { text: String },
}

impl ReviewDecision {
    pub fn name(&self) -> &'static str {
        match self {
            ReviewDecision::Accept => "accept",
            ReviewDecision::Decline { .. } => "decline",
            ReviewDecision::Amend { .. } => "amend",
        }
    }
}

/// What a decision did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewOutcome {
    pub proposal: WorkItemId,
    pub decision: String,
    /// The proposal's status after the decision.
    pub status: Status,
    /// The `code` item an acceptance filed.
    #[serde(default)]
    pub code_item: Option<WorkItemId>,
    /// Why nothing changed, when nothing did (already decided, closed).
    #[serde(default)]
    pub note: Option<String>,
}

/// Where a proposal's review stands (`proposals.review`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Pending,
    Accepted,
    Declined,
}

impl ReviewState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReviewState::Pending => "pending",
            ReviewState::Accepted => "accepted",
            ReviewState::Declined => "declined",
        }
    }

    /// Conservative parse: unreadable reads as `pending`, which files
    /// nothing and executes nothing.
    pub fn parse(raw: &str) -> ReviewState {
        match raw {
            "accepted" => ReviewState::Accepted,
            "declined" => ReviewState::Declined,
            _ => ReviewState::Pending,
        }
    }
}

/// What probation found about an accepted proposal (section 10,
/// Execution): whether its named metric moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalOutcome {
    /// Accepted; the work or its probation window is still running.
    Pending,
    /// The metric moved the way the proposal said.
    Moved,
    /// The metric did not move: recorded, and evidence for the next cycle.
    NotMoved,
    /// The metric moved the wrong way past the rollback condition; a
    /// rollback was proposed.
    Regressed,
    /// The metric could not be measured again.
    Unmeasurable,
}

impl ProposalOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProposalOutcome::Pending => "pending",
            ProposalOutcome::Moved => "moved",
            ProposalOutcome::NotMoved => "not_moved",
            ProposalOutcome::Regressed => "regressed",
            ProposalOutcome::Unmeasurable => "unmeasurable",
        }
    }

    pub fn parse(raw: &str) -> ProposalOutcome {
        match raw {
            "moved" => ProposalOutcome::Moved,
            "not_moved" => ProposalOutcome::NotMoved,
            "regressed" => ProposalOutcome::Regressed,
            "unmeasurable" => ProposalOutcome::Unmeasurable,
            _ => ProposalOutcome::Pending,
        }
    }

    pub fn is_final(&self) -> bool {
        !matches!(self, ProposalOutcome::Pending)
    }
}

// ── one evaluation pass, as its callers see it ────────────────────────

/// An item a pass filed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FiledItem {
    pub id: WorkItemId,
    pub kind: WorkKind,
    pub criterion: Criterion,
    pub subject: String,
    pub title: String,
}

/// A finding a pass did not file, and why (the gate, a limit, a
/// rejection).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFinding {
    pub criterion: Criterion,
    pub subject: String,
    pub why: String,
}

/// What one probation check concluded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbationResult {
    pub proposal: WorkItemId,
    pub metric: String,
    pub outcome: ProposalOutcome,
    #[serde(default)]
    pub baseline: Option<f64>,
    #[serde(default)]
    pub observed: Option<f64>,
}

/// What a projection pass did on the review surface.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionReport {
    /// Issues created, by item.
    pub created: Vec<WorkItemId>,
    /// Issues rewritten because the item changed or a hand edit drifted
    /// from the projection.
    pub updated: Vec<WorkItemId>,
    /// Items whose issue was already current.
    pub unchanged: u32,
    /// Decision events synced back.
    pub decisions: u32,
    /// Calls that failed; the next pass retries them.
    pub errors: Vec<String>,
}

/// One evaluation pass: metrics, findings, filings, decisions synced back,
/// probation and projection. `POST /api/work/evaluate` answers with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationReport {
    pub at: DateTime<Utc>,
    pub metrics: Vec<MetricValue>,
    /// The metrics that moved the wrong way since the previous pass.
    #[serde(default)]
    pub regressions: Vec<String>,
    #[serde(default)]
    pub filed: Vec<FiledItem>,
    #[serde(default)]
    pub skipped: Vec<SkippedFinding>,
    #[serde(default)]
    pub decisions: Vec<ReviewOutcome>,
    #[serde(default)]
    pub probation: Vec<ProbationResult>,
    #[serde(default)]
    pub projection: Option<ProjectionReport>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_expectation_of_section_1_1_has_a_metric() {
        let expectations = [
            "Finish what it is given",
            "Finish it correctly",
            "Persist before surfacing",
            "Surface rarely, and only when stuck",
            "Know what went wrong",
            "Grow its own capability",
            "Improve on evidence",
            "Stay inside bounds",
        ];
        for e in expectations {
            assert!(METRICS.iter().any(|m| m.expectation == e), "{e}");
        }
        assert!(METRICS.len() >= expectations.len());
        let mut names: Vec<_> = METRICS.iter().map(|m| m.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), METRICS.len());
        assert!(metric_spec(UNKNOWN_ERROR_RATE).is_some_and(|m| m.direction == Direction::ToZero));
    }

    #[test]
    fn criteria_and_states_round_trip_and_parse_conservatively() {
        for c in Criterion::ALL {
            assert_eq!(Criterion::parse(c.as_str()), Some(c));
            assert_eq!(serde_json::to_value(c).unwrap(), c.as_str());
        }
        assert_eq!(ReviewState::parse("bogus"), ReviewState::Pending);
        assert_eq!(ProposalOutcome::parse("bogus"), ProposalOutcome::Pending);
        assert!(!ProposalOutcome::Pending.is_final());
        let d = ReviewDecision::Decline {
            reason: Some("no".into()),
        };
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["decision"], "decline");
        assert_eq!(serde_json::from_value::<ReviewDecision>(json).unwrap(), d);
    }

    #[test]
    fn a_routing_move_reads_its_tier_and_class() {
        assert_eq!(routing_move("3 code"), Some((3, "code".to_string())));
        assert_eq!(
            routing_move(" 2 capability:build "),
            Some((2, "capability:build".to_string()))
        );
        for bad in ["", "code", "x code", "3 ", "3"] {
            assert_eq!(routing_move(bad), None, "{bad:?}");
        }
    }
}
