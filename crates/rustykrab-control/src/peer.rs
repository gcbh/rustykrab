//! The delegation contract between a controller's peer worker and the node
//! that runs its briefs (plan sections 5, 12.1 and 13; Phase 5).
//!
//! A peer is a paired RustyKrab node reached over the delegated-task API
//! the `nodes` tool already uses: `POST /api/tasks` to submit, `GET
//! /api/tasks/{id}` to poll, `DELETE /api/tasks/{id}` to cancel. Phase 5
//! adds a typed half to it, defined here once so both ends agree:
//!
//! - a [`TaskSubmission`] may carry the work item it runs, the tools the
//!   node must activate before its first model call (`required_tools`), the
//!   typed [`Brief`], and the controller's `run` id, which makes the
//!   submission idempotent: a controller that restarted mid-run resubmits
//!   its brief and gets back the task the node already has;
//! - a required tool outside the node's ceiling is refused, never dropped:
//!   `422` with a [`CeilingRefused`] naming each tool and why
//!   ([`RefusalReason`]);
//! - a [`TaskView`] carries the typed result (`report`, the node's
//!   `result_json`) and what the run spent (`usage`); a run that ended
//!   without a report still returns one, whose `error` is the failure as
//!   the node classified it ([`failed_run_report`]);
//! - `GET /api/node` answers with a [`NodeAdvertisement`]: the node's
//!   models, the tools and MCP servers a delegated run may use there, and
//!   its machine, which the registry records on the peer's `workers` row
//!   and refreshes with its health. Pairing (`POST /api/pair`) returns the
//!   same advertisement beside the new device token.
//!
//! Credentials stay on the node that holds them (section 12.1): nothing in
//! a submission is a secret. The controller authenticates with the token it
//! was given or paired for, in the `Authorization` header only, and keeps
//! that token in its encrypted secret store ([`token_secret`]), never in the
//! worker's stored spec or its API view.
//!
//! [`NodeWorkers`] is the node side's seam: the worker that runs one
//! structured task inside the ceiling the node's policy gives it.

use std::sync::Arc;

use rustykrab_core::work::{ResultReport, WorkerKind};
use rustykrab_core::Error;
use serde::{Deserialize, Serialize};

use crate::errors::{classify, Context};
use crate::worker::{run_failure_input, Brief, RunUsage, Worker, WorkerCapabilities};

/// The delegated-task routes, under a node's base URL.
pub const TASKS_PATH: &str = "/api/tasks";
/// The advertisement route.
pub const NODE_PATH: &str = "/api/node";
/// The `error` of a 422 refusal.
pub const OUTSIDE_CEILING: &str = "outside_ceiling";

/// The secret a peer worker's token is kept under, in the controller's
/// encrypted secret store.
pub fn token_secret(worker: &str) -> String {
    format!("worker.{worker}.token")
}

/// `POST /api/tasks`. The free-text fields are the ones the `nodes` tool
/// sends; the rest are the peer worker's typed half. camelCase on the wire.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSubmission {
    /// The instruction. For a structured task, the brief rendered as text,
    /// which a node without structured delegation runs as it stands.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    /// Further delegation hops the task may make; absent is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hop_budget: Option<i64>,
    /// A tighter tool limit than the node's own, never a wider one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// The work item the task runs, on the submitting controller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item_id: Option<String>,
    /// Activated before the run's first model call, inside the node's
    /// ceiling; one outside it refuses the submission.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_tools: Vec<String>,
    /// The typed brief. Present: a structured task, whose result is typed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief: Option<Brief>,
    /// The controller's run id: one task per run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
}

/// A delegated task as `GET /api/tasks/{id}` shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskView {
    pub id: String,
    /// `queued | running | done | failed | cancelled`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// Seconds the task has been alive.
    #[serde(default)]
    pub elapsed_secs: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// Times the node claimed it; above one, a restart interrupted a run
    /// and the task went back to the queue.
    #[serde(default)]
    pub attempts: u32,
    /// The typed result of a structured task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<ResultReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<RunUsage>,
}

impl TaskView {
    /// Whether the node will change the task no further.
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "done" | "failed" | "cancelled")
    }
}

/// Why a node refuses a required tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// The node has no such tool.
    Unknown,
    /// Withheld from every delegated run: the credential family, outbound
    /// messaging, the sub-agent family.
    AlwaysDenied,
    /// Outside the node's own delegation allowlist.
    NodePolicy,
    /// Outside the tighter limit the submission itself asked for.
    TaskLimit,
    /// Onward delegation, with no hops left.
    HopBudget,
}

