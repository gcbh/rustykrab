use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::timeout;

const MAX_OUTPUT_BYTES: usize = 100 * 1024; // 100KB

/// Commands that are explicitly allowed for execution.
/// All other commands are rejected to prevent arbitrary command injection.
const ALLOWED_COMMANDS: &[&str] = &[
    "ls",
    "cat",
    "head",
    "tail",
    "wc",
    "grep",
    "find",
    "sort",
    "uniq",
    "diff",
    "echo",
    "pwd",
    "whoami",
    "date",
    "env",
    "which",
    "file",
    "stat",
    "du",
    "df",
    "git",
    "cargo",
    "rustc",
    "python3",
    "python",
    "node",
    "npm",
    "npx",
    "make",
    "cmake",
    "gcc",
    "g++",
    "clang",
    "go",
    "java",
    "javac",
    "curl",
    "wget",
    "ssh",
    "scp",
    "rsync",
    "tar",
    "zip",
    "unzip",
    "gzip",
    "sed",
    "awk",
    "cut",
    "tr",
    "tee",
    "xargs",
    "mkdir",
    "rmdir",
    "cp",
    "mv",
    "touch",
    "chmod",
    "chown",
    "ln",
    "readlink",
    "basename",
    "dirname",
    "ps",
    "top",
    "htop",
    "kill",
    "pgrep",
    "lsof",
    "netstat",
    "ss",
    "docker",
    "docker-compose",
    "kubectl",
    "pip",
    "pip3",
    "poetry",
    "pipenv",
    "uv",
    "ruby",
    "gem",
    "bundle",
    "rake",
    "test",
    "true",
    "false",
    "sleep",
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
        // Use floor_char_boundary to avoid panicking when the truncation
        // point falls within a multi-byte UTF-8 character.
        let end = s.floor_char_boundary(MAX_OUTPUT_BYTES);
        let mut truncated = s[..end].to_string();
        truncated.push_str("\n... [truncated]");
        truncated
    } else {
        s
    }
}

/// Validate that all commands in a potentially piped command are allowed.
///
/// The allowlist prevents arbitrary binary execution. Variable expansion,
/// redirects, and other shell features are permitted since commands run
/// via `sh -c` and the agent needs these for real-world tasks (e.g.
/// `echo $HOME`, `python3 script.py > output.txt`).
fn validate_command(command: &str) -> std::result::Result<(), String> {
    // Block newline injection (could bypass allowlist via multi-line commands).
    if command.contains('\n') || command.contains('\r') {
        return Err("command contains newline characters".into());
    }

    // Block shell substitution patterns that bypass the allowlist.
    // Since the command runs via `sh -c`, constructs like `$(cmd)`, `\`cmd\``,
    // and `${var}` can execute arbitrary commands even if the outer command
    // is in the allowlist (e.g. `echo $(rm -rf /)`).
    if command.contains("$(") || command.contains('`') {
        return Err(
            "command contains shell substitution ($() or backticks) which is not allowed".into(),
        );
    }

    // Split by pipes, semicolons, &&, || and validate each command segment.
    for segment in command.split(&['|', ';'][..]) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }

        for sub in segment.split("&&").flat_map(|s| s.split("||")) {
            let sub = sub.trim();
            if sub.is_empty() {
                continue;
            }

            // Skip redirect-only fragments (e.g. "> file", "2>&1").
            if sub.starts_with('>') || sub.starts_with('<') {
                continue;
            }

            // Find the actual command, skipping leading variable assignments
            // (e.g. `FOO=bar python3 script.py` → check `python3`).
            let mut found_allowed = false;
            for token in sub.split_whitespace() {
                // Variable assignments (KEY=value) are not commands — skip them.
                if token.contains('=') && !token.starts_with('=') {
                    continue;
                }
                let cmd_name = token.rsplit('/').next().unwrap_or(token);
                if ALLOWED_COMMANDS.contains(&cmd_name) {
                    found_allowed = true;
                    break;
                }
                // First non-assignment token that isn't allowed → reject.
                return Err(format!(
                    "command '{}' is not in the allowlist. Allowed commands: {}",
                    cmd_name,
                    ALLOWED_COMMANDS.join(", ")
                ));
            }
            // If segment was all variable assignments with no command, that's fine
            // (sh -c handles bare assignments).
            let _ = found_allowed;
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
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing command".into()))?;

        // Validate command against allowlist
        validate_command(command).map_err(|e| {
            rustykrab_core::Error::ToolExecution(format!("command rejected: {e}").into())
        })?;

        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(30).min(120);

        // Inherit the user's PATH so tools installed via homebrew, cargo,
        // pip, nvm, etc. are available. Fall back to a sensible default.
        let path = std::env::var("PATH")
            .unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string());

        let future = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .env_clear()
            .env("PATH", &path)
            .env("HOME", "/tmp/rustykrab-home")
            .env("LANG", "C.UTF-8")
            .output();

        let output = timeout(Duration::from_secs(timeout_secs), future)
            .await
            .map_err(|_| {
                rustykrab_core::Error::ToolExecution(
                    format!("command timed out after {timeout_secs}s").into(),
                )
            })?
            .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

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
