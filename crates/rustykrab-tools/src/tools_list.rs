use async_trait::async_trait;
use rustykrab_core::active_tools::with_session_context;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use serde_json::{json, Value};

/// Categorize a tool by its name. Keeps the Tool trait minimal while still
/// giving agents a useful axis to filter on.
pub(crate) fn categorize(name: &str) -> &'static str {
    match name {
        "tools_list" | "tools_load" => "meta",
        "read" | "write" | "edit" | "apply_patch" => "filesystem",
        "exec" | "process" | "code_execution" => "runtime",
        "web_fetch" | "x_search" | "http_request" | "http_session" | "browser" => "web",
        "image" | "video" | "canvas" => "media",
        "cron" | "gateway" => "automation",
        "gmail" | "notion" | "obsidian" => "integration",
        "skills" => "skills",
        "message" => "messaging",
        "nodes" => "devices",
        _ if name.starts_with("memory_") => "memory",
        _ if name.starts_with("sessions_")
            || name.starts_with("session_")
            || name.starts_with("agents_")
            || name == "subagents" =>
        {
            "session"
        }
        _ if name.starts_with("credential_") => "credentials",
        _ => "general",
    }
}

/// Meta-tool that lists every tool the current session is allowed to use,
/// with optional filtering by category.
pub struct ToolsListTool;

impl ToolsListTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ToolsListTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ToolsListTool {
    fn name(&self) -> &str {
        "tools_list"
    }

    fn description(&self) -> &str {
        "List every tool the current session is allowed to use. Returns an \
         array of {name, description, category}. Pass `category` to filter."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "category": {
                        "type": "string",
                        "description": "Optional category to filter the listing (e.g. \"filesystem\", \"web\", \"memory\")."
                    }
                },
                "required": []
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let category_filter = args
            .get("category")
            .and_then(Value::as_str)
            .map(str::to_owned);

        let listing = with_session_context(|ctx| {
            let caps = ctx.capabilities.clone();
            ctx.all_tools
                .iter()
                .filter(|t| t.available() && caps.can_use_tool(t.name()))
                .map(|t| {
                    let name = t.name().to_string();
                    let category = categorize(&name).to_string();
                    json!({
                        "name": name,
                        "description": t.description(),
                        "category": category,
                    })
                })
                .filter(|entry| match &category_filter {
                    Some(c) => entry
                        .get("category")
                        .and_then(Value::as_str)
                        .map(|v| v == c)
                        .unwrap_or(false),
                    None => true,
                })
                .collect::<Vec<_>>()
        })
        .ok_or_else(|| {
            Error::ToolExecution("tools_list invoked outside of an agent session context".into())
        })?;

        Ok(json!({ "tools": listing }))
    }
}
