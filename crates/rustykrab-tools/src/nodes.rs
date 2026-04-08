use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

/// A built-in tool that discovers and interacts with paired RustyKrab nodes.
pub struct NodesTool {
    client: reqwest::Client,
}

impl NodesTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
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
        "Discover and interact with paired RustyKrab nodes on the network."
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
                        "description": "The action to perform: discover new nodes, list known nodes, or send a message to a node"
                    },
                    "node_id": {
                        "type": "string",
                        "description": "Target node ID (required for 'send' action)"
                    },
                    "message": {
                        "type": "string",
                        "description": "Message to send to the target node (required for 'send' action)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing action".into()))?;

        let discovery_url = std::env::var("NODES_DISCOVERY_URL").ok();

        match action {
            "discover" => {
                if let Some(url) = &discovery_url {
                    let resp = self
                        .client
                        .get(format!("{url}/discover"))
                        .send()
                        .await
                        .map_err(|e| {
                            rustykrab_core::Error::ToolExecution(format!(
                                "node discovery failed: {e}"
                            ).into())
                        })?;

                    let body: Value = resp.json().await.map_err(|e| {
                        rustykrab_core::Error::ToolExecution(format!(
                            "failed to parse discovery response: {e}"
                        ).into())
                    })?;

                    Ok(json!({
                        "action": "discover",
                        "nodes": body,
                    }))
                } else {
                    Ok(json!({
                        "action": "discover",
                        "nodes": [],
                        "note": "No NODES_DISCOVERY_URL configured. Only local node available.",
                    }))
                }
            }
            "list" => {
                if let Some(url) = &discovery_url {
                    let resp = self
                        .client
                        .get(format!("{url}/nodes"))
                        .send()
                        .await
                        .map_err(|e| {
                            rustykrab_core::Error::ToolExecution(format!(
                                "node list failed: {e}"
                            ).into())
                        })?;

                    let body: Value = resp.json().await.map_err(|e| {
                        rustykrab_core::Error::ToolExecution(format!(
                            "failed to parse nodes response: {e}"
                        ).into())
                    })?;

                    Ok(json!({
                        "action": "list",
                        "nodes": body,
                    }))
                } else {
                    let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "local".into());
                    Ok(json!({
                        "action": "list",
                        "nodes": [{
                            "id": "local",
                            "name": hostname,
                            "status": "online",
                        }],
                    }))
                }
            }
            "send" => {
                let node_id = args["node_id"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(
                        "missing node_id for send action".into(),
                    )
                })?;
                let message = args["message"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(
                        "missing message for send action".into(),
                    )
                })?;

                let url = discovery_url.ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(
                        "NODES_DISCOVERY_URL required to send messages to nodes".into(),
                    )
                })?;

                let resp = self
                    .client
                    .post(format!("{url}/nodes/{node_id}/send"))
                    .json(&json!({"message": message}))
                    .send()
                    .await
                    .map_err(|e| {
                        rustykrab_core::Error::ToolExecution(format!(
                            "failed to send to node: {e}"
                        ).into())
                    })?;

                let status = resp.status().as_u16();
                let body = resp.text().await.map_err(|e| {
                    rustykrab_core::Error::ToolExecution(format!(
                        "failed to read response: {e}"
                    ).into())
                })?;

                Ok(json!({
                    "action": "send",
                    "node_id": node_id,
                    "success": status < 400,
                    "status": status,
                    "response": body,
                }))
            }
            _ => Err(rustykrab_core::Error::ToolExecution(format!(
                "unknown nodes action: {action}"
            ).into())),
        }
    }
}
