use async_trait::async_trait;
use rustykrab_core::error::ToolError;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

use crate::security;

/// Maximum file size to read (10 MB).
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// A built-in tool that reads the contents of a file.
///
/// Security: Validates paths to prevent traversal attacks and blocks
/// access to sensitive system directories.
pub struct ReadTool;

impl ReadTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ReadTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file at a given path."
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
                        "description": "The absolute path to the file to read"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start reading from (0-based)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing path".into()))?;

        // Validate path for traversal and blocked directories
        let safe_path = security::validate_path(path).map_err(|e| {
            rustykrab_core::Error::ToolExecution(format!("path rejected: {e}").into())
        })?;

        // Check file size before reading
        let metadata = tokio::fs::metadata(&safe_path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                rustykrab_core::Error::ToolExecution(ToolError::not_found(format!(
                    "file not found: {path}"
                )))
            } else {
                rustykrab_core::Error::ToolExecution(format!("failed to stat {path}: {e}").into())
            }
        })?;

        if metadata.len() > MAX_FILE_SIZE {
            return Err(rustykrab_core::Error::ToolExecution(
                format!(
                "file is too large ({} bytes, max {} bytes). Use offset/limit to read a portion.",
                metadata.len(),
                MAX_FILE_SIZE
            )
                .into(),
            ));
        }

        let content = tokio::fs::read_to_string(&safe_path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                rustykrab_core::Error::ToolExecution(ToolError::not_found(format!(
                    "file not found: {path}"
                )))
            } else {
                rustykrab_core::Error::ToolExecution(format!("failed to read {path}: {e}").into())
            }
        })?;

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().map(|v| v as usize);

        let sliced: Vec<&str> = if offset > 0 || limit.is_some() {
            let start = offset.min(total_lines);
            let end = match limit {
                Some(l) => (start + l).min(total_lines),
                None => total_lines,
            };
            lines[start..end].to_vec()
        } else {
            lines
        };

        let result_lines = sliced.len();
        let result_content = sliced.join("\n");

        Ok(json!({
            "content": result_content,
            "lines": result_lines,
        }))
    }
}
