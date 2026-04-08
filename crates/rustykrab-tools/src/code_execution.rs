use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::timeout;

/// Resolve the full path to python3, checking common locations
/// so that pyenv/Homebrew/conda installs are found even after env_clear().
fn which_python() -> Option<std::path::PathBuf> {
    // Check the current PATH first (before we clear it)
    if let Ok(output) = std::process::Command::new("which")
        .arg("python3")
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                // Resolve shims (pyenv) to the actual binary
                if let Ok(canonical) = std::fs::canonicalize(&path) {
                    return Some(canonical);
                }
                return Some(path.into());
            }
        }
    }
    None
}

/// A built-in tool that executes Python code in a sandboxed environment.
///
/// Security improvements:
/// - Uses unique temp files per invocation (UUID-based) to prevent race conditions
/// - Cleans up temp files in all code paths
/// - Enforces timeout limits
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
        "Execute Python code and return stdout/stderr. Has access to the user's \
         Python environment including installed packages. If a package is missing, \
         install it with subprocess.check_call(['pip', 'install', 'package_name']). \
         Use this for data processing, file format conversion, calculations, \
         and any task that benefits from Python libraries."
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
                        "description": "Timeout in seconds (default: 30, max: 120)"
                    }
                },
                "required": ["code"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let code = args["code"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing code".into()))?;

        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(30).min(120);

        // Use UUID for unique temp file names to prevent race conditions
        // across concurrent sessions (fixes predictable PID-based naming)
        let unique_id = uuid::Uuid::new_v4();
        let tmp_dir = std::env::temp_dir().join("rustykrab_sandbox");

        // Create sandbox temp directory with restricted scope
        tokio::fs::create_dir_all(&tmp_dir)
            .await
            .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

        let tmp_file = tmp_dir.join(format!("exec_{}.py", unique_id));

        tokio::fs::write(&tmp_file, code)
            .await
            .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

        // Resolve the full path to python3 before clearing the environment.
        // This ensures we find the user's Python (e.g., pyenv, Homebrew)
        // rather than falling back to a system Python that may lack packages.
        let python_path = which_python().unwrap_or_else(|| "python3".into());

        // Include the Python's bin directory in PATH so pip is also available.
        // This allows the model to install packages if needed.
        let python_bin_dir = python_path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let sandbox_path = if python_bin_dir.is_empty() {
            "/usr/local/bin:/usr/bin:/bin".to_string()
        } else {
            format!("{python_bin_dir}:/usr/local/bin:/usr/bin:/bin")
        };

        let future = tokio::process::Command::new(&python_path)
            .arg(&tmp_file)
            .env_clear()
            .env("PATH", &sandbox_path)
            .env("HOME", std::env::temp_dir())
            .env("LANG", "C.UTF-8")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .current_dir(&tmp_dir)
            .output();

        let result = timeout(Duration::from_secs(timeout_secs), future).await;

        // Clean up temp file regardless of outcome
        let _ = tokio::fs::remove_file(&tmp_file).await;

        let output = result
            .map_err(|_| {
                rustykrab_core::Error::ToolExecution(format!(
                    "code execution timed out after {timeout_secs}s"
                ).into())
            })?
            .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

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
