//! MCP client over the Streamable HTTP transport (MCP spec rev 2025-03-26).
//!
//! Each request is a single HTTP POST whose body is one JSON-RPC 2.0 object.
//! The server responds with either:
//!   * `application/json`      — a single JSON-RPC response, or
//!   * `text/event-stream`     — an SSE stream that includes the response.
//!
//! The transport is request/response only here: we do not open a server-push
//! GET stream, since the connector use-case (initialize, `tools/list`,
//! `tools/call`) never needs server-initiated requests.

use std::sync::atomic::{AtomicU64, Ordering};

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing;

use crate::mcp::{InitializeResult, McpToolDef, McpToolResult};

/// Header used by the Streamable HTTP transport to track session continuity
/// across requests. The server sets it on the `initialize` response and
/// expects it echoed on every subsequent request.
const MCP_SESSION_HEADER: &str = "mcp-session-id";

/// MCP client speaking JSON-RPC over Streamable HTTP.
pub struct McpHttpClient {
    client: reqwest::Client,
    url: String,
    extra_headers: HeaderMap,
    session_id: Mutex<Option<String>>,
    next_id: AtomicU64,
    tools: Mutex<Vec<McpToolDef>>,
}

impl McpHttpClient {
    /// Connect to a remote MCP server and run the `initialize` handshake.
    ///
    /// `bearer_token`, if provided, is sent as `Authorization: Bearer <token>`
    /// on every request. Per-server auth schemes other than bearer can be
    /// added later by accepting a `HeaderMap` instead.
    pub async fn connect(url: &str, bearer_token: Option<&str>) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?;

        let mut extra_headers = HeaderMap::new();
        if let Some(token) = bearer_token {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| format!("invalid bearer token: {e}"))?;
            extra_headers.insert(AUTHORIZATION, value);
        }

        let this = Self {
            client,
            url: url.to_string(),
            extra_headers,
            session_id: Mutex::new(None),
            next_id: AtomicU64::new(1),
            tools: Mutex::new(Vec::new()),
        };

        let init_params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "rustykrab",
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        let init_result = this.request("initialize", Some(init_params)).await?;
        let _init: InitializeResult = serde_json::from_value(init_result)
            .map_err(|e| format!("failed to parse initialize result: {e}"))?;
        this.notify("notifications/initialized", None).await?;

        tracing::info!(url = %url, "MCP HTTP client initialized");
        Ok(this)
    }

    /// Send a JSON-RPC request and return the matching response payload.
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params.unwrap_or(Value::Object(serde_json::Map::new())),
        });

        let mut req = self
            .client
            .post(&self.url)
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .headers(self.extra_headers.clone())
            .json(&body);

        if let Some(sid) = self.session_id.lock().await.clone() {
            req = req.header(MCP_SESSION_HEADER, sid);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("HTTP {status}: {body}"));
        }

        if let Some(sid) = resp
            .headers()
            .get(MCP_SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock().await = Some(sid.to_string());
        }

        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        if content_type.starts_with("text/event-stream") {
            let body = resp
                .text()
                .await
                .map_err(|e| format!("failed to read SSE body: {e}"))?;
            extract_jsonrpc_from_sse(&body, id)
        } else {
            let value: Value = resp
                .json()
                .await
                .map_err(|e| format!("failed to parse JSON response: {e}"))?;
            extract_jsonrpc_from_value(&value, id)
        }
    }

    /// Send a JSON-RPC notification (no `id`, no response expected).
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params.unwrap_or(Value::Object(serde_json::Map::new())),
        });

        let mut req = self
            .client
            .post(&self.url)
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .headers(self.extra_headers.clone())
            .json(&body);

        if let Some(sid) = self.session_id.lock().await.clone() {
            req = req.header(MCP_SESSION_HEADER, sid);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("HTTP notify failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("HTTP {status}: {body}"));
        }
        Ok(())
    }

    /// Fetch and cache the server's tool list.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, String> {
        let result = self.request("tools/list", None).await?;
        #[derive(serde::Deserialize)]
        struct ToolsList {
            tools: Vec<McpToolDef>,
        }
        let list: ToolsList = serde_json::from_value(result)
            .map_err(|e| format!("failed to parse tools/list: {e}"))?;
        *self.tools.lock().await = list.tools.clone();
        Ok(list.tools)
    }

    /// Invoke a tool on the remote server.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<McpToolResult, String> {
        let params = serde_json::json!({ "name": name, "arguments": arguments });
        let result = self.request("tools/call", Some(params)).await?;
        serde_json::from_value(result)
            .map_err(|e| format!("failed to parse tools/call result: {e}"))
    }

    /// Tool definitions captured by the most recent `list_tools` call.
    pub async fn cached_tools(&self) -> Vec<McpToolDef> {
        self.tools.lock().await.clone()
    }
}

