use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::memory_backend::MemoryBackend;

/// A tool that retrieves a specific memory entry by ID.
pub struct MemoryGetTool {
    backend: Arc<dyn MemoryBackend>,
}

impl MemoryGetTool {
    pub fn new(backend: Arc<dyn MemoryBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for MemoryGetTool {
    fn name(&self) -> &str {
        "memory_get"
    }

    fn description(&self) -> &str {
        "Retrieve a specific memory entry by ID."
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
                        "description": "The unique identifier of the memory entry"
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

        let entry = self
            .backend
            .get(memory_id)
            .await
            .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

        Ok(entry)
    }
}
