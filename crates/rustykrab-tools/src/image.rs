use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

/// Maximum image download size (50 MB).
const MAX_IMAGE_DOWNLOAD_SIZE: usize = 50 * 1024 * 1024;

/// A built-in tool that analyzes images from URLs or file paths.
pub struct ImageTool {
    client: reqwest::Client,
}

impl ImageTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
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
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing source".into()))?;

        let image_bytes = if source.starts_with("http://") || source.starts_with("https://") {
            // SSRF protection: validate URL before making request
            crate::security::validate_url(source).await.map_err(|e| {
                rustykrab_core::Error::ToolExecution(format!("URL validation failed: {e}").into())
            })?;

            let resp = self.client.get(source).send().await.map_err(|e| {
                rustykrab_core::Error::ToolExecution(
                    format!("failed to download image: {e}").into(),
                )
            })?;

            // Check content-length header for early rejection
            if let Some(len) = resp.content_length() {
                if len > MAX_IMAGE_DOWNLOAD_SIZE as u64 {
                    return Err(rustykrab_core::Error::ToolExecution(
                        format!("image too large: {len} bytes (max {MAX_IMAGE_DOWNLOAD_SIZE})")
                            .into(),
                    ));
                }
            }

            // Read body with size limit via chunked reading
            let mut bytes = Vec::new();
            let mut resp = resp;
            while let Some(chunk) = resp.chunk().await.map_err(|e| {
                rustykrab_core::Error::ToolExecution(
                    format!("failed to read image bytes: {e}").into(),
                )
            })? {
                bytes.extend_from_slice(&chunk);
                if bytes.len() > MAX_IMAGE_DOWNLOAD_SIZE {
                    return Err(rustykrab_core::Error::ToolExecution(
                        format!(
                            "image download exceeded size limit ({MAX_IMAGE_DOWNLOAD_SIZE} bytes)"
                        )
                        .into(),
                    ));
                }
            }
            bytes
        } else {
            // Path traversal protection: validate path before reading
            let safe_path = crate::security::validate_path(source).map_err(|e| {
                rustykrab_core::Error::ToolExecution(format!("path validation failed: {e}").into())
            })?;

            tokio::fs::read(&safe_path).await.map_err(|e| {
                rustykrab_core::Error::ToolExecution(
                    format!("failed to read image file: {e}").into(),
                )
            })?
        };

        // Compute base64 length without allocating the encoded string.
        let b64_len = image_bytes.len().div_ceil(3) * 4;

        let prompt = args["prompt"].as_str().unwrap_or("");

        Ok(json!({
            "source": source,
            "size_bytes": image_bytes.len(),
            "base64_length": b64_len,
            "prompt": prompt,
            "analysis": "Image loaded successfully. Pass to model for analysis."
        }))
    }
}
