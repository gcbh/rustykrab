use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, SandboxRequirements, Tool};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Default per-request timeout. Generous because a node may be running a local
/// model at single-digit tokens/sec, where one delegated task legitimately takes
/// minutes.
const DEFAULT_TIMEOUT_SECS: u64 = 900;

/// A peer RustyKrab instance this agent can delegate work to.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteNode {
    /// Short identifier used as `node_id` in tool calls.
    pub id: String,
    /// Base URL of the peer's gateway, e.g. `http://100.97.221.58:3000`.
    pub url: String,
    /// Bearer token for the peer's gateway.
    ///
    /// This is a shared static token, not a paired-device token — the
    /// project's device-pairing flow (`DeviceStore`: per-device tokens,
    /// SHA-256 hashed at rest, individually revocable) exists for the phone
    /// use case and would be the better fit here too: a node could redeem a
    /// pairing code for its own attributable, revocable token instead of
    /// sharing the gateway's master credential. Left as a shared token for
    /// now — adopting device pairing is a deliberate follow-up, not an
    /// oversight.
    pub token: String,
    /// Human-readable note — hardware, model, intended role. Surfaced to the
    /// model so it can pick a node deliberately.
    #[serde(default)]
    pub description: Option<String>,
}

impl RemoteNode {
    fn api(&self, path: &str) -> String {
        format!("{}/api{}", self.url.trim_end_matches('/'), path)
    }

    /// Origin sent on node-to-node requests.
    ///
    /// The peer's origin check is a CSRF defence: it stops a malicious *web page*
    /// from driving a localhost agent (the ClawJacked class of attack). It is not
    /// an authentication mechanism — that is the bearer token, over a private
    /// network. A server-side client is not subject to browser origin rules and
    /// legitimately sets its own, so we send a loopback origin the peer accepts
    /// rather than requiring every node to widen its allowlist.
    fn origin(&self) -> &'static str {
        "http://127.0.0.1:3000"
    }
}

/// Delegate work to peer RustyKrab instances over the network.
pub struct NodesTool {
    client: reqwest::Client,
}

impl NodesTool {
    pub fn new() -> Self {
        let timeout = std::env::var("RUSTYKRAB_NODE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("failed to build HTTP client"),
        }
    }

    /// Parse the configured node list from `RUSTYKRAB_NODES`.
    ///
    /// Format is a JSON array, e.g.
    /// `[{"id":"m4max","url":"http://100.97.221.58:3000","token":"...",
    ///    "description":"M4 Max - qwen3.8:27b-mlx, coding"}]`
    fn configured_nodes() -> Result<Vec<RemoteNode>> {
        let raw = match std::env::var("RUSTYKRAB_NODES") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => return Ok(Vec::new()),
        };
        serde_json::from_str(&raw).map_err(|e| {
            Error::ToolExecution(
                format!(
                    "RUSTYKRAB_NODES is not valid JSON ({e}). Expected an array of \
                 {{\"id\",\"url\",\"token\",\"description\"}} objects."
                )
                .into(),
            )
        })
    }

    fn find(nodes: &[RemoteNode], id: &str) -> Result<RemoteNode> {
        nodes.iter().find(|n| n.id == id).cloned().ok_or_else(|| {
            let known: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
            Error::ToolExecution(
                format!(
                    "unknown node '{id}'. Configured nodes: {}",
                    if known.is_empty() {
                        "(none)".to_string()
                    } else {
                        known.join(", ")
                    }
                )
                .into(),
            )
        })
    }

    /// Probe a node's public health endpoint. Returns round-trip latency.
    async fn probe(&self, node: &RemoteNode) -> std::result::Result<u128, String> {
        let start = Instant::now();
        let resp = self
            .client
            .get(node.api("/health"))
            .header("Origin", node.origin())
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("unreachable: {e}"))?;
        if resp.status().is_success() {
            Ok(start.elapsed().as_millis())
        } else {
            Err(format!("HTTP {}", resp.status()))
        }
    }

    /// Send a task to a node and return its reply.
    ///
    /// Creates a fresh conversation on the peer so delegated work does not share
    /// context with whatever else that node is doing.
    async fn send(&self, node: &RemoteNode, message: &str) -> Result<Value> {
        let started = Instant::now();

        let convo: Value = self
            .post(node, &node.api("/conversations"), json!({}))
            .await?;
        let convo_id = convo["id"].as_str().ok_or_else(|| {
            Error::ToolExecution(
                format!("node '{}' returned no conversation id: {convo}", node.id).into(),
            )
        })?;

        let reply: Value = self
            .post(
                node,
                &node.api(&format!("/conversations/{convo_id}/messages")),
                json!({ "content": message }),
            )
            .await?;

        // Assistant messages are {"content": {"type": "text", "data": "..."}}.
        let text = reply["content"]["data"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| reply["content"].to_string());

        Ok(json!({
            "node_id": node.id,
            "conversation_id": convo_id,
            "response": text,
            "elapsed_secs": started.elapsed().as_secs(),
        }))
    }

    async fn post(&self, node: &RemoteNode, url: &str, body: Value) -> Result<Value> {
        let resp = self
            .client
            .post(url)
            .bearer_auth(&node.token)
            .header("Origin", node.origin())
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                Error::ToolExecution(
                    format!("node '{}' at {} is unreachable: {e}", node.id, node.url).into(),
                )
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let hint = match status.as_u16() {
                401 | 403 => " (check the node's token, and that its gateway is reachable)",
                _ => "",
            };
            return Err(Error::ToolExecution(
                format!("node '{}' returned {status}{hint}: {body}", node.id).into(),
            ));
        }

        resp.json().await.map_err(|e| {
            Error::ToolExecution(format!("node '{}' returned invalid JSON: {e}", node.id).into())
        })
    }
}

