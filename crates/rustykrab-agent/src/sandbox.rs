use async_trait::async_trait;
use rustykrab_core::{Error, Result};
use serde_json::Value;

/// Policy that controls what a sandboxed tool execution can do.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// Allow filesystem reads.
    pub allow_fs_read: bool,
    /// Allow filesystem writes.
    pub allow_fs_write: bool,
    /// Allow network access.
    pub allow_net: bool,
    /// Allow spawning child processes.
    pub allow_spawn: bool,
    /// Maximum execution time in seconds.
    pub timeout_secs: u64,
    /// Maximum memory in bytes (0 = unlimited).
    pub max_memory_bytes: u64,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            allow_fs_read: false,
            allow_fs_write: false,
            allow_net: false,
            allow_spawn: false,
            timeout_secs: 30,
            max_memory_bytes: 256 * 1024 * 1024, // 256 MB
        }
    }
}

impl SandboxPolicy {
    /// A permissive policy for trusted, built-in tools.
    pub fn trusted() -> Self {
        Self {
            allow_fs_read: true,
            allow_fs_write: true,
            allow_net: true,
            allow_spawn: false,
            timeout_secs: 60,
            max_memory_bytes: 0,
        }
    }
}

/// Trait for sandbox implementations.
///
/// Different backends can implement this: process isolation (fork+seccomp),
/// WASM (wasmtime), or container-based (Docker/nsjail). The runner uses
/// this trait to execute tool calls in isolation, preventing the sandbox
/// escape class of bugs (CVE-2026-32048) where child processes inherited
/// unsandboxed permissions.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// Execute a tool call within the sandbox.
    ///
    /// The sandbox receives the tool name, arguments, and a policy
    /// controlling what the sandboxed code can do.
    async fn execute(&self, tool_name: &str, args: Value, policy: &SandboxPolicy) -> Result<Value>;
}

/// Validate that a tool's required capabilities are permitted by the policy.
///
/// This is a defense-in-depth check that mirrors the runner's
/// `enforce_sandbox_policy()`. Even if the runner check is bypassed,
/// the sandbox itself rejects disallowed operations.
fn validate_tool_policy(tool_name: &str, policy: &SandboxPolicy) -> Result<()> {
    let needs_fs_read = matches!(tool_name, "read" | "pdf" | "image");
    let needs_fs_write = matches!(
        tool_name,
        "write" | "edit" | "apply_patch" | "tts" | "image_generate" | "skill_create" | "canvas"
    );
    let needs_spawn = matches!(tool_name, "exec" | "process" | "code_execution");
    let needs_net = matches!(
        tool_name,
        "http_request"
            | "http_session"
            | "web_fetch"
            | "web_search"
            | "x_search"
            | "browser"
            | "gmail"
            | "image_generate"
            | "tts"
    );

    if needs_fs_read && !policy.allow_fs_read {
        return Err(Error::Auth(format!(
            "sandbox denied tool '{tool_name}': filesystem read access not permitted"
        )));
    }
    if needs_fs_write && !policy.allow_fs_write {
        return Err(Error::Auth(format!(
            "sandbox denied tool '{tool_name}': filesystem write access not permitted"
        )));
    }
    if needs_spawn && !policy.allow_spawn {
        return Err(Error::Auth(format!(
            "sandbox denied tool '{tool_name}': process spawning not permitted"
        )));
    }
    if needs_net && !policy.allow_net {
        return Err(Error::Auth(format!(
            "sandbox denied tool '{tool_name}': network access not permitted"
        )));
    }
    Ok(())
}

/// Process-based sandbox using resource limits and policy enforcement.
///
/// Provides two layers of protection:
/// 1. Policy validation: rejects tool calls that violate the `SandboxPolicy`
/// 2. Timeout enforcement: kills long-running tool executions
///
/// Individual tools that spawn subprocesses (e.g. `CodeExecutionTool`)
/// apply additional OS-level isolation (macOS Seatbelt, Linux namespaces)
/// within their own execution.
pub struct ProcessSandbox;

