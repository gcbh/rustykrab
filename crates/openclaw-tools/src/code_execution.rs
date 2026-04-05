use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::timeout;

/// A built-in tool that executes Python code in a sandboxed environment.
pub struct CodeExecutionTool;

impl CodeExecutionTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CodeExecutionTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for CodeExecutionTool {
    fn name(&self) -> &str {
        "code_execution"
    }

    fn description(&self) -> &str {
        "Execute Python code in a sandboxed environment and return the result."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "The Python code to execute"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Timeout in seconds (default: 30)"
                    }
                },
                "required": ["code"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let code = args["code"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing code".into()))?;

        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(30);

        // Write code to a temporary file
        let tmp_dir = std::env::temp_dir();
        let tmp_file = tmp_dir.join(format!("openclaw_exec_{}.py", std::process::id()));

        tokio::fs::write(&tmp_file, code)
            .await
            .map_err(|e| openclaw_core::Error::ToolExecution(e.to_string()))?;

        let future = tokio::process::Command::new("python3")
            .arg(&tmp_file)
            .output();

        let result = timeout(Duration::from_secs(timeout_secs), future).await;

        // Clean up temp file regardless of outcome
        let _ = tokio::fs::remove_file(&tmp_file).await;

        let output = result
            .map_err(|_| {
                openclaw_core::Error::ToolExecution(format!(
                    "code execution timed out after {timeout_secs}s"
                ))
            })?
            .map_err(|e| openclaw_core::Error::ToolExecution(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let exit_code = output.status.code().unwrap_or(-1);

        Ok(json!({
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": exit_code,
        }))
    }
}
