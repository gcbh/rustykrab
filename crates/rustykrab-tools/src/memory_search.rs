use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
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
        "Search long-term memory for facts, preferences, decisions, and past \
         conversation turns. Use this whenever the user refers to something \
         not visible in the current context — earlier conversations, stated \
         preferences, prior plans or decisions — or when you are uncertain \
         about a detail you may have known before. Pass session_id to search \
         only one conversation's history."
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
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Optional conversation id; restricts the search to \
                                        memories from that conversation's history"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing query".into()))?;

        let tags: Vec<String> = args["tags"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let limit = args["limit"].as_u64().unwrap_or(10) as usize;
        let session_id = args["session_id"].as_str();

        let results = self
            .backend
            .search(query, &tags, limit, session_id)
            .await
            .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

        let count = results.as_array().map(|a| a.len()).unwrap_or(0);

        Ok(json!({
            "results": results,
            "count": count,
        }))
    }
}
