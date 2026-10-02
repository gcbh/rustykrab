//! Outcome instrumentation — the Monitor stage of the self-improvement
//! outer loop (see `DREAMING.md`).
//!
//! Nothing here changes agent behaviour. These types record *what happened*
//! and *which artifacts contributed*, so a later offline stage can decide
//! whether a memory or skill is earning its place.
//!
//! The governing rule is that an artifact may only be optimized once its
//! desired outcome is declared and its real outcomes are measurable. These
//! types make both halves explicit: [`SignalClass`] declares what evidence
//! is trusted, and [`OutcomeRecord`] carries the observed result along with
//! the [`Attribution`] set that produced it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The class of feedback an outcome judgement rests on.
///
/// Ordered by reliability: a verifiable post-condition ("the calendar event
/// exists") is worth more than a model's opinion that the answer looked
/// good. Recording the class alongside the verdict keeps a cheap, abundant
/// proxy from being mistaken for ground truth later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalClass {
    /// Machine-checkable post-condition: a file was written, an event was
    /// created, a build succeeded.
    Verifiable,
    /// The user said so — a correction, a confirmation, a redo.
    Explicit,
    /// Behavioural proxy — retries, rephrasing, abandonment, clean
    /// completion. Cheap and abundant, but biased and noisy.
    Implicit,
    /// A model's judgement against a declared purpose. Measures
    /// plausibility, not correctness; usable as a filter, never as truth.
    Judge,
}

impl SignalClass {
    /// Reliability rank, higher is better. Used to break ties when several
    /// signals describe the same execution.
    pub fn reliability(&self) -> u8 {
        match self {
            Self::Verifiable => 4,
            Self::Explicit => 3,
            Self::Implicit => 2,
            Self::Judge => 1,
        }
    }

    /// Whether this class may stand alone as evidence for a change that is
    /// costly to reverse.
    ///
    /// Proxies are for volume; ground truth is what keeps the proxy honest.
    /// A loop that promotes on `Implicit` or `Judge` evidence alone is
    /// optimizing its own measurement rather than the system.
    pub fn is_ground_truth(&self) -> bool {
        matches!(self, Self::Verifiable | Self::Explicit)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Verifiable => "verifiable",
            Self::Explicit => "explicit",
            Self::Implicit => "implicit",
            Self::Judge => "judge",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "verifiable" => Some(Self::Verifiable),
            "explicit" => Some(Self::Explicit),
            "implicit" => Some(Self::Implicit),
            "judge" => Some(Self::Judge),
            _ => None,
        }
    }
}

/// The result of one observed execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeVerdict {
    Success,
    Failure,
    /// Observed, but the evidence does not support calling it either way.
    /// Recorded rather than discarded: a high ambiguous rate is itself a
    /// finding about the signal, not about the artifact.
    Ambiguous,
}

impl OutcomeVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Ambiguous => "ambiguous",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "success" => Some(Self::Success),
            "failure" => Some(Self::Failure),
            "ambiguous" => Some(Self::Ambiguous),
            _ => None,
        }
    }

    /// How this verdict moves an artifact's helpful/harmful counters.
    ///
    /// Ambiguous outcomes move neither: counting them as either would let
    /// noise accumulate into a confident-looking score.
    pub fn counter_delta(&self) -> (u32, u32) {
        match self {
            Self::Success => (1, 0),
            Self::Failure => (0, 1),
            Self::Ambiguous => (0, 0),
        }
    }
}

/// The kind of artifact an outcome is attributed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionKind {
    /// A skill that was active for the turn.
    Skill,
    /// A memory that was retrieved into the turn's context.
    Memory,
    /// A tool that was invoked during the turn.
    Tool,
}

impl AttributionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Memory => "memory",
            Self::Tool => "tool",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "skill" => Some(Self::Skill),
            "memory" => Some(Self::Memory),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }
}

/// One artifact that contributed to a turn.
///
/// This is the credit-assignment unit: without it an outcome says only
/// "the turn went badly" and there is nothing to act on. With it, a later
/// stage can ask "which memories were in context when this went wrong?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribution {
    pub kind: AttributionKind,
    /// Identifier of the artifact — a skill name, a memory UUID, a tool
    /// name. Opaque here; interpreted by whichever store owns that kind.
    pub id: String,
}

impl Attribution {
    pub fn skill(name: impl Into<String>) -> Self {
        Self {
            kind: AttributionKind::Skill,
            id: name.into(),
        }
    }

    pub fn memory(id: Uuid) -> Self {
        Self {
            kind: AttributionKind::Memory,
            id: id.to_string(),
        }
    }

    pub fn tool(name: impl Into<String>) -> Self {
        Self {
            kind: AttributionKind::Tool,
            id: name.into(),
        }
    }
}

/// Counters describing the mechanics of a turn, independent of its verdict.
///
/// These feed the implicit signal: a turn that completed cleanly in three
/// iterations looks different from one that burned forty and retried every
/// tool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionCounters {
    pub tool_calls: u32,
    pub tool_failures: u32,
    pub iterations: u32,
    pub compactions: u32,
}

