use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use rustykrab_store::SecretStore;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const KEY_API_URL: &str = "obsidian_api_url";
const KEY_API_KEY: &str = "obsidian_api_key";
const KEY_SYNC_FOLDER: &str = "obsidian_sync_folder";
const DEFAULT_API_URL: &str = "https://127.0.0.1:27124";

// ---------------------------------------------------------------------------
// Tool struct
// ---------------------------------------------------------------------------

pub struct ObsidianTool {
    secrets: SecretStore,
    client: reqwest::Client,
}

impl ObsidianTool {
    pub fn new(secrets: SecretStore) -> Self {
        // The Obsidian Local REST API uses a self-signed certificate by default.
        // Accept invalid certs since this is a localhost-only connection.
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap_or_default();
        Self { secrets, client }
    }

    fn get_api_url(&self) -> String {
        self.secrets
            .get(KEY_API_URL)
            .unwrap_or_else(|_| DEFAULT_API_URL.to_string())
    }

    fn get_api_key(&self) -> Result<String> {
        self.secrets.get(KEY_API_KEY).map_err(|_| {
            Error::ToolExecution(
                "obsidian_api_key not found. Store it with: \
                 obsidian(action='setup', api_key='...') or \
                 credential_write(action='set', name='obsidian_api_key', value='...')"
                    .into(),
            )
        })
    }

    /// Build an authorized request to the Obsidian Local REST API.
    fn obsidian_request(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let api_key = self.get_api_key()?;
        let base_url = self.get_api_url();
        let url = format!("{base_url}{path}");
        Ok(self
            .client
            .request(method, &url)
            .header("Authorization", format!("Bearer {api_key}")))
    }

