use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};

/// A built-in tool that replaces an exact string match in a file.
pub struct EditTool;

impl EditTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EditTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace an exact string match in a file with new content."
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
                        "description": "The absolute path to the file to edit"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "The exact string to find and replace"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "The replacement string"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Whether to replace all occurrences (default: false)"
                    }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing path".into()))?;
        let old_string = args["old_string"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing old_string".into()))?;
        let new_string = args["new_string"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing new_string".into()))?;
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);

        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| openclaw_core::Error::ToolExecution(
                format!("failed to read {path}: {e}"),
            ))?;

        let match_count = content.matches(old_string).count();

        if match_count == 0 {
            return Err(openclaw_core::Error::ToolExecution(
                format!("old_string not found in {path}"),
            ));
        }

        if !replace_all && match_count > 1 {
            return Err(openclaw_core::Error::ToolExecution(
                format!(
                    "old_string is not unique in {path} ({match_count} occurrences found). \
                     Use replace_all: true to replace all, or provide a more specific string."
                ),
            ));
        }

        let (new_content, replacements) = if replace_all {
            let result = content.replace(old_string, new_string);
            (result, match_count)
        } else {
            let result = content.replacen(old_string, new_string, 1);
            (result, 1)
        };

        tokio::fs::write(path, &new_content)
            .await
            .map_err(|e| openclaw_core::Error::ToolExecution(
                format!("failed to write {path}: {e}"),
            ))?;

        Ok(json!({
            "edited": true,
            "replacements": replacements,
        }))
    }
}
