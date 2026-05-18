use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, SandboxRequirements, Tool};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::security;

/// Upper bound on cached named sessions. Each `reqwest::Client` owns its
/// own connection pool and cookie jar, so an unbounded map would leak
/// sockets and FDs if a caller invents fresh session names.
const MAX_SESSIONS: usize = 32;

/// A cookie-aware HTTP client with named sessions.
///
/// Unlike the stateless `http_request` tool, this maintains cookies
/// across requests within named sessions — enabling login flows,
/// authenticated API access, and multi-step web interactions.
///
/// Each named session has its own cookie jar, so multiple services
/// can be accessed simultaneously without cookie conflicts.
///
/// Uses `tokio::sync::Mutex` to avoid blocking the async runtime
/// (fixes ASYNC-H2).
pub struct HttpSessionTool {
    inner: Arc<Mutex<SessionCache>>,
}

/// LRU-bounded cache of `reqwest::Client`s keyed by session name.
struct SessionCache {
    clients: HashMap<String, reqwest::Client>,
    /// Recency order: front = least-recently-used, back = most-recent.
    order: VecDeque<String>,
}

impl SessionCache {
    fn new() -> Self {
        Self {
            clients: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn touch(&mut self, key: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.to_string());
    }

    fn get_or_create(&mut self, name: &str) -> reqwest::Client {
        if let Some(client) = self.clients.get(name).cloned() {
            self.touch(name);
            return client;
        }
        while self.clients.len() >= MAX_SESSIONS {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.clients.remove(&oldest);
                }
                None => break,
            }
        }
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .user_agent("RustyKrab/0.1 (AI Agent)")
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        self.clients.insert(name.to_string(), client.clone());
        self.order.push_back(name.to_string());
        client
    }

    fn remove(&mut self, name: &str) {
        self.clients.remove(name);
        if let Some(pos) = self.order.iter().position(|k| k == name) {
            self.order.remove(pos);
        }
    }
}

impl HttpSessionTool {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(SessionCache::new())),
        }
    }

    async fn get_or_create_client(&self, session_name: &str) -> reqwest::Client {
        let mut cache = self.inner.lock().await;
        cache.get_or_create(session_name)
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

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_net: true,
            ..SandboxRequirements::default()
        }
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
            let mut cache = self.inner.lock().await;
            cache.remove(session_name);
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
            .await
            .map_err(|e| Error::ToolExecution(e.into()))?;

        let method = args["method"].as_str().unwrap_or("GET").to_uppercase();

        let client = self.get_or_create_client(session_name).await;

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
            .filter_map(|(k, v)| v.to_str().ok().map(|val| (k.to_string(), val.to_string())))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_cache_evicts_lru_when_over_capacity() {
        let tool = HttpSessionTool::new();
        for i in 0..(MAX_SESSIONS + 5) {
            tool.get_or_create_client(&format!("s-{i}")).await;
        }
        let cache = tool.inner.lock().await;
        assert_eq!(cache.clients.len(), MAX_SESSIONS);
        assert!(!cache.clients.contains_key("s-0"));
        assert!(cache
            .clients
            .contains_key(&format!("s-{}", MAX_SESSIONS + 4)));
    }

    #[tokio::test]
    async fn session_cache_reuses_existing_client() {
        let tool = HttpSessionTool::new();
        let a = tool.get_or_create_client("same").await;
        let b = tool.get_or_create_client("same").await;
        // reqwest::Client is cheap to clone and clones share the same inner pool.
        // Verifying we didn't grow the map is the easier invariant.
        let cache = tool.inner.lock().await;
        assert_eq!(cache.clients.len(), 1);
        drop(a);
        drop(b);
    }

    #[tokio::test]
    async fn session_cache_get_refreshes_recency() {
        let tool = HttpSessionTool::new();
        for i in 0..MAX_SESSIONS {
            tool.get_or_create_client(&format!("s-{i}")).await;
        }
        // Re-accessing s-0 should bump it to most-recent so s-1 evicts next.
        tool.get_or_create_client("s-0").await;
        tool.get_or_create_client("overflow").await;

        let cache = tool.inner.lock().await;
        assert!(cache.clients.contains_key("s-0"));
        assert!(!cache.clients.contains_key("s-1"));
        assert!(cache.clients.contains_key("overflow"));
    }

    #[tokio::test]
    async fn session_cache_remove_clears_order() {
        let tool = HttpSessionTool::new();
        tool.get_or_create_client("x").await;
        {
            let mut cache = tool.inner.lock().await;
            cache.remove("x");
            assert!(!cache.clients.contains_key("x"));
            assert!(!cache.order.iter().any(|k| k == "x"));
        }
    }
}
