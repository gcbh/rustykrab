use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

/// A built-in tool that searches X (Twitter) posts and returns matching results.
pub struct XSearchTool {
    client: reqwest::Client,
}

impl XSearchTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for XSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for XSearchTool {
    fn name(&self) -> &str {
        "x_search"
    }

    fn description(&self) -> &str {
        "Search X (Twitter) posts and return matching results."
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
                        "description": "The search query"
                    },
                    "num_results": {
                        "type": "integer",
                        "description": "Number of results to return (default: 5)"
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
        let num_results = args["num_results"].as_u64().unwrap_or(5);

        let api_url = std::env::var("X_API_URL").ok();
        let bearer_token = std::env::var("X_API_BEARER_TOKEN").ok();

        match (api_url, bearer_token) {
            (Some(base_url), Some(token)) => {
                let resp = self
                    .client
                    .get(&base_url)
                    .query(&[("query", query), ("max_results", &num_results.to_string())])
                    .header("Authorization", format!("Bearer {}", token))
                    .send()
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

                let body: Value = resp
                    .json()
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

                // Extract results from the API response
                let results = body["data"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| {
                        json!({
                            "author": r["author"].as_str().unwrap_or(""),
                            "text": r["text"].as_str().unwrap_or(""),
                            "url": r["url"].as_str().unwrap_or(""),
                            "created_at": r["created_at"].as_str().unwrap_or(""),
                        })
                    })
                    .collect::<Vec<_>>();

                Ok(json!({
                    "query": query,
                    "results": results,
                }))
            }
            _ => Err(rustykrab_core::Error::ToolExecution(
                "X search requires X_API_URL and X_API_BEARER_TOKEN environment variables to be set.".into(),
            )),
        }
    }
}
