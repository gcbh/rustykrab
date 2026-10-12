//! Read/write the discussion attached to an existing work item.
use crate::work_backend::{host_principal, host_provenance, WorkBackend};
use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{work::WorkEvent, Error, Result, Tool, ToolError};
use rustykrab_store::QuestionRow;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkDiscussion {
    pub item: String,
    /// Newest first; offset and total let a reader retrieve older context.
    pub comments: Vec<WorkEvent>,
    pub activity: Vec<WorkEvent>,
    pub questions: Vec<QuestionRow>,
    pub total_comments: usize,
    pub offset: usize,
}
pub struct WorkDiscussTool {
    backend: Arc<dyn WorkBackend>,
}
impl WorkDiscussTool {
    pub fn new(backend: Arc<dyn WorkBackend>) -> Self {
        Self { backend }
    }
}
#[async_trait]
impl Tool for WorkDiscussTool {
    fn name(&self) -> &str {
        "work_discuss"
    }
    fn description(&self) -> &str {
        "Read or append a comment to an existing work item. Preserve user direction and agent updates here across runs. A comment changes no status and grants no consent; pending questions still require explicit answers. Read older comments with offset/limit. Author comes from the host."
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().into(),
            description: self.description().into(),
            parameters: json!({"type":"object","properties":{"item":{"type":"string"},"action":{"type":"string","enum":["read","comment"]},"text":{"type":"string","maxLength":4096},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":20}},"required":["item","action"],"additionalProperties":false}),
        }
    }
    async fn execute(&self, args: Value) -> Result<Value> {
        rustykrab_core::schema_validate::validate_tool_args(&self.schema().parameters, &args)
            .map_err(Error::ToolExecution)?;
        let item = args["item"]
            .as_str()
            .unwrap()
            .trim()
            .trim_start_matches('#');
        if item.is_empty() {
            return Err(Error::ToolExecution(ToolError::invalid_input(
                "Name an existing work item.",
            )));
        }
        if args["action"] == "comment" {
            let text = args["text"].as_str().unwrap_or_default();
            Ok(json!({"comment":self.backend.comment(item, text, host_provenance()).await?}))
        } else {
            Ok(serde_json::to_value(
                self.backend
                    .discussion(
                        item,
                        args["offset"].as_u64().unwrap_or(0) as usize,
                        args["limit"].as_u64().unwrap_or(4) as usize,
                        &host_principal(),
                    )
                    .await?,
            )
            .map_err(|e| Error::Internal(e.to_string()))?)
        }
    }
}
