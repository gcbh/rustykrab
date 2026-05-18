use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use serde_json::{json, Value};

/// Explicit "I'm done" signal the model emits when the user's request is
/// fully handled. The runner watches for a successful call to this tool,
/// surfaces the supplied `summary` as the assistant's final message, and
/// then terminates the loop — so a text-only response (which Ollama maps
/// to `StopReason::EndTurn` regardless of intent) no longer doubles as
/// "task finished."
pub struct TaskCompleteTool;

impl TaskCompleteTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TaskCompleteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TaskCompleteTool {
    fn name(&self) -> &str {
        "task_complete"
    }

    fn description(&self) -> &str {
        "Signal that the user's request is fully handled and you are ready to send \
         your final answer. The `summary` you pass becomes the message the user sees, \
         so make it complete and self-contained — the deliverable itself, not a \
         meta-description of what you did. Call this ONLY when every step of the task \
         is done; if work remains, call the next tool instead. The agent loop stops as \
         soon as this call returns."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "The final answer for the user — the deliverable itself, \
                                        not a meta-description of what you did."
                    }
                },
                "required": ["summary"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if summary.is_empty() {
            return Err(Error::ToolExecution(
                "task_complete requires a non-empty `summary` describing the final answer".into(),
            ));
        }
        Ok(json!({ "ok": true, "summary": summary }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_missing_summary() {
        let tool = TaskCompleteTool::new();
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("summary"));
    }

    #[tokio::test]
    async fn rejects_empty_summary() {
        let tool = TaskCompleteTool::new();
        let err = tool.execute(json!({ "summary": "   " })).await.unwrap_err();
        assert!(err.to_string().contains("summary"));
    }

    #[tokio::test]
    async fn echoes_summary_back() {
        let tool = TaskCompleteTool::new();
        let out = tool
            .execute(json!({ "summary": "found 3 hotels" }))
            .await
            .unwrap();
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["summary"], json!("found 3 hotels"));
    }

    #[test]
    fn schema_advertises_required_summary() {
        let schema = TaskCompleteTool::new().schema();
        assert_eq!(schema.name, "task_complete");
        let required = schema.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "summary"));
    }
}
