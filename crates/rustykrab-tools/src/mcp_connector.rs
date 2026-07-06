//! MCP connector — wraps tools served by remote MCP servers as native agent
//! [`Tool`]s. Supports both Streamable HTTP and stdio transports.
//!
//! Configuration (env vars only, per project convention):
//!
//! ```text
//! RUSTYKRAB_MCP_SERVERS=name1,name2,...
//!
//! # Per-server, common:
//! RUSTYKRAB_MCP_<NAME>_TRANSPORT=http|stdio          # default: http
//!
//! # HTTP transport:
//! RUSTYKRAB_MCP_<NAME>_URL=https://...               # required for http
//! RUSTYKRAB_MCP_<NAME>_TOKEN=...                     # shorthand for `Authorization: Bearer <token>`
//! RUSTYKRAB_MCP_<NAME>_HEADER_<KEY>=<value>          # arbitrary header, repeatable
//!
//! # stdio transport:
//! RUSTYKRAB_MCP_<NAME>_COMMAND=npx                   # required for stdio
//! RUSTYKRAB_MCP_<NAME>_ARGS=-y,@scope/pkg,mcp        # comma-separated, optional
//! RUSTYKRAB_MCP_<NAME>_ENV_<KEY>=<value>             # child env var, repeatable
//! ```
//!
//! `HEADER_<KEY>` translates `_` to `-` and lowercases the key — so
//! `RUSTYKRAB_MCP_DATADOG_HEADER_DD_API_KEY` becomes the `dd-api-key`
//! header (HTTP headers are case-insensitive). `ENV_<KEY>` is passed
//! through verbatim as the child process's env var, e.g.
//! `RUSTYKRAB_MCP_DATADOG_ENV_DD_API_KEY` sets `DD_API_KEY` for the child.
//!
//! Server names are case-insensitive in env-var keys (we upper-case them
//! before lookup) and appear verbatim — lower-cased — in the resulting
//! tool name: `mcp__<server>__<remote-tool-name>`. The `mcp__` prefix
//! mirrors how Claude Code surfaces MCP tools and prevents collisions
//! with built-in tool names.
//!
//! ## Credential refs
//!
//! Any value (`_TOKEN`, `_HEADER_<K>`, `_ENV_<K>`) may be either a literal
//! or a reference resolved at connect time:
//!
//! ```text
//! ref:store:mcp.<server>.<field>           # encrypted local SecretStore
//! ref:keychain:<service>/<account>         # macOS Keychain (no-op elsewhere)
//! ```
//!
//! Store refs are namespaced — server `github` can only resolve store keys
//! beginning with `mcp.github.`, preventing cross-server credential leakage
//! via a misconfigured env var. Keychain refs are not namespaced; the
//! operator's choice of env var IS the gate.
//!
//! Failures connecting to any single server are logged and skipped: a bad
//! server config never blocks startup or other servers.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION};
use rustykrab_channels::mcp::{McpContent, McpToolDef, McpToolResult};
use rustykrab_channels::{McpClient, McpHttpClient};
use rustykrab_core::error::{ToolError, ToolErrorKind};
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, SandboxRequirements, Tool};
use rustykrab_store::SecretStore;
use serde_json::{json, Value};

/// Transport-agnostic handle to an MCP server. Both variants expose the
/// same `call_tool` / `list_tools` surface; we branch here so the tool
/// wrapper doesn't have to.
enum McpTransport {
    Http(Arc<McpHttpClient>),
    Stdio(Arc<McpClient>),
}

impl McpTransport {
    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> std::result::Result<McpToolResult, String> {
        match self {
            McpTransport::Http(c) => c.call_tool(name, arguments).await,
            McpTransport::Stdio(c) => c.call_tool(name, arguments).await,
        }
    }
}

/// A single tool exposed by a remote MCP server.
pub struct McpRemoteTool {
    name: String,
    description: String,
    parameters: Value,
    server: String,
    remote_tool: String,
    transport: Arc<McpTransport>,
}