impl ExecutionCounters {
    /// Whether the turn shows no mechanical distress.
    pub fn is_clean(&self) -> bool {
        self.tool_failures == 0
    }
}

/// One observed outcome, with the artifacts that produced it.
///
/// Persisted verbatim and never mutated. Aggregation into per-artifact
/// counters happens downstream, so a change in how outcomes are scored can
/// be recomputed from history rather than re-collected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeRecord {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub session_id: Uuid,
    pub recorded_at: DateTime<Utc>,
    pub verdict: OutcomeVerdict,
    /// Which class of evidence produced `verdict`.
    pub signal: SignalClass,
    /// Confidence in the verdict, 0.0–1.0.
    pub confidence: f64,
    /// Short human-readable note on why this verdict was reached.
    pub detail: Option<String>,
    pub counters: ExecutionCounters,
    /// Artifacts that were in play for this turn.
    pub attributions: Vec<Attribution>,
    /// Build that produced this record, so a scoring change can be scoped
    /// to the builds it applies to.
    pub rustykrab_version: Option<String>,
}

impl OutcomeRecord {
    pub fn new(
        conversation_id: Uuid,
        session_id: Uuid,
        verdict: OutcomeVerdict,
        signal: SignalClass,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            conversation_id,
            session_id,
            recorded_at: Utc::now(),
            verdict,
            signal,
            confidence: 1.0,
            detail: None,
            counters: ExecutionCounters::default(),
            attributions: Vec::new(),
            rustykrab_version: Some(crate::VERSION.to_string()),
        }
    }

    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = confidence.clamp(0.0, 1.0);
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_counters(mut self, counters: ExecutionCounters) -> Self {
        self.counters = counters;
        self
    }

    pub fn with_attributions(mut self, attributions: Vec<Attribution>) -> Self {
        self.attributions = attributions;
        self
    }

    /// Whether this record is strong enough to justify a costly change on
    /// its own: ground-truth evidence, high confidence, and an unambiguous
    /// verdict.
    pub fn is_actionable(&self) -> bool {
        self.signal.is_ground_truth()
            && self.confidence >= 0.7
            && self.verdict != OutcomeVerdict::Ambiguous
    }
}

/// Running helpful/harmful tallies for one artifact.
///
/// The shape is borrowed from ACE's playbook bullets, where each item
/// carries counters recording how often it helped or hurt. Aggregated from
/// [`OutcomeRecord`]s rather than incremented in place, so the tally can
/// always be rebuilt from the underlying history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeTally {
    pub helpful: u32,
    pub harmful: u32,
    pub ambiguous: u32,
}

impl OutcomeTally {
    /// Total unambiguous observations backing this tally.
    pub fn decisive(&self) -> u32 {
        self.helpful + self.harmful
    }

    /// Share of unambiguous observations that were positive.
    ///
    /// Returns `None` below `min_observations`, because a 1-for-1 record is
    /// not a 100% success rate — it is one data point. Callers must decide
    /// what to do with too little evidence rather than reading a
    /// confident-looking number off a near-empty sample.
    pub fn success_rate(&self, min_observations: u32) -> Option<f64> {
        let decisive = self.decisive();
        if decisive < min_observations.max(1) {
            return None;
        }
        Some(self.helpful as f64 / decisive as f64)
    }

    pub fn record(&mut self, verdict: OutcomeVerdict) {
        match verdict {
            OutcomeVerdict::Success => self.helpful += 1,
            OutcomeVerdict::Failure => self.harmful += 1,
            OutcomeVerdict::Ambiguous => self.ambiguous += 1,
        }
    }
}

/// A destination for outcome records.
///
/// Declared here so the agent loop can emit outcomes without depending on
/// a storage crate; the concrete implementation lives with the database.
/// Recording is best-effort by contract — a failure to persist an outcome
/// must never fail the turn that produced it, since instrumentation exists
/// to observe the system, not to gate it.
#[async_trait::async_trait]
pub trait OutcomeSink: Send + Sync {
    async fn record_outcome(&self, record: OutcomeRecord) -> crate::Result<()>;
}

