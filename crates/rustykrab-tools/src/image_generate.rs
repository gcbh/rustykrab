use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

/// A built-in tool that generates images from text prompts using a configured API.
pub struct ImageGenerateTool {
    client: reqwest::Client,
}

impl ImageGenerateTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for ImageGenerateTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ImageGenerateTool {
    fn name(&self) -> &str {
        "image_generate"
    }

    fn description(&self) -> &str {
        "Generate an image from a text prompt using a configured image generation API."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Text prompt describing the image to generate"
                    },
                    "width": {
                        "type": "integer",
                        "description": "Image width in pixels (default: 1024)"
                    },
                    "height": {
                        "type": "integer",
                        "description": "Image height in pixels (default: 1024)"
                    },
                    "output_path": {
                        "type": "string",
                        "description": "Optional file path to save the generated image"
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let prompt = args["prompt"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing prompt".into()))?;

        let width = args["width"].as_u64().unwrap_or(1024);
        let height = args["height"].as_u64().unwrap_or(1024);
        let output_path = args["output_path"].as_str();

        let api_url = std::env::var("IMAGE_API_URL").map_err(|_| {
            rustykrab_core::Error::ToolExecution(
                "image generation requires IMAGE_API_URL to be configured".into(),
            )
        })?;

        let api_key = std::env::var("IMAGE_API_KEY").unwrap_or_default();

        let mut req = self.client.post(&api_url).json(&json!({
            "prompt": prompt,
            "width": width,
            "height": height,
        }));

        if !api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {api_key}"));
        }

        let resp = req.send().await.map_err(|e| {
            rustykrab_core::Error::ToolExecution(
                format!("image generation request failed: {e}").into(),
            )
        })?;

        let resp_bytes = resp.bytes().await.map_err(|e| {
            rustykrab_core::Error::ToolExecution(format!("failed to read response: {e}").into())
        })?;

        let saved_path = if let Some(path) = output_path {
            // Path traversal protection: validate output path before writing
            let safe_path = crate::security::validate_path(path).map_err(|e| {
                rustykrab_core::Error::ToolExecution(
                    format!("output path validation failed: {e}").into(),
                )
            })?;
            tokio::fs::write(&safe_path, &resp_bytes)
                .await
                .map_err(|e| {
                    rustykrab_core::Error::ToolExecution(
                        format!("failed to save image: {e}").into(),
                    )
                })?;
            safe_path.to_string_lossy().to_string()
        } else {
            String::new()
        };

        Ok(json!({
            "generated": true,
            "path": saved_path,
            "prompt": prompt,
        }))
    }
}
