use async_trait::async_trait;
use rustykrab_core::error::ToolError;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, SandboxRequirements, Tool};
use serde_json::{json, Value};

use crate::security;

/// Maximum file size to read (10 MB).
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// A built-in tool that reads the contents of a file.
///
/// Security: Validates paths to prevent traversal attacks and blocks
/// access to sensitive system directories.
pub struct ReadTool {
    /// The directory this tool is bound to instead of the daemon's
    /// workspace: a run's own checkout. Paths are taken from it and may
    /// not leave it.
    root: Option<std::path::PathBuf>,
}

impl ReadTool {
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

impl Default for ReadTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file at a given path."
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_fs_read: true,
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
                        "description": "The absolute path to the file to read"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start reading from (0-based)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing path".into()))?;

        // Validate path for traversal and blocked directories
        let safe_path = self.validate(path).map_err(|e| {
            rustykrab_core::Error::ToolExecution(format!("path rejected: {e}").into())
        })?;

        // Check file size before reading
        let metadata = tokio::fs::metadata(&safe_path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                rustykrab_core::Error::ToolExecution(ToolError::not_found(format!(
                    "file not found: {path}"
                )))
            } else {
                rustykrab_core::Error::ToolExecution(format!("failed to stat {path}: {e}").into())
            }
        })?;

        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().map(|v| v as usize);

        if offset > 0 || limit.is_some() {
            // Stream line by line: skip `offset` lines, take up to `limit`,
            // and stop reading as soon as the window is filled instead of
            // loading the whole file. This also lets a windowed read work on
            // files larger than MAX_FILE_SIZE.
            use tokio::io::AsyncBufReadExt;

            let file = tokio::fs::File::open(&safe_path).await.map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    rustykrab_core::Error::ToolExecution(ToolError::not_found(format!(
                        "file not found: {path}"
                    )))
                } else {
                    rustykrab_core::Error::ToolExecution(
                        format!("failed to read {path}: {e}").into(),
                    )
                }
            })?;
            let mut lines = tokio::io::BufReader::new(file).lines();

            let mut selected: Vec<String> = Vec::new();
            let mut line_no: usize = 0;
            while limit.is_none_or(|l| selected.len() < l) {
                let Some(line) = lines.next_line().await.map_err(|e| {
                    rustykrab_core::Error::ToolExecution(
                        format!("failed to read {path}: {e}").into(),
                    )
                })?
                else {
                    break;
                };
                if line_no >= offset {
                    selected.push(line);
                }
                line_no += 1;
            }

            return Ok(json!({
                "content": selected.join("\n"),
                "lines": selected.len(),
            }));
        }

        if metadata.len() > MAX_FILE_SIZE {
            return Err(rustykrab_core::Error::ToolExecution(
                format!(
                "file is too large ({} bytes, max {} bytes). Use offset/limit to read a portion.",
                metadata.len(),
                MAX_FILE_SIZE
            )
                .into(),
            ));
        }

        let content = tokio::fs::read_to_string(&safe_path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                rustykrab_core::Error::ToolExecution(ToolError::not_found(format!(
                    "file not found: {path}"
                )))
            } else {
                rustykrab_core::Error::ToolExecution(format!("failed to read {path}: {e}").into())
            }
        })?;

        let lines: Vec<&str> = content.lines().collect();
        let result_lines = lines.len();
        let result_content = lines.join("\n");

        Ok(json!({
            "content": result_content,
            "lines": result_lines,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a fixture file inside the current directory (the workspace
    /// boundary enforced by `security::validate_path`).
    fn write_fixture(name: &str, content: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::Builder::new()
            .prefix("read-tool-test-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        (dir, path.to_string_lossy().into_owned())
    }

    fn numbered_lines(count: usize) -> String {
        (0..count).map(|i| format!("line {i}\n")).collect()
    }

    /// A run's own checkout, outside the daemon's workspace, as a worker's
    /// worktree is.
    fn checkout() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("bound-tool-test-")
            .tempdir()
            .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        dir
    }

    /// Bound to a directory, the file tools work there by relative path,
    /// where the unbound ones refuse it as outside the workspace.
    #[tokio::test]
    async fn bound_to_a_directory_the_file_tools_work_inside_it() {
        let dir = checkout();
        let inside = dir.path().join("src/lib.rs").to_string_lossy().into_owned();
        let unbound = ReadTool::new().execute(json!({ "path": inside })).await;
        assert!(
            unbound
                .unwrap_err()
                .to_string()
                .contains("outside the workspace boundary"),
            "the daemon's own read tool does not reach a worktree"
        );

        let read = ReadTool::in_dir(dir.path());
        let got = read.execute(json!({ "path": "src/lib.rs" })).await.unwrap();
        assert_eq!(got["content"], "pub fn a() {}");
        let got = read.execute(json!({ "path": inside })).await.unwrap();
        assert_eq!(got["lines"], 1, "an absolute path inside it reads too");

        crate::WriteTool::in_dir(dir.path())
            .execute(json!({ "path": "src/new.rs", "content": "pub fn b() {}\n" }))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/new.rs")).unwrap(),
            "pub fn b() {}\n"
        );
        crate::EditTool::in_dir(dir.path())
            .execute(json!({
                "path": "src/lib.rs",
                "old_string": "fn a()",
                "new_string": "fn renamed()"
            }))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
            "pub fn renamed() {}\n"
        );
    }

    /// The bound is a boundary: nothing outside the directory, by an
    /// absolute path, `..`, or a symlink that leaves it.
    #[tokio::test]
    async fn bound_to_a_directory_the_file_tools_do_not_leave_it() {
        let dir = checkout();
        let other = checkout();
        let outside = other.path().join("src/lib.rs");
        let read = ReadTool::in_dir(dir.path());
        for path in [outside.to_string_lossy().into_owned(), "../x".to_string()] {
            let err = read.execute(json!({ "path": path })).await.unwrap_err();
            assert!(err.to_string().contains("path rejected"), "{path}: {err}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, dir.path().join("escape.rs")).unwrap();
            let err = read
                .execute(json!({ "path": "escape.rs" }))
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("outside the workspace boundary"),
                "{err}"
            );
        }
        let err = crate::WriteTool::in_dir(dir.path())
            .execute(json!({
                "path": other.path().join("planted.rs").to_string_lossy(),
                "content": "x"
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("path rejected"), "{err}");
        assert!(!other.path().join("planted.rs").exists());
    }

    #[tokio::test]
    async fn reads_whole_file_without_offset() {
        let (_dir, path) = write_fixture("whole.txt", &numbered_lines(5));
        let result = ReadTool::new()
            .execute(json!({ "path": path }))
            .await
            .unwrap();
        assert_eq!(result["lines"], 5);
        assert_eq!(result["content"], "line 0\nline 1\nline 2\nline 3\nline 4");
    }

    #[tokio::test]
    async fn offset_and_limit_stream_a_window() {
        let (_dir, path) = write_fixture("window.txt", &numbered_lines(10));
        let result = ReadTool::new()
            .execute(json!({ "path": path, "offset": 3, "limit": 2 }))
            .await
            .unwrap();
        assert_eq!(result["lines"], 2);
        assert_eq!(result["content"], "line 3\nline 4");
    }

    #[tokio::test]
    async fn limit_alone_takes_from_start() {
        let (_dir, path) = write_fixture("limit.txt", &numbered_lines(10));
        let result = ReadTool::new()
            .execute(json!({ "path": path, "limit": 3 }))
            .await
            .unwrap();
        assert_eq!(result["lines"], 3);
        assert_eq!(result["content"], "line 0\nline 1\nline 2");
    }

    #[tokio::test]
    async fn offset_alone_reads_to_end() {
        let (_dir, path) = write_fixture("offset.txt", &numbered_lines(5));
        let result = ReadTool::new()
            .execute(json!({ "path": path, "offset": 3 }))
            .await
            .unwrap();
        assert_eq!(result["lines"], 2);
        assert_eq!(result["content"], "line 3\nline 4");
    }

    #[tokio::test]
    async fn offset_past_end_returns_empty() {
        let (_dir, path) = write_fixture("past-end.txt", &numbered_lines(3));
        let result = ReadTool::new()
            .execute(json!({ "path": path, "offset": 100 }))
            .await
            .unwrap();
        assert_eq!(result["lines"], 0);
        assert_eq!(result["content"], "");
    }

    #[tokio::test]
    async fn large_file_readable_with_window_but_capped_without() {
        // A file above MAX_FILE_SIZE must reject a whole-file read but still
        // serve a windowed read via streaming.
        let line = format!("{}\n", "x".repeat(1023));
        let count = (MAX_FILE_SIZE as usize / 1024) + 16;
        let content: String = std::iter::repeat_n(line.as_str(), count).collect();
        let (_dir, path) = write_fixture("large.txt", &content);

        let whole = ReadTool::new().execute(json!({ "path": path })).await;
        let err = whole.expect_err("whole-file read of oversized file must fail");
        assert!(err.to_string().contains("too large"), "got: {err}");

        let window = ReadTool::new()
            .execute(json!({ "path": path, "offset": 1, "limit": 2 }))
            .await
            .unwrap();
        assert_eq!(window["lines"], 2);
        assert_eq!(window["content"], format!("{0}\n{0}", "x".repeat(1023)));
    }
}
