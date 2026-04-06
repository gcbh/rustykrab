use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::memory_backend::MemoryBackend;

/// A tool that saves a fact to associative memory with tags.
///
/// The agent calls this when it encounters information worth
/// remembering. Tags should capture the concepts that should
/// trigger recall of this fact in future turns.
pub struct MemorySaveTool {
    backend: Arc<dyn MemoryBackend>,
}

impl MemorySaveTool {
    pub fn new(backend: Arc<dyn MemoryBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for MemorySaveTool {
    fn name(&self) -> &str {
        "memory_save"
    }

    fn description(&self) -> &str {
        "Save an important fact to long-term memory with association tags. \
         Use this to remember decisions, user preferences, errors, or any \
         information you may need later. Tags should be words/concepts that \
         would trigger recall of this fact."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "fact": {
                        "type": "string",
                        "description": "The fact or knowledge to remember"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Association tags — words/concepts that should trigger recall of this fact"
                    }
                },
                "required": ["fact", "tags"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let fact = args["fact"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing fact".into()))?;

        let tags: Vec<String> = args["tags"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        if tags.is_empty() {
            return Err(openclaw_core::Error::ToolExecution(
                "at least one tag is required".into(),
            ));
        }

        self.backend.save(fact, &tags).await
    }
}
