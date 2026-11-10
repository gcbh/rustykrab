//! A lazily allocated shell workspace owned by one conversational turn.

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, SandboxRequirements, Tool, ToolError};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use tokio::sync::OnceCell;

use crate::exec::{rustup_home, seatbelt_available, ExecTool};

/// A turn's shell commands share one private directory. Dropping the turn
/// releases that directory. A caller cannot select or reuse another binding.
pub struct ScratchExecTool {
    data_dir: PathBuf,
    scratch: OnceCell<TempDir>,
}

impl ScratchExecTool {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            scratch: OnceCell::new(),
        }
    }

    async fn allocate(&self) -> std::io::Result<TempDir> {
        let parent = self.data_dir.join("turn-scratch");
        tokio::fs::create_dir_all(&parent).await?;
        let parent = tokio::fs::canonicalize(parent).await?;
        let scratch = tempfile::Builder::new()
            .prefix("turn-")
            .tempdir_in(parent)?;
        for name in ["work", "home", "tmp"] {
            tokio::fs::create_dir(scratch.path().join(name)).await?;
        }
        Ok(scratch)
    }
}

#[async_trait]
impl Tool for ScratchExecTool {
    fn name(&self) -> &str {
        "scratch_exec"
    }

    fn description(&self) -> &str {
        "Write, build, and run code using installed toolchains in this turn's clean scratch          directory. Calls share files within the turn. Use shell commands to create files          there and return results via stdout. The sandbox confines writes to scratch and          disables network access. Scratch is discarded when the turn ends."
    }

    fn available(&self) -> bool {
        seatbelt_available()
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_fs_read: true,
            needs_fs_write: true,
            needs_spawn: true,
            ..SandboxRequirements::default()
        }
    }

    fn schema(&self) -> ToolSchema {
        let mut schema = ExecTool::new().schema();
        schema.name = self.name().into();
        schema.description = self.description().into();
        schema.parameters["properties"]["command"]["description"] =
            Value::String("Shell command in this turn's isolated scratch directory".into());
        schema
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        if !self.available() {
            return Err(Error::ToolExecution(ToolError::permission_denied(
                "isolated scratch execution requires macOS Seatbelt",
            )));
        }
        let scratch = self
            .scratch
            .get_or_try_init(|| self.allocate())
            .await
            .map_err(|error| Error::ToolExecution(ToolError::transient(error.to_string())))?;
        ExecTool::in_scratch(scratch.path(), &self.data_dir)
            .execute(args)
            .await
    }
}

/// Escape a literal path in the Seatbelt profile.
fn path_rule(kind: &str, path: &Path) -> String {
    let path = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("({kind} \"{path}\")")
}

pub(crate) fn profile(scratch: &Path, private: &[PathBuf]) -> String {
    let home = std::env::var_os("HOME").and_then(|p| std::fs::canonicalize(p).ok());
    let scratch = std::fs::canonicalize(scratch).unwrap_or_else(|_| scratch.to_path_buf());
    let private: Vec<_> = private
        .iter()
        .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
        .collect();
    let blocked = [
        Path::new("/Users"),
        Path::new("/Volumes"),
        Path::new("/private/tmp"),
        Path::new("/private/var/folders"),
    ]
    .into_iter()
    .chain(home.as_deref())
    .chain(private.iter().map(PathBuf::as_path))
    .map(|path| path_rule("subpath", path))
    .collect::<Vec<_>>()
    .join(" ");

    // Read only the installed toolchain directories that need an exception to
    // the private-home rule. Never open a credential or service-data ancestor.
    let path = std::env::var_os("PATH").unwrap_or_default();
    let toolchains = std::env::split_paths(&path)
        .filter(|path| path.is_absolute())
        .chain(rustup_home())
        .chain(home.iter().map(|home| home.join(".cargo/bin")))
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .filter(|path| {
            home.as_ref()
                .is_some_and(|home| path != home && path.starts_with(home))
                && private
                    .iter()
                    .all(|private| !private.starts_with(path) && !path.starts_with(private))
        });
    let readable: BTreeSet<_> = std::iter::once(scratch.clone()).chain(toolchains).collect();
    let ancestors: BTreeSet<_> = readable
        .iter()
        .flat_map(|path| path.ancestors().skip(1))
        .map(|path| path_rule("literal", path))
        .collect();
    let reads = readable
        .iter()
        .map(|path| path_rule("subpath", path))
        .collect::<Vec<_>>()
        .join(" ");
    let ancestors = ancestors.into_iter().collect::<Vec<_>>().join(" ");
    let write = path_rule("subpath", &scratch);
    format!(
        "(version 1)(allow default)\n\
         (deny file-write*)(allow file-write* {write} (literal \"/dev/null\"))\n\
         (deny file-read* {blocked})(allow file-read* {reads})\n\
         (allow file-read-metadata {ancestors})\n\
         (deny network*)(deny mach-lookup)\n\
         (deny signal)(allow signal (target same-sandbox))"
    )
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use serde_json::json;

    async fn run(tool: &ScratchExecTool, command: &str) -> Value {
        tool.execute(json!({"command":command})).await.unwrap()
    }

    fn quoted(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
    }

    #[tokio::test]
    async fn concurrent_turns_keep_their_own_files_and_discard_them() {
        let data = tempfile::tempdir().unwrap();
        let private = data.path().join("outside.txt");
        std::fs::write(&private, "private").unwrap();
        let a = ScratchExecTool::new(data.path().into());
        let b = ScratchExecTool::new(data.path().into());
        assert!(!data.path().join("turn-scratch").exists());

        let (left, right) = tokio::join!(
            run(
                &a,
                "test ! -e program.awk && printf 'BEGIN {print 41}' > program.awk"
            ),
            run(
                &b,
                "test ! -e program.awk && printf 'BEGIN {print 42}' > program.awk"
            )
        );
        assert_eq!(left["exit_code"], 0, "{left}");
        assert_eq!(right["exit_code"], 0, "{right}");
        let a_dir = a.scratch.get().unwrap().path().to_path_buf();
        let b_dir = b.scratch.get().unwrap().path().to_path_buf();
        assert_ne!(a_dir, b_dir);
        assert_eq!(run(&a, "awk -f program.awk").await["stdout"], "41\n");
        assert_eq!(run(&b, "awk -f program.awk").await["stdout"], "42\n");

        for path in [private, b_dir.join("work/program.awk")] {
            let path = quoted(&path);
            assert_ne!(run(&a, &format!("cat {path}")).await["exit_code"], 0);
            assert_ne!(
                run(&a, &format!("printf unsafe > {path}")).await["exit_code"],
                0
            );
        }
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let result = run(&a, &format!("/usr/bin/nc -z -w 1 127.0.0.1 {port}")).await;
        assert_ne!(result["exit_code"], 0, "{result}");

        drop(a);
        drop(b);
        assert!(!a_dir.exists());
        assert!(!b_dir.exists());
    }
}
