//! The error taxonomy and its classifiers (plan section 9).
//!
//! Every failure the controller sees maps to a typed [`WorkError`]. The
//! mapping is code first: [`classify`] reads the shape of what failed (a tool
//! result, a provider response, an exit code, a verifier verdict, a policy
//! check, a spent budget, a capability check) and, where that shape is
//! generic, the message against an ordered rule table ([`MESSAGE_RULES`]).
//! Whatever no rule recognises is `unknown/unclassified`, which is a defect in
//! observability: [`is_defect`] says so, and [`internal_item_draft`] drafts the
//! `internal` item that adds the missing probe, log line, check or parser.
//!
//! [`fingerprint`] hashes class, subclass, tool, worker kind and the
//! normalised message, so the same failure shares a fingerprint across items
//! and runs, and [`Recurrence`] counts fingerprints for the ladder's
//! promotion to order 3.
//!
//! Layout: `classify.rs` maps inputs to errors, `rules.rs` holds the message
//! rule table, `fingerprint.rs` normalises messages, hashes them and counts
//! recurrence. This file holds the input vocabulary and the defect path.

mod classify;
mod fingerprint;
mod rules;

pub use classify::{classify, classify_with};
pub use fingerprint::{fingerprint, normalise, Recurrence, DEFAULT_PROMOTE_THRESHOLD};
pub use rules::{MessageRule, MESSAGE_RULES};

use rustykrab_core::work::{
    ArtifactRef, ErrorClass, ErrorSubclass, WorkError, WorkItemDraft, WorkKind, WorkerKind,
};
use rustykrab_core::ToolErrorKind;
use serde::{Deserialize, Serialize};

/// `observed_by` of an error no classifier recognised.
pub const UNCLASSIFIED: &str = "unclassified";

/// What the controller saw fail, before classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureInput {
    /// A tool call returned an error. `tool` may be empty when the caller
    /// does not know it; [`Context::tool`] fills in.
    ToolResult {
        tool: String,
        kind: ToolErrorKind,
        message: String,
    },
    /// A model provider's response could not be used.
    Provider {
        problem: ProviderProblem,
        detail: String,
    },
    /// A process exited unsuccessfully. `code` is `None` when a signal killed
    /// it.
    ProcessExit {
        code: Option<i32>,
        stderr_tail: String,
    },
    /// The verifier rejected a worker's claim.
    Verifier {
        verdict: VerifierVerdict,
        detail: String,
    },
    /// A policy check stopped the work.
    Policy { stop: PolicyStop, detail: String },
    /// A budget on the item is spent.
    BudgetExhausted { budget: BudgetKind, detail: String },
    /// A capability check found something missing. `name` is the tool,
    /// credential, consent, resource or topic it names.
    CapabilityGap { gap: GapKind, name: String },
    /// A bare message with no structure around it.
    Raw { message: String },
}

impl FailureInput {
    /// Map a core [`rustykrab_core::Error`] to the input the controller
    /// classifies. Variants with a precise meaning map to a precise input
    /// (a pending credential approval is a consent gap, a context overflow a
    /// capacity gap, a content-policy stop a refusal); the rest keep the
    /// core's own [`ToolErrorKind`] and message, so the rule table refines
    /// them.
    pub fn from_core_error(tool: Option<&str>, err: &rustykrab_core::Error) -> FailureInput {
        use rustykrab_core::Error;
        match err {
            Error::ToolExecution(te) => FailureInput::ToolResult {
                tool: tool.unwrap_or_default().to_string(),
                kind: te.kind,
                message: te.message.clone(),
            },
            Error::ModelEmptyResponse(m) => FailureInput::Provider {
                problem: ProviderProblem::Empty,
                detail: m.clone(),
            },
            Error::ContentPolicy => FailureInput::Provider {
                problem: ProviderProblem::Refusal,
                detail: err.to_string(),
            },
            Error::ModelRateLimit(m) | Error::ModelOverloaded(m) => FailureInput::Provider {
                problem: ProviderProblem::Unavailable,
                detail: m.clone(),
            },
            Error::ContextBudgetExceeded {
                estimated_input_tokens,
                input_budget_tokens,
            } => FailureInput::CapabilityGap {
                gap: GapKind::Capacity,
                name: format!(
                    "context window: {estimated_input_tokens} tokens needed, \
                     {input_budget_tokens} available"
                ),
            },
            Error::PendingApproval { name, .. } => FailureInput::CapabilityGap {
                gap: GapKind::Consent,
                name: name.clone(),
            },
            other => FailureInput::ToolResult {
                tool: tool.unwrap_or_default().to_string(),
                kind: other.kind(),
                message: other.to_string(),
            },
        }
    }
}

