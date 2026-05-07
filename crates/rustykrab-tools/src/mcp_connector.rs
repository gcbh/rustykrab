//! MCP connector — wraps tools served by remote MCP servers (Streamable HTTP
//! transport) as native agent [`Tool`]s.
//!
//! Configuration (env vars only, per project convention):
//!
//! ```text
//! RUSTYKRAB_MCP_SERVERS=name1,name2,...
//! RUSTYKRAB_MCP_<NAME>_URL=https://...     # required, http(s) endpoint
//! RUSTYKRAB_MCP_<NAME>_TOKEN=...           # optional Bearer token
//! ```
//!
//! Server names are case-insensitive in env-var keys (we upper-case them
//! before lookup) and appear verbatim — lower-cased — in the resulting
//! tool name: `mcp__<server>__<remote-tool-name>`. The `mcp__` prefix
//! mirrors how Claude Code surfaces MCP tools and prevents collisions
//! with built-in tool names.
//!
//! Failures connecting to any single server are logged and skipped: a bad
//! server config never blocks startup or other servers.

use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_channels::mcp::{McpContent, McpToolDef};
use rustykrab_channels::McpHttpClient;
use rustykrab_core::error::{ToolError, ToolErrorKind};
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, SandboxRequirements, Tool};
use serde_json::{json, Value};

/// A single tool exposed by a remote MCP server.
pub struct McpRemoteTool {
    name: String,
    description: String,
    parameters: Value,
    server: String,
    remote_tool: String,
    client: Arc<McpHttpClient>,
}

impl McpRemoteTool {
    pub fn new(server: &str, def: McpToolDef, client: Arc<McpHttpClient>) -> Self {
        let prefixed = format!("mcp__{}__{}", server.to_lowercase(), def.name);
        let description = def
            .description
            .unwrap_or_else(|| format!("MCP tool `{}` from server `{server}`", def.name));
        // MCP tools advertise an `inputSchema` (JSON Schema). Pass it through
        // as the agent's parameter schema; default to a permissive empty
        // object when the server omits it.
        let parameters = if def.input_schema.is_object() {
            def.input_schema
        } else {
            json!({ "type": "object", "properties": {} })
        };
        Self {
            name: prefixed,
            description,
            parameters,
            server: server.to_string(),
            remote_tool: def.name,
            client,
        }
    }
}

#[async_trait]
impl Tool for McpRemoteTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_net: true,
            ..SandboxRequirements::default()
        }
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let result = self
            .client
            .call_tool(&self.remote_tool, args)
            .await
            .map_err(|e| {
                Error::ToolExecution(ToolError {
                    kind: ToolErrorKind::Transient,
                    message: format!(
                        "MCP server '{}' tool '{}' failed: {e}",
                        self.server, self.remote_tool
                    ),
                })
            })?;

        let content = render_content(&result.content);
        if result.is_error {
            return Err(Error::ToolExecution(ToolError::internal(content)));
        }
        Ok(json!({ "content": content }))
    }
}

fn render_content(content: &[McpContent]) -> String {
    let mut out = String::new();
    for block in content {
        if let Some(text) = &block.text {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

/// Connect to every configured MCP server and return their tools.
///
/// Returns an empty Vec if `RUSTYKRAB_MCP_SERVERS` is unset or empty.
/// Per-server failures are logged at WARN and do not propagate.
pub async fn mcp_connector_tools() -> Vec<Arc<dyn Tool>> {
    let raw = match std::env::var("RUSTYKRAB_MCP_SERVERS") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Vec::new(),
    };

    let names: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut out: Vec<Arc<dyn Tool>> = Vec::new();
    for name in names {
        match connect_server(&name).await {
            Ok(server_tools) => out.extend(server_tools),
            Err(e) => tracing::warn!(
                server = %name,
                error = %e,
                "MCP server connect failed; skipping"
            ),
        }
    }
    out
}

async fn connect_server(name: &str) -> std::result::Result<Vec<Arc<dyn Tool>>, String> {
    let upper = name.to_uppercase();
    let url_key = format!("RUSTYKRAB_MCP_{upper}_URL");
    let token_key = format!("RUSTYKRAB_MCP_{upper}_TOKEN");

    let url = std::env::var(&url_key).map_err(|_| format!("missing env var {url_key}"))?;
    let token = std::env::var(&token_key).ok();

    let client = McpHttpClient::connect(&url, token.as_deref())
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let defs = client
        .list_tools()
        .await
        .map_err(|e| format!("list_tools: {e}"))?;
    let client = Arc::new(client);

    let tool_count = defs.len();
    let tools: Vec<Arc<dyn Tool>> = defs
        .into_iter()
        .map(|d| Arc::new(McpRemoteTool::new(name, d, client.clone())) as Arc<dyn Tool>)
        .collect();

    tracing::info!(server = %name, tool_count, "MCP connector registered");
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_tool_name_is_prefixed_and_lowercased() {
        // We can't construct an McpRemoteTool here without a real client,
        // but we can at least verify the rendering helper.
        let blocks = vec![
            McpContent {
                content_type: "text".into(),
                text: Some("hello".into()),
                data: None,
                mime_type: None,
            },
            McpContent {
                content_type: "text".into(),
                text: Some("world".into()),
                data: None,
                mime_type: None,
            },
        ];
        assert_eq!(render_content(&blocks), "hello\nworld");
    }

    #[tokio::test]
    async fn connector_returns_empty_when_unset() {
        // Safety: the test uses a dedicated env var set/cleared in this
        // process. No other test reads RUSTYKRAB_MCP_SERVERS.
        std::env::remove_var("RUSTYKRAB_MCP_SERVERS");
        let tools = mcp_connector_tools().await;
        assert!(tools.is_empty());
    }
}