impl Default for NodesTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for NodesTool {
    fn name(&self) -> &str {
        "nodes"
    }

    fn description(&self) -> &str {
        "Delegate a task to a peer RustyKrab instance on another machine. Use \
         'list' to see available nodes and what each is suited for, 'discover' to \
         check which are online, and 'send' to hand a self-contained task to one \
         and get its result. Delegated tasks run with that node's own tools and \
         filesystem, so include everything the node needs in the message."
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_net: true,
            ..SandboxRequirements::default()
        }
    }

    /// Hidden from the model unless at least one node is configured — otherwise
    /// it advertises a capability that cannot work.
    fn available(&self) -> bool {
        Self::configured_nodes()
            .map(|n| !n.is_empty())
            .unwrap_or(false)
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["discover", "list", "send"],
                        "description": "'list' shows configured nodes, 'discover' probes which are online, 'send' delegates a task"
                    },
                    "node_id": {
                        "type": "string",
                        "description": "Target node id (required for 'send')"
                    },
                    "message": {
                        "type": "string",
                        "description": "The task to delegate (required for 'send'). Must be self-contained: the node does not share this conversation's context."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing action".into()))?;

        let nodes = Self::configured_nodes()?;
        if nodes.is_empty() {
            return Err(Error::ToolExecution(
                "no nodes configured. Set RUSTYKRAB_NODES to a JSON array of \
                 {\"id\",\"url\",\"token\",\"description\"} objects."
                    .into(),
            ));
        }

        match action {
            // Never include tokens in tool output — it goes into the model's context.
            "list" => Ok(json!({
                "nodes": nodes.iter().map(|n| json!({
                    "id": n.id,
                    "url": n.url,
                    "description": n.description,
                })).collect::<Vec<_>>()
            })),

            "discover" => {
                let mut out = Vec::new();
                for node in &nodes {
                    let entry = match self.probe(node).await {
                        Ok(ms) => json!({
                            "id": node.id, "url": node.url, "status": "online",
                            "latency_ms": ms, "description": node.description,
                        }),
                        Err(e) => json!({
                            "id": node.id, "url": node.url, "status": "offline",
                            "error": e, "description": node.description,
                        }),
                    };
                    out.push(entry);
                }
                Ok(json!({ "nodes": out }))
            }

            "send" => {
                let node_id = args["node_id"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("'send' requires node_id".into()))?;
                let message = args["message"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("'send' requires message".into()))?;
                if message.trim().is_empty() {
                    return Err(Error::ToolExecution(
                        "'send' requires a non-empty message".into(),
                    ));
                }
                let node = Self::find(&nodes, node_id)?;
                self.send(&node, message).await
            }

            other => Err(Error::ToolExecution(
                format!("unknown action '{other}' (expected discover, list, or send)").into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes_json() -> String {
        json!([
            {"id": "m4max", "url": "http://100.97.221.58:3000/", "token": "secret-token",
             "description": "M4 Max - qwen3.8:27b-mlx, coding"},
            {"id": "other", "url": "http://10.0.0.5:3000", "token": "t2"}
        ])
        .to_string()
    }

    #[test]
    fn parses_configured_nodes() {
        let nodes: Vec<RemoteNode> = serde_json::from_str(&nodes_json()).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].id, "m4max");
        assert_eq!(
            nodes[0].description.as_deref(),
            Some("M4 Max - qwen3.8:27b-mlx, coding")
        );
        // description is optional
        assert!(nodes[1].description.is_none());
    }

    #[test]
    fn builds_api_urls_without_double_slashes() {
        let nodes: Vec<RemoteNode> = serde_json::from_str(&nodes_json()).unwrap();
        assert_eq!(
            nodes[0].api("/conversations"),
            "http://100.97.221.58:3000/api/conversations"
        );
        assert_eq!(nodes[1].api("/health"), "http://10.0.0.5:3000/api/health");
    }

    #[test]
    fn unknown_node_error_lists_known_ids() {
        let nodes: Vec<RemoteNode> = serde_json::from_str(&nodes_json()).unwrap();
        let err = NodesTool::find(&nodes, "nope").unwrap_err().to_string();
        assert!(err.contains("m4max") && err.contains("other"), "got: {err}");
    }

    #[tokio::test]
    async fn list_never_leaks_tokens() {
        // Set/remove is process-global; keep this the only test touching the var.
        std::env::set_var("RUSTYKRAB_NODES", nodes_json());
        let out = NodesTool::new()
            .execute(json!({"action": "list"}))
            .await
            .unwrap();
        let rendered = out.to_string();
        assert!(rendered.contains("m4max"));
        assert!(
            !rendered.contains("secret-token"),
            "tool output must not carry tokens into model context: {rendered}"
        );
        std::env::remove_var("RUSTYKRAB_NODES");
    }

    #[test]
    fn malformed_config_is_a_clear_error() {
        std::env::set_var("RUSTYKRAB_NODES", "not json");
        let err = NodesTool::configured_nodes().unwrap_err().to_string();
        assert!(err.contains("RUSTYKRAB_NODES"), "got: {err}");
        std::env::remove_var("RUSTYKRAB_NODES");
    }
}
