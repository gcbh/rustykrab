use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};

use crate::security;

/// A built-in tool that makes HTTP requests.
///
/// Security: URLs are validated to prevent SSRF attacks. Requests to
/// private/internal IP ranges and cloud metadata endpoints are blocked.
pub struct HttpRequestTool {
    client: reqwest::Client,
}

impl HttpRequestTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

impl Default for HttpRequestTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for HttpRequestTool {
    fn name(&self) -> &str {
        "http_request"
    }

    fn description(&self) -> &str {
        "Make an HTTP request to a URL and return the response body."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "PUT", "DELETE", "PATCH"],
                        "description": "HTTP method"
                    },
                    "url": {
                        "type": "string",
                        "description": "The URL to request"
                    },
                    "body": {
                        "type": "string",
                        "description": "Optional request body"
                    }
                },
                "required": ["method", "url"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let method = args["method"]
            .as_str()
            .unwrap_or("GET")
            .to_uppercase();
        let url = args["url"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing url".into()))?;

        // SSRF protection: validate URL before making request
        security::validate_url(url)
            .map_err(|e| openclaw_core::Error::ToolExecution(e.into()))?;

        let mut req = match method.as_str() {
            "POST" => self.client.post(url),
            "PUT" => self.client.put(url),
            "DELETE" => self.client.delete(url),
            "PATCH" => self.client.patch(url),
            _ => self.client.get(url),
        };

        if let Some(body) = args["body"].as_str() {
            req = req.body(body.to_string());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| openclaw_core::Error::ToolExecution(e.to_string().into()))?;

        let status = resp.status().as_u16();
        let body = resp
            .text()
            .await
            .map_err(|e| openclaw_core::Error::ToolExecution(e.to_string().into()))?;

        if body.len() > 5_000_000 {
            return Err(openclaw_core::Error::ToolExecution(
                "response exceeds 5MB size limit".into(),
            ));
        }

        Ok(json!({
            "status": status,
            "body": body,
        }))
    }
}