/// What was wrong with a provider response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderProblem {
    /// Nothing at all came back.
    Empty,
    /// The response did not parse (a malformed tool call, broken JSON).
    Format,
    /// The model declined.
    Refusal,
    /// The model repeated itself without progress.
    Loop,
    /// The model called a tool that is not in its set. `detail` names it.
    HallucinatedTool,
    /// The provider was rate limited, overloaded or down: an upstream error,
    /// which rung 0 retries.
    Unavailable,
}

impl ProviderProblem {
    pub fn subclass(&self) -> ErrorSubclass {
        match self {
            ProviderProblem::Empty => ErrorSubclass::Empty,
            ProviderProblem::Format => ErrorSubclass::Format,
            ProviderProblem::Refusal => ErrorSubclass::Refusal,
            ProviderProblem::Loop => ErrorSubclass::Loop,
            ProviderProblem::HallucinatedTool => ErrorSubclass::HallucinatedTool,
            ProviderProblem::Unavailable => ErrorSubclass::UpstreamError,
        }
    }
}

/// The verifier's verdict on a claim (section 5: every result is a claim).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerifierVerdict {
    ClaimMismatch,
    CheckFailed,
    Incomplete,
}

impl VerifierVerdict {
    pub fn subclass(&self) -> ErrorSubclass {
        match self {
            VerifierVerdict::ClaimMismatch => ErrorSubclass::ClaimMismatch,
            VerifierVerdict::CheckFailed => ErrorSubclass::CheckFailed,
            VerifierVerdict::Incomplete => ErrorSubclass::Incomplete,
        }
    }
}

/// The policy check that stopped the work (section 8: persistence is bounded
/// by policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyStop {
    Scope,
    SingleWriter,
    Ceiling,
}

impl PolicyStop {
    pub fn subclass(&self) -> ErrorSubclass {
        match self {
            PolicyStop::Scope => ErrorSubclass::Scope,
            PolicyStop::SingleWriter => ErrorSubclass::SingleWriter,
            PolicyStop::Ceiling => ErrorSubclass::Ceiling,
        }
    }
}

/// Which of an item's budgets is spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BudgetKind {
    Iterations,
    Tokens,
    Wall,
    Repairs,
}

impl BudgetKind {
    pub fn subclass(&self) -> ErrorSubclass {
        match self {
            BudgetKind::Iterations => ErrorSubclass::Iterations,
            BudgetKind::Tokens => ErrorSubclass::Tokens,
            BudgetKind::Wall => ErrorSubclass::Wall,
            BudgetKind::Repairs => ErrorSubclass::Repairs,
        }
    }
}

/// What a capability gap is missing. Finer than the `capability_gap`
/// subclasses, because the ladder's order 2 needs two distinctions the core
/// taxonomy does not draw: `capacity` (more than the fleet has, so order 2c
/// requests it) against `compute` (a resource that exists, so order 2a
/// acquires it), and `install` (a missing binary or package, which the
/// taxonomy files as `environment/dependency`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapKind {
    Tool,
    Credential,
    Consent,
    Compute,
    Knowledge,
    Capacity,
    Install,
}

impl GapKind {
    pub const ALL: [GapKind; 7] = [
        GapKind::Tool,
        GapKind::Credential,
        GapKind::Consent,
        GapKind::Compute,
        GapKind::Knowledge,
        GapKind::Capacity,
        GapKind::Install,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            GapKind::Tool => "tool",
            GapKind::Credential => "credential",
            GapKind::Consent => "consent",
            GapKind::Compute => "compute",
            GapKind::Knowledge => "knowledge",
            GapKind::Capacity => "capacity",
            GapKind::Install => "install",
        }
    }

    pub fn parse(raw: &str) -> Option<GapKind> {
        GapKind::ALL.iter().copied().find(|g| g.as_str() == raw)
    }

    /// The subclass a gap is recorded under. `capacity` has no subclass of
    /// its own yet and is recorded as `compute`; the detail keeps the
    /// distinction (see [`gap_of`]).
    pub fn subclass(&self) -> ErrorSubclass {
        match self {
            GapKind::Tool => ErrorSubclass::ToolGap,
            GapKind::Credential => ErrorSubclass::Credential,
            GapKind::Consent => ErrorSubclass::Consent,
            GapKind::Compute | GapKind::Capacity => ErrorSubclass::Compute,
            GapKind::Knowledge => ErrorSubclass::Knowledge,
            GapKind::Install => ErrorSubclass::Dependency,
        }
    }

    /// The gap a subclass implies when the detail does not name one.
    fn from_subclass(subclass: ErrorSubclass) -> Option<GapKind> {
        match subclass {
            ErrorSubclass::ToolGap => Some(GapKind::Tool),
            ErrorSubclass::Credential => Some(GapKind::Credential),
            ErrorSubclass::Consent => Some(GapKind::Consent),
            ErrorSubclass::Compute => Some(GapKind::Compute),
            ErrorSubclass::Knowledge => Some(GapKind::Knowledge),
            ErrorSubclass::Dependency => Some(GapKind::Install),
            _ => None,
        }
    }
}

