use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

/// A built-in tool that creates and manipulates visual canvas content.
pub struct CanvasTool {
    client: reqwest::Client,
}

impl CanvasTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for CanvasTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for CanvasTool {
    fn name(&self) -> &str {
        "canvas"
    }

    fn description(&self) -> &str {
        "Create and manipulate visual canvas content: present HTML/SVG, evaluate scripts, and take snapshots."
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
                        "enum": ["present", "evaluate", "snapshot"],
                        "description": "The canvas action to perform"
                    },
                    "content": {
                        "type": "string",
                        "description": "HTML or SVG content to present (for 'present' action)"
                    },
                    "expression": {
                        "type": "string",
                        "description": "JavaScript expression to evaluate (for 'evaluate' action)"
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

        let canvas_api_url = std::env::var("CANVAS_API_URL").ok();

        match action {
            "present" => {
                let content = args["content"]
                    .as_str()
                    .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing content for present action".into()))?;

                if let Some(api_url) = &canvas_api_url {
                    let resp = self
                        .client
                        .post(api_url)
                        .json(&json!({"action": "present", "content": content}))
                        .send()
                        .await
                        .map_err(|e| rustykrab_core::Error::ToolExecution(format!("canvas present failed: {e}").into()))?;

                    let body = resp
                        .text()
                        .await
                        .map_err(|e| rustykrab_core::Error::ToolExecution(format!("failed to read response: {e}").into()))?;

                    Ok(json!({
                        "action": "present",
                        "success": true,
                        "result": body,
                    }))
                } else {
                    // Without a canvas API, validate and return the content
                    let content_len = content.len();
                    Ok(json!({
                        "action": "present",
                        "success": true,
                        "result": format!("Content accepted ({content_len} bytes). No canvas API configured for rendering."),
                        "content": content,
                    }))
                }
            }
            "evaluate" => {
                let expression = args["expression"]
                    .as_str()
                    .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing expression for evaluate action".into()))?;

                let api_url = canvas_api_url.ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(
                        "canvas evaluate action requires CANVAS_API_URL to be configured".into(),
                    )
                })?;

                let resp = self
                    .client
                    .post(&api_url)
                    .json(&json!({"action": "evaluate", "expression": expression}))
                    .send()
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(format!("canvas evaluate failed: {e}").into()))?;

                let body = resp
                    .text()
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(format!("failed to read response: {e}").into()))?;

                Ok(json!({
                    "action": "evaluate",
                    "success": true,
                    "result": body,
                }))
            }
            "snapshot" => {
                let api_url = canvas_api_url.ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(
                        "canvas snapshot action requires CANVAS_API_URL to be configured".into(),
                    )
                })?;

                let resp = self
                    .client
                    .post(&api_url)
                    .json(&json!({"action": "snapshot"}))
                    .send()
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(format!("canvas snapshot failed: {e}").into()))?;

                let body = resp
                    .text()
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(format!("failed to read response: {e}").into()))?;

                Ok(json!({
                    "action": "snapshot",
                    "success": true,
                    "result": body,
                }))
            }
            _ => Err(rustykrab_core::Error::ToolExecution(format!(
                "unknown canvas action: {action}"
            ).into())),
        }
    }
}
