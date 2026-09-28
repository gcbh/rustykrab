//! Workers as the controller sees them (plan section 5): a name, a kind,
//! advertised capabilities, and one way to run a brief. The implementations
//! live in `rustykrab-agent`: `LocalWorker`, and `ExternalWorker` for the
//! `claude_code` and `codex` kinds; peers come in Phase 5. A run that ends
//! without a result returns a [`RunFailure`], which [`run_failure_input`]
//! turns back into what the classifier reads.

use async_trait::async_trait;
use rustykrab_core::work::{
    ArtifactRef, Budget, Evidence, InputRef, ResultReport, WorkItemId, WorkKind, WorkerKind,
};
use rustykrab_core::Error;
use serde::{Deserialize, Serialize};

use crate::errors::{BudgetKind, FailureInput, GapKind, PolicyStop, ProviderProblem};
use crate::workspace::Workspace;

/// What a worker advertises (plan section 5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCapabilities {
    pub models: Vec<String>,
    pub tools: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub repos: Vec<String>,
    pub machine: Option<String>,
    pub writable_resources: Vec<String>,
}

/// The brief a worker receives (plan section 6, step 4, and 6.3). Typed
/// fields, pointers not prose; the worker opens what it needs from the store
/// and the recall archive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Brief {
    pub item: WorkItemId,
    pub kind: WorkKind,
    pub title: String,
    pub objective: String,
    pub done_when: String,
    pub constraints: Vec<String>,
    pub decisions_made: Vec<String>,
    pub artifact_refs: Vec<ArtifactRef>,
    pub required_tools: Vec<String>,
    pub required_mcp_servers: Vec<String>,
    pub writable_resources: Vec<String>,
    /// The fan-in block, capped per plan section 6.3; ids beyond the cap are
    /// listed in `more_inputs`.
    pub inputs: Vec<InputRef>,
    pub more_inputs: Vec<WorkItemId>,
    /// Evidence from the item's own failed attempts, for a repair run.
    pub prior_evidence: Vec<Evidence>,
    /// The error class and detail of the last failure, for a repair run.
    pub last_error: Option<String>,
    pub budget: Budget,
    pub origin_conversation_id: Option<String>,
    /// The id the controller gave this run, recorded as the item's `run`
    /// evidence at lease time. A worker that keeps a transcript keeps it
    /// under this id (the local worker's conversation id), so the pointer
    /// outlives a run that is cancelled or lost mid-way.
    #[serde(default)]
    pub run: Option<String>,
    /// For a `code` item with a repository: the isolated worktree the run
    /// works in, its branch and the parent commit the controller pinned
    /// (see [`crate::workspace`]). The worker creates it before the run and
    /// removes the directory after; the controller verifies against it.
    #[serde(default)]
    pub workspace: Option<Workspace>,
}

/// The artifact kind a worker's adapter attests for each command the agent
/// ran, read from the agent's own event stream (a model-written one is
/// dropped). A claimed check is verified against these (plan section 5:
/// "the named checks ran").
pub const COMMAND_RUN: &str = "command_run";

/// A worker the controller can lease an item to.
#[async_trait]
pub trait Worker: Send + Sync {
    /// Stable, human-addressable name ("pinch", "krabby").
    fn name(&self) -> &str;
    fn kind(&self) -> WorkerKind;
    fn capabilities(&self) -> WorkerCapabilities;
    /// How many items it may run at once.
    fn concurrency(&self) -> usize {
        1
    }
    fn healthy(&self) -> bool {
        true
    }
    /// Run one brief to its typed result. The controller verifies the result
    /// before anything counts; a worker never transitions an item.
    async fn run(&self, brief: Brief) -> Result<ResultReport, Error>;

    /// What the run with this id (the brief's `run`) spent and what the
    /// worker counted about itself, once the run has ended or been
    /// stopped. The controller asks once per run, records the spend
    /// (`work_spend`) and the counts as a `run` event on the item. `None`:
    /// the worker keeps no such numbers, and the controller records its own
    /// wall time with no token count.
    fn usage(&self, _run: &str) -> Option<RunUsage> {
        None
    }
}

/// What one worker run spent and counted (see [`Worker::usage`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunUsage {
    /// Tokens the run's model calls consumed (prompt and completion).
    pub tokens: u64,
    pub wall_ms: u64,
    /// Model turns the run took.
    pub iterations: u32,
    /// Completion reminders the runner sent after a text-only reply (plan
    /// section 12.1 measures these as net-negative; the controller records
    /// how often they happen).
    pub reminders: u32,
}

// ── a run that ends without a result ──────────────────────────────────────

