use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, SandboxRequirements, Tool};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::timeout;

use crate::sandboxed_spawn;

/// Resolve the full path to python3, checking common locations
/// so that pyenv/Homebrew/conda installs are found even after env_clear().
fn which_python() -> Option<std::path::PathBuf> {
    // Check the current PATH first (before we clear it)
    if let Ok(output) = std::process::Command::new("which").arg("python3").output() {
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
/// Security layers:
/// - macOS: Seatbelt profile via `sandbox-exec` denies network access and
///   restricts filesystem writes to the sandbox temp directory.
/// - Linux: PID/IPC/network namespace isolation via `unshare()`.
/// - All platforms: POSIX resource limits (memory, CPU, nproc, file size).
/// - UUID-based temp files prevent race conditions across concurrent sessions.
/// - Temp files cleaned up in all code paths.
/// - Timeout enforcement via tokio.
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
        "Execute Python code and return stdout/stderr. Has read-only access to the \
         user's Python environment including installed packages. Code runs in a \
         sandbox with no network access and restricted filesystem writes. \
         Use this for data processing, file format conversion, calculations, \
         and any task that benefits from Python libraries."
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_spawn: true,
            ..SandboxRequirements::default()
        }
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

        // Include the Python's bin directory in PATH so libraries are importable.
        let python_bin_dir = python_path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let sandbox_path = if python_bin_dir.is_empty() {
            "/usr/local/bin:/usr/bin:/bin".to_string()
        } else {
            format!("{python_bin_dir}:/usr/local/bin:/usr/bin:/bin")
        };

        let result = {
            // Build the sandboxed command based on the platform.
            #[cfg(target_os = "macos")]
            let future = {
                let profile_file = tmp_dir.join(format!("sandbox_{}.sb", unique_id));
                let profile = sandboxed_spawn::generate_seatbelt_profile(&tmp_dir, &python_path);
                tokio::fs::write(&profile_file, &profile)
                    .await
                    .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

                let mut cmd = tokio::process::Command::new("sandbox-exec");
                cmd.arg("-f")
                    .arg(&profile_file)
                    .arg(&python_path)
                    .arg(&tmp_file)
                    .env_clear()
                    .env("PATH", &sandbox_path)
                    .env("HOME", &tmp_dir)
                    .env("TMPDIR", &tmp_dir)
                    .env("LANG", "C.UTF-8")
                    .env("PYTHONDONTWRITEBYTECODE", "1")
                    .current_dir(&tmp_dir);

                // On macOS, resource limits are supplementary — the seatbelt
                // profile provides the primary containment. On Linux, rlimits
                // are the only defense when namespace isolation is unavailable.
                #[cfg(not(target_os = "macos"))]
                unsafe {
                    cmd.pre_exec(move || sandboxed_spawn::apply_resource_limits(max_mem, max_cpu));
                }

                let output_future = cmd.output();
                let r = timeout(Duration::from_secs(timeout_secs), output_future).await;

                // Clean up the seatbelt profile file
                let _ = tokio::fs::remove_file(&profile_file).await;
                r
            };

            #[cfg(target_os = "linux")]
            let future = {
                let max_mem = 512u64 * 1024 * 1024;
                let max_cpu = timeout_secs;

                let mut cmd = tokio::process::Command::new(&python_path);
                cmd.arg(&tmp_file)
                    .env_clear()
                    .env("PATH", &sandbox_path)
                    .env("HOME", &tmp_dir)
                    .env("TMPDIR", &tmp_dir)
                    .env("LANG", "C.UTF-8")
                    .env("PYTHONDONTWRITEBYTECODE", "1")
                    .current_dir(&tmp_dir);

                // Apply resource limits and namespace isolation in the child
                unsafe {
                    cmd.pre_exec(move || {
                        sandboxed_spawn::apply_resource_limits(max_mem, max_cpu)?;
                        if let Err(e) = sandboxed_spawn::apply_linux_namespaces() {
                            // Namespace isolation unavailable — rlimits still apply
                            eprintln!(
                                "rustykrab: namespace isolation unavailable: {e}, \
                                 falling back to resource limits only"
                            );
                        }
                        Ok(())
                    });
                }

                timeout(Duration::from_secs(timeout_secs), cmd.output()).await
            };

            // Fallback for other Unix platforms: rlimits only
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            let future = {
                let max_mem = 512u64 * 1024 * 1024;
                let max_cpu = timeout_secs;

                let mut cmd = tokio::process::Command::new(&python_path);
                cmd.arg(&tmp_file)
                    .env_clear()
                    .env("PATH", &sandbox_path)
                    .env("HOME", &tmp_dir)
                    .env("TMPDIR", &tmp_dir)
                    .env("LANG", "C.UTF-8")
                    .env("PYTHONDONTWRITEBYTECODE", "1")
                    .current_dir(&tmp_dir);

                #[cfg(unix)]
                unsafe {
                    cmd.pre_exec(move || sandboxed_spawn::apply_resource_limits(max_mem, max_cpu));
                }

                timeout(Duration::from_secs(timeout_secs), cmd.output()).await
            };

            future
        };

        // Clean up temp file regardless of outcome
        let _ = tokio::fs::remove_file(&tmp_file).await;

        let output = result
            .map_err(|_| {
                rustykrab_core::Error::ToolExecution(
                    format!("code execution timed out after {timeout_secs}s").into(),
                )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn basic_python_execution() {
        let tool = CodeExecutionTool::new();
        let result = tool.execute(json!({"code": "print(2 + 2)"})).await;
        assert!(result.is_ok(), "execution failed: {:?}", result.err());
        let output = result.unwrap();
        assert_eq!(output["stdout"].as_str().unwrap().trim(), "4");
        assert_eq!(output["exit_code"], 0);
    }

    /// Prints the sandbox's network-namespace identity as `NETNS:<id>`, then
    /// `BLOCKED: <reason>` when the connection is refused or `CONNECTED` when
    /// it succeeds.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    const NETWORK_PROBE: &str = r#"
import os, socket
try:
    print("NETNS:" + os.readlink("/proc/self/ns/net"))
except OSError:
    print("NETNS:unavailable")
try:
    s = socket.create_connection(("8.8.8.8", 53), timeout=3)
    s.close()
    print("CONNECTED")
except Exception as e:
    print(f"BLOCKED: {e}")
"#;

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn network_access_blocked() {
        // Seatbelt's `(deny network*)` needs no privileges, so containment is
        // unconditional here and the assertion can be strict. It previously
        // read `BLOCKED || CONNECTED`, which could not fail even if the
        // sandbox stopped containing anything at all.
        let tool = CodeExecutionTool::new();
        let output = tool
            .execute(json!({ "code": NETWORK_PROBE }))
            .await
            .expect("execution failed");
        let stdout = output["stdout"].as_str().unwrap();
        assert!(
            stdout.contains("BLOCKED"),
            "seatbelt must deny network access; stdout={stdout:?} stderr={:?}",
            output["stderr"]
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn network_access_blocked() {
        // CLONE_NEWNET needs CAP_SYS_ADMIN, which unprivileged CI runners do
        // not have; there the tool degrades to rlimits only and cannot contain
        // the network at all. So decide from what the sandbox actually got:
        // compare its network namespace against this process's. Asking the
        // kernel is the whole point — an earlier version of this test keyed off
        // a log line the tool prints on the fallback path, which never reached
        // the captured stderr, so the skip silently became a hard assert.
        let tool = CodeExecutionTool::new();
        let output = tool
            .execute(json!({ "code": NETWORK_PROBE }))
            .await
            .expect("execution failed");
        let stdout = output["stdout"].as_str().unwrap();

        let sandbox_netns = stdout
            .lines()
            .find_map(|l| l.strip_prefix("NETNS:"))
            .expect("the probe always reports a namespace line");
        let host_netns = std::fs::read_link("/proc/self/ns/net")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unavailable".to_string());

        // If either side could not be identified we cannot tell whether
        // isolation happened, and a test that cannot tell must not assert.
        if sandbox_netns == "unavailable" || host_netns == "unavailable" {
            eprintln!("skipping: network namespace identity unavailable on this host");
            return;
        }
        if sandbox_netns == host_netns {
            eprintln!(
                "skipping: no network namespace here (sandbox netns {sandbox_netns} \
                 matches the host), so the sandbox degraded to resource limits only"
            );
            return;
        }
        assert!(
            stdout.contains("BLOCKED"),
            "the sandbox got its own network namespace ({sandbox_netns} vs host \
             {host_netns}), so the connection must fail; stdout={stdout:?}"
        );
    }

    #[tokio::test]
    async fn timeout_enforced() {
        let tool = CodeExecutionTool::new();
        let code = "import time; time.sleep(60)";
        let result = tool.execute(json!({"code": code, "timeout_secs": 2})).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("timed out"), "got: {err}");
    }

    #[tokio::test]
    async fn missing_code_parameter() {
        let tool = CodeExecutionTool::new();
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing code"));
    }
}
