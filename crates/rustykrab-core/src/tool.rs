use async_trait::async_trait;
use serde_json::Value;

use crate::error::Result;
use crate::types::ToolSchema;

/// Declares what sandbox capabilities a tool requires at runtime.
///
/// Each tool overrides [`Tool::sandbox_requirements`] to declare its needs.
/// The sandbox enforces these requirements against the session's policy —
/// no hardcoded tool-name allowlists required.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxRequirements {
    /// Tool reads from the filesystem.
    pub needs_fs_read: bool,
    /// Tool writes to the filesystem.
    pub needs_fs_write: bool,
    /// Tool makes network requests.
    pub needs_net: bool,
    /// Tool spawns child processes.
    pub needs_spawn: bool,
}

impl SandboxRequirements {
    /// Returns true if this tool can cause external side effects
    /// (writes, network calls, or process spawning).
    pub fn has_side_effects(&self) -> bool {
        self.needs_fs_write || self.needs_net || self.needs_spawn
    }
}

/// Trait implemented by every tool available to the agent.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique name used to invoke this tool.
    fn name(&self) -> &str;

    /// Human-readable description shown to the model.
    fn description(&self) -> &str;

    /// JSON Schema describing the expected parameters.
    fn schema(&self) -> ToolSchema;

    /// Whether this tool is currently available for use.
    ///
    /// Tools that depend on external configuration (environment variables,
    /// credentials, etc.) should override this to return `false` when the
    /// required configuration is missing.  Unavailable tools are excluded
    /// from the schemas sent to the model so it never attempts to call a
    /// tool that will fail due to missing configuration.
    fn available(&self) -> bool {
        true
    }

    /// Declare the sandbox capabilities this tool requires.
    ///
    /// The runner checks these requirements against the session's
    /// [`SandboxPolicy`] before execution. Tools that need no special
    /// capabilities (e.g. in-memory operations) can use the default,
    /// which requires nothing.
    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements::default()
    }

    /// Execute the tool with the given arguments, returning a JSON result.
    async fn execute(&self, args: Value) -> Result<Value>;
}
