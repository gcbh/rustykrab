//! Lightweight MCP (Model Context Protocol) client over stdio.
//!
//! Speaks JSON-RPC 2.0 to a child process's stdin/stdout. Designed to be
//! transport-agnostic but currently implements the stdio transport used by
//! most MCP servers (including hyperframes).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing;

/// A single MCP tool definition returned by `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDef {
    pub name: String,
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

/// Content block returned by `tools/call`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpContent {
    #[serde(rename = "type")]
    pub content_type: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(rename = "mimeType", default)]
    pub mime_type: Option<String>,
}

/// Result of a `tools/call` invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolResult {
    pub content: Vec<McpContent>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
}

/// Server capabilities returned during initialization.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(default)]
    pub tools: Option<Value>,
    #[serde(default)]
    pub resources: Option<Value>,
    #[serde(default)]
    pub prompts: Option<Value>,
}

/// Result of MCP `initialize` handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: ServerCapabilities,
    #[serde(default, rename = "serverInfo")]
    pub server_info: Option<Value>,
}

// -- Internal JSON-RPC types --

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<u64>,
    result: Option<Value>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[allow(dead_code)]
    data: Option<Value>,
}

type PendingMap = HashMap<u64, oneshot::Sender<Result<Value, String>>>;

/// MCP client that communicates with a child process over stdio.
pub struct McpClient {
    /// Channel to send raw JSON lines to the writer task.
    write_tx: mpsc::Sender<String>,
    /// Monotonically increasing request ID.
    next_id: AtomicU64,
    /// Pending requests awaiting responses.
    pending: Arc<Mutex<PendingMap>>,
    /// Handle to the child process (for lifecycle management).
    child: Mutex<Option<Child>>,
    /// Cached tool definitions after `tools/list`.
    tools: Mutex<Vec<McpToolDef>>,
}

impl McpClient {
    /// Spawn an MCP server process and perform the `initialize` handshake.
    ///
    /// `command` is the program to run (e.g. "npx"), `args` are its arguments
    /// (e.g. `["hyperframes", "mcp"]`), and `env` provides additional
    /// environment variables for the child process.
    pub async fn spawn(
        command: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Result<Self, String> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        for (k, v) in env {
            cmd.env(k, v);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn MCP server `{command}`: {e}"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or("MCP server has no stdout")?;
        let stdin = child
            .stdin
            .take()
            .ok_or("MCP server has no stdin")?;

        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));

        // Writer task: serializes JSON lines to child stdin.
        let (write_tx, mut write_rx) = mpsc::channel::<String>(64);
        let mut stdin = stdin;
        tokio::spawn(async move {
            while let Some(line) = write_rx.recv().await {
                if let Err(e) = stdin.write_all(line.as_bytes()).await {
                    tracing::error!("MCP write error: {e}");
                    break;
                }
                if let Err(e) = stdin.write_all(b"\n").await {
                    tracing::error!("MCP write newline error: {e}");
                    break;
                }
                if let Err(e) = stdin.flush().await {
                    tracing::error!("MCP flush error: {e}");
                    break;
                }
            }
        });

