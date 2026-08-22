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

        // The backend already answers with `{ results, count }` (plus a
        // `session_scope` note when it widened the search). Wrapping that
        // again nested the real payload one level down and — because the
        // response is an object, not an array — made `count` 0 on every
        // single call.
        //
        // The model reads that count. `{"count": 0, "results": {"count": 6,
        // ...}}` says "nothing found" in the field a reader checks first,
        // while six memories sit inside it, which is a good way to make
        // recall look unreliable when retrieval worked perfectly.
        Ok(self
            .backend
            .search(query, &tags, limit, session_id)
            .await
            .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct SixResults;

    #[async_trait]
    impl MemoryBackend for SixResults {
        async fn search(
            &self,
            _query: &str,
            _tags: &[String],
            _limit: usize,
            _session_id: Option<&str>,
        ) -> rustykrab_core::Result<Value> {
            Ok(json!({
                "results": (0..6).map(|i| json!({ "content": format!("memory {i}") }))
                    .collect::<Vec<_>>(),
                "count": 6,
            }))
        }
        async fn get(&self, _: &str) -> rustykrab_core::Result<Value> {
            Ok(json!({}))
        }
        async fn save(&self, _: &str, _: &[String]) -> rustykrab_core::Result<Value> {
            Ok(json!({}))
        }
        async fn delete(&self, _: &str) -> rustykrab_core::Result<Value> {
            Ok(json!({}))
        }
        async fn list(&self) -> rustykrab_core::Result<Value> {
            Ok(json!({}))
        }
    }

    #[tokio::test]
    async fn reports_the_count_the_backend_found() {
        // The model reads `count` first. Reporting 0 while six memories sit
        // nested inside says "nothing found" about a search that worked.
        let tool = MemorySearchTool::new(Arc::new(SixResults));
        let out = tool.execute(json!({ "query": "kettle" })).await.unwrap();

        assert_eq!(out["count"], 6);
        assert_eq!(
            out["results"].as_array().map(|a| a.len()),
            Some(6),
            "results must be the array itself, not an object wrapping one"
        );
    }
}
