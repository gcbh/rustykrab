//! `work_plan`: file a whole graph of work items, typed edges and parent
//! links in one atomic call (plan sections 6.1 and 14.1).
//!
//! The planner's one call, and the orchestration conversation's. The host
//! reads the call into the shared [`WorkPlan`] shape and hands it to the
//! [`WorkBackend`], whose controller validates the whole graph before
//! writing anything: accepted whole with real ids, or rejected whole with
//! every failed check and the temp ids it names, so the planner fixes the
//! graph in the same run (an append, not a re-prefill, section 12).
//!
//! The answer is the filing outcome itself (`outcome`, then `root`, `ids`,
//! `held`, `warnings`, or `failed`), so the transcript records exactly what
//! the controller decided. An accepted graph also carries `{"ok": true,
//! "summary"}`, the run-ending shape: in a worker run (the planner's) an
//! accepted plan ends the run, since the planner's output is one accepted
//! `work_plan` call.

use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::work::{ItemRef, PlanOutcome, WorkPlan};
use rustykrab_core::{validate_tool_args, Error, Result, Tool, ToolError};
use serde_json::{json, Value};

use crate::work_backend::{host_provenance, WorkBackend};
use crate::work_file::{check_keys, Problem};

/// Top-level keys a call may carry.
const PLAN_KEYS: [&str; 4] = ["root", "items", "edges", "rationale"];
/// Items one call may carry before the controller's own cap refuses it.
const ITEMS_MAX: usize = 24;

/// Files a whole graph through a [`WorkBackend`].
pub struct WorkPlanTool {
    backend: Arc<dyn WorkBackend>,
}

impl WorkPlanTool {
    pub fn new(backend: Arc<dyn WorkBackend>) -> Self {
        Self { backend }
    }
}

fn rejected(failed: Vec<String>) -> Value {
    json!({
        "outcome": "rejected",
        "failed": failed.into_iter().map(|detail| json!({ "reason": "invalid_item", "detail": detail })).collect::<Vec<_>>(),
        "next": "fix every failed check, then call work_plan again",
    })
}

/// The outcome as the controller returned it, plus the run-ending shape on
/// acceptance and the next step on rejection.
fn render(outcome: &PlanOutcome) -> Value {
    let mut out = serde_json::to_value(outcome).unwrap_or_else(|_| json!({}));
    match outcome {
        PlanOutcome::Accepted(a) => {
            out["ok"] = json!(true);
            let mut summary = format!("Filed the plan: {} items under {}", a.ids.len(), a.root);
            if !a.held.is_empty() {
                summary.push_str(&format!("; {} held until the user approves", a.held.len()));
            }
            out["summary"] = json!(summary);
        }
        PlanOutcome::Rejected(_) => {
            out["next"] = json!(
                "nothing was filed; fix every failed check (the temp ids named are yours) and \
                 call work_plan again"
            );
        }
    }
    out
}

#[async_trait]
impl Tool for WorkPlanTool {
    fn name(&self) -> &str {
        "work_plan"
    }