/// Classify a completed run from behavioural evidence alone.
///
/// This is the [`SignalClass::Implicit`] signal: it observes whether the
/// turn *ran* cleanly, which is not the same as whether the user got what
/// they wanted. Confidence is deliberately capped well below certainty and
/// the verdict is never treated as ground truth — a clean run that produced
/// a wrong answer looks identical here. Stronger signals (a verified
/// post-condition, an explicit correction) override this when available.
pub fn classify_run(errored: bool, counters: ExecutionCounters) -> (OutcomeVerdict, f64, String) {
    if errored {
        return (
            OutcomeVerdict::Failure,
            0.6,
            "run returned an error".to_string(),
        );
    }
    if counters.tool_failures > 0 {
        return (
            OutcomeVerdict::Ambiguous,
            0.5,
            format!(
                "run completed with {} of {} tool calls failing",
                counters.tool_failures, counters.tool_calls
            ),
        );
    }
    (
        OutcomeVerdict::Success,
        0.4,
        "run completed without tool failures".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_class_round_trips() {
        for s in [
            SignalClass::Verifiable,
            SignalClass::Explicit,
            SignalClass::Implicit,
            SignalClass::Judge,
        ] {
            assert_eq!(SignalClass::parse(s.as_str()), Some(s));
        }
        assert_eq!(
            SignalClass::parse("VERIFIABLE"),
            Some(SignalClass::Verifiable)
        );
        assert_eq!(SignalClass::parse("nonsense"), None);
    }

    #[test]
    fn only_verifiable_and_explicit_are_ground_truth() {
        assert!(SignalClass::Verifiable.is_ground_truth());
        assert!(SignalClass::Explicit.is_ground_truth());
        assert!(!SignalClass::Implicit.is_ground_truth());
        assert!(!SignalClass::Judge.is_ground_truth());
    }

    #[test]
    fn reliability_is_strictly_ordered() {
        assert!(SignalClass::Verifiable.reliability() > SignalClass::Explicit.reliability());
        assert!(SignalClass::Explicit.reliability() > SignalClass::Implicit.reliability());
        assert!(SignalClass::Implicit.reliability() > SignalClass::Judge.reliability());
    }

    #[test]
    fn ambiguous_verdicts_move_neither_counter() {
        assert_eq!(OutcomeVerdict::Success.counter_delta(), (1, 0));
        assert_eq!(OutcomeVerdict::Failure.counter_delta(), (0, 1));
        assert_eq!(OutcomeVerdict::Ambiguous.counter_delta(), (0, 0));
    }

    #[test]
    fn tally_needs_enough_evidence_before_reporting_a_rate() {
        let mut tally = OutcomeTally::default();
        tally.record(OutcomeVerdict::Success);
        // One success is not a 100% success rate.
        assert_eq!(tally.success_rate(3), None);

        tally.record(OutcomeVerdict::Success);
        tally.record(OutcomeVerdict::Failure);
        assert_eq!(tally.success_rate(3), Some(2.0 / 3.0));
    }

    #[test]
    fn tally_ignores_ambiguous_in_rate() {
        let mut tally = OutcomeTally::default();
        tally.record(OutcomeVerdict::Success);
        tally.record(OutcomeVerdict::Failure);
        tally.record(OutcomeVerdict::Ambiguous);
        assert_eq!(tally.decisive(), 2);
        assert_eq!(tally.ambiguous, 1);
        assert_eq!(tally.success_rate(1), Some(0.5));
    }

    #[test]
    fn actionable_requires_ground_truth_and_confidence() {
        let conv = Uuid::new_v4();
        let sess = Uuid::new_v4();

        let verifiable =
            OutcomeRecord::new(conv, sess, OutcomeVerdict::Failure, SignalClass::Verifiable);
        assert!(verifiable.is_actionable());

        let judged = OutcomeRecord::new(conv, sess, OutcomeVerdict::Failure, SignalClass::Judge);
        assert!(!judged.is_actionable());

        let unsure =
            OutcomeRecord::new(conv, sess, OutcomeVerdict::Failure, SignalClass::Verifiable)
                .with_confidence(0.2);
        assert!(!unsure.is_actionable());

        let ambiguous = OutcomeRecord::new(
            conv,
            sess,
            OutcomeVerdict::Ambiguous,
            SignalClass::Verifiable,
        );
        assert!(!ambiguous.is_actionable());
    }

    #[test]
    fn classify_flags_errors_as_failure() {
        let (verdict, _, detail) = classify_run(true, ExecutionCounters::default());
        assert_eq!(verdict, OutcomeVerdict::Failure);
        assert!(detail.contains("error"));
    }

    #[test]
    fn classify_treats_partial_tool_failure_as_ambiguous() {
        // The run finished, but something went wrong along the way. Calling
        // that either a success or a failure would be inventing certainty.
        let counters = ExecutionCounters {
            tool_calls: 4,
            tool_failures: 1,
            iterations: 3,
            compactions: 0,
        };
        let (verdict, _, _) = classify_run(false, counters);
        assert_eq!(verdict, OutcomeVerdict::Ambiguous);
    }

    #[test]
    fn classify_never_claims_high_confidence() {
        // A clean run that produced a wrong answer looks identical to a
        // clean run that produced a right one, so behavioural evidence
        // must not be recorded as if it were ground truth.
        let clean = ExecutionCounters {
            tool_calls: 2,
            ..Default::default()
        };
        let (verdict, confidence, _) = classify_run(false, clean);
        assert_eq!(verdict, OutcomeVerdict::Success);
        assert!(
            confidence < 0.7,
            "implicit success must stay below the actionable threshold"
        );

        let record = OutcomeRecord::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            verdict,
            SignalClass::Implicit,
        )
        .with_confidence(confidence);
        assert!(!record.is_actionable());
    }

    #[test]
    fn confidence_is_clamped() {
        let r = OutcomeRecord::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            OutcomeVerdict::Success,
            SignalClass::Implicit,
        )
        .with_confidence(4.2);
        assert_eq!(r.confidence, 1.0);
    }
}