impl McpRemoteTool {
    fn new(server: &str, def: McpToolDef, transport: Arc<McpTransport>) -> Self {
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
            transport,
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
            .transport
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
///
/// `secrets` is used to resolve `ref:store:...` values; pass the daemon's
/// shared [`SecretStore`].
pub async fn mcp_connector_tools(secrets: &SecretStore) -> Vec<Arc<dyn Tool>> {
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
        match connect_server(&name, secrets).await {
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

async fn connect_server(
    name: &str,
    secrets: &SecretStore,
) -> std::result::Result<Vec<Arc<dyn Tool>>, String> {
    let upper = name.to_uppercase();
    let transport_key = format!("RUSTYKRAB_MCP_{upper}_TRANSPORT");
    let transport_raw = std::env::var(&transport_key)
        .unwrap_or_else(|_| "http".to_string())
        .trim()
        .to_lowercase();

    let (transport, defs) = match transport_raw.as_str() {
        "http" | "" => connect_http(name, &upper, secrets).await?,
        "stdio" => connect_stdio(name, &upper, secrets).await?,
        other => {
            return Err(format!(
                "unknown transport `{other}` for MCP server `{name}`"
            ))
        }
    };

    let transport = Arc::new(transport);
    let tool_count = defs.len();
    let tools: Vec<Arc<dyn Tool>> = defs
        .into_iter()
        .map(|d| Arc::new(McpRemoteTool::new(name, d, transport.clone())) as Arc<dyn Tool>)
        .collect();

    tracing::info!(server = %name, transport = %transport_raw, tool_count, "MCP connector registered");
    Ok(tools)
}

async fn connect_http(
    server: &str,
    upper: &str,
    secrets: &SecretStore,
) -> std::result::Result<(McpTransport, Vec<McpToolDef>), String> {
    let url_key = format!("RUSTYKRAB_MCP_{upper}_URL");
    let url = std::env::var(&url_key).map_err(|_| format!("missing env var {url_key}"))?;

    let headers = build_http_headers(server, upper, secrets).await?;
    let client = McpHttpClient::connect(&url, headers)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let defs = client
        .list_tools()
        .await
        .map_err(|e| format!("list_tools: {e}"))?;
    Ok((McpTransport::Http(Arc::new(client)), defs))
}

async fn connect_stdio(
    server: &str,
    upper: &str,
    secrets: &SecretStore,
) -> std::result::Result<(McpTransport, Vec<McpToolDef>), String> {
    let command_key = format!("RUSTYKRAB_MCP_{upper}_COMMAND");
    let command =
        std::env::var(&command_key).map_err(|_| format!("missing env var {command_key}"))?;

    let args_key = format!("RUSTYKRAB_MCP_{upper}_ARGS");
    let args_raw = std::env::var(&args_key).unwrap_or_default();
    let args: Vec<String> = args_raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    // Resolve ENV_ entries — values may be `ref:` references.
    let mut resolved_env: Vec<(String, String)> = Vec::new();
    for (key, raw_value) in collect_prefixed(upper, "ENV_") {
        let value = resolve_value_ref(server, &raw_value, secrets)
            .await
            .map_err(|e| format!("ENV_{key}: {e}"))?;
        resolved_env.push((key, value));
    }
    let env_refs: Vec<(&str, &str)> = resolved_env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let client = McpClient::spawn(&command, &arg_refs, &env_refs)
        .await
        .map_err(|e| format!("spawn: {e}"))?;
    let defs = client
        .list_tools()
        .await
        .map_err(|e| format!("list_tools: {e}"))?;
    Ok((McpTransport::Stdio(Arc::new(client)), defs))
}

/// Assemble HTTP headers from `RUSTYKRAB_MCP_<NAME>_TOKEN` (bearer shorthand)
/// plus every `RUSTYKRAB_MCP_<NAME>_HEADER_<KEY>` entry. Values may be
/// `ref:` references resolved via `secrets`.
async fn build_http_headers(
    server: &str,
    upper: &str,
    secrets: &SecretStore,
) -> std::result::Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();

    if let Ok(raw) = std::env::var(format!("RUSTYKRAB_MCP_{upper}_TOKEN")) {
        let token = resolve_value_ref(server, &raw, secrets)
            .await
            .map_err(|e| format!("TOKEN: {e}"))?;
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| format!("invalid bearer token: {e}"))?;
        headers.insert(AUTHORIZATION, value);
    }

    for (key, raw_value) in collect_prefixed(upper, "HEADER_") {
        let header_name = key.replace('_', "-").to_lowercase();
        let name = HeaderName::from_bytes(header_name.as_bytes())
            .map_err(|e| format!("invalid header name `{header_name}`: {e}"))?;
        let resolved = resolve_value_ref(server, &raw_value, secrets)
            .await
            .map_err(|e| format!("HEADER_{key}: {e}"))?;
        let value = HeaderValue::from_str(&resolved)
            .map_err(|e| format!("invalid header value for `{header_name}`: {e}"))?;
        headers.insert(name, value);
    }

    Ok(headers)
}

/// Collect every env var matching `RUSTYKRAB_MCP_<UPPER>_<PREFIX><KEY>` and
/// return `(<KEY>, value)` pairs. `<KEY>` preserves the original casing of
/// the env var (env vars are conventionally upper-case, but we don't enforce).
fn collect_prefixed(upper: &str, prefix: &str) -> Vec<(String, String)> {
    let full_prefix = format!("RUSTYKRAB_MCP_{upper}_{prefix}");
    let mut out = Vec::new();
    for (k, v) in std::env::vars() {
        if let Some(rest) = k.strip_prefix(&full_prefix) {
            if !rest.is_empty() {
                out.push((rest.to_string(), v));
            }
        }
    }
    out
}

/// Resolve a raw env-var value, expanding `ref:` references if present.
///
/// Recognised forms:
/// - Literal value (no `ref:` prefix) → returned as-is.
/// - `ref:store:<name>` → looked up in the encrypted SecretStore. `<name>`
///   must begin with `mcp.<server-lowercase>.` to prevent one server's
///   config from pulling another server's credentials.
/// - `ref:keychain:<service>/<account>` → looked up via the macOS
///   Keychain. Not namespaced — the operator's choice of env var is the
///   trust boundary.
///
/// Errors carry the ref kind but never the looked-up value. Audit logs
/// emitted by this function never log the resolved value either.
async fn resolve_value_ref(
    server: &str,
    raw: &str,
    secrets: &SecretStore,
) -> std::result::Result<String, String> {
    let rest = match raw.strip_prefix("ref:") {
        Some(r) => r,
        None => return Ok(raw.to_string()),
    };

    let (kind, target) = rest
        .split_once(':')
        .ok_or_else(|| format!("malformed credential ref `{raw}`: expected `ref:<kind>:...`"))?;

    match kind {
        "store" => {
            let name = target.trim();
            let expected_prefix = format!("mcp.{}.", server.to_lowercase());
            if !name.starts_with(&expected_prefix) {
                return Err(format!(
                    "store ref `{name}` is outside server `{server}`'s namespace \
                     (must begin with `{expected_prefix}`)"
                ));
            }
            let value = secrets.get(name).await.map_err(|e| match e {
                rustykrab_core::Error::NotFound(_) => {
                    format!("store ref `{name}` not found in SecretStore")
                }
                other => format!("store ref `{name}`: {other}"),
            })?;
            tracing::info!(
                server = %server,
                ref_kind = "store",
                ref_name = %name,
                "resolved credential ref"
            );
            Ok(value)
        }
        "keychain" => {
            let (service, account) = target.split_once('/').ok_or_else(|| {
                format!(
                    "malformed keychain ref `{raw}`: expected \
                     `ref:keychain:<service>/<account>`"
                )
            })?;
            if !rustykrab_store::keychain::keychain_available() {
                return Err(format!(
                    "keychain ref `{service}/{account}` requested but \
                     OS credential store is unavailable on this platform"
                ));
            }
            let cred = rustykrab_store::keychain::get_credential(service, account)
                .map_err(|e| format!("keychain ref `{service}/{account}`: {e}"))?
                .ok_or_else(|| {
                    format!("keychain ref `{service}/{account}` not found in OS keychain")
                })?;
            tracing::info!(
                server = %server,
                ref_kind = "keychain",
                ref_service = %service,
                ref_account = %account,
                "resolved credential ref"
            );
            Ok(cred.value)
        }
        other => Err(format!(
            "unknown credential ref kind `{other}` in `{raw}` \
             (expected `store` or `keychain`)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_content_joins_text_blocks_with_newlines() {
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
        let secrets = make_test_secrets();
        let tools = mcp_connector_tools(&secrets).await;
        assert!(tools.is_empty());
    }

    /// Open a SecretStore backed by a fresh on-disk SQLite database in a
    /// tempdir. The tempdir is leaked so the file survives for the test;
    /// the OS reclaims `/tmp` on process exit.
    fn make_test_secrets() -> SecretStore {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = rustykrab_store::Store::open(dir.path(), vec![0u8; 32]).expect("open store");
        std::mem::forget(dir);
        store.secrets()
    }

    #[tokio::test]
    async fn resolve_value_ref_passes_through_literals() {
        let secrets = make_test_secrets();
        let v = resolve_value_ref("github", "ghp_literaltoken", &secrets)
            .await
            .unwrap();
        assert_eq!(v, "ghp_literaltoken");
    }

    #[tokio::test]
    async fn resolve_value_ref_store_lookup_succeeds_in_namespace() {
        let secrets = make_test_secrets();
        secrets
            .set("mcp.github.token", "stored-token-value")
            .await
            .unwrap();
        let v = resolve_value_ref("github", "ref:store:mcp.github.token", &secrets)
            .await
            .unwrap();
        assert_eq!(v, "stored-token-value");
    }

    #[tokio::test]
    async fn resolve_value_ref_store_lookup_is_case_insensitive_on_server() {
        let secrets = make_test_secrets();
        secrets
            .set("mcp.github.token", "stored-token-value")
            .await
            .unwrap();
        let v = resolve_value_ref("GitHub", "ref:store:mcp.github.token", &secrets)
            .await
            .unwrap();
        assert_eq!(v, "stored-token-value");
    }

    #[tokio::test]
    async fn resolve_value_ref_store_rejects_cross_server_lookup() {
        let secrets = make_test_secrets();
        secrets
            .set("mcp.notion.token", "other-server-token")
            .await
            .unwrap();
        let err = resolve_value_ref("github", "ref:store:mcp.notion.token", &secrets)
            .await
            .unwrap_err();
        assert!(
            err.contains("outside server"),
            "expected namespace error, got: {err}"
        );
    }

    #[tokio::test]
    async fn resolve_value_ref_store_rejects_unnamespaced_keys() {
        let secrets = make_test_secrets();
        secrets.set("global_secret", "x").await.unwrap();
        let err = resolve_value_ref("github", "ref:store:global_secret", &secrets)
            .await
            .unwrap_err();
        assert!(err.contains("outside server"));
    }

    #[tokio::test]
    async fn resolve_value_ref_store_missing_key_errors() {
        let secrets = make_test_secrets();
        let err = resolve_value_ref("github", "ref:store:mcp.github.missing", &secrets)
            .await
            .unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[tokio::test]
    async fn resolve_value_ref_rejects_unknown_kind() {
        let secrets = make_test_secrets();
        let err = resolve_value_ref("github", "ref:vault:foo", &secrets)
            .await
            .unwrap_err();
        assert!(err.contains("unknown credential ref kind"));
    }

    #[tokio::test]
    async fn resolve_value_ref_rejects_malformed_ref() {
        let secrets = make_test_secrets();
        let err = resolve_value_ref("github", "ref:store", &secrets)
            .await
            .unwrap_err();
        assert!(err.contains("malformed"));
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn resolve_value_ref_keychain_unavailable_on_non_mac() {
        let secrets = make_test_secrets();
        let err = resolve_value_ref("github", "ref:keychain:Service/account", &secrets)
            .await
            .unwrap_err();
        assert!(err.contains("unavailable"), "got: {err}");
    }

    #[tokio::test]
    async fn build_http_headers_token_becomes_bearer() {
        // Safety: unique prefix; no other test reads these.
        let upper = "TESTTOKEN";
        let secrets = make_test_secrets();
        std::env::set_var(format!("RUSTYKRAB_MCP_{upper}_TOKEN"), "abc123");
        let headers = build_http_headers("testtoken", upper, &secrets)
            .await
            .unwrap();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer abc123");
        std::env::remove_var(format!("RUSTYKRAB_MCP_{upper}_TOKEN"));
    }

    #[tokio::test]
    async fn build_http_headers_supports_arbitrary_headers() {
        // Safety: unique prefix; no other test reads these.
        let upper = "TESTDD";
        let secrets = make_test_secrets();
        std::env::set_var(
            format!("RUSTYKRAB_MCP_{upper}_HEADER_DD_API_KEY"),
            "key-aaa",
        );
        std::env::set_var(
            format!("RUSTYKRAB_MCP_{upper}_HEADER_DD_APPLICATION_KEY"),
            "key-bbb",
        );
        let headers = build_http_headers("testdd", upper, &secrets).await.unwrap();
        assert_eq!(headers.get("dd-api-key").unwrap(), "key-aaa");
        assert_eq!(headers.get("dd-application-key").unwrap(), "key-bbb");
        std::env::remove_var(format!("RUSTYKRAB_MCP_{upper}_HEADER_DD_API_KEY"));
        std::env::remove_var(format!("RUSTYKRAB_MCP_{upper}_HEADER_DD_APPLICATION_KEY"));
    }

    #[tokio::test]
    async fn build_http_headers_resolves_token_ref() {
        // Safety: unique prefix; no other test reads these.
        let upper = "TESTREF";
        let secrets = make_test_secrets();
        secrets
            .set("mcp.testref.token", "resolved-bearer")
            .await
            .unwrap();
        std::env::set_var(
            format!("RUSTYKRAB_MCP_{upper}_TOKEN"),
            "ref:store:mcp.testref.token",
        );
        let headers = build_http_headers("testref", upper, &secrets)
            .await
            .unwrap();
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer resolved-bearer"
        );
        std::env::remove_var(format!("RUSTYKRAB_MCP_{upper}_TOKEN"));
    }

    #[tokio::test]
    async fn build_http_headers_resolves_header_ref() {
        // Safety: unique prefix; no other test reads these.
        let upper = "TESTHDRREF";
        let secrets = make_test_secrets();
        secrets
            .set("mcp.testhdrref.api_key", "secret-key")
            .await
            .unwrap();
        std::env::set_var(
            format!("RUSTYKRAB_MCP_{upper}_HEADER_X_API_KEY"),
            "ref:store:mcp.testhdrref.api_key",
        );
        let headers = build_http_headers("testhdrref", upper, &secrets)
            .await
            .unwrap();
        assert_eq!(headers.get("x-api-key").unwrap(), "secret-key");
        std::env::remove_var(format!("RUSTYKRAB_MCP_{upper}_HEADER_X_API_KEY"));
    }

    #[tokio::test]
    async fn build_http_headers_rejects_cross_server_ref() {
        // Safety: unique prefix; no other test reads these.
        let upper = "TESTXSERVER";
        let secrets = make_test_secrets();
        secrets
            .set("mcp.notion.token", "wrong-server")
            .await
            .unwrap();
        std::env::set_var(
            format!("RUSTYKRAB_MCP_{upper}_TOKEN"),
            "ref:store:mcp.notion.token",
        );
        let err = build_http_headers("testxserver", upper, &secrets)
            .await
            .unwrap_err();
        assert!(err.contains("outside server"), "got: {err}");
        std::env::remove_var(format!("RUSTYKRAB_MCP_{upper}_TOKEN"));
    }

    #[test]
    fn collect_prefixed_returns_matching_keys_only() {
        let upper = "TESTCOLLECT";
        std::env::set_var(format!("RUSTYKRAB_MCP_{upper}_ENV_FOO"), "1");
        std::env::set_var(format!("RUSTYKRAB_MCP_{upper}_ENV_BAR"), "2");
        std::env::set_var(format!("RUSTYKRAB_MCP_{upper}_HEADER_BAZ"), "3");
        let mut got = collect_prefixed(upper, "ENV_");
        got.sort();
        assert_eq!(
            got,
            vec![
                ("BAR".to_string(), "2".to_string()),
                ("FOO".to_string(), "1".to_string())
            ]
        );
        std::env::remove_var(format!("RUSTYKRAB_MCP_{upper}_ENV_FOO"));
        std::env::remove_var(format!("RUSTYKRAB_MCP_{upper}_ENV_BAR"));
        std::env::remove_var(format!("RUSTYKRAB_MCP_{upper}_HEADER_BAZ"));
    }
}