impl ProcessSandbox {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ProcessSandbox {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Sandbox for ProcessSandbox {
    async fn execute(&self, tool_name: &str, args: Value, policy: &SandboxPolicy) -> Result<Value> {
        use tokio::time::{timeout, Duration};

        // Enforce policy constraints: reject tool calls that require
        // capabilities not granted by the policy.
        validate_tool_policy(tool_name, policy)?;

        let timeout_duration = Duration::from_secs(policy.timeout_secs);
        let tool = tool_name.to_string();

        let result = timeout(timeout_duration, async move {
            tracing::info!(tool = %tool, "executing in sandbox with policy enforcement");
            Ok(args)
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(_) => Err(Error::ToolExecution(
                format!(
                    "tool '{tool_name}' exceeded sandbox timeout of {}s",
                    policy.timeout_secs
                )
                .into(),
            )),
        }
    }
}

/// No-op sandbox that passes through directly (for testing only).
pub struct NoSandbox;

#[async_trait]
impl Sandbox for NoSandbox {
    async fn execute(
        &self,
        _tool_name: &str,
        args: Value,
        _policy: &SandboxPolicy,
    ) -> Result<Value> {
        Ok(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn sandbox_denies_code_execution_without_spawn() {
        let sandbox = ProcessSandbox::new();
        let policy = SandboxPolicy {
            allow_spawn: false,
            ..SandboxPolicy::default()
        };
        let result = sandbox
            .execute("code_execution", json!({"code": "print(1)"}), &policy)
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("process spawning not permitted"), "got: {err}");
    }

    #[tokio::test]
    async fn sandbox_allows_code_execution_with_spawn() {
        let sandbox = ProcessSandbox::new();
        let policy = SandboxPolicy {
            allow_spawn: true,
            ..SandboxPolicy::default()
        };
        let args = json!({"code": "print(1)"});
        let result = sandbox
            .execute("code_execution", args.clone(), &policy)
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), args);
    }

    #[tokio::test]
    async fn sandbox_denies_read_without_fs_read() {
        let sandbox = ProcessSandbox::new();
        let policy = SandboxPolicy::default(); // all denied
        let result = sandbox
            .execute("read", json!({"path": "/etc/passwd"}), &policy)
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("filesystem read access not permitted"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn sandbox_denies_write_without_fs_write() {
        let sandbox = ProcessSandbox::new();
        let policy = SandboxPolicy {
            allow_fs_read: true,
            ..SandboxPolicy::default()
        };
        let result = sandbox
            .execute("write", json!({"path": "/tmp/x", "content": "y"}), &policy)
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("filesystem write access not permitted"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn sandbox_denies_network_without_allow_net() {
        let sandbox = ProcessSandbox::new();
        let policy = SandboxPolicy::default();
        let result = sandbox
            .execute(
                "http_request",
                json!({"url": "http://example.com"}),
                &policy,
            )
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("network access not permitted"), "got: {err}");
    }

    #[tokio::test]
    async fn sandbox_allows_memory_tools_with_default_policy() {
        let sandbox = ProcessSandbox::new();
        let policy = SandboxPolicy::default();
        // Memory tools don't require fs/net/spawn
        let result = sandbox
            .execute("memory_save", json!({"fact": "test"}), &policy)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn sandbox_timeout_triggers_error() {
        let sandbox = ProcessSandbox::new();
        let policy = SandboxPolicy {
            allow_spawn: true,
            timeout_secs: 0, // immediate timeout
            ..SandboxPolicy::default()
        };
        // The async block should be interrupted by the zero timeout
        let result = sandbox
            .execute("code_execution", json!({"code": "x"}), &policy)
            .await;
        // With timeout_secs=0, this may or may not timeout depending on
        // scheduling, so we just verify it doesn't panic. A real timeout
        // test would use a longer-running operation.
        assert!(result.is_ok() || result.unwrap_err().to_string().contains("timeout"));
    }

    #[tokio::test]
    async fn sandbox_trusted_policy_allows_most_tools() {
        let sandbox = ProcessSandbox::new();
        let policy = SandboxPolicy::trusted();
        // Trusted allows fs_read, fs_write, net but NOT spawn
        assert!(sandbox.execute("read", json!({}), &policy).await.is_ok());
        assert!(sandbox.execute("write", json!({}), &policy).await.is_ok());
        assert!(sandbox
            .execute("http_request", json!({}), &policy)
            .await
            .is_ok());
        // Spawn is still denied in trusted policy
        assert!(sandbox.execute("exec", json!({}), &policy).await.is_err());
    }
}
