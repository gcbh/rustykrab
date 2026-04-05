mod credential_read;
mod credential_write;
mod http_request;
mod http_session;
mod web_fetch;
mod web_search;

pub use credential_read::CredentialReadTool;
pub use credential_write::CredentialWriteTool;
pub use http_request::HttpRequestTool;
pub use http_session::HttpSessionTool;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;

/// Collect all built-in tools into a Vec.
///
/// Tools that need access to the secret store (credential_read, credential_write)
/// require a `SecretStore` handle. The remaining tools are stateless or self-contained.
pub fn builtin_tools(
    secrets: openclaw_store::SecretStore,
) -> Vec<std::sync::Arc<dyn openclaw_core::Tool>> {
    vec![
        std::sync::Arc::new(HttpRequestTool::new()),
        std::sync::Arc::new(WebFetchTool::new()),
        std::sync::Arc::new(WebSearchTool::new()),
        std::sync::Arc::new(HttpSessionTool::new()),
        std::sync::Arc::new(CredentialReadTool::new(secrets.clone())),
        std::sync::Arc::new(CredentialWriteTool::new(secrets)),
    ]
}
