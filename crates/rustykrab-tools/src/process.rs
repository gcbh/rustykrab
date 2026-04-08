use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

/// Commands allowed to be started as background processes.
const ALLOWED_PROCESS_COMMANDS: &[&str] = &[
    "python3", "python", "node", "npm", "npx", "cargo", "make",
    "docker", "docker-compose", "kubectl",
    "git", "ssh", "java", "go", "ruby",
    "tail", "watch",
];

/// A built-in tool that manages background processes: start, stop, or list.
///
/// Security: Only allowlisted commands can be started as background
/// processes. Shell metacharacters and command substitution are rejected.
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

/// Validate that a command is safe to spawn as a background process.
fn validate_process_command(command: &str) -> std::result::Result<(), String> {
    // Reject shell metacharacters that enable injection
    if command.contains("$(") || command.contains('`')
        || command.contains("<(") || command.contains(">(")
        || command.contains("${")
        || command.contains(';') || command.contains("&&")
        || command.contains("||") || command.contains('|')
    {
        return Err("shell operators, pipes, and command substitution are not allowed in process commands".into());
    }

    let base_cmd = command.trim().split_whitespace().next().unwrap_or("");
    let cmd_name = base_cmd.rsplit('/').next().unwrap_or(base_cmd);

    if !ALLOWED_PROCESS_COMMANDS.contains(&cmd_name) {
        return Err(format!(
            "command '{}' is not allowed as a background process. Allowed: {}",
            cmd_name,
            ALLOWED_PROCESS_COMMANDS.join(", ")
        ));
    }

    Ok(())
}

#[async_trait]
impl Tool for ProcessTool {
    fn name(&self) -> &str {
        "process"
    }

    fn description(&self) -> &str {
        "Manage background processes: start, stop, or list. Only allowlisted commands can be started."
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
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing action".into()))?;

        match action {
            "start" => {
                let command = args["command"].as_str().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(
                        "missing command for 'start' action".into(),
                    )
                })?;

                // Validate command against allowlist
                validate_process_command(command).map_err(|e| {
                    rustykrab_core::Error::ToolExecution(format!("command rejected: {e}").into())
                })?;

                // Parse the command into parts and execute directly (no shell)
                let parts: Vec<&str> = command.trim().split_whitespace().collect();
                let (program, cmd_args) = parts.split_first().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution("empty command".into())
                })?;

                let child = tokio::process::Command::new(program)
                    .args(cmd_args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .env_clear()
                    .env("PATH", "/usr/local/bin:/usr/bin:/bin")
                    .env("HOME", std::env::var("HOME").unwrap_or_default())
                    .env("LANG", "C.UTF-8")
                    .spawn()
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

                let pid = child.id().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(
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
                    rustykrab_core::Error::ToolExecution("missing pid for 'stop' action".into())
                })?;

                // Validate PID is positive and reasonable
                if pid <= 1 {
                    return Err(rustykrab_core::Error::ToolExecution(
                        "cannot stop PID 0 or 1 (system processes)".into(),
                    ));
                }

                let output = tokio::process::Command::new("kill")
                    .arg(pid.to_string())
                    .output()
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

                if output.status.success() {
                    Ok(json!({
                        "action": "stop",
                        "pid": pid,
                        "status": "terminated",
                    }))
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    Err(rustykrab_core::Error::ToolExecution(format!(
                        "failed to stop process {pid}: {stderr}"
                    ).into()))
                }
            }
            "list" => {
                let output = tokio::process::Command::new("ps")
                    .args(["aux", "--no-headers"])
                    .output()
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

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
            other => Err(rustykrab_core::Error::ToolExecution(format!(
                "unknown action: {other}"
            ).into())),
        }
    }
}