    /// Send a request and return status code + body text.
    async fn send_text(&self, req: reqwest::RequestBuilder) -> Result<(u16, String)> {
        let resp = req.send().await.map_err(|e| {
            Error::ToolExecution(format!("Obsidian API request failed: {e}").into())
        })?;
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| {
            Error::ToolExecution(format!("failed to read Obsidian response: {e}").into())
        })?;
        Ok((status, body))
    }

    /// Send a request and parse the JSON response.
    async fn send_and_parse(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let (status, body) = self.send_text(req).await?;
        if status >= 400 {
            return Err(Error::ToolExecution(
                format!("Obsidian API returned {status}: {body}").into(),
            ));
        }
        serde_json::from_str(&body).map_err(|e| {
            Error::ToolExecution(format!("failed to parse Obsidian response: {e}").into())
        })
    }

    // -----------------------------------------------------------------------
    // Action: setup
    // -----------------------------------------------------------------------

    async fn action_setup(&self, args: &Value) -> Result<Value> {
        let api_key = args["api_key"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'api_key' parameter".into()))?;

        self.secrets
            .set(KEY_API_KEY, api_key)
            .map_err(|e| Error::ToolExecution(format!("failed to store API key: {e}").into()))?;

        if let Some(url) = args["api_url"].as_str() {
            self.secrets.set(KEY_API_URL, url).map_err(|e| {
                Error::ToolExecution(format!("failed to store API URL: {e}").into())
            })?;
        }

        if let Some(folder) = args["sync_folder"].as_str() {
            self.secrets.set(KEY_SYNC_FOLDER, folder).map_err(|e| {
                Error::ToolExecution(format!("failed to store sync folder: {e}").into())
            })?;
        }

        // Verify connectivity.
        let base_url = if let Some(url) = args["api_url"].as_str() {
            url.to_string()
        } else {
            self.get_api_url()
        };

        let resp = self
            .client
            .get(format!("{base_url}/"))
            .header("Authorization", format!("Bearer {api_key}"))
            .send()
            .await
            .map_err(|e| {
                Error::ToolExecution(
                    format!(
                        "failed to connect to Obsidian at {base_url}: {e}. \
                         Is the Obsidian Local REST API plugin installed and running?"
                    )
                    .into(),
                )
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            return Err(Error::ToolExecution(
                format!("Obsidian authentication failed ({status}): {err}").into(),
            ));
        }

        let info: Value = resp.json().await.unwrap_or(json!({}));

        Ok(json!({
            "status": "connected",
            "api_url": base_url,
            "info": info,
            "message": "Obsidian Local REST API connected successfully. API key stored securely.",
        }))
    }

    // -----------------------------------------------------------------------
    // Action: create_note
    // -----------------------------------------------------------------------

    async fn action_create_note(&self, args: &Value) -> Result<Value> {
        let path = args["path"].as_str().ok_or_else(|| {
            Error::ToolExecution("missing 'path' parameter (e.g. 'Notes/MyNote.md')".into())
        })?;

        let content = args["content"].as_str().unwrap_or("");
        let path = ensure_md_extension(path);

        let req = self
            .obsidian_request(reqwest::Method::PUT, &format!("/vault/{path}"))?
            .header("Content-Type", "text/markdown")
            .body(content.to_string());

        let (status, body) = self.send_text(req).await?;

        if status >= 400 {
            return Err(Error::ToolExecution(
                format!("failed to create note (HTTP {status}): {body}").into(),
            ));
        }

        Ok(json!({
            "path": path,
            "message": format!("Note '{path}' created successfully."),
        }))
    }

    // -----------------------------------------------------------------------
    // Action: get_note
    // -----------------------------------------------------------------------

    async fn action_get_note(&self, args: &Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'path' parameter".into()))?;

        let path = ensure_md_extension(path);

        let req = self
            .obsidian_request(reqwest::Method::GET, &format!("/vault/{path}"))?
            .header("Accept", "text/markdown");

        let (status, body) = self.send_text(req).await?;

        if status == 404 {
            return Err(Error::ToolExecution(
                format!("Note not found: {path}").into(),
            ));
        }
        if status >= 400 {
            return Err(Error::ToolExecution(
                format!("failed to get note (HTTP {status}): {body}").into(),
            ));
        }

        Ok(json!({
            "path": path,
            "content": body,
        }))
    }

    // -----------------------------------------------------------------------
    // Action: append_content
    // -----------------------------------------------------------------------

    async fn action_append_content(&self, args: &Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'path' parameter".into()))?;

        let content = args["content"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'content' parameter".into()))?;

        let path = ensure_md_extension(path);

        let req = self
            .obsidian_request(reqwest::Method::PATCH, &format!("/vault/{path}"))?
            .header("Content-Type", "text/markdown")
            .header("Content-Insertion-Position", "end")
            .body(content.to_string());

        let (status, body) = self.send_text(req).await?;

        if status >= 400 {
            return Err(Error::ToolExecution(
                format!("failed to append to note (HTTP {status}): {body}").into(),
            ));
        }

        Ok(json!({
            "path": path,
            "message": format!("Content appended to '{path}' successfully."),
        }))
    }

    // -----------------------------------------------------------------------
    // Action: search
    // -----------------------------------------------------------------------

    async fn action_search(&self, args: &Value) -> Result<Value> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'query' parameter".into()))?;

        let context_length = args["context_length"].as_u64().unwrap_or(100);

        let api_key = self.get_api_key()?;
        let base_url = self.get_api_url();

        let resp = self
            .client
            .post(format!("{base_url}/search/simple/"))
            .header("Authorization", format!("Bearer {api_key}"))
            .query(&[
                ("query", query.to_string()),
                ("contextLength", context_length.to_string()),
            ])
            .send()
            .await
            .map_err(|e| Error::ToolExecution(format!("Obsidian search failed: {e}").into()))?;

        let status = resp.status();
        let body = resp.text().await.map_err(|e| {
            Error::ToolExecution(format!("failed to read search response: {e}").into())
        })?;

        if !status.is_success() {
            return Err(Error::ToolExecution(
                format!("Obsidian search failed ({status}): {body}").into(),
            ));
        }

        let results: Value = serde_json::from_str(&body).map_err(|e| {
            Error::ToolExecution(format!("failed to parse search response: {e}").into())
        })?;

        let count = results.as_array().map(|a| a.len()).unwrap_or(0);

        Ok(json!({
            "results": results,
            "count": count,
            "query": query,
        }))
    }

    // -----------------------------------------------------------------------
    // Action: delete_note
    // -----------------------------------------------------------------------

    async fn action_delete_note(&self, args: &Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'path' parameter".into()))?;

        let path = ensure_md_extension(path);

        let req = self.obsidian_request(reqwest::Method::DELETE, &format!("/vault/{path}"))?;

        let (status, body) = self.send_text(req).await?;

        if status == 404 {
            return Err(Error::ToolExecution(
                format!("Note not found: {path}").into(),
            ));
        }
        if status >= 400 {
            return Err(Error::ToolExecution(
                format!("failed to delete note (HTTP {status}): {body}").into(),
            ));
        }

        Ok(json!({
            "path": path,
            "message": format!("Note '{path}' deleted successfully."),
        }))
    }

    // -----------------------------------------------------------------------
    // Action: list_notes
    // -----------------------------------------------------------------------

    async fn action_list_notes(&self, args: &Value) -> Result<Value> {
        let directory = args["directory"].as_str().unwrap_or("");

        let req = self
            .obsidian_request(reqwest::Method::GET, "/vault/")?
            .header("Accept", "application/json");

        let data = self.send_and_parse(req).await?;

        let files: Vec<Value> = data["files"]
            .as_array()
            .map(|arr| {
                let dir_prefix = directory.trim_matches('/');
                if dir_prefix.is_empty() {
                    arr.clone()
                } else {
                    arr.iter()
                        .filter(|f| {
                            f.as_str()
                                .map(|s| s.starts_with(dir_prefix))
                                .unwrap_or(false)
                        })
                        .cloned()
                        .collect()
                }
            })
            .unwrap_or_default();

        let count = files.len();

        Ok(json!({
            "files": files,
            "count": count,
            "directory": directory,
        }))
    }
}

// ---------------------------------------------------------------------------
// Tool trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Tool for ObsidianTool {
    fn name(&self) -> &str {
        "obsidian"
    }

    fn description(&self) -> &str {
        "Create and manage Obsidian vault notes via the Local REST API plugin. \
         Supports creating, reading, updating, searching, and deleting markdown notes. \
         Documents synced from Notion are automatically stored in the configured sync folder. \
         Requires the Obsidian Local REST API community plugin."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "setup",
                            "create_note",
                            "get_note",
                            "append_content",
                            "search",
                            "delete_note",
                            "list_notes"
                        ],
                        "description": "The Obsidian action to perform"
                    },
                    "api_key": {
                        "type": "string",
                        "description": "(setup) API key from the Obsidian Local REST API plugin settings"
                    },
                    "api_url": {
                        "type": "string",
                        "description": "(setup) API URL (default: https://127.0.0.1:27124)"
                    },
                    "sync_folder": {
                        "type": "string",
                        "description": "(setup) Vault folder for Notion-synced documents (e.g. 'Notion Sync')"
                    },
                    "path": {
                        "type": "string",
                        "description": "(create_note/get_note/append_content/delete_note) Note path in vault (e.g. 'Folder/Note.md')"
                    },
                    "content": {
                        "type": "string",
                        "description": "(create_note/append_content) Markdown content for the note"
                    },
                    "query": {
                        "type": "string",
                        "description": "(search) Search query text"
                    },
                    "context_length": {
                        "type": "integer",
                        "description": "(search) Characters of context around each match (default 100)"
                    },
                    "directory": {
                        "type": "string",
                        "description": "(list_notes) Filter by vault directory path (default: list all)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'action' parameter".into()))?;

        match action {
            "setup" => self.action_setup(&args).await,
            "create_note" => self.action_create_note(&args).await,
            "get_note" => self.action_get_note(&args).await,
            "append_content" => self.action_append_content(&args).await,
            "search" => self.action_search(&args).await,
            "delete_note" => self.action_delete_note(&args).await,
            "list_notes" => self.action_list_notes(&args).await,
            _ => Err(Error::ToolExecution(
                format!("unknown obsidian action: '{action}'").into(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Ensure a path ends with `.md`.
fn ensure_md_extension(path: &str) -> String {
    if path.ends_with(".md") {
        path.to_string()
    } else {
        format!("{path}.md")
    }
}

/// Sanitize a title for use as a vault filename.
fn sanitize_filename(title: &str) -> String {
    title
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            _ => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Build note content with YAML frontmatter for Notion-synced documents.
fn format_synced_note(
    title: &str,
    content: Option<&str>,
    notion_id: Option<&str>,
    notion_url: Option<&str>,
) -> String {
    let mut note = String::new();

    // YAML frontmatter with Notion metadata.
    note.push_str("---\n");
    if let Some(id) = notion_id {
        note.push_str(&format!("notion_id: \"{id}\"\n"));
    }
    if let Some(url) = notion_url {
        note.push_str(&format!("notion_url: \"{url}\"\n"));
    }
    note.push_str("source: notion-sync\n");
    note.push_str("---\n\n");

    note.push_str(&format!("# {title}\n\n"));

    if let Some(md) = content {
        note.push_str(md);
        if !md.ends_with('\n') {
            note.push('\n');
        }
    }

    note
}

// ---------------------------------------------------------------------------
// Public sync functions (called from notion.rs)
// ---------------------------------------------------------------------------

/// Attempt to sync a document to Obsidian if configured.
///
/// Returns `Ok(Some(json))` on success, `Ok(None)` if Obsidian is not configured,
/// or `Err(message)` if sync was attempted but failed.
pub async fn try_sync_to_obsidian(
    secrets: &SecretStore,
    title: &str,
    content: Option<&str>,
    notion_id: Option<&str>,
    notion_url: Option<&str>,
) -> std::result::Result<Option<Value>, String> {
    // Check if Obsidian is configured.
    let api_key = match secrets.get(KEY_API_KEY) {
        Ok(key) => key,
        Err(_) => return Ok(None), // Not configured — skip silently.
    };

    let api_url = secrets
        .get(KEY_API_URL)
        .unwrap_or_else(|_| DEFAULT_API_URL.to_string());

    let sync_folder = secrets.get(KEY_SYNC_FOLDER).ok();

    // Build the vault path from the title.
    let filename = sanitize_filename(title);
    let vault_path = match &sync_folder {
        Some(folder) => {
            let folder = folder.trim_matches('/');
            format!("{folder}/{filename}.md")
        }
        None => format!("{filename}.md"),
    };

    let note_content = format_synced_note(title, content, notion_id, notion_url);

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|e| format!("failed to create HTTP client: {e}"))?;

    let resp = client
        .put(format!("{api_url}/vault/{vault_path}"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "text/markdown")
        .body(note_content)
        .send()
        .await
        .map_err(|e| format!("Obsidian sync request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Obsidian sync failed ({status}): {body}"));
    }

    Ok(Some(json!({
        "obsidian_path": vault_path,
        "message": format!("Synced to Obsidian vault: {vault_path}"),
    })))
}

/// Attempt to append content to an existing Obsidian note derived from a title.
///
/// Same return semantics as [`try_sync_to_obsidian`].
pub async fn try_sync_append_to_obsidian(
    secrets: &SecretStore,
    title: &str,
    content: &str,
) -> std::result::Result<Option<Value>, String> {
    let api_key = match secrets.get(KEY_API_KEY) {
        Ok(key) => key,
        Err(_) => return Ok(None),
    };

    let api_url = secrets
        .get(KEY_API_URL)
        .unwrap_or_else(|_| DEFAULT_API_URL.to_string());

    let sync_folder = secrets.get(KEY_SYNC_FOLDER).ok();

    let filename = sanitize_filename(title);
    let vault_path = match &sync_folder {
        Some(folder) => {
            let folder = folder.trim_matches('/');
            format!("{folder}/{filename}.md")
        }
        None => format!("{filename}.md"),
    };

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|e| format!("failed to create HTTP client: {e}"))?;

    let resp = client
        .patch(format!("{api_url}/vault/{vault_path}"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "text/markdown")
        .header("Content-Insertion-Position", "end")
        .body(format!("\n{content}"))
        .send()
        .await
        .map_err(|e| format!("Obsidian append sync failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Obsidian append sync failed ({status}): {body}"));
    }

    Ok(Some(json!({
        "obsidian_path": vault_path,
        "message": format!("Appended to Obsidian note: {vault_path}"),
    })))
}
