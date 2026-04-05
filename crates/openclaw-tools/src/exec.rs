use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::timeout;

const MAX_OUTPUT_BYTES: usize = 100 * 1024; // 100KB

/// A built-in tool that executes shell commands and returns their output.
pub struct ExecTool;

impl ExecTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ExecTool {
    fn default() -> Self {
        Self::new()
    }
}

fn truncate_output(s: String) -> String {
    if s.len() > MAX_OUTPUT_BYTES {
        let mut truncated = s[..MAX_OUTPUT_BYTES].to_string();
        truncated.push_str("\n... [truncated]");
        truncated
    } else {
        s
    }
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its output."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Timeout in seconds (default: 30)"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing command".into()))?;

        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(30);

        let future = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output();

        let output = timeout(Duration::from_secs(timeout_secs), future)
            .await
            .map_err(|_| {
                openclaw_core::Error::ToolExecution(format!(
                    "command timed out after {timeout_secs}s"
                ))
            })?
            .map_err(|e| openclaw_core::Error::ToolExecution(e.to_string()))?;

        let stdout = truncate_output(String::from_utf8_lossy(&output.stdout).into_owned());
        let stderr = truncate_output(String::from_utf8_lossy(&output.stderr).into_owned());
        let exit_code = output.status.code().unwrap_or(-1);

        Ok(json!({
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
        }))
    }
}