fn extract_jsonrpc_from_value(value: &Value, expected_id: u64) -> Result<Value, String> {
    if let Some(arr) = value.as_array() {
        for item in arr {
            if let Some(v) = match_response(item, expected_id) {
                return v;
            }
        }
        return Err(format!("no JSON-RPC response found for id {expected_id}"));
    }
    match_response(value, expected_id)
        .unwrap_or_else(|| Err("response missing id field".to_string()))
}

fn match_response(value: &Value, expected_id: u64) -> Option<Result<Value, String>> {
    let id = value.get("id")?.as_u64()?;
    if id != expected_id {
        return None;
    }
    if let Some(err) = value.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown");
        return Some(Err(format!("MCP error {code}: {msg}")));
    }
    Some(Ok(value.get("result").cloned().unwrap_or(Value::Null)))
}

fn extract_jsonrpc_from_sse(body: &str, expected_id: u64) -> Result<Value, String> {
    for event in body.split("\n\n") {
        let mut data = String::new();
        for line in event.lines() {
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = rest.strip_prefix(' ').unwrap_or(rest);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(payload);
        }
        if data.is_empty() {
            continue;
        }
        let parsed: Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(e) => {
                tracing::trace!("SSE non-JSON data (parse error: {e}): {data}");
                continue;
            }
        };
        if let Some(result) = match_response(&parsed, expected_id) {
            return result;
        }
    }
    Err(format!(
        "no JSON-RPC response in SSE stream for id {expected_id}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_sse_event() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n";
        let v = extract_jsonrpc_from_sse(body, 7).unwrap();
        assert_eq!(v["ok"], serde_json::Value::Bool(true));
    }

    #[test]
    fn skips_unrelated_events_and_finds_match() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n\
                    : keep-alive\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"value\":42}}\n\n";
        let v = extract_jsonrpc_from_sse(body, 2).unwrap();
        assert_eq!(v["value"], serde_json::json!(42));
    }

    #[test]
    fn surfaces_jsonrpc_error() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"error\":{\"code\":-32601,\"message\":\"method not found\"}}\n\n";
        let err = extract_jsonrpc_from_sse(body, 3).unwrap_err();
        assert!(err.contains("-32601"));
        assert!(err.contains("method not found"));
    }

    #[test]
    fn parses_plain_json_object() {
        let v: Value = serde_json::json!({"jsonrpc":"2.0","id":5,"result":{"x":1}});
        let out = extract_jsonrpc_from_value(&v, 5).unwrap();
        assert_eq!(out["x"], serde_json::json!(1));
    }

    #[test]
    fn parses_plain_json_array_response() {
        let v: Value = serde_json::json!([
            {"jsonrpc":"2.0","id":1,"result":{}},
            {"jsonrpc":"2.0","id":9,"result":{"hit":true}}
        ]);
        let out = extract_jsonrpc_from_value(&v, 9).unwrap();
        assert_eq!(out["hit"], serde_json::Value::Bool(true));
    }

    #[test]
    fn missing_response_is_error() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        assert!(extract_jsonrpc_from_sse(body, 99).is_err());
    }
}
