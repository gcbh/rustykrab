use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

use crate::security;

/// A built-in tool that applies a unified diff patch to one or more files.
pub struct ApplyPatchTool;

impl ApplyPatchTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ApplyPatchTool {
    fn default() -> Self {
        Self::new()
    }
}

/// A parsed hunk from a unified diff.
struct Hunk {
    old_start: usize,
    old_count: usize,
    lines: Vec<DiffLine>,
}

enum DiffLine {
    Context(String),
    Add(String),
    Remove(String),
}

/// A parsed file diff containing the target path and its hunks.
struct FileDiff {
    path: String,
    hunks: Vec<Hunk>,
}

fn parse_unified_diff(patch: &str) -> std::result::Result<Vec<FileDiff>, String> {
    let mut file_diffs: Vec<FileDiff> = Vec::new();
    let lines: Vec<&str> = patch.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        // Look for --- a/path line
        if lines[i].starts_with("--- ") {
            // Next line should be +++ b/path
            if i + 1 >= lines.len() || !lines[i + 1].starts_with("+++ ") {
                i += 1;
                continue;
            }

            let plus_line = lines[i + 1];
            let path = plus_line
                .strip_prefix("+++ b/")
                .or_else(|| plus_line.strip_prefix("+++ "))
                .ok_or_else(|| format!("invalid +++ line: {plus_line}"))?
                .to_string();

            i += 2;

            let mut hunks = Vec::new();

            // Parse hunks for this file
            while i < lines.len() && !lines[i].starts_with("--- ") {
                if lines[i].starts_with("@@ ") {
                    let hunk_header = lines[i];
                    let (old_start, old_count) = parse_hunk_header(hunk_header)?;
                    i += 1;

                    let mut hunk_lines = Vec::new();
                    while i < lines.len()
                        && !lines[i].starts_with("@@ ")
                        && !lines[i].starts_with("--- ")
                    {
                        let line = lines[i];
                        if let Some(rest) = line.strip_prefix('+') {
                            hunk_lines.push(DiffLine::Add(rest.to_string()));
                        } else if let Some(rest) = line.strip_prefix('-') {
                            hunk_lines.push(DiffLine::Remove(rest.to_string()));
                        } else if let Some(rest) = line.strip_prefix(' ') {
                            hunk_lines.push(DiffLine::Context(rest.to_string()));
                        } else if line == "\\ No newline at end of file" {
                            // skip
                        } else {
                            // Treat as context line (some diffs omit the leading space)
                            hunk_lines.push(DiffLine::Context(line.to_string()));
                        }
                        i += 1;
                    }

                    hunks.push(Hunk {
                        old_start,
                        old_count,
                        lines: hunk_lines,
                    });
                } else {
                    i += 1;
                }
            }

            if !hunks.is_empty() {
                file_diffs.push(FileDiff { path, hunks });
            }
        } else {
            i += 1;
        }
    }

    if file_diffs.is_empty() {
        return Err("no file diffs found in patch".into());
    }

    Ok(file_diffs)
}

fn parse_hunk_header(header: &str) -> std::result::Result<(usize, usize), String> {
    // Format: @@ -old_start,old_count +new_start,new_count @@
    let header = header.trim_start_matches("@@ ");
    let parts: Vec<&str> = header.splitn(3, ' ').collect();
    if parts.is_empty() {
        return Err(format!("invalid hunk header: {header}"));
    }

    let old_range = parts[0]
        .strip_prefix('-')
        .ok_or_else(|| format!("invalid old range in hunk header: {header}"))?;

    let (old_start, old_count) = if let Some((start, count)) = old_range.split_once(',') {
        (
            start.parse::<usize>().map_err(|e| e.to_string())?,
            count.parse::<usize>().map_err(|e| e.to_string())?,
        )
    } else {
        (old_range.parse::<usize>().map_err(|e| e.to_string())?, 1)
    };

    Ok((old_start, old_count))
}

fn apply_hunks(original: &str, hunks: &[Hunk]) -> std::result::Result<String, String> {
    let old_lines: Vec<&str> = original.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut old_idx: usize = 0; // 0-based index into old_lines

    for hunk in hunks {
        let hunk_start = if hunk.old_start == 0 { 0 } else { hunk.old_start - 1 };

        // Copy lines before this hunk
        while old_idx < hunk_start && old_idx < old_lines.len() {
            result.push(old_lines[old_idx].to_string());
            old_idx += 1;
        }

        // Apply hunk lines
        let mut consumed = 0;
        for diff_line in &hunk.lines {
            match diff_line {
                DiffLine::Context(_text) => {
                    if old_idx < old_lines.len() {
                        result.push(old_lines[old_idx].to_string());
                        old_idx += 1;
                    }
                    consumed += 1;
                }
                DiffLine::Remove(_text) => {
                    old_idx += 1;
                    consumed += 1;
                }
                DiffLine::Add(text) => {
                    result.push(text.clone());
                }
            }
        }

        // If we consumed fewer old lines than expected, advance
        let expected_consumed = hunk.old_count;
        while consumed < expected_consumed && old_idx < old_lines.len() {
            old_idx += 1;
            consumed += 1;
        }
    }

    // Copy remaining lines
    while old_idx < old_lines.len() {
        result.push(old_lines[old_idx].to_string());
        old_idx += 1;
    }

    let mut output = result.join("\n");
    // Preserve trailing newline if original had one
    if original.ends_with('\n') {
        output.push('\n');
    }

    Ok(output)
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Apply a unified diff patch to one or more files."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "The unified diff patch content to apply"
                    }
                },
                "required": ["patch"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let patch = args["patch"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing patch".into()))?;

        let file_diffs = parse_unified_diff(patch)
            .map_err(|e| rustykrab_core::Error::ToolExecution(format!("failed to parse patch: {e}").into()))?;

        let mut files_modified = 0;

        for file_diff in &file_diffs {
            let path = &file_diff.path;

            // Validate each file path from the patch for traversal attacks
            let safe_path = security::validate_path(path)
                .map_err(|e| rustykrab_core::Error::ToolExecution(
                    format!("patch path rejected for '{path}': {e}").into(),
                ))?;

            let original = tokio::fs::read_to_string(&safe_path)
                .await
                .map_err(|e| rustykrab_core::Error::ToolExecution(
                    format!("failed to read {path}: {e}").into(),
                ))?;

            let patched = apply_hunks(&original, &file_diff.hunks)
                .map_err(|e| rustykrab_core::Error::ToolExecution(
                    format!("failed to apply hunks to {path}: {e}").into(),
                ))?;

            tokio::fs::write(&safe_path, &patched)
                .await
                .map_err(|e| rustykrab_core::Error::ToolExecution(
                    format!("failed to write {path}: {e}").into(),
                ))?;

            files_modified += 1;
        }

        Ok(json!({
            "applied": true,
            "files_modified": files_modified,
        }))
    }
}