        // Reader task: reads JSON lines from child stdout and dispatches.
        let pending_reader = pending.clone();
        let reader = BufReader::new(stdout);
        tokio::spawn(async move {
            let mut lines = reader.lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let line = line.trim().to_string();
                        if line.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<JsonRpcResponse>(&line) {
                            Ok(resp) => {
                                if let Some(id) = resp.id {
                                    let mut map = pending_reader.lock().await;
                                    if let Some(tx) = map.remove(&id) {
                                        let result = if let Some(err) = resp.error {
                                            Err(format!(
                                                "MCP error {}: {}",
                                                err.code, err.message
                                            ))
                                        } else {
                                            Ok(resp.result.unwrap_or(Value::Null))
                                        };
                                        let _ = tx.send(result);
                                    }
                                }
                                // Notifications (no id) are logged but not dispatched.
                            }
                            Err(e) => {
                                tracing::trace!(
                                    "MCP non-JSON line (parse error: {e}): {line}"
                                );
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::info!("MCP server stdout closed");
                        break;
                    }
                    Err(e) => {
                        tracing::error!("MCP read error: {e}");
                        break;
                    }
                }
            }
        });

        let client = Self {
            write_tx,
            next_id: AtomicU64::new(1),
            pending,
            child: Mutex::new(Some(child)),
            tools: Mutex::new(Vec::new()),
        };

        // Perform the MCP initialize handshake.
        let init_params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "rustykrab",
                "version": "0.1.0"
            }
        });

        let init_result = client.request("initialize", Some(init_params)).await?;
        let _init: InitializeResult = serde_json::from_value(init_result)
            .map_err(|e| format!("failed to parse initialize result: {e}"))?;

        // Send initialized notification (no id, no response expected).
        client.notify("notifications/initialized", None).await?;

        tracing::info!("MCP client initialized");

        Ok(client)
    }

    /// Send a JSON-RPC request and wait for the response.
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };

        let line = serde_json::to_string(&req)
            .map_err(|e| format!("failed to serialize request: {e}"))?;

        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending.lock().await;
            map.insert(id, tx);
        }

        self.write_tx
            .send(line)
            .await
            .map_err(|e| format!("failed to send to MCP writer: {e}"))?;

        match tokio::time::timeout(std::time::Duration::from_secs(120), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("MCP response channel dropped".to_string()),
            Err(_) => {
                // Clean up the pending entry on timeout.
                let mut map = self.pending.lock().await;
                map.remove(&id);
                Err("MCP request timed out (120s)".to_string())
            }
        }
    }

    /// Send a JSON-RPC notification (no id, no response expected).
    pub async fn notify(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), String> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params.unwrap_or(Value::Object(serde_json::Map::new()))
        });

        let line = serde_json::to_string(&notification)
            .map_err(|e| format!("failed to serialize notification: {e}"))?;

        self.write_tx
            .send(line)
            .await
            .map_err(|e| format!("failed to send notification: {e}"))?;

        Ok(())
    }

    /// List available tools from the MCP server.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, String> {
        let result = self.request("tools/list", None).await?;

        #[derive(Deserialize)]
        struct ToolsList {
            tools: Vec<McpToolDef>,
        }

        let list: ToolsList = serde_json::from_value(result)
            .map_err(|e| format!("failed to parse tools/list: {e}"))?;

        // Cache the tools.
        let mut cached = self.tools.lock().await;
        *cached = list.tools.clone();

        Ok(list.tools)
    }

    /// Call a tool on the MCP server.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<McpToolResult, String> {
        let params = serde_json::json!({
            "name": name,
            "arguments": arguments
        });

        let result = self.request("tools/call", Some(params)).await?;

        serde_json::from_value(result)
            .map_err(|e| format!("failed to parse tools/call result: {e}"))
    }

    /// Get cached tool definitions (call `list_tools` first).
    pub async fn cached_tools(&self) -> Vec<McpToolDef> {
        self.tools.lock().await.clone()
    }

    /// Gracefully shut down the MCP server.
    pub async fn shutdown(&self) {
        // Try to send a clean shutdown if the server supports it.
        let _ = self.notify("notifications/cancelled", None).await;

        let mut child = self.child.lock().await;
        if let Some(mut c) = child.take() {
            // Give the process a moment to exit cleanly.
            tokio::select! {
                _ = c.wait() => {},
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                    tracing::warn!("MCP server did not exit cleanly, killing");
                    let _ = c.kill().await;
                }
            }
        }
    }

    /// Check if the child process is still running.
    pub async fn is_alive(&self) -> bool {
        let mut child = self.child.lock().await;
        match child.as_mut() {
            Some(c) => c.try_wait().ok().flatten().is_none(),
            None => false,
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Best-effort synchronous kill on drop.
        if let Ok(mut guard) = self.child.try_lock() {
            if let Some(ref mut c) = *guard {
                let _ = c.start_kill();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_rpc_request_serializes() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "tools/list".to_string(),
            params: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"method\":\"tools/list\""));
        assert!(!json.contains("params"));
    }
}
