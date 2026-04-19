use async_trait::async_trait;
use rustykrab_core::active_tools::with_session_context;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use serde_json::{json, Value};

/// Meta-tool that marks a set of tools as "active" on the current session.
///
/// Subsequent model API calls include only the schemas for the meta-tools
/// plus the currently-active set, keeping the per-request payload small.
pub struct ToolsLoadTool;

impl ToolsLoadTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ToolsLoadTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ToolsLoadTool {
    fn name(&self) -> &str {
        "tools_load"
    }

    fn description(&self) -> &str {
        "Load a set of tools into the current session's active set so \
         subsequent API calls include their full schemas. Accepts an array \
         of tool names."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "names": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Tool names to activate for this session."
                    }
                },
                "required": ["names"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let names: Vec<String> = args
            .get("names")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::ToolExecution("missing `names` array".into()))?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();

        if names.is_empty() {
            return Err(Error::ToolExecution(
                "`names` must contain at least one tool name".into(),
            ));
        }

        let result = with_session_context(|ctx| {
            let caps = ctx.capabilities.clone();

            let mut loaded = Vec::new();
            let mut unknown = Vec::new();
            let mut forbidden = Vec::new();

            for requested in &names {
                let matched = ctx
                    .all_tools
                    .iter()
                    .find(|t| t.name() == requested.as_str() && t.available());

                match matched {
                    None => unknown.push(requested.clone()),
                    Some(tool) => {
                        if caps.can_use_tool(tool.name()) {
                            loaded.push(tool.name().to_string());
                        } else {
                            forbidden.push(requested.clone());
                        }
                    }
                }
            }

            if !loaded.is_empty() {
                ctx.active_tools
                    .activate(ctx.conversation_id, loaded.iter().cloned());
            }

            let active_now: Vec<String> = {
                let mut v: Vec<String> = ctx
                    .active_tools
                    .active_for(ctx.conversation_id)
                    .into_iter()
                    .collect();
                v.sort();
                v
            };

            json!({
                "loaded": loaded,
                "unknown": unknown,
                "forbidden": forbidden,
                "active": active_now,
            })
        })
        .ok_or_else(|| {
            Error::ToolExecution("tools_load invoked outside of an agent session context".into())
        })?;

        Ok(result)
    }
}