/// How a worker run ended without a typed result, for [`Worker::run`] to
/// return and the controller to classify.
///
/// The trait's error is core's [`Error`], which has no variant for a spent
/// budget or a reply that never reported, so a `RunFailure` travels as
/// [`Error::Internal`] behind [`RunFailure::PREFIX`]. The controller reads
/// any error `run` returns through [`run_failure_input`]: a `RunFailure`
/// comes back typed, and every other error (a provider failure, a refusal)
/// goes through [`FailureInput::from_core_error`] as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunFailure {
    /// An item budget ran out before the worker reported.
    Budget { budget: BudgetKind, detail: String },
    /// The model's output could not end the run: nothing at all (`Empty`),
    /// or prose with no report (`Format`).
    Model {
        problem: ProviderProblem,
        detail: String,
    },
    /// The worker lacks something the brief requires; `name` is the tool,
    /// server or credential missing.
    Gap { gap: GapKind, name: String },
    /// The worker's own process exited without a result: an external
    /// agent's CLI, by code, or `None` when a signal ended it.
    Process {
        code: Option<i32>,
        stderr_tail: String,
    },
    /// The worker refused the brief on a policy check: a repository
    /// outside the ones it was added for is `scope`.
    Policy { stop: PolicyStop, detail: String },
}

impl RunFailure {
    /// What the error message of a `RunFailure` starts with.
    pub const PREFIX: &'static str = "worker run ended without a result: ";

    pub fn into_error(self) -> Error {
        Error::Internal(self.to_string())
    }

    /// The `RunFailure` an error carries, if it is one.
    pub fn from_error(err: &Error) -> Option<RunFailure> {
        let Error::Internal(message) = err else {
            return None;
        };
        let (head, detail) = message.strip_prefix(Self::PREFIX)?.split_once(": ")?;
        let detail = detail.to_string();
        match head.split_once('/')? {
            ("budget", word) => {
                let budget = BUDGETS.iter().copied().find(|b| budget_word(*b) == word)?;
                Some(RunFailure::Budget { budget, detail })
            }
            ("model", word) => {
                let problem = PROBLEMS
                    .iter()
                    .copied()
                    .find(|p| problem_word(*p) == word)?;
                Some(RunFailure::Model { problem, detail })
            }
            ("capability_gap", word) => Some(RunFailure::Gap {
                gap: GapKind::parse(word)?,
                name: detail,
            }),
            ("process", word) => Some(RunFailure::Process {
                code: match word {
                    "signal" => None,
                    code => Some(code.parse().ok()?),
                },
                stderr_tail: detail,
            }),
            ("policy", word) => Some(RunFailure::Policy {
                stop: STOPS.iter().copied().find(|s| stop_word(*s) == word)?,
                detail,
            }),
            _ => None,
        }
    }

    /// The input [`crate::errors::classify`] takes.
    pub fn input(&self) -> FailureInput {
        match self.clone() {
            RunFailure::Budget { budget, detail } => {
                FailureInput::BudgetExhausted { budget, detail }
            }
            RunFailure::Model { problem, detail } => FailureInput::Provider { problem, detail },
            RunFailure::Gap { gap, name } => FailureInput::CapabilityGap { gap, name },
            RunFailure::Process { code, stderr_tail } => {
                FailureInput::ProcessExit { code, stderr_tail }
            }
            RunFailure::Policy { stop, detail } => FailureInput::Policy { stop, detail },
        }
    }
}

impl std::fmt::Display for RunFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code;
        let (class, word, detail) = match self {
            RunFailure::Budget { budget, detail } => ("budget", budget_word(*budget), detail),
            RunFailure::Model { problem, detail } => ("model", problem_word(*problem), detail),
            RunFailure::Gap { gap, name } => ("capability_gap", gap.as_str(), name),
            RunFailure::Process {
                code: exit,
                stderr_tail,
            } => {
                code = exit.map_or_else(|| "signal".to_string(), |c| c.to_string());
                ("process", code.as_str(), stderr_tail)
            }
            RunFailure::Policy { stop, detail } => ("policy", stop_word(*stop), detail),
        };
        write!(f, "{}{class}/{word}: {detail}", Self::PREFIX)
    }
}

/// What the controller classifies for an error [`Worker::run`] returned.
pub fn run_failure_input(err: &Error) -> FailureInput {
    match RunFailure::from_error(err) {
        Some(failure) => failure.input(),
        None => FailureInput::from_core_error(None, err),
    }
}

const BUDGETS: [BudgetKind; 4] = [
    BudgetKind::Iterations,
    BudgetKind::Tokens,
    BudgetKind::Wall,
    BudgetKind::Repairs,
];

