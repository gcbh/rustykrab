use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::gateway_backend::GatewayBackend;

/// A tool that queries and manages the OpenClaw gateway.
pub struct GatewayTool {
    backend: Arc<dyn GatewayBackend>,
}

impl GatewayTool {
    pub fn new(backend: Arc<dyn GatewayBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for GatewayTool {
    fn name(&self) -> &str {
        "gateway"
    }

    fn description(&self) -> &str {
        "Query and manage the OpenClaw gateway: status, config, and health."
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
                        "enum": ["status", "health", "config"],
                        "description": "The gateway action to perform"
                    },
                    "key": {
                        "type": "string",
                        "description": "Config key (for config action)"
                    },
                    "value": {
                        "type": "string",
                        "description": "Config value to set (for config action)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing action".into()))?;

        match action {
            "status" => {
                let status = self
                    .backend
                    .status()
                    .await
                    .map_err(|e| openclaw_core::Error::ToolExecution(e.to_string().into()))?;

                Ok(json!({
                    "action": "status",
                    "status": status,
                }))
            }
            "health" => {
                let health = self
                    .backend
                    .health()
                    .await
                    .map_err(|e| openclaw_core::Error::ToolExecution(e.to_string().into()))?;

                Ok(json!({
                    "action": "health",
                    "health": health,
                }))
            }
            "config" => {
                let key = args["key"].as_str();
                let value = args["value"].as_str();

                if let (Some(k), Some(v)) = (key, value) {
                    let result = self
                        .backend
                        .set_config(k, v)
                        .await
                        .map_err(|e| openclaw_core::Error::ToolExecution(e.to_string().into()))?;

                    Ok(json!({
                        "action": "config",
                        "operation": "set",
                        "result": result,
                    }))
                } else {
                    let config = self
                        .backend
                        .get_config(key)
                        .await
                        .map_err(|e| openclaw_core::Error::ToolExecution(e.to_string().into()))?;

                    Ok(json!({
                        "action": "config",
                        "operation": "get",
                        "config": config,
                    }))
                }
            }
            _ => Err(openclaw_core::Error::ToolExecution(
                format!("unknown action: {action}").into(),
            )),
        }
    }
}