/// A capability gap read back from a classified error: what is missing and
/// the name of the thing missing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Gap {
    pub kind: GapKind,
    pub subject: String,
}

impl Gap {
    /// Whether two gaps are the same need, so the ladder files one
    /// capability item per gap. The subject is the thing missing, so it
    /// compares exactly, ignoring case and surrounding space: two servers are
    /// two needs.
    pub fn same_need(&self, other: &Gap) -> bool {
        self.kind == other.kind
            && self.subject.trim().to_lowercase() == other.subject.trim().to_lowercase()
    }
}

/// The detail [`classify`] writes for a capability gap: `needs <gap>:
/// <subject>`. [`gap_of`] reads it back.
pub(crate) fn gap_detail(gap: GapKind, subject: &str) -> String {
    format!("needs {}: {}", gap.as_str(), subject)
}

/// The capability gap an error records, if it is one: every
/// `capability_gap` error and `environment/dependency` (an install).
///
/// The kind and subject come from the `needs <gap>: <subject>` detail that
/// [`classify`] writes; an error classified elsewhere (a worker's own
/// report) falls back to the kind its subclass implies, with the whole
/// detail as the subject.
pub fn gap_of(err: &WorkError) -> Option<Gap> {
    let implied = GapKind::from_subclass(err.subclass)?;
    let named = err
        .detail
        .strip_prefix("needs ")
        .and_then(|rest| rest.split_once(": "))
        .and_then(|(kind, subject)| GapKind::parse(kind).map(|k| (k, subject)))
        .filter(|(kind, _)| kind.subclass() == err.subclass);
    Some(match named {
        Some((kind, subject)) => Gap {
            kind,
            subject: subject.to_string(),
        },
        None => Gap {
            kind: implied,
            subject: err.detail.clone(),
        },
    })
}

/// What [`classify`] knows beyond the failure itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Context<'a> {
    /// The tool the failing step called, when the input does not carry it.
    pub tool: Option<&'a str>,
    /// The kind of worker the item ran on.
    pub worker_kind: Option<WorkerKind>,
}

/// `unknown` is a defect in observability (section 9): each one files an
/// `internal` item.
pub fn is_defect(err: &WorkError) -> bool {
    err.class == ErrorClass::Unknown
}