const PROBLEMS: [ProviderProblem; 6] = [
    ProviderProblem::Empty,
    ProviderProblem::Format,
    ProviderProblem::Refusal,
    ProviderProblem::Loop,
    ProviderProblem::HallucinatedTool,
    ProviderProblem::Unavailable,
];

const STOPS: [PolicyStop; 3] = [
    PolicyStop::Scope,
    PolicyStop::SingleWriter,
    PolicyStop::Ceiling,
];

fn stop_word(stop: PolicyStop) -> &'static str {
    stop.subclass().as_str()
}

fn budget_word(budget: BudgetKind) -> &'static str {
    budget.subclass().as_str()
}

fn problem_word(problem: ProviderProblem) -> &'static str {
    match problem {
        ProviderProblem::Unavailable => "unavailable",
        other => other.subclass().as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::{classify, Context};
    use rustykrab_core::work::{ErrorClass, ErrorSubclass};

    fn all() -> Vec<RunFailure> {
        let mut out: Vec<RunFailure> = BUDGETS
            .iter()
            .map(|&budget| RunFailure::Budget {
                budget,
                detail: "25 of 25 iterations used: no result_report".into(),
            })
            .collect();
        out.extend(PROBLEMS.iter().map(|&problem| RunFailure::Model {
            problem,
            detail: "ended with text: see: the notes".into(),
        }));
        out.extend(GapKind::ALL.iter().map(|&gap| RunFailure::Gap {
            gap,
            name: "mcp server linear".into(),
        }));
        out.push(RunFailure::Process {
            code: Some(2),
            stderr_tail: "error: unknown flag: --bogus".into(),
        });
        out.push(RunFailure::Process {
            code: None,
            stderr_tail: String::new(),
        });
        out.extend(STOPS.iter().map(|&stop| RunFailure::Policy {
            stop,
            detail: "repo:/elsewhere is not one of this worker's repositories".into(),
        }));
        out
    }

    #[test]
    fn every_failure_round_trips_through_core_error() {
        for failure in all() {
            let err = failure.clone().into_error();
            assert!(err.to_string().starts_with(RunFailure::PREFIX), "{err}");
            assert_eq!(RunFailure::from_error(&err), Some(failure));
        }
    }

    #[test]
    fn a_run_failure_classifies_by_its_type_not_its_words() {
        let ctx = Context {
            tool: None,
            worker_kind: Some(rustykrab_core::work::WorkerKind::Local),
        };
        let cases = [
            (
                RunFailure::Budget {
                    budget: BudgetKind::Iterations,
                    detail: "25 of 25 iterations used".into(),
                },
                ErrorSubclass::Iterations,
            ),
            (
                RunFailure::Budget {
                    budget: BudgetKind::Wall,
                    detail: "wall budget of 60s spent".into(),
                },
                ErrorSubclass::Wall,
            ),
            (
                RunFailure::Model {
                    problem: ProviderProblem::Empty,
                    detail: "no text and no report".into(),
                },
                ErrorSubclass::Empty,
            ),
            (
                RunFailure::Model {
                    problem: ProviderProblem::Format,
                    detail: "ended with text and no result_report after 3 reminders".into(),
                },
                ErrorSubclass::Format,
            ),
            (
                RunFailure::Gap {
                    gap: GapKind::Tool,
                    name: "browser".into(),
                },
                ErrorSubclass::ToolGap,
            ),
        ];
        for (failure, want) in cases {
            let got = classify(&run_failure_input(&failure.into_error()), &ctx);
            assert_eq!(got.subclass, want);
            assert_ne!(got.class, ErrorClass::Unknown);
        }
    }

    #[test]
    fn other_errors_classify_as_the_core_reports_them() {
        let empty = Error::ModelEmptyResponse("zero tokens".into());
        assert_eq!(RunFailure::from_error(&empty), None);
        assert_eq!(
            run_failure_input(&empty),
            FailureInput::from_core_error(None, &empty)
        );
        // The prefix only counts at the start, and only on `Internal`.
        let quoted = Error::Internal(format!("tool said: {}budget/wall: x", RunFailure::PREFIX));
        assert_eq!(RunFailure::from_error(&quoted), None);
        let other = Error::Config(format!("{}budget/wall: x", RunFailure::PREFIX));
        assert_eq!(RunFailure::from_error(&other), None);
        let unknown = Error::Internal(format!("{}budget/forever: x", RunFailure::PREFIX));
        assert_eq!(RunFailure::from_error(&unknown), None);
    }
}
