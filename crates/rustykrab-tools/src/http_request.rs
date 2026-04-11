use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
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
        let method = args["method"].as_str().unwrap_or("GET").to_uppercase();
        let url = args["url"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing url".into()))?;

        // SSRF protection: validate URL before making request
        security::validate_url(url)
            .await
            .map_err(|e| rustykrab_core::Error::ToolExecution(e.into()))?;

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

        let mut resp = req
            .send()
            .await
            .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

        let status = resp.status().as_u16();

        // Check Content-Length header before reading body to prevent OOM
        // on multi-gigabyte responses.
        const MAX_BODY_SIZE: usize = 5_000_000;
        if let Some(len) = resp.content_length() {
            if len > MAX_BODY_SIZE as u64 {
                return Err(rustykrab_core::Error::ToolExecution(
                    format!("response Content-Length ({len} bytes) exceeds 5MB limit").into(),
                ));
            }
        }

        // Stream body with a size cap for chunked/missing Content-Length responses.
        let mut body_bytes = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?
        {
            body_bytes.extend_from_slice(&chunk);
            if body_bytes.len() > MAX_BODY_SIZE {
                return Err(rustykrab_core::Error::ToolExecution(
                    "response exceeds 5MB size limit".into(),
                ));
            }
        }
        let body = String::from_utf8_lossy(&body_bytes).to_string();

        Ok(json!({
            "status": status,
            "body": body,
        }))
    }
}
