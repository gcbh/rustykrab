use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};

/// A built-in tool that reads the contents of a file.
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
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing path".into()))?;

        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| openclaw_core::Error::ToolExecution(format!("failed to read {path}: {e}")))?;

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