/// The `internal` item the ladder's order 3 files.
///
/// For an `unknown` error it asks for the probe, log line, check or parser
/// that would have classified the failure, and is done when the attached
/// evidence classifies on replay (scenario 14). For a known error whose
/// fingerprint recurs it asks for the cause to be fixed rather than the
/// symptom repaired again. Either way the error's own refs and `evidence`
/// are attached, and the constraint keeps an observability item to
/// measurement, so it may run without review under standing policy
/// (section 10).
pub fn internal_item_draft(err: &WorkError, evidence: Vec<ArtifactRef>) -> WorkItemDraft {
    let mut artifact_refs = err.artifact_refs.clone();
    for r in evidence {
        if !artifact_refs.contains(&r) {
            artifact_refs.push(r);
        }
    }
    let fp = &err.fingerprint;
    let (title, objective, done_when, constraints) = if is_defect(err) {
        (
            format!("Classify unknown failure {fp}"),
            format!(
                "Add the probe, log line, check or parser that would have classified this \
                 failure. It was recorded as unknown (observed_by {}): {}",
                err.observed_by, err.detail
            ),
            format!(
                "Replaying the attached evidence through the classifier yields a class other \
                 than unknown, and a test pins that classification for fingerprint {fp}."
            ),
            vec!["Add measurement only; change no behaviour beyond classification.".to_string()],
        )
    } else {
        (
            format!(
                "Stop recurring {}/{} failure {fp}",
                err.class.as_str(),
                err.subclass.as_str()
            ),
            format!(
                "The same failure recurred across items (fingerprint {fp}, observed_by {}): \
                 {}. Fix the cause, or add the check that catches it before dispatch, instead \
                 of repairing the symptom again.",
                err.observed_by, err.detail
            ),
            format!(
                "The attached evidence no longer reproduces the failure, or a check rejects it \
                 before dispatch, and fingerprint {fp} stops recurring."
            ),
            Vec::new(),
        )
    };
    WorkItemDraft {
        kind: Some(WorkKind::Internal),
        title,
        objective,
        done_when,
        constraints,
        artifact_refs,
        ..WorkItemDraft::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(subclass: ErrorSubclass, detail: &str) -> WorkError {
        WorkError {
            class: subclass.class(),
            subclass,
            fingerprint: "0123456789abcdef".into(),
            detail: detail.into(),
            artifact_refs: vec![],
            observed_by: "test".into(),
        }
    }

    #[test]
    fn unknown_is_the_only_defect() {
        assert!(is_defect(&err(ErrorSubclass::Unclassified, "?")));
        assert!(!is_defect(&err(ErrorSubclass::Timeout, "slow")));
        assert!(!is_defect(&err(ErrorSubclass::Scope, "out of scope")));
    }

    #[test]
    fn an_unknown_error_drafts_an_observability_item_with_its_evidence() {
        let mut e = err(ErrorSubclass::Unclassified, "flux capacitor desynchronised");
        e.observed_by = UNCLASSIFIED.into();
        e.artifact_refs = vec![ArtifactRef {
            kind: "message".into(),
            value: "msg-1".into(),
        }];
        let evidence = vec![
            ArtifactRef {
                kind: "path".into(),
                value: "/var/log/worker.log".into(),
            },
            // A duplicate of the error's own ref is attached once.
            ArtifactRef {
                kind: "message".into(),
                value: "msg-1".into(),
            },
        ];
        let d = internal_item_draft(&e, evidence);
        assert_eq!(d.kind, Some(WorkKind::Internal));
        assert!(d.title.contains("0123456789abcdef"), "{}", d.title);
        assert!(
            d.objective
                .contains("probe, log line, check or parser that would have classified"),
            "{}",
            d.objective
        );
        assert!(d.objective.contains("flux capacitor desynchronised"));
        assert!(d.done_when.contains("other than unknown"));
        assert_eq!(d.artifact_refs.len(), 2);
        assert_eq!(d.artifact_refs[1].value, "/var/log/worker.log");
        assert_eq!(d.constraints.len(), 1);
        // The model proposes, code transitions: nothing here is a status,
        // and the host fills parent and provenance.
        assert!(d.parent.is_none());
        assert!(!d.plan);
    }

    #[test]
    fn a_recurring_known_error_drafts_a_fix_the_cause_item() {
        let e = err(ErrorSubclass::CheckFailed, "cargo test failed: 3 tests");
        let d = internal_item_draft(&e, vec![]);
        assert_eq!(d.kind, Some(WorkKind::Internal));
        assert!(d.title.contains("verification/check_failed"), "{}", d.title);
        assert!(d.objective.contains("recurred"));
        assert!(d.constraints.is_empty());
    }

    #[test]
    fn gap_detail_reads_back_kind_and_subject() {
        let e = err(
            ErrorSubclass::Credential,
            &gap_detail(GapKind::Credential, "carrier login"),
        );
        assert_eq!(
            gap_of(&e),
            Some(Gap {
                kind: GapKind::Credential,
                subject: "carrier login".into()
            })
        );
        // Capacity is recorded as compute; the detail keeps the difference.
        let e = err(
            ErrorSubclass::Compute,
            &gap_detail(GapKind::Capacity, "context window"),
        );
        assert_eq!(gap_of(&e).map(|g| g.kind), Some(GapKind::Capacity));
        let e = err(ErrorSubclass::Compute, &gap_detail(GapKind::Compute, "gpu"));
        assert_eq!(gap_of(&e).map(|g| g.kind), Some(GapKind::Compute));
        // A detail that names a gap of another subclass is not trusted.
        let e = err(ErrorSubclass::ToolGap, "needs credential: nope");
        assert_eq!(gap_of(&e).map(|g| g.kind), Some(GapKind::Tool));
        // A worker's own free-form detail falls back to the subclass.
        let e = err(ErrorSubclass::Dependency, "jq: command not found");
        assert_eq!(
            gap_of(&e),
            Some(Gap {
                kind: GapKind::Install,
                subject: "jq: command not found".into()
            })
        );
        assert_eq!(gap_of(&err(ErrorSubclass::Timeout, "slow")), None);
    }

    #[test]
    fn gaps_are_the_same_need_only_for_the_same_kind_and_subject() {
        let gap = |kind, subject: &str| Gap {
            kind,
            subject: subject.into(),
        };
        let a = gap(GapKind::Tool, "github");
        assert!(a.same_need(&gap(GapKind::Tool, " GitHub ")));
        assert!(!a.same_need(&gap(GapKind::Tool, "gitlab")));
        assert!(!a.same_need(&gap(GapKind::Install, "github")));
    }

    #[test]
    fn every_gap_kind_round_trips_and_names_a_gap_subclass() {
        for g in GapKind::ALL {
            assert_eq!(GapKind::parse(g.as_str()), Some(g));
            let sub = g.subclass();
            assert!(
                sub.class() == ErrorClass::CapabilityGap || sub == ErrorSubclass::Dependency,
                "{g:?}"
            );
        }
    }
}
