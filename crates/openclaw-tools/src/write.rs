use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};

/// A built-in tool that writes content to a file.
pub struct WriteTool;

impl WriteTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file at a given path, creating directories as needed."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The absolute path to the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing path".into()))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing content".into()))?;

        let file_path = std::path::Path::new(path);
        if let Some(parent) = file_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| openclaw_core::Error::ToolExecution(
                    format!("failed to create directories for {path}: {e}"),
                ))?;
        }

        let bytes = content.len();
        tokio::fs::write(path, content)
            .await
            .map_err(|e| openclaw_core::Error::ToolExecution(
                format!("failed to write {path}: {e}"),
            ))?;

        Ok(json!({
            "written": true,
            "bytes": bytes,
        }))
    }
}
