use async_trait::async_trait;
use base64::Engine;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};

/// A built-in tool that analyzes images from URLs or file paths.
pub struct ImageTool {
    client: reqwest::Client,
}

impl ImageTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for ImageTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ImageTool {
    fn name(&self) -> &str {
        "image"
    }

    fn description(&self) -> &str {
        "Analyze an image from a URL or file path and describe its contents."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "URL or file path of the image to analyze"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Optional prompt describing what to look for in the image"
                    }
                },
                "required": ["source"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let source = args["source"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing source".into()))?;

        let image_bytes = if source.starts_with("http://") || source.starts_with("https://") {
            // SSRF protection: validate URL before making request
            crate::security::validate_url(source)
                .map_err(|e| openclaw_core::Error::ToolExecution(format!("URL validation failed: {e}")))?;

            self.client
                .get(source)
                .send()
                .await
                .map_err(|e| openclaw_core::Error::ToolExecution(format!("failed to download image: {e}")))?
                .bytes()
                .await
                .map_err(|e| openclaw_core::Error::ToolExecution(format!("failed to read image bytes: {e}")))?
                .to_vec()
        } else {
            // Path traversal protection: validate path before reading
            let safe_path = crate::security::validate_path(source)
                .map_err(|e| openclaw_core::Error::ToolExecution(format!("path validation failed: {e}")))?;

            tokio::fs::read(&safe_path)
                .await
                .map_err(|e| openclaw_core::Error::ToolExecution(format!("failed to read image file: {e}")))?
        };

        let b64 = base64::engine::general_purpose::STANDARD.encode(&image_bytes);
        let b64_len = b64.len();

        Ok(json!({
            "source": source,
            "base64_length": b64_len,
            "analysis": "Image loaded successfully. Pass to model for analysis."
        }))
    }
}