impl RefusalReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            RefusalReason::Unknown => "unknown",
            RefusalReason::AlwaysDenied => "always_denied",
            RefusalReason::NodePolicy => "node_policy",
            RefusalReason::TaskLimit => "task_limit",
            RefusalReason::HopBudget => "hop_budget",
        }
    }
}

/// One required tool a node refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub tool: String,
    pub reason: RefusalReason,
}

/// The `422` body of a submission whose `required_tools` reach outside the
/// node's ceiling: every refused tool, with why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CeilingRefused {
    /// Always [`OUTSIDE_CEILING`].
    pub error: String,
    pub message: String,
    pub refused: Vec<Refusal>,
}

impl CeilingRefused {
    pub fn new(refused: Vec<Refusal>) -> CeilingRefused {
        let named: Vec<String> = refused
            .iter()
            .map(|r| format!("{} ({})", r.tool, r.reason.as_str()))
            .collect();
        CeilingRefused {
            error: OUTSIDE_CEILING.to_string(),
            message: format!(
                "required tools outside this node's ceiling: {}",
                named.join(", ")
            ),
            refused,
        }
    }
}

/// `GET /api/node`: what a delegated run may use on this node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeAdvertisement {
    /// Whether the node takes structured submissions. A node that does not
    /// runs the text of one and returns text, which a peer worker reports
    /// as a result it cannot use.
    pub structured: bool,
    /// Models, the tools and MCP servers inside the delegation ceiling, the
    /// machine, and the resources a delegated run may write.
    pub capabilities: WorkerCapabilities,
    /// The node's build.
    #[serde(default)]
    pub version: String,
}

/// How a node runs a structured delegated task (the node side of Phase 5).
/// The composition root implements it over the local worker kind.
pub trait NodeWorkers: Send + Sync {
    /// A worker whose tool ceiling is exactly `allowed`: the node's
    /// delegation ceiling for one task. It activates the brief's
    /// `required_tools` before its first model call and returns the typed
    /// result.
    fn worker(&self, allowed: &[String]) -> Arc<dyn Worker>;

    /// What the node advertises for delegated runs whose ceiling is
    /// `ceiling`.
    fn advertise(&self, ceiling: &[String]) -> WorkerCapabilities;
}

/// The typed result a node returns for a structured run that ended without
/// a report: an empty-handed summary and the failure as the node's own
/// classifier types it, from where the evidence is. A typed `RunFailure`
/// (a spent budget, a model that never reported, a tool outside the
/// ceiling) keeps its class.
pub fn failed_run_report(node: &str, err: &Error) -> ResultReport {
    let error = classify(
        &run_failure_input(err),
        &Context {
            tool: None,
            worker_kind: Some(WorkerKind::Peer),
        },
    );
    ResultReport {
        summary: format!("the run on {node} ended without a result report"),
        error: Some(error),
        ..ResultReport::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::BudgetKind;
    use crate::worker::RunFailure;
    use rustykrab_core::work::{ErrorClass, ErrorSubclass};

    #[test]
    fn a_free_text_submission_is_the_nodes_tool_shape() {
        let wire: TaskSubmission =
            serde_json::from_value(serde_json::json!({ "message": "do it", "hopBudget": 0 }))
                .unwrap();
        assert_eq!(wire.message, "do it");
        assert!(wire.brief.is_none() && wire.required_tools.is_empty());
        let out = serde_json::to_value(&wire).unwrap();
        assert_eq!(
            out,
            serde_json::json!({ "message": "do it", "hopBudget": 0 })
        );
    }

    #[test]
    fn a_refusal_names_every_tool_and_why() {
        let body = CeilingRefused::new(vec![
            Refusal {
                tool: "credential_read".into(),
                reason: RefusalReason::AlwaysDenied,
            },
            Refusal {
                tool: "teleport".into(),
                reason: RefusalReason::Unknown,
            },
        ]);
        let wire = serde_json::to_value(&body).unwrap();
        assert_eq!(wire["error"], OUTSIDE_CEILING);
        assert_eq!(wire["refused"][0]["reason"], "always_denied");
        assert!(body.message.contains("teleport (unknown)"));
    }

    #[test]
    fn a_failed_run_keeps_its_type_across_the_wire() {
        let err = RunFailure::Budget {
            budget: BudgetKind::Wall,
            detail: "600s wall budget spent".into(),
        }
        .into_error();
        let report = failed_run_report("krabby", &err);
        let error = report.error.expect("typed");
        assert_eq!(error.class, ErrorClass::Budget);
        assert_eq!(error.subclass, ErrorSubclass::Wall);
        assert!(report.summary.contains("krabby"));
    }

    #[test]
    fn the_token_secret_is_a_valid_secret_name() {
        let name = token_secret("krabby-2");
        assert!(name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')));
    }
}
