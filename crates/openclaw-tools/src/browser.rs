use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};

/// A built-in tool that controls a headless browser for web automation.
pub struct BrowserTool {
    client: reqwest::Client,
}

impl BrowserTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for BrowserTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        "Control a headless browser: navigate to URLs, click elements, take screenshots, and extract content."
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
                        "enum": ["navigate", "click", "screenshot", "content", "evaluate"],
                        "description": "The browser action to perform"
                    },
                    "url": {
                        "type": "string",
                        "description": "URL to navigate to (for 'navigate' action)"
                    },
                    "selector": {
                        "type": "string",
                        "description": "CSS selector for the element to interact with (for 'click' action)"
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
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing action".into()))?;

        let browser_ws_url = std::env::var("BROWSER_WS_URL").ok();

        match action {
            "navigate" => {
                let url = args["url"]
                    .as_str()
                    .ok_or_else(|| openclaw_core::Error::ToolExecution("missing url for navigate action".into()))?;

                if let Some(ws_url) = &browser_ws_url {
                    // Send navigate command to browser automation endpoint
                    let resp = self
                        .client
                        .post(ws_url)
                        .json(&json!({"action": "navigate", "url": url}))
                        .send()
                        .await
                        .map_err(|e| openclaw_core::Error::ToolExecution(format!("browser navigate failed: {e}")))?;

                    let body = resp
                        .text()
                        .await
                        .map_err(|e| openclaw_core::Error::ToolExecution(format!("failed to read response: {e}")))?;

                    Ok(json!({
                        "action": "navigate",
                        "url": url,
                        "success": true,
                        "result": body,
                    }))
                } else {
                    // Fallback: fetch URL content directly with reqwest
                    let resp = self
                        .client
                        .get(url)
                        .send()
                        .await
                        .map_err(|e| openclaw_core::Error::ToolExecution(format!("failed to fetch URL: {e}")))?;

                    let status = resp.status().as_u16();
                    let body = resp
                        .text()
                        .await
                        .map_err(|e| openclaw_core::Error::ToolExecution(format!("failed to read response: {e}")))?;

                    Ok(json!({
                        "action": "navigate",
                        "url": url,
                        "success": true,
                        "status": status,
                        "content": body,
                        "note": "Used HTTP fallback - no browser automation endpoint configured",
                    }))
                }
            }
            "content" => {
                let url = args["url"].as_str();

                if let Some(ws_url) = &browser_ws_url {
                    let mut payload = json!({"action": "content"});
                    if let Some(u) = url {
                        payload["url"] = json!(u);
                    }

                    let resp = self
                        .client
                        .post(ws_url)
                        .json(&payload)
                        .send()
                        .await
                        .map_err(|e| openclaw_core::Error::ToolExecution(format!("browser content failed: {e}")))?;

                    let body = resp
                        .text()
                        .await
                        .map_err(|e| openclaw_core::Error::ToolExecution(format!("failed to read response: {e}")))?;

                    Ok(json!({
                        "action": "content",
                        "success": true,
                        "content": body,
                    }))
                } else if let Some(u) = url {
                    // Fallback: fetch content directly
                    let resp = self
                        .client
                        .get(u)
                        .send()
                        .await
                        .map_err(|e| openclaw_core::Error::ToolExecution(format!("failed to fetch URL: {e}")))?;

                    let body = resp
                        .text()
                        .await
                        .map_err(|e| openclaw_core::Error::ToolExecution(format!("failed to read response: {e}")))?;

                    Ok(json!({
                        "action": "content",
                        "success": true,
                        "content": body,
                        "note": "Used HTTP fallback - no browser automation endpoint configured",
                    }))
                } else {
                    Err(openclaw_core::Error::ToolExecution(
                        "content action requires either BROWSER_WS_URL or a url parameter".into(),
                    ))
                }
            }
            "click" => {
                let selector = args["selector"]
                    .as_str()
                    .ok_or_else(|| openclaw_core::Error::ToolExecution("missing selector for click action".into()))?;

                let ws_url = browser_ws_url.ok_or_else(|| {
                    openclaw_core::Error::ToolExecution(
                        "browser tool requires BROWSER_WS_URL to be configured for click action".into(),
                    )
                })?;

                let resp = self
                    .client
                    .post(&ws_url)
                    .json(&json!({"action": "click", "selector": selector}))
                    .send()
                    .await
                    .map_err(|e| openclaw_core::Error::ToolExecution(format!("browser click failed: {e}")))?;

                let body = resp
                    .text()
                    .await
                    .map_err(|e| openclaw_core::Error::ToolExecution(format!("failed to read response: {e}")))?;

                Ok(json!({
                    "action": "click",
                    "selector": selector,
                    "success": true,
                    "result": body,
                }))
            }
            "screenshot" => {
                let ws_url = browser_ws_url.ok_or_else(|| {
                    openclaw_core::Error::ToolExecution(
                        "browser tool requires BROWSER_WS_URL to be configured for screenshot action".into(),
                    )
                })?;

                let resp = self
                    .client
                    .post(&ws_url)
                    .json(&json!({"action": "screenshot"}))
                    .send()
                    .await
                    .map_err(|e| openclaw_core::Error::ToolExecution(format!("browser screenshot failed: {e}")))?;

                let body = resp
                    .text()
                    .await
                    .map_err(|e| openclaw_core::Error::ToolExecution(format!("failed to read response: {e}")))?;

                Ok(json!({
                    "action": "screenshot",
                    "success": true,
                    "result": body,
                }))
            }
            "evaluate" => {
                let expression = args["expression"]
                    .as_str()
                    .ok_or_else(|| openclaw_core::Error::ToolExecution("missing expression for evaluate action".into()))?;

                let ws_url = browser_ws_url.ok_or_else(|| {
                    openclaw_core::Error::ToolExecution(
                        "browser tool requires BROWSER_WS_URL to be configured for evaluate action".into(),
                    )
                })?;

                let resp = self
                    .client
                    .post(&ws_url)
                    .json(&json!({"action": "evaluate", "expression": expression}))
                    .send()
                    .await
                    .map_err(|e| openclaw_core::Error::ToolExecution(format!("browser evaluate failed: {e}")))?;

                let body = resp
                    .text()
                    .await
                    .map_err(|e| openclaw_core::Error::ToolExecution(format!("failed to read response: {e}")))?;

                Ok(json!({
                    "action": "evaluate",
                    "expression": expression,
                    "success": true,
                    "result": body,
                }))
            }
            _ => Err(openclaw_core::Error::ToolExecution(format!(
                "unknown browser action: {action}"
            ))),
        }
    }
}
