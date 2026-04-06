use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::timeout;

const MAX_OUTPUT_BYTES: usize = 100 * 1024; // 100KB

/// Commands that are explicitly allowed for execution.
/// All other commands are rejected to prevent arbitrary command injection.
const ALLOWED_COMMANDS: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "grep", "find", "sort", "uniq", "diff",
    "echo", "pwd", "whoami", "date", "env", "which", "file", "stat", "du", "df",
    "git", "cargo", "rustc", "python3", "python", "node", "npm", "npx",
    "make", "cmake", "gcc", "g++", "clang", "go", "java", "javac",
    "curl", "wget", "ssh", "scp", "rsync", "tar", "zip", "unzip", "gzip",
    "sed", "awk", "cut", "tr", "tee", "xargs", "mkdir", "rmdir", "cp", "mv",
    "touch", "chmod", "chown", "ln", "readlink", "basename", "dirname",
    "ps", "top", "htop", "kill", "pgrep", "lsof", "netstat", "ss",
    "docker", "docker-compose", "kubectl",
    "pip", "pip3", "poetry", "pipenv", "uv",
    "ruby", "gem", "bundle", "rake",
    "test", "true", "false", "sleep",
];

/// A built-in tool that executes shell commands and returns their output.
///
/// Security: Commands are validated against an allowlist to prevent
/// arbitrary command injection. Shell metacharacters in arguments are
/// handled safely by parsing the command into parts.
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

/// Validate that all commands in a potentially piped command are allowed.
fn validate_command(command: &str) -> std::result::Result<(), String> {
    // Block newline injection (could bypass allowlist via multi-line commands)
    if command.contains('\n') || command.contains('\r') {
        return Err("command contains newline characters".into());
    }
    // Block all variable expansion (not just ${...} but also bare $VAR)
    if command.contains('$') {
        return Err("command contains variable expansion ($)".into());
    }
    // Block shell redirects (could write/overwrite arbitrary files)
    if command.contains('>') || command.contains('<') {
        return Err("command contains redirects".into());
    }

    // Reject dangerous shell operators: $(...), `...`, process substitution
    if command.contains('`') {
        return Err("command substitution and variable expansion are not allowed".into());
    }

    // Split by pipes and semicolons, validate each segment
    for segment in command.split(&['|', ';'][..]) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }

        // Handle && and || by splitting further
        for sub in segment.split("&&").flat_map(|s| s.split("||")) {
            let sub = sub.trim();
            if sub.is_empty() {
                continue;
            }

            // Skip redirections (>, <, >>, 2>&1 etc.)
            if sub.starts_with('>') || sub.starts_with('<') {
                continue;
            }

            let base = sub.split_whitespace().next().unwrap_or("");
            let cmd_name = base.rsplit('/').next().unwrap_or(base);

            if !ALLOWED_COMMANDS.contains(&cmd_name) {
                return Err(format!(
                    "command '{}' is not in the allowlist. Allowed commands: {}",
                    cmd_name,
                    ALLOWED_COMMANDS.join(", ")
                ));
            }
        }
    }

    Ok(())
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its output. Commands are validated against an allowlist."
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
                        "description": "The shell command to execute (must use allowed commands)"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Timeout in seconds (default: 30, max: 120)"
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

        // Validate command against allowlist
        validate_command(command).map_err(|e| {
            openclaw_core::Error::ToolExecution(format!("command rejected: {e}"))
        })?;

        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(30).min(120);

        let future = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("LANG", "C.UTF-8")
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
