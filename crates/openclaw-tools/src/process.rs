use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};

/// A built-in tool that manages background processes: start, stop, or list.
pub struct ProcessTool;

impl ProcessTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ProcessTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ProcessTool {
    fn name(&self) -> &str {
        "process"
    }

    fn description(&self) -> &str {
        "Manage background processes: start, stop, or list."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["start", "stop", "list"],
                        "description": "The action to perform"
                    },
                    "command": {
                        "type": "string",
                        "description": "The command to start (required for 'start' action)"
                    },
                    "pid": {
                        "type": "integer",
                        "description": "The PID of the process to stop (required for 'stop' action)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing action".into()))?;

        match action {
            "start" => {
                let command = args["command"].as_str().ok_or_else(|| {
                    openclaw_core::Error::ToolExecution(
                        "missing command for 'start' action".into(),
                    )
                })?;

                let child = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map_err(|e| openclaw_core::Error::ToolExecution(e.to_string()))?;

                let pid = child.id().ok_or_else(|| {
                    openclaw_core::Error::ToolExecution(
                        "failed to get PID of spawned process".into(),
                    )
                })?;

                Ok(json!({
                    "action": "start",
                    "pid": pid,
                    "command": command,
                    "status": "started",
                }))
            }
            "stop" => {
                let pid = args["pid"].as_i64().ok_or_else(|| {
                    openclaw_core::Error::ToolExecution("missing pid for 'stop' action".into())
                })?;

                let output = tokio::process::Command::new("kill")
                    .arg(pid.to_string())
                    .output()
                    .await
                    .map_err(|e| openclaw_core::Error::ToolExecution(e.to_string()))?;

                if output.status.success() {
                    Ok(json!({
                        "action": "stop",
                        "pid": pid,
                        "status": "terminated",
                    }))
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    Err(openclaw_core::Error::ToolExecution(format!(
                        "failed to stop process {pid}: {stderr}"
                    )))
                }
            }
            "list" => {
                let output = tokio::process::Command::new("ps")
                    .args(["aux", "--no-headers"])
                    .output()
                    .await
                    .map_err(|e| openclaw_core::Error::ToolExecution(e.to_string()))?;

                let stdout = String::from_utf8_lossy(&output.stdout);
                let processes: Vec<Value> = stdout
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 11 {
                            json!({
                                "user": parts[0],
                                "pid": parts[1],
                                "cpu": parts[2],
                                "mem": parts[3],
                                "command": parts[10..].join(" "),
                            })
                        } else {
                            json!({ "raw": line })
                        }
                    })
                    .collect();

                Ok(json!({
                    "action": "list",
                    "processes": processes,
                }))
            }
            other => Err(openclaw_core::Error::ToolExecution(format!(
                "unknown action: {other}"
            ))),
        }
    }
}
