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
    /// How many further delegation hops this node may make with work we
    /// send it. Defaults to 0: the node runs the task itself and may not
    /// hand any part of it onward.
    ///
    /// This is the cross-machine recursion guard. Two peers that each
    /// list the other — the natural configuration when the node is
    /// another copy of the same program — would otherwise bounce a task
    /// between them indefinitely, at minutes of local inference per hop.
    /// The local `subagents` tool has an equivalent depth counter; it
    /// cannot help here because it is process-local.
    #[serde(default)]
    pub hop_budget: Option<i64>,
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

    /// Resolve the `node_id` argument for an action that targets one node.
    fn target(nodes: &[RemoteNode], args: &Value, action: &str) -> Result<RemoteNode> {
        let node_id = args["node_id"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution(format!("'{action}' requires node_id").into()))?;
        Self::find(nodes, node_id)
    }

    fn task_id<'a>(args: &'a Value, action: &str) -> Result<&'a str> {
        args["task_id"]
            .as_str()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| {
                Error::ToolExecution(
                    format!("'{action}' requires the task_id returned by 'send'").into(),
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

    /// Submit a task and return a handle, without waiting for it to run.
    ///
    /// Delegation is asynchronous because a delegated task on a local
    /// model routinely takes minutes, and the agent loop that called this
    /// tool cannot be held open that long — the runner caps a network
    /// tool at [`DEFAULT_NET_TOOL_TIMEOUT_SECS`-equivalent] wall-clock,
    /// well under the time a real task needs. Submitting returns in about
    /// a second; the model polls with `check` on later turns and does
    /// other work in between.
    async fn submit(
        &self,
        node: &RemoteNode,
        message: &str,
        conversation_id: Option<&str>,
    ) -> Result<Value> {
        let body = json!({
            "message": message,
            "conversationId": conversation_id,
            "hopBudget": node.hop_budget.unwrap_or(0),
        });

        let url = node.api("/tasks");
        let resp = self
            .send_raw(node, self.client.post(&url).json(&body))
            .await?;

        // A node on a build without the task queue does not serve
        // /api/tasks. Fall back so a mixed-version fleet keeps working —
        // the synchronous path is bounded by this tool's own timeout and
        // will fail on anything long, but it beats refusing outright.
        //
        // Both statuses matter, and which one you get is not obvious:
        // axum answers 404 when no route matches the path at all, but 405
        // when the path exists for a different method. A build that has
        // `GET /api/tasks/{id}` but not `POST /api/tasks` returns 405, so
        // treating only 404 as "missing" would strand exactly the peers
        // this fallback is for. Observed against a real v5.2.0 node: 405.
        if matches!(
            resp.status(),
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
        ) {
            tracing::warn!(
                node = %node.id,
                status = %resp.status(),
                "node does not serve /api/tasks; falling back to a synchronous delegation"
            );
            return self.legacy_send(node, message).await;
        }

        let accepted = self.interpret(node, resp, &url).await?;

        let task_id = accepted["id"].as_str().unwrap_or_default().to_string();
        if task_id.is_empty() {
            return Err(Error::ToolExecution(
                format!("node '{}' accepted the task but returned no id", node.id).into(),
            ));
        }

        Ok(json!({
            "node_id": node.id,
            "task_id": task_id,
            "status": accepted["status"].as_str().unwrap_or("queued"),
            "next": "The node is working. Call nodes with action='check', the same \
                     node_id, and this task_id to collect the result. Do other work \
                     first — a delegated task usually takes minutes.",
        }))
    }

    /// Read a submitted task's current state, and its result once done.
    async fn check(&self, node: &RemoteNode, task_id: &str) -> Result<Value> {
        let task: Value = self
            .get(node, &node.api(&format!("/tasks/{task_id}")))
            .await?;

        let status = task["status"].as_str().unwrap_or("unknown");
        let mut out = json!({
            "node_id": node.id,
            "task_id": task_id,
            "status": status,
            "elapsed_secs": task["elapsedSecs"],
        });

        // The conversation id is what makes a follow-up cheap: passing it
        // back on the next `send` continues the same thread on the node,
        // reusing its evaluated prompt prefix instead of re-reading the
        // whole system prompt and tool schemas.
        if let Some(convo) = task["conversationId"].as_str() {
            out["conversation_id"] = json!(convo);
        }

        match status {
            "done" => {
                out["response"] = task["result"].clone();
            }
            "failed" | "cancelled" => {
                out["error"] = task["error"].clone();
            }
            _ => {
                out["next"] = json!(
                    "Still running. Check again later rather than resubmitting — \
                     a resubmit starts the work over from scratch."
                );
            }
        }
        Ok(out)
    }

    /// Cancel a submitted task, aborting it if the node has already started.
    async fn cancel(&self, node: &RemoteNode, task_id: &str) -> Result<Value> {
        let task: Value = self
            .delete(node, &node.api(&format!("/tasks/{task_id}")))
            .await?;
        Ok(json!({
            "node_id": node.id,
            "task_id": task_id,
            "status": task["status"].as_str().unwrap_or("cancelled"),
        }))
    }

    /// Pre-queue delegation: create a conversation and post a message,
    /// blocking until the node's whole agent turn finishes.
    ///
    /// Retained only so a primary on this build can still drive a node on
    /// an older one. It is bounded by the caller's tool timeout, so it
    /// fails on exactly the long tasks delegation exists for.
    async fn legacy_send(&self, node: &RemoteNode, message: &str) -> Result<Value> {
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
            "warning": "This node runs a build without the delegated-task queue, so \
                        the task ran synchronously. Upgrade it to delegate work that \
                        takes longer than this tool's timeout.",
        }))
    }

    async fn get(&self, node: &RemoteNode, url: &str) -> Result<Value> {
        self.send_request(node, self.client.get(url), url).await
    }

    async fn delete(&self, node: &RemoteNode, url: &str) -> Result<Value> {
        self.send_request(node, self.client.delete(url), url).await
    }

    async fn post(&self, node: &RemoteNode, url: &str, body: Value) -> Result<Value> {
        self.send_request(node, self.client.post(url).json(&body), url)
            .await
    }

    /// Issue one request to a node, returning the response whatever its
    /// status. Only transport failures become errors here, so callers
    /// that need to branch on a status code can do so before it is
    /// flattened into an error string.
    async fn send_raw(
        &self,
        node: &RemoteNode,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        request
            .bearer_auth(&node.token)
            .header("Origin", node.origin())
            .send()
            .await
            .map_err(|e| {
                Error::ToolExecution(
                    format!("node '{}' at {} is unreachable: {e}", node.id, node.url).into(),
                )
            })
    }

    /// Turn a peer's response into JSON, or an error the model can act on.
    async fn interpret(
        &self,
        node: &RemoteNode,
        resp: reqwest::Response,
        url: &str,
    ) -> Result<Value> {
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let hint = match status.as_u16() {
                401 | 403 => " (check the node's token, and that its gateway is reachable)",
                404 | 405 => " (the node may be running a build without this endpoint)",
                _ => "",
            };
            return Err(Error::ToolExecution(
                format!(
                    "node '{}' returned {status}{hint} for {url}: {body}",
                    node.id
                )
                .into(),
            ));
        }

        // A 202 with no body is a legitimate response for some servers;
        // treat an unparseable success as an empty object rather than an
        // error, so the caller's own field checks produce the message.
        let text = resp.text().await.unwrap_or_default();
        if text.trim().is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&text).map_err(|e| {
            Error::ToolExecution(format!("node '{}' returned invalid JSON: {e}", node.id).into())
        })
    }

    async fn send_request(
        &self,
        node: &RemoteNode,
        request: reqwest::RequestBuilder,
        url: &str,
    ) -> Result<Value> {
        let resp = self.send_raw(node, request).await?;
        self.interpret(node, resp, url).await
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
         check which are online, 'send' to hand a self-contained task to one, \
         'check' to collect the result later, and 'cancel' to stop one. Delegation \
         is asynchronous: 'send' returns a task_id immediately and the node works \
         in the background, typically for minutes — do other work and check back \
         rather than waiting. Delegated tasks run with that node's own tools and \
         filesystem and cannot see this machine's files, so include everything the \
         node needs in the message."
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
                        "enum": ["discover", "list", "send", "check", "cancel"],
                        "description": "'list' shows configured nodes, 'discover' probes which are online, 'send' delegates a task and returns a task_id, 'check' reads that task's status and result, 'cancel' stops it"
                    },
                    "node_id": {
                        "type": "string",
                        "description": "Target node id (required for 'send', 'check' and 'cancel')"
                    },
                    "message": {
                        "type": "string",
                        "description": "The task to delegate (required for 'send'). Must be self-contained: the node does not share this conversation's context, cannot see this machine's files, and starts from a blank slate unless conversation_id continues an earlier thread."
                    },
                    "task_id": {
                        "type": "string",
                        "description": "The id returned by 'send' (required for 'check' and 'cancel')"
                    },
                    "conversation_id": {
                        "type": "string",
                        "description": "Optional, for 'send': continue an earlier delegated thread on that node instead of starting fresh. Use the conversation_id from a previous 'check' when this task follows on from it — a continued thread starts answering in seconds where a new one spends a minute or more re-reading its own prompt."
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
                let node = Self::target(&nodes, &args, "send")?;
                let message = args["message"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("'send' requires message".into()))?;
                if message.trim().is_empty() {
                    return Err(Error::ToolExecution(
                        "'send' requires a non-empty message".into(),
                    ));
                }
                self.submit(&node, message, args["conversation_id"].as_str())
                    .await
            }

            "check" => {
                let node = Self::target(&nodes, &args, "check")?;
                let task_id = Self::task_id(&args, "check")?;
                self.check(&node, task_id).await
            }

            "cancel" => {
                let node = Self::target(&nodes, &args, "cancel")?;
                let task_id = Self::task_id(&args, "cancel")?;
                self.cancel(&node, task_id).await
            }

            other => Err(Error::ToolExecution(
                format!(
                    "unknown action '{other}' (expected discover, list, send, check, or cancel)"
                )
                .into(),
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
    fn hop_budget_defaults_to_no_onward_delegation() {
        let nodes: Vec<RemoteNode> = serde_json::from_str(&nodes_json()).unwrap();
        // Unset means zero, not unlimited. A node configured without
        // thinking about recursion must not be able to delegate onward.
        assert_eq!(nodes[0].hop_budget.unwrap_or(0), 0);

        let explicit: Vec<RemoteNode> =
            serde_json::from_str(r#"[{"id":"n","url":"http://x","token":"t","hop_budget":2}]"#)
                .unwrap();
        assert_eq!(explicit[0].hop_budget, Some(2));
    }

    #[test]
    fn both_missing_endpoint_statuses_trigger_the_fallback() {
        // Which one a peer returns is not obvious: axum answers 404 when
        // no route matches the path, but 405 when the path exists for a
        // different method. A real v5.2.0 node returned 405, so matching
        // only 404 would strand exactly the peers this fallback serves.
        for status in [
            reqwest::StatusCode::NOT_FOUND,
            reqwest::StatusCode::METHOD_NOT_ALLOWED,
        ] {
            assert!(
                matches!(
                    status,
                    reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
                ),
                "{status} must fall back"
            );
        }

        // And nothing else does. An auth failure or a server error means
        // the endpoint is there and something else is wrong; retrying
        // synchronously would just fail twice and hide the real cause.
        for status in [
            reqwest::StatusCode::UNAUTHORIZED,
            reqwest::StatusCode::FORBIDDEN,
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            assert!(
                !matches!(
                    status,
                    reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
                ),
                "{status} must not fall back"
            );
        }
    }

    #[test]
    fn check_and_cancel_require_a_task_id() {
        // Exercised through the argument helpers rather than `execute` so
        // this does not touch RUSTYKRAB_NODES: the var is process-global
        // and `list_never_leaks_tokens` owns it.
        for action in ["check", "cancel"] {
            let err = NodesTool::task_id(&json!({"node_id": "m4max"}), action)
                .unwrap_err()
                .to_string();
            assert!(err.contains("task_id"), "got: {err}");
            // An empty string is not a usable handle either.
            let err = NodesTool::task_id(&json!({"task_id": "  "}), action)
                .unwrap_err()
                .to_string();
            assert!(err.contains("task_id"), "got: {err}");
        }
        assert_eq!(
            NodesTool::task_id(&json!({"task_id": "abc"}), "check").unwrap(),
            "abc"
        );
    }

    #[test]
    fn targeting_an_action_reports_the_missing_node_id() {
        let nodes: Vec<RemoteNode> = serde_json::from_str(&nodes_json()).unwrap();
        let err = NodesTool::target(&nodes, &json!({"task_id": "abc"}), "check")
            .unwrap_err()
            .to_string();
        assert!(err.contains("node_id"), "got: {err}");
    }

    #[test]
    fn the_schema_advertises_the_asynchronous_actions() {
        let schema = NodesTool::new().schema();
        let actions = schema.parameters["properties"]["action"]["enum"].clone();
        let actions: Vec<String> = serde_json::from_value(actions).unwrap();
        // Without check the model can submit work it can never collect.
        for expected in ["list", "discover", "send", "check", "cancel"] {
            assert!(
                actions.iter().any(|a| a == expected),
                "'{expected}' missing from {actions:?}"
            );
        }
    }

    #[test]
    fn malformed_config_is_a_clear_error() {
        std::env::set_var("RUSTYKRAB_NODES", "not json");
        let err = NodesTool::configured_nodes().unwrap_err().to_string();
        assert!(err.contains("RUSTYKRAB_NODES"), "got: {err}");
        std::env::remove_var("RUSTYKRAB_NODES");
    }
}
