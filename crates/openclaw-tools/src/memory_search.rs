use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::memory_backend::MemoryBackend;

/// A tool that searches long-term memory entries by tags or keywords.
pub struct MemorySearchTool {
    backend: Arc<dyn MemoryBackend>,
}

impl MemorySearchTool {
    pub fn new(backend: Arc<dyn MemoryBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search long-term memory entries by tags or keywords."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query string"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tags to filter by"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default 10)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing query".into()))?;

        let tags: Vec<String> = args["tags"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let limit = args["limit"].as_u64().unwrap_or(10) as usize;

        let results = self
            .backend
            .search(query, &tags, limit)
            .await
            .map_err(|e| openclaw_core::Error::ToolExecution(e.to_string().into()))?;

        let count = results
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);

        Ok(json!({
            "results": results,
            "count": count,
        }))
    }
}
