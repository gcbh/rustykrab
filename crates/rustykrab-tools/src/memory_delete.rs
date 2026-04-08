use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::memory_backend::MemoryBackend;

/// A tool that deletes a stale or incorrect memory entry.
///
/// Implements adaptive forgetting: the agent can prune outdated
/// information to keep retrieval clean and reduce interference.
pub struct MemoryDeleteTool {
    backend: Arc<dyn MemoryBackend>,
}

impl MemoryDeleteTool {
    pub fn new(backend: Arc<dyn MemoryBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for MemoryDeleteTool {
    fn name(&self) -> &str {
        "memory_delete"
    }

    fn description(&self) -> &str {
        "Delete a stale or incorrect memory entry by ID. \
         Use this to prune outdated facts that are no longer true."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "memory_id": {
                        "type": "string",
                        "description": "The unique identifier of the memory entry to delete"
                    }
                },
                "required": ["memory_id"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let memory_id = args["memory_id"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing memory_id".into()))?;

        self.backend.delete(memory_id).await
    }
}
