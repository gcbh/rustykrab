use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// Commands allowed to be started as background processes.
const ALLOWED_PROCESS_COMMANDS: &[&str] = &[
    "python3",
    "python",
    "node",
    "npm",
    "npx",
    "cargo",
    "make",
    "docker",
    "docker-compose",
    "kubectl",
    "git",
    "ssh",
    "java",
    "go",
    "ruby",
    "tail",
    "watch",
];

/// A built-in tool that manages background processes: start, stop, or list.
///
/// Security: Only allowlisted commands can be started as background
/// processes. Shell metacharacters and command substitution are rejected.
///
/// Spawned child handles are retained so they can be awaited/killed later,
/// preventing zombie process leaks.
pub struct ProcessTool {
    children: Arc<Mutex<HashMap<u32, tokio::process::Child>>>,
}

impl ProcessTool {
    pub fn new() -> Self {
        Self {
            children: Arc::new(Mutex::new(HashMap::new())),
        }
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
    if command.contains("$(")
        || command.contains('`')
        || command.contains("<(")
        || command.contains(">(")
        || command.contains("${")
        || command.contains(';')
        || command.contains("&&")
        || command.contains("||")
        || command.contains('|')
    {
        return Err(
            "shell operators, pipes, and command substitution are not allowed in process commands"
                .into(),
        );
    }

    let base_cmd = command.split_whitespace().next().unwrap_or("");

    // Reject absolute or relative paths to prevent allowlist bypass —
    // e.g. /tmp/python would match "python" but execute a malicious binary.
    if base_cmd.contains('/') {
        return Err("absolute or relative paths are not allowed; use command names only".into());
    }

    if !ALLOWED_PROCESS_COMMANDS.contains(&base_cmd) {
        return Err(format!(
            "command '{}' is not allowed as a background process. Allowed: {}",
            base_cmd,
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
                let parts: Vec<&str> = command.split_whitespace().collect();
                let (program, cmd_args) = parts
                    .split_first()
                    .ok_or_else(|| rustykrab_core::Error::ToolExecution("empty command".into()))?;

                let mut cmd = tokio::process::Command::new(program);
                cmd.args(cmd_args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .env_clear();

                // Forward safe environment variables needed by common tools
                // (docker, kubectl, npm, ssh, etc.) while keeping secrets out.
                const SAFE_ENV_VARS: &[&str] = &[
                    "PATH",
                    "HOME",
                    "USER",
                    "LOGNAME",
                    "SHELL",
                    "TERM",
                    "LANG",
                    "LC_ALL",
                    "LC_CTYPE",
                    "TMPDIR",
                    "XDG_RUNTIME_DIR",
                    "XDG_CONFIG_HOME",
                    "XDG_DATA_HOME",
                    "DOCKER_HOST",
                    "DOCKER_CONFIG",
                    "KUBECONFIG",
                    "NODE_PATH",
                    "NPM_CONFIG_PREFIX",
                    "SSH_AUTH_SOCK",
                    "CARGO_HOME",
                    "RUSTUP_HOME",
                    "GOPATH",
                ];
                for var_name in SAFE_ENV_VARS {
                    if let Ok(val) = std::env::var(var_name) {
                        cmd.env(var_name, val);
                    }
                }
                // Ensure PATH and LANG always have sensible defaults
                if std::env::var("PATH").is_err() {
                    cmd.env("PATH", "/usr/local/bin:/usr/bin:/bin");
                }
                if std::env::var("LANG").is_err() {
                    cmd.env("LANG", "C.UTF-8");
                }

                let child = cmd
                    .spawn()
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

                let pid = child.id().ok_or_else(|| {
                    rustykrab_core::Error::ToolExecution(
                        "failed to get PID of spawned process".into(),
                    )
                })?;

                // Retain the child handle to prevent zombie process leaks.
                self.children.lock().await.insert(pid, child);

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

                // Only allow killing processes that were spawned by this tool.
                // This prevents arbitrary PID termination of system or
                // third-party processes.
                let pid_u32 = pid as u32;
                if let Some(mut child) = self.children.lock().await.remove(&pid_u32) {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    Ok(json!({
                        "action": "stop",
                        "pid": pid,
                        "status": "terminated",
                    }))
                } else {
                    Err(rustykrab_core::Error::ToolExecution(
                        format!("process {pid} was not spawned by this tool and cannot be stopped")
                            .into(),
                    ))
                }
            }
            "list" => {
                // Only list processes managed by this tool, not all system processes.
                let children = self.children.lock().await;
                let processes: Vec<Value> =
                    children.keys().map(|pid| json!({ "pid": pid })).collect();

                Ok(json!({
                    "action": "list",
                    "count": processes.len(),
                    "processes": processes,
                }))
            }
            other => Err(rustykrab_core::Error::ToolExecution(
                format!("unknown action: {other}").into(),
            )),
        }
    }
}