    fn description(&self) -> &str {
        "File the WHOLE graph for a multi-step request in ONE call: items (each with a tmp \
         name, title, objective, done_when, and a budget), typed edges ({item, kind: blocks | \
         waits_for | conditional_on_failure | supersedes, depends_on}) and parent links. Refer \
         to new items as {\"tmp\": name} and existing ones by id. A step is its own item only \
         for a wait on the world, independent fan-out, a different worker or writable \
         resource, an approval point, or a plan B; everything else stays inside one item. The \
         controller accepts the graph whole or rejects it whole with every failed check."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "root": {
                        "description": "An existing item id (new items without a parent go under it), or {\"tmp\": name} of the one new item with no parent."
                    },
                    "items": {
                        "type": "array",
                        "description": "The new items. Each: tmp, title, objective, done_when, budget {iterations, tokens, wall_seconds, repairs}; optional parent ({\"tmp\"} or id), kind (personal | research), constraints, decisions_made, artifact_refs, required_tools, writable_resources, inputs_from, trigger, expires_at, priority.",
                        "items": { "type": "object" }
                    },
                    "edges": {
                        "type": "array",
                        "description": "{item, kind, depends_on}: {item: b, kind: blocks, depends_on: a} means a blocks b.",
                        "items": { "type": "object" }
                    },
                    "rationale": {
                        "type": "string",
                        "description": "One line, shown in the plan preview."
                    }
                },
                "required": ["root", "items"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let schema = self.schema();
        validate_tool_args(&schema.parameters, &args).map_err(Error::ToolExecution)?;
        let Some(obj) = args.as_object() else {
            return Ok(rejected(vec!["arguments must be an object".into()]));
        };
        let mut problems: Vec<Problem> = Vec::new();
        check_keys(obj, &PLAN_KEYS, "", &mut problems);
        if obj
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|items| items.len() > ITEMS_MAX)
        {
            problems.push(Problem::invalid(
                "",
                "items",
                format!("more than {ITEMS_MAX} items; a plan is small"),
            ));
        }
        if !problems.is_empty() {
            return Ok(rejected(problems.iter().map(|p| p.line()).collect()));
        }
        let plan: WorkPlan = match serde_json::from_value(args.clone()) {
            Ok(plan) => plan,
            Err(e) => {
                return Ok(rejected(vec![format!(
                    "the graph does not read as a plan: {e}"
                )]))
            }
        };
        if let ItemRef::Id(id) = &plan.root {
            if id.trim().is_empty() {
                return Ok(rejected(vec!["root: is empty".into()]));
            }
        }
        let outcome = self
            .backend
            .plan(plan, host_provenance())
            .await
            .map_err(|e| {
                Error::ToolExecution(ToolError::internal(format!("work_plan failed: {e}")))
            })?;
        Ok(render(&outcome))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_backend::{StubWorkBackend, WorkCall};
    use rustykrab_core::work::{FailedCheck, PlanRejected, RejectionReason};

    fn call() -> Value {
        json!({
            "root": { "tmp": "trip" },
            "items": [
                { "tmp": "trip", "parent": null, "kind": "personal", "title": "Trip",
                  "objective": "o", "done_when": "d",
                  "budget": { "iterations": 100, "tokens": 200000, "wall_seconds": 6000, "repairs": 2 } },
                { "tmp": "flights", "parent": { "tmp": "trip" }, "title": "Flights",
                  "objective": "o", "done_when": "d" }
            ],
            "edges": [],
            "rationale": "book it"
        })
    }

    #[tokio::test]
    async fn an_accepted_plan_reads_as_the_outcome_and_ends_the_run() {
        let stub = Arc::new(StubWorkBackend::new());
        let out = WorkPlanTool::new(stub.clone())
            .execute(call())
            .await
            .unwrap();
        assert_eq!(out["outcome"], "accepted", "{out}");
        assert_eq!(out["ok"], true);
        assert!(crate::worker_run_end_summary("work_plan", &out).is_some());
        let parsed: PlanOutcome = serde_json::from_value(out).unwrap();
        assert!(matches!(parsed, PlanOutcome::Accepted(a) if a.ids.len() == 2));
        assert!(matches!(stub.calls()[0], WorkCall::Plan { .. }));
    }

    #[tokio::test]
    async fn a_rejection_names_every_failed_check_and_does_not_end_the_run() {
        let stub = Arc::new(StubWorkBackend::new());
        stub.push_outcome(PlanOutcome::Rejected(PlanRejected {
            failed: vec![FailedCheck {
                reason: RejectionReason::Cycle,
                offending: vec![ItemRef::Tmp {
                    tmp: "flights".into(),
                }],
                detail: "a blocks edge between an item and its own descendant".into(),
            }],
        }));
        let out = WorkPlanTool::new(stub).execute(call()).await.unwrap();
        assert_eq!(out["outcome"], "rejected");
        assert_eq!(out["failed"][0]["reason"], "cycle");
        assert!(crate::worker_run_end_summary("work_plan", &out).is_none());
        let parsed: PlanOutcome = serde_json::from_value(out).unwrap();
        assert!(matches!(parsed, PlanOutcome::Rejected(_)));
    }

    #[tokio::test]
    async fn host_owned_keys_and_unreadable_graphs_are_refused_before_the_backend() {
        let stub = Arc::new(StubWorkBackend::new());
        let tool = WorkPlanTool::new(stub.clone());
        let mut forged = call();
        forged["status"] = json!("done");
        assert!(tool.execute(forged).await.is_err() || stub.calls().is_empty());
        let mut broken = call();
        broken["items"][0]["title"] = json!(7);
        let out = tool.execute(broken).await.unwrap();
        assert_eq!(out["outcome"], "rejected");
        assert!(stub.calls().is_empty());
    }
}
