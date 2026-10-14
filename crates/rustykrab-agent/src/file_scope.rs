//! File tools for exact operator grants, without an ambient shell or directory grant.
use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use rustykrab_core::{types::ToolSchema, Error, Result, SandboxRequirements, Tool};
use serde_json::Value;

pub(crate) struct ScopedFileTool {
    pub inner: Arc<dyn Tool>,
    pub workspace: Option<PathBuf>,
    pub files: Vec<PathBuf>,
    pub readable_files: Vec<PathBuf>,
}

#[async_trait]
impl Tool for ScopedFileTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn schema(&self) -> ToolSchema {
        self.inner.schema()
    }
    fn sandbox_requirements(&self) -> SandboxRequirements {
        self.inner.sandbox_requirements()
    }
    fn available(&self) -> bool {
        self.inner.available()
    }

    async fn execute(&self, mut args: Value) -> Result<Value> {
        let requested = args["path"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing path".into()))?;
        let mut allowed = self.workspace.as_ref().and_then(|root| {
            rustykrab_tools::security::validate_path_in(root, requested)
                .ok()
                .map(|p| (root.clone(), p))
        });
        if allowed.is_none() && std::path::Path::new(requested).is_absolute() {
            let inputs = if self.name() == "read" {
                self.readable_files.as_slice()
            } else {
                &[]
            };
            for file in self.files.iter().chain(inputs) {
                if std::fs::symlink_metadata(file).is_ok_and(|m| m.file_type().is_symlink()) {
                    continue;
                }
                let Some(parent) = file.parent() else {
                    continue;
                };
                let Ok(expected) =
                    rustykrab_tools::security::validate_path_in(parent, &file.to_string_lossy())
                else {
                    continue;
                };
                let Ok(actual) = rustykrab_tools::security::validate_path_in(parent, requested)
                else {
                    continue;
                };
                if actual == expected {
                    allowed = Some((parent.to_path_buf(), actual));
                    break;
                }
            }
        }
        let (root, path) = allowed.ok_or_else(|| {
            Error::ToolExecution("path is outside this run's worktree and exact file grants".into())
        })?;
        args["path"] = Value::String(path.to_string_lossy().into_owned());
        match self.name() {
            "read" => rustykrab_tools::ReadTool::in_dir(root).execute(args).await,
            "edit" => rustykrab_tools::EditTool::in_dir(root).execute(args).await,
            "write" => rustykrab_tools::WriteTool::in_dir(root).execute(args).await,
            _ => Err(Error::ToolExecution("unsupported scoped file tool".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn exact_output_grants_allow_creation_reading_and_editing_but_reject_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("audit.json");
        let tool = |inner: Arc<dyn Tool>| ScopedFileTool {
            inner,
            workspace: None,
            files: vec![output.clone()],
            readable_files: Vec::new(),
        };
        tool(Arc::new(rustykrab_tools::WriteTool::new()))
            .execute(json!({"path":output,"content":"one"}))
            .await
            .unwrap();
        let read = tool(Arc::new(rustykrab_tools::ReadTool::new()))
            .execute(json!({"path":output}))
            .await
            .unwrap();
        assert!(read.to_string().contains("one"));
        tool(Arc::new(rustykrab_tools::EditTool::new()))
            .execute(json!({"path":output,"old_string":"one","new_string":"two"}))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "two");
        for path in [
            dir.path().join("other.json"),
            dir.path().join("../outside.json"),
            PathBuf::from("audit.json"),
        ] {
            assert!(tool(Arc::new(rustykrab_tools::WriteTool::new()))
                .execute(json!({"path":path,"content":"bad"}))
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn read_only_inputs_cannot_be_modified() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("index.json");
        std::fs::write(&input, "original input").unwrap();
        let scope = |inner: Arc<dyn Tool>| ScopedFileTool {
            inner,
            workspace: None,
            files: vec![],
            readable_files: vec![input.clone()],
        };
        scope(Arc::new(rustykrab_tools::ReadTool::new()))
            .execute(json!({"path":input}))
            .await
            .unwrap();
        assert!(scope(Arc::new(rustykrab_tools::WriteTool::new()))
            .execute(json!({"path":input,"content":"overwrite"}))
            .await
            .is_err());
        assert!(scope(Arc::new(rustykrab_tools::EditTool::new()))
            .execute(json!({"path":input,"old_string":"original","new_string":"changed"}))
            .await
            .is_err());
        assert_eq!(std::fs::read_to_string(input).unwrap(), "original input");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_granted_file_cannot_redirect_to_a_sibling_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("audit.json");
        let victim = dir.path().join("secret.json");
        std::fs::write(&victim, "keep").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, &output).unwrap();
        let tool = ScopedFileTool {
            inner: Arc::new(rustykrab_tools::WriteTool::new()),
            workspace: None,
            files: vec![output.clone()],
            readable_files: Vec::new(),
        };
        assert!(tool
            .execute(json!({"path":output,"content":"bad"}))
            .await
            .is_err());
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "keep");
    }
}
