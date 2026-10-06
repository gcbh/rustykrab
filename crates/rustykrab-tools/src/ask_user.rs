//! `ask_user` and `capability_request`: a worker's typed questions (plan
//! sections 7 and 14).
//!
//! `ask_user` files one question with a class and options. The model only
//! asks: the controller's router classifies, and a question it can answer
//! now (a recorded default, a decision standing judgment covers) comes
//! straight back in the result so the run goes on. Only a blocking-now
//! question outside delegated authority reaches the user; then the item
//! parks, the run ends, and the item (not the conversation) resumes with
//! the answer.
//!
//! `capability_request` generalises `credential_request` to anything the
//! worker cannot proceed without: a credential or consent only the user can
//! give parks the item on a question, and a tool, install, compute or
//! knowledge gap goes to the ladder's order 2. Either way the item parks and
//! wakes when the need is met, which is `credential_request`'s park-and-wake
//! applied to work items.
//!
//! Both are for worker runs only: outside one there is no item to park, and
//! a conversation asks the user in its reply. A parked answer carries
//! `{"ok": true, "summary"}`, the run-ending shape, so the worker's run
//! ends on it.

use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::questions::{QuestionClass, QuestionKind};
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{validate_tool_args, Error, Result, Tool, ToolError};
use serde_json::{json, Value};

use crate::work_backend::{
    host_provenance, with_work_run, AskOutcome, AskRequest, CapabilityAsk, WorkBackend,
};
use crate::work_file::{
    check_keys, take_opt_text, take_text, take_text_list, Problem, ENTRY_MAX, NAME_MAX,
};

const TEXT_MAX: usize = 500;
const OPTIONS_MAX: usize = 8;
const CAPABILITY_KINDS: [&str; 6] = [
    "credential",
    "consent",
    "tool",
    "install",
    "compute",
    "knowledge",
];

fn not_in_a_run(tool: &str) -> Error {
    Error::ToolExecution(ToolError::invalid_input(format!(
        "{tool} is for a worker's run on a work item; in a conversation, ask the user in your \
         reply"
    )))
}

fn refused(problems: &[Problem]) -> Value {
    json!({
        "outcome": "rejected",
        "failed": problems.iter().map(Problem::line).collect::<Vec<_>>(),
        "next": "fix every failed check, then call again",
    })
}

/// The answer as the model reads it.
fn render(outcome: &AskOutcome) -> Value {
    let mut out = json!({
        "question": outcome.question,
        "class": outcome.class.as_str(),
        "note": outcome.note,
    });
    if let Some(answer) = &outcome.answer {
        out["answered"] = json!(true);
        out["answer"] = json!(answer);
        if let Some(by) = &outcome.answered_by {
            out["answered_by"] = json!(by);
        }
        out["next"] = json!("use this answer and carry on with the item");
    } else if outcome.parked {
        out["ok"] = json!(true);
        out["parked"] = json!(true);
        out["summary"] = json!(if outcome.note.is_empty() {
            "Parked: the item resumes with the answer.".to_string()
        } else {
            outcome.note.clone()
        });
    } else {
        out["answered"] = json!(false);
        out["next"] = json!("carry on; the answer will reach a later step");
    }
    out
}

/// Files a worker's typed question through a [`WorkBackend`].
pub struct AskUserTool {
    backend: Arc<dyn WorkBackend>,
}

impl AskUserTool {
    pub fn new(backend: Arc<dyn WorkBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user ONE question you cannot answer yourself, typed: text, class \
         (blocking_now: nothing moves without it; blocking_later: needed for a later step; \
         researchable: a fact that can be looked up; defaultable: it has a sensible default, \
         put it first in options), kind (decision or consent) and options. The controller \
         decides who answers: a default or standing judgment answers at once and the result \
         gives you the answer; only a real decision reaches the user, and then your run ends \
         and the item resumes with the answer."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The question, one or two sentences." },
                    "class": {
                        "type": "string",
                        "enum": QuestionClass::ALL.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
                    },
                    "kind": { "type": "string", "enum": ["decision", "consent"] },
                    "options": { "type": "array", "items": { "type": "string" }, "description": "The choices, most likely first." },
                    "default": { "type": "string", "description": "The recorded default, if the question has one." }
                },
                "required": ["text"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let schema = self.schema();
        validate_tool_args(&schema.parameters, &args).map_err(Error::ToolExecution)?;
        let Some(item) = with_work_run(|r| r.item.clone()) else {
            return Err(not_in_a_run("ask_user"));
        };
        let obj = args.as_object().cloned().unwrap_or_default();
        let mut problems = Vec::new();
        check_keys(
            &obj,
            &["text", "class", "kind", "options", "default"],
            "",
            &mut problems,
        );
        let text = take_text(&obj, "text", "", TEXT_MAX, true, &mut problems);
        let class = take_opt_text(&obj, "class", "", NAME_MAX, &mut problems);
        let kind = match take_opt_text(&obj, "kind", "", NAME_MAX, &mut problems) {
            None => QuestionKind::Decision,
            Some(raw) => match QuestionKind::parse(&raw) {
                Some(k @ (QuestionKind::Decision | QuestionKind::Consent)) => k,
                _ => {
                    problems.push(Problem::invalid(
                        "",
                        "kind",
                        "must be decision or consent; for a credential use capability_request",
                    ));
                    QuestionKind::Decision
                }
            },
        };
        let options = take_text_list(&obj, "options", "", OPTIONS_MAX, ENTRY_MAX, &mut problems);
        let default = take_opt_text(&obj, "default", "", ENTRY_MAX, &mut problems);
        if !problems.is_empty() {
            return Ok(refused(&problems));
        }
        let request = AskRequest {
            text,
            class,
            kind,
            options,
            default,
        };
        let outcome = self
            .backend
            .ask(item, request, host_provenance())
            .await
            .map_err(|e| {
                Error::ToolExecution(ToolError::internal(format!("ask_user failed: {e}")))
            })?;
        Ok(render(&outcome))
    }
}

