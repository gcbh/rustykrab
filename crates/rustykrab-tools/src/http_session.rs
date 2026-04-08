use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use serde_json::{json, Value};

use crate::security;

/// A cookie-aware HTTP client with named sessions.
///
/// Unlike the stateless `http_request` tool, this maintains cookies
/// across requests within named sessions — enabling login flows,
/// authenticated API access, and multi-step web interactions.
///
/// Each named session has its own cookie jar, so multiple services
/// can be accessed simultaneously without cookie conflicts.
pub struct HttpSessionTool {
    sessions: Arc<Mutex<HashMap<String, reqwest::Client>>>,
}

impl HttpSessionTool {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn get_or_create_client(&self, session_name: &str) -> reqwest::Client {
        let mut sessions = self.sessions.lock().unwrap();
        sessions
            .entry(session_name.to_string())
            .or_insert_with(|| {
                reqwest::Client::builder()
                    .cookie_store(true)
                    .user_agent("RustyKrab/0.1 (AI Agent)")
                    .timeout(std::time::Duration::from_secs(30))
                    .redirect(reqwest::redirect::Policy::limited(10))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new())
            })
            .clone()
    }
}

impl Default for HttpSessionTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for HttpSessionTool {
    fn name(&self) -> &str {
        "http_session"
    }

    fn description(&self) -> &str {
        "Make HTTP requests with persistent cookie sessions. Unlike http_request, \
         this tool maintains cookies across requests within a named session, enabling \
         login flows and authenticated web interactions. Use different session names \
         for different services."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "session": {
                        "type": "string",
                        "description": "Name for this session (e.g., 'github', 'jira'). Cookies persist within a session."
                    },
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD"],
                        "description": "HTTP method"
                    },
                    "url": {
                        "type": "string",
                        "description": "The URL to request"
                    },
                    "headers": {
                        "type": "object",
                        "description": "Optional HTTP headers as key-value pairs",
                        "additionalProperties": { "type": "string" }
                    },
                    "body": {
                        "type": "string",
                        "description": "Optional request body (for POST/PUT/PATCH)"
                    },
                    "content_type": {
                        "type": "string",
                        "enum": ["json", "form", "text"],
                        "description": "Content type shorthand: 'json' for application/json, 'form' for application/x-www-form-urlencoded, 'text' for text/plain (default: auto-detect)"
                    },
                    "action": {
                        "type": "string",
                        "enum": ["request", "clear"],
                        "description": "Action: 'request' makes an HTTP request (default), 'clear' destroys the session and its cookies"
                    }
                },
                "required": ["session"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let session_name = args["session"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing session name".into()))?;

        let action = args["action"].as_str().unwrap_or("request");

        if action == "clear" {
            let mut sessions = self.sessions.lock().unwrap();
            sessions.remove(session_name);
            return Ok(json!({
                "status": "session_cleared",
                "session": session_name,
            }));
        }

        let url = args["url"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing url".into()))?;

        // SSRF protection: validate URL before making request
        security::validate_url(url)
            .map_err(|e| Error::ToolExecution(e.into()))?;

        let method = args["method"]
            .as_str()
            .unwrap_or("GET")
            .to_uppercase();

        let client = self.get_or_create_client(session_name);

        let mut req = match method.as_str() {
            "POST" => client.post(url),
            "PUT" => client.put(url),
            "DELETE" => client.delete(url),
            "PATCH" => client.patch(url),
            "HEAD" => client.head(url),
            _ => client.get(url),
        };

        // Apply custom headers
        if let Some(headers) = args["headers"].as_object() {
            for (key, value) in headers {
                if let Some(val) = value.as_str() {
                    req = req.header(key.as_str(), val);
                }
            }
        }

        // Apply body with content type
        if let Some(body) = args["body"].as_str() {
            let content_type = args["content_type"].as_str().unwrap_or("text");
            req = match content_type {
                "json" => req
                    .header("Content-Type", "application/json")
                    .body(body.to_string()),
                "form" => req
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(body.to_string()),
                _ => req.body(body.to_string()),
            };
        }

        let resp = req
            .send()
            .await
            .map_err(|e| Error::ToolExecution(format!("request failed: {e}").into()))?;

        let status = resp.status().as_u16();
        let status_text = resp.status().canonical_reason().unwrap_or("").to_string();

        // Collect response headers
        let response_headers: HashMap<String, String> = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|val| (k.to_string(), val.to_string()))
            })
            .collect();

        let body = resp
            .text()
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to read response: {e}").into()))?;

        // Truncate very large responses
        let max_len = 100_000;
        let truncated = body.len() > max_len;
        let body_out = if truncated {
            format!("{}...\n[Truncated at {} chars]", &body[..max_len], max_len)
        } else {
            body
        };

        Ok(json!({
            "session": session_name,
            "status": status,
            "status_text": status_text,
            "headers": response_headers,
            "body": body_out,
            "truncated": truncated,
        }))
    }
}
