use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use serde_json::{json, Value};

use crate::security;

/// Maximum characters to return from a fetched page to avoid context explosion.
const MAX_CONTENT_LENGTH: usize = 50_000;

/// A tool that fetches a web page and returns cleaned, readable text.
///
/// Unlike the raw `http_request` tool, this strips HTML tags, scripts,
/// and styles to produce content the model can actually reason about.
pub struct WebFetchTool {
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("RustyKrab/0.1 (AI Agent)")
                .timeout(std::time::Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a web page and return its content as clean, readable text with HTML stripped. \
         Use this to read articles, documentation, or any web page."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch"
                    },
                    "include_links": {
                        "type": "boolean",
                        "description": "Whether to include [link text](url) in the output (default: false)"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing url".into()))?;

        // SSRF protection: validate URL before making request
        security::validate_url(url).await
            .map_err(|e| Error::ToolExecution(e.into()))?;

        let include_links = args["include_links"].as_bool().unwrap_or(false);

        let resp = self
            .client
            .get(url)
            .header("Accept", "text/html,application/xhtml+xml,text/plain")
            .send()
            .await
            .map_err(|e| Error::ToolExecution(format!("fetch failed: {e}").into()))?;

        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = resp
            .text()
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to read body: {e}").into()))?;

        // If it's not HTML, return raw text (could be JSON, plain text, etc.)
        let text = if content_type.contains("text/html") || content_type.contains("xhtml") {
            crate::sanitize::html_to_text(&body, include_links)
        } else {
            body
        };

        // Truncate to avoid context explosion.
        let truncated = text.len() > MAX_CONTENT_LENGTH;
        let content = if truncated {
            let end = text
                .char_indices()
                .take_while(|(i, _)| *i < MAX_CONTENT_LENGTH)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(MAX_CONTENT_LENGTH);
            format!("{}...\n\n[Content truncated at {} characters]", &text[..end], MAX_CONTENT_LENGTH)
        } else {
            text
        };

        Ok(json!({
            "status": status,
            "content": content,
            "truncated": truncated,
        }))
    }
}