/// Parks a worker on a capability through a [`WorkBackend`].
pub struct CapabilityRequestTool {
    backend: Arc<dyn WorkBackend>,
}

impl CapabilityRequestTool {
    pub fn new(backend: Arc<dyn WorkBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for CapabilityRequestTool {
    fn name(&self) -> &str {
        "capability_request"
    }

    fn description(&self) -> &str {
        "Stop and ask for something this item cannot proceed without: kind credential (a \
         login or key only the user holds), consent (a yes the user must give), tool, install, \
         compute or knowledge (a gap the system can close itself). Name it and say why. The \
         item parks and resumes when the need is met; your run ends here. If a tool is merely \
         not loaded, load it with tools_load instead."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": CAPABILITY_KINDS },
                    "name": { "type": "string", "description": "The credential, tool, resource or topic, by name." },
                    "reason": { "type": "string", "description": "Why the item needs it, one sentence." }
                },
                "required": ["kind", "name"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let schema = self.schema();
        validate_tool_args(&schema.parameters, &args).map_err(Error::ToolExecution)?;
        let Some(item) = with_work_run(|r| r.item.clone()) else {
            return Err(not_in_a_run("capability_request"));
        };
        let obj = args.as_object().cloned().unwrap_or_default();
        let mut problems = Vec::new();
        check_keys(&obj, &["kind", "name", "reason"], "", &mut problems);
        let kind = take_text(&obj, "kind", "", NAME_MAX, true, &mut problems);
        if !kind.is_empty() && !CAPABILITY_KINDS.contains(&kind.as_str()) {
            problems.push(Problem::invalid(
                "",
                "kind",
                format!("`{kind}` is not one of {}", CAPABILITY_KINDS.join(", ")),
            ));
        }
        let name = take_text(&obj, "name", "", NAME_MAX * 2, true, &mut problems);
        let reason = take_opt_text(&obj, "reason", "", TEXT_MAX, &mut problems).unwrap_or_default();
        if !problems.is_empty() {
            return Ok(refused(&problems));
        }
        let outcome = self
            .backend
            .request_capability(
                item,
                CapabilityAsk { kind, name, reason },
                host_provenance(),
            )
            .await
            .map_err(|e| {
                Error::ToolExecution(ToolError::internal(format!(
                    "capability_request failed: {e}"
                )))
            })?;
        Ok(render(&outcome))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_backend::{StubWorkBackend, WorkCall, WorkRunContext, WORK_RUN_CONTEXT};

    fn run() -> WorkRunContext {
        WorkRunContext {
            item: "item-1".into(),
            actor: "worker:pinch".into(),
        }
    }

    #[tokio::test]
    async fn a_question_outside_a_worker_run_is_refused() {
        let stub = Arc::new(StubWorkBackend::new());
        let err = AskUserTool::new(stub.clone())
            .execute(json!({ "text": "Which florist?" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("worker"), "{err}");
        assert!(stub.calls().is_empty());
    }

    #[tokio::test]
    async fn a_parked_question_ends_the_run_and_carries_the_typed_request() {
        let stub = Arc::new(StubWorkBackend::new());
        let out = WORK_RUN_CONTEXT
            .scope(
                run(),
                AskUserTool::new(stub.clone()).execute(json!({
                    "text": "Which florist, Petals or Stems?",
                    "class": "blocking_now",
                    "options": ["Petals", "Stems"],
                })),
            )
            .await
            .unwrap();
        assert_eq!(out["parked"], true);
        assert!(crate::worker_run_end_summary("ask_user", &out).is_some());
        let WorkCall::Ask { item, request, .. } = &stub.calls()[0] else {
            panic!("no ask");
        };
        assert_eq!(item, "item-1");
        assert_eq!(request.options, vec!["Petals", "Stems"]);
        assert_eq!(request.class.as_deref(), Some("blocking_now"));
    }

    #[test]
    fn an_answered_question_does_not_end_the_run() {
        let out = render(&AskOutcome {
            question: "q1".into(),
            class: QuestionClass::Defaultable,
            answer: Some("9am".into()),
            answered_by: Some("default".into()),
            parked: false,
            note: "answered with its recorded default".into(),
        });
        assert_eq!(out["answer"], "9am");
        assert!(crate::worker_run_end_summary("ask_user", &out).is_none());
    }

    #[tokio::test]
    async fn a_capability_request_names_its_kind() {
        let stub = Arc::new(StubWorkBackend::new());
        let tool = CapabilityRequestTool::new(stub.clone());
        let bad = WORK_RUN_CONTEXT
            .scope(run(), tool.execute(json!({ "kind": "magic", "name": "x" })))
            .await;
        assert!(bad.is_err(), "an unknown kind fails the schema");
        assert!(stub.calls().is_empty());
        let out = WORK_RUN_CONTEXT
            .scope(
                run(),
                tool.execute(json!({ "kind": "credential", "name": "carrier login", "reason": "to switch plans" })),
            )
            .await
            .unwrap();
        assert_eq!(out["parked"], true);
        assert!(
            matches!(&stub.calls()[0], WorkCall::Capability { request, .. } if request.kind == "credential")
        );
    }
}
