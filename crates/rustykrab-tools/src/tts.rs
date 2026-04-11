use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

/// Maximum TTS response size (100 MB).
const MAX_TTS_RESPONSE_SIZE: usize = 100 * 1024 * 1024;

/// A built-in tool that converts text to speech audio using a configured TTS API.
pub struct TtsTool {
    client: reqwest::Client,
}

impl TtsTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
        }
    }
}

impl Default for TtsTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TtsTool {
    fn name(&self) -> &str {
        "tts"
    }

    fn description(&self) -> &str {
        "Convert text to speech audio using a configured TTS API."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The text to convert to speech"
                    },
                    "voice": {
                        "type": "string",
                        "description": "Voice to use for synthesis (default: \"default\")"
                    },
                    "output_path": {
                        "type": "string",
                        "description": "File path to save the generated audio"
                    }
                },
                "required": ["text", "output_path"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let text = args["text"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing text".into()))?;

        let voice = args["voice"].as_str().unwrap_or("default");

        let output_path = args["output_path"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing output_path".into()))?;

        let api_url = std::env::var("TTS_API_URL").map_err(|_| {
            rustykrab_core::Error::ToolExecution("TTS requires TTS_API_URL to be configured".into())
        })?;

        let api_key = std::env::var("TTS_API_KEY").unwrap_or_default();

        // Path traversal protection: validate output path before writing
        let safe_output_path = crate::security::validate_path(output_path).map_err(|e| {
            rustykrab_core::Error::ToolExecution(
                format!("output path validation failed: {e}").into(),
            )
        })?;

        let text_length = text.len();

        let mut req = self.client.post(&api_url).json(&json!({
            "text": text,
            "voice": voice,
        }));

        if !api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {api_key}"));
        }

        let mut resp = req.send().await.map_err(|e| {
            rustykrab_core::Error::ToolExecution(format!("TTS request failed: {e}").into())
        })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            return Err(rustykrab_core::Error::ToolExecution(
                format!("TTS API returned {status}: {err}").into(),
            ));
        }

        // Check content-length header for early rejection
        if let Some(len) = resp.content_length() {
            if len > MAX_TTS_RESPONSE_SIZE as u64 {
                return Err(rustykrab_core::Error::ToolExecution(
                    format!("TTS response too large: {len} bytes (max {MAX_TTS_RESPONSE_SIZE})")
                        .into(),
                ));
            }
        }

        // Read body with size limit via chunked reading
        let mut audio_bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| {
            rustykrab_core::Error::ToolExecution(format!("failed to read TTS response: {e}").into())
        })? {
            audio_bytes.extend_from_slice(&chunk);
            if audio_bytes.len() > MAX_TTS_RESPONSE_SIZE {
                return Err(rustykrab_core::Error::ToolExecution(
                    format!("TTS response exceeded size limit ({MAX_TTS_RESPONSE_SIZE} bytes)")
                        .into(),
                ));
            }
        }

        tokio::fs::write(&safe_output_path, &audio_bytes)
            .await
            .map_err(|e| {
                rustykrab_core::Error::ToolExecution(
                    format!("failed to save audio file: {e}").into(),
                )
            })?;

        Ok(json!({
            "generated": true,
            "path": output_path,
            "text_length": text_length,
        }))
    }
}
