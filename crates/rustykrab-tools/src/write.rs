use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, SandboxRequirements, Tool};
use serde_json::{json, Value};

use crate::security;

/// A built-in tool that writes content to a file.
///
/// Security: Validates paths to prevent traversal attacks and blocks
/// writes to sensitive system directories.
pub struct WriteTool {
    /// The directory this tool is bound to instead of the daemon's
    /// workspace: a run's own checkout. Paths are taken from it and may
    /// not leave it.
    root: Option<std::path::PathBuf>,
}

impl WriteTool {
    pub fn new() -> Self {
        Self { root: None }
    }

    /// The tool bound to `dir`: a relative path is taken from it, and no
    /// path outside it is accepted. A worker run in a worktree gets its
    /// file tools this way, as it gets `exec`.
    pub fn in_dir(dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            root: Some(dir.into()),
        }
    }

    fn validate(&self, path: &str) -> std::result::Result<std::path::PathBuf, String> {
        match &self.root {
            Some(root) => security::validate_path_in(root, path),
            None => security::validate_path(path),
        }
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file at a given path, creating directories as needed."
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_fs_write: true,
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
                    "path": {
                        "type": "string",
                        "description": "The absolute path to the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing path".into()))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing content".into()))?;

        // Validate path for traversal and blocked directories
        let safe_path = self.validate(path).map_err(|e| {
            rustykrab_core::Error::ToolExecution(format!("path rejected: {e}").into())
        })?;

        let file_path = std::path::Path::new(&safe_path);
        if let Some(parent) = file_path.parent() {
            // Re-validate the parent directory
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    rustykrab_core::Error::ToolExecution(
                        format!("failed to create directories for {path}: {e}").into(),
                    )
                })?;
            }
        }

        let bytes = content.len();
        tokio::fs::write(&safe_path, content).await.map_err(|e| {
            rustykrab_core::Error::ToolExecution(format!("failed to write {path}: {e}").into())
        })?;

        Ok(json!({
            "written": true,
            "bytes": bytes,
        }))
    }
}
