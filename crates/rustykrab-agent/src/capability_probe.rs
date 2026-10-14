//! Registered host observations through the local runner's real tool boundary.
//! No model call, arbitrary shell replay or raw/private result in the receipt.
use crate::runner::{execute_single_tool, ToolIndex};
use crate::sandbox::Sandbox;
use rustykrab_control::capability_proof::{
    OperationProbe, ProbeOutcome, ProbeRequest, RegisteredAction,
};
use rustykrab_core::{capability::CapabilitySet, types::ToolCall, Tool};
use serde_json::{json, Value};
use std::sync::Arc;

fn call(request: &ProbeRequest) -> Option<ToolCall> {
    let (name, arguments) = match &request.action {
        RegisteredAction::BinaryAvailable { binary }
            if ["cargo", "node"].contains(&binary.as_str()) =>
        {
            (
                "exec",
                json!({"command":format!("{binary} --version"),"timeout_secs":30}),
            )
        }
        RegisteredAction::ToolCallable { tool, args }
            if tool == "work_resources" && *args == json!({}) =>
        {
            ("work_resources", args.clone())
        }
        _ => return None,
    };
    Some(ToolCall {
        id: request.expected.run.clone(),
        name: name.into(),
        arguments,
    })
}

fn version_line(stdout: &str, binary: &str) -> bool {
    let prefix = match binary {
        "cargo" => "cargo ",
        "node" => "v",
        _ => return false,
    };
    stdout.lines().any(|line| {
        line.strip_prefix(prefix).is_some_and(|v| {
            let mut parts = v.split('.');
            [parts.next(), parts.next()]
                .into_iter()
                .all(|p| p.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())))
        })
    })
}

fn postcondition(action: &RegisteredAction, output: &Value) -> bool {
    match action {
        RegisteredAction::BinaryAvailable { binary } => {
            output.get("exit_code").and_then(Value::as_i64) == Some(0)
                && output
                    .get("stdout")
                    .and_then(Value::as_str)
                    .is_some_and(|s| version_line(s, binary))
        }
        RegisteredAction::ToolCallable { tool, .. } => {
            tool == "work_resources"
                && output.is_object()
                && output.get("workers").is_some_and(Value::is_array)
        }
    }
}

pub(crate) async fn observe(
    request: &ProbeRequest,
    tools: &[Arc<dyn Tool>],
    ceiling: &[String],
    sandbox: &Arc<dyn Sandbox>,
) -> OperationProbe {
    let report = |outcome, observation| request.expected.observation(outcome, observation);
    let Some(call) = call(request) else {
        return report(
            ProbeOutcome::Unobserved,
            "operation/arguments have no registered probe",
        );
    };
    let Some(tool) = tools.iter().find(|t| t.name() == call.name) else {
        return report(
            ProbeOutcome::Unobserved,
            "registered tool is absent from this worker",
        );
    };
    if !tool.available() {
        return report(
            ProbeOutcome::Unobserved,
            "registered tool is currently unavailable",
        );
    }
    if call.name == "work_resources" && tool.sandbox_requirements().has_side_effects() {
        return report(
            ProbeOutcome::Unobserved,
            "tool requirements exceed the registered read-only operation",
        );
    }
    let names: Vec<_> = ceiling.iter().map(String::as_str).collect();
    let capabilities = CapabilitySet::for_tools_permissive(&names);
    match execute_single_tool(
        &call,
        &ToolIndex::new(tools),
        sandbox,
        &capabilities,
        uuid::Uuid::new_v4(),
    )
    .await
    {
        Ok(result) if !result.is_error && postcondition(&request.action, &result.output) => report(
            ProbeOutcome::Ok,
            "registered operation executed; observed postcondition matched",
        ),
        Ok(_) => report(
            ProbeOutcome::Failed,
            "registered operation executed; observed postcondition failed",
        ),
        Err(_) => report(
            ProbeOutcome::Failed,
            "registered operation failed at the worker tool boundary",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registered_postconditions_require_observed_success_not_rust_ok() {
        for (binary, line) in [("cargo", "cargo 1.90.0 (fixture)"), ("node", "v24.21.0")] {
            let action = RegisteredAction::BinaryAvailable {
                binary: binary.into(),
            };
            assert!(postcondition(
                &action,
                &json!({"exit_code":0,"stdout":line})
            ));
            for bad in [
                json!({"exit_code":1,"stdout":line}),
                json!({"exit_code":0,"stdout":"unrelated successful response"}),
                json!({"stdout":line}),
                json!({"exit_code":0,"stdout":"cargo not-installed"}),
            ] {
                assert!(!postcondition(&action, &bad));
            }
        }
        let action = RegisteredAction::ToolCallable {
            tool: "work_resources".into(),
            args: json!({}),
        };
        assert!(postcondition(&action, &json!({"workers":[]})));
        for bad in [
            json!({"error":"denied"}),
            json!({"workers":"not an array"}),
            json!([]),
        ] {
            assert!(!postcondition(&action, &bad));
        }
    }
}
