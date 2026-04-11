use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use rustykrab_store::SecretStore;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const NOTION_API_BASE: &str = "https://api.notion.com/v1";
const NOTION_VERSION: &str = "2022-06-28";

// SecretStore keys
const KEY_API_TOKEN: &str = "notion_api_token";
const KEY_DEFAULT_PARENT: &str = "notion_default_parent";

/// Maximum blocks per append request (Notion API limit).
const MAX_BLOCKS_PER_REQUEST: usize = 100;

// ---------------------------------------------------------------------------
// Tool struct
// ---------------------------------------------------------------------------

pub struct NotionTool {
    secrets: SecretStore,
    client: reqwest::Client,
}

impl NotionTool {
    pub fn new(secrets: SecretStore) -> Self {
        Self {
            secrets,
            client: reqwest::Client::new(),
        }
    }

    /// Get the stored Notion API token.
    fn get_token(&self) -> Result<String> {
        self.secrets.get(KEY_API_TOKEN).map_err(|_| {
            Error::ToolExecution(
                "notion_api_token not found. Store it with: \
                 notion(action='setup', api_token='ntn_...') or \
                 credential_write(action='set', name='notion_api_token', value='ntn_...')"
                    .into(),
            )
        })
    }

    /// Get the optional default parent page ID.
    fn get_default_parent(&self) -> Option<String> {
        self.secrets.get(KEY_DEFAULT_PARENT).ok()
    }

    /// Build an authorized request to the Notion API.
    fn notion_request(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let token = self.get_token()?;
        let url = format!("{NOTION_API_BASE}{path}");
        Ok(self
            .client
            .request(method, &url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Notion-Version", NOTION_VERSION)
            .header("Content-Type", "application/json"))
    }

    /// Send a request and parse the JSON response, returning a friendly error on failure.
    async fn send_and_parse(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let resp = req
            .send()
            .await
            .map_err(|e| Error::ToolExecution(format!("Notion API request failed: {e}").into()))?;

        let status = resp.status();
        let body = resp.text().await.map_err(|e| {
            Error::ToolExecution(format!("failed to read Notion response: {e}").into())
        })?;

        if !status.is_success() {
            return Err(Error::ToolExecution(
                format!("Notion API returned {status}: {body}").into(),
            ));
        }

        serde_json::from_str(&body).map_err(|e| {
            Error::ToolExecution(format!("failed to parse Notion response: {e}").into())
        })
    }

    // -----------------------------------------------------------------------
    // Action: setup
    // -----------------------------------------------------------------------

    async fn action_setup(&self, args: &Value) -> Result<Value> {
        let api_token = args["api_token"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'api_token' parameter".into()))?;

        self.secrets
            .set(KEY_API_TOKEN, api_token)
            .map_err(|e| Error::ToolExecution(format!("failed to store API token: {e}").into()))?;

        // Optionally store a default parent page ID.
        if let Some(parent_id) = args["default_parent_page_id"].as_str() {
            self.secrets
                .set(KEY_DEFAULT_PARENT, parent_id)
                .map_err(|e| {
                    Error::ToolExecution(format!("failed to store default parent: {e}").into())
                })?;
        }

        // Verify the token by calling /users/me.
        let token = api_token.to_string();
        let req = self
            .client
            .get(format!("{NOTION_API_BASE}/users/me"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Notion-Version", NOTION_VERSION);

        let resp = req.send().await.map_err(|e| {
            Error::ToolExecution(format!("failed to verify Notion token: {e}").into())
        })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            return Err(Error::ToolExecution(
                format!("Notion authentication failed ({status}): {err}").into(),
            ));
        }

        let user: Value = resp.json().await.map_err(|e| {
            Error::ToolExecution(format!("failed to parse user response: {e}").into())
        })?;

        let bot_name = user["name"].as_str().unwrap_or("unknown");

        Ok(json!({
            "status": "authenticated",
            "bot_name": bot_name,
            "message": format!("Notion integration '{bot_name}' connected successfully. Token stored securely."),
        }))
    }

    // -----------------------------------------------------------------------
    // Action: search
    // -----------------------------------------------------------------------

    async fn action_search(&self, args: &Value) -> Result<Value> {
        let query = args["query"].as_str().unwrap_or("");
        let filter_type = args["filter"].as_str(); // "page" or "database"
        let page_size = args["page_size"].as_u64().unwrap_or(10).min(100);
        let start_cursor = args["start_cursor"].as_str();

        let mut body = json!({
            "query": query,
            "page_size": page_size,
        });

        if let Some(ft) = filter_type {
            body["filter"] = json!({ "value": ft, "property": "object" });
        }
        if let Some(cursor) = start_cursor {
            body["start_cursor"] = json!(cursor);
        }

        let req = self
            .notion_request(reqwest::Method::POST, "/search")?
            .json(&body);

        let data = self.send_and_parse(req).await?;

        let results = data["results"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|item| {
                        let obj_type = item["object"].as_str().unwrap_or("");
                        let id = item["id"].as_str().unwrap_or("");
                        let title = extract_title(item);
                        let url = item["url"].as_str().unwrap_or("");
                        let last_edited = item["last_edited_time"].as_str().unwrap_or("");
                        json!({
                            "type": obj_type,
                            "id": id,
                            "title": title,
                            "url": url,
                            "last_edited": last_edited,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let has_more = data["has_more"].as_bool().unwrap_or(false);
        let next_cursor = data["next_cursor"].as_str();

        Ok(json!({
            "results": results,
            "count": results.len(),
            "has_more": has_more,
            "next_cursor": next_cursor,
        }))
    }

    // -----------------------------------------------------------------------
    // Action: get_page
    // -----------------------------------------------------------------------

    async fn action_get_page(&self, args: &Value) -> Result<Value> {
        let page_id = args["page_id"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'page_id' parameter".into()))?;

        let include_content = args["include_content"].as_bool().unwrap_or(true);

        // Fetch page properties.
        let req = self.notion_request(reqwest::Method::GET, &format!("/pages/{page_id}"))?;
        let page = self.send_and_parse(req).await?;

        let title = extract_title(&page);
        let url = page["url"].as_str().unwrap_or("").to_string();

        let mut result = json!({
            "id": page_id,
            "title": title,
            "url": url,
            "created_time": page["created_time"],
            "last_edited_time": page["last_edited_time"],
            "properties": page["properties"],
        });

        if include_content {
            let blocks = self.get_block_children(page_id, None).await?;
            result["content"] = blocks;
        }

        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Action: create_page
    // -----------------------------------------------------------------------

    async fn action_create_page(&self, args: &Value) -> Result<Value> {
        let title = args["title"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'title' parameter".into()))?;

        // Determine parent: explicit > default stored > error.
        let parent_id = args["parent_page_id"]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| self.get_default_parent())
            .ok_or_else(|| {
                Error::ToolExecution(
                    "missing 'parent_page_id'. Provide one or set a default via setup.".into(),
                )
            })?;

        let icon = args["icon"].as_str();
        let cover_url = args["cover_url"].as_str();

        // Build page creation payload.
        let mut body = json!({
            "parent": { "page_id": parent_id },
            "properties": {
                "title": {
                    "title": [{
                        "text": { "content": title }
                    }]
                }
            },
        });

        if let Some(emoji) = icon {
            body["icon"] = json!({ "type": "emoji", "emoji": emoji });
        }

        if let Some(url) = cover_url {
            body["cover"] = json!({ "type": "external", "external": { "url": url } });
        }

        // Add children blocks if provided.
        if let Some(blocks) = args.get("children") {
            if let Some(arr) = blocks.as_array() {
                // Notion limits to 100 children in create; take first batch.
                let capped: Vec<_> = arr.iter().take(MAX_BLOCKS_PER_REQUEST).cloned().collect();
                body["children"] = Value::Array(capped);
            } else if let Some(markdown) = blocks.as_str() {
                body["children"] = Value::Array(markdown_to_blocks(markdown));
            }
        }

        let req = self
            .notion_request(reqwest::Method::POST, "/pages")?
            .json(&body);

        let page = self.send_and_parse(req).await?;

        let page_id = page["id"].as_str().unwrap_or("");
        let url = page["url"].as_str().unwrap_or("");

        // If there were more than 100 blocks, append the rest.
        if let Some(blocks) = args.get("children") {
            if let Some(arr) = blocks.as_array() {
                if arr.len() > MAX_BLOCKS_PER_REQUEST {
                    let remaining = &arr[MAX_BLOCKS_PER_REQUEST..];
                    self.append_blocks_batched(page_id, remaining).await?;
                }
            }
        }

        let mut result = json!({
            "id": page_id,
            "url": url,
            "title": title,
            "message": format!("Page '{}' created successfully.", title),
        });

        // Sync to Obsidian if configured.
        let markdown_content = args.get("children").and_then(|c| c.as_str());
        match crate::obsidian::try_sync_to_obsidian(
            &self.secrets,
            title,
            markdown_content,
            Some(page_id),
            Some(url),
        )
        .await
        {
            Ok(Some(sync)) => {
                result["obsidian_sync"] = sync;
            }
            Ok(None) => {} // Obsidian not configured.
            Err(e) => {
                tracing::warn!("Obsidian sync failed for '{}': {}", title, e);
                result["obsidian_sync_error"] = json!(e);
            }
        }

        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Action: append_blocks
    // -----------------------------------------------------------------------

    async fn action_append_blocks(&self, args: &Value) -> Result<Value> {
        let page_id = args["page_id"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'page_id' parameter".into()))?;

        let blocks = if let Some(arr) = args["blocks"].as_array() {
            arr.clone()
        } else if let Some(markdown) = args["content"].as_str() {
            markdown_to_blocks(markdown)
        } else {
            return Err(Error::ToolExecution(
                "provide either 'blocks' (array of Notion blocks) or 'content' (markdown string)"
                    .into(),
            ));
        };

        self.append_blocks_batched(page_id, &blocks).await?;

        let mut result = json!({
            "page_id": page_id,
            "blocks_added": blocks.len(),
            "message": format!("Appended {} block(s) to page.", blocks.len()),
        });

        // Sync append to Obsidian if content was provided as markdown.
        if let Some(markdown) = args["content"].as_str() {
            if self.secrets.get("obsidian_api_key").is_ok() {
                // Look up the page title to derive the Obsidian vault path.
                let title =
                    match self.notion_request(reqwest::Method::GET, &format!("/pages/{page_id}")) {
                        Ok(req) => match self.send_and_parse(req).await {
                            Ok(page) => extract_title(&page),
                            Err(_) => String::new(),
                        },
                        Err(_) => String::new(),
                    };

                if !title.is_empty() {
                    match crate::obsidian::try_sync_append_to_obsidian(
                        &self.secrets,
                        &title,
                        markdown,
                    )
                    .await
                    {
                        Ok(Some(sync)) => {
                            result["obsidian_sync"] = sync;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!("Obsidian sync failed for append to '{}': {}", title, e);
                            result["obsidian_sync_error"] = json!(e);
                        }
                    }
                }
            }
        }

        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Action: create_database
    // -----------------------------------------------------------------------

    async fn action_create_database(&self, args: &Value) -> Result<Value> {
        let title = args["title"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'title' parameter".into()))?;

        let parent_id = args["parent_page_id"]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| self.get_default_parent())
            .ok_or_else(|| {
                Error::ToolExecution(
                    "missing 'parent_page_id'. Provide one or set a default via setup.".into(),
                )
            })?;

        let properties = args.get("properties").cloned().unwrap_or_else(|| {
            // Sensible default for a trip-planning database.
            json!({
                "Name": { "title": {} },
                "Status": {
                    "select": {
                        "options": [
                            { "name": "Planning", "color": "yellow" },
                            { "name": "Booked", "color": "green" },
                            { "name": "Done", "color": "blue" }
                        ]
                    }
                },
                "Date": { "date": {} },
                "Notes": { "rich_text": {} },
            })
        });

        let icon = args["icon"].as_str();

        let mut body = json!({
            "parent": { "page_id": parent_id },
            "title": [{ "text": { "content": title } }],
            "properties": properties,
        });

        if let Some(emoji) = icon {
            body["icon"] = json!({ "type": "emoji", "emoji": emoji });
        }

        let req = self
            .notion_request(reqwest::Method::POST, "/databases")?
            .json(&body);

        let db = self.send_and_parse(req).await?;

        Ok(json!({
            "id": db["id"],
            "url": db["url"],
            "title": title,
            "message": format!("Database '{}' created successfully.", title),
        }))
    }

    // -----------------------------------------------------------------------
    // Action: add_database_entry
    // -----------------------------------------------------------------------

    async fn action_add_database_entry(&self, args: &Value) -> Result<Value> {
        let database_id = args["database_id"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'database_id' parameter".into()))?;

        let properties = args
            .get("properties")
            .ok_or_else(|| Error::ToolExecution("missing 'properties' parameter".into()))?
            .clone();

        let mut body = json!({
            "parent": { "database_id": database_id },
            "properties": properties,
        });

        if let Some(children) = args.get("children") {
            if let Some(arr) = children.as_array() {
                let capped: Vec<_> = arr.iter().take(MAX_BLOCKS_PER_REQUEST).cloned().collect();
                body["children"] = Value::Array(capped);
            }
        }

        let req = self
            .notion_request(reqwest::Method::POST, "/pages")?
            .json(&body);

        let page = self.send_and_parse(req).await?;

        Ok(json!({
            "id": page["id"],
            "url": page["url"],
            "message": "Database entry created successfully.",
        }))
    }

    // -----------------------------------------------------------------------
    // Action: update_page
    // -----------------------------------------------------------------------

    async fn action_update_page(&self, args: &Value) -> Result<Value> {
        let page_id = args["page_id"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'page_id' parameter".into()))?;

        let mut body = json!({});

        if let Some(props) = args.get("properties") {
            body["properties"] = props.clone();
        }

        if let Some(emoji) = args["icon"].as_str() {
            body["icon"] = json!({ "type": "emoji", "emoji": emoji });
        }

        if let Some(url) = args["cover_url"].as_str() {
            body["cover"] = json!({ "type": "external", "external": { "url": url } });
        }

        if let Some(archived) = args["archived"].as_bool() {
            body["archived"] = json!(archived);
        }

        let req = self
            .notion_request(reqwest::Method::PATCH, &format!("/pages/{page_id}"))?
            .json(&body);

        let page = self.send_and_parse(req).await?;

        Ok(json!({
            "id": page["id"],
            "url": page["url"],
            "message": "Page updated successfully.",
        }))
    }

    // -----------------------------------------------------------------------
    // Action: delete_block
    // -----------------------------------------------------------------------

    async fn action_delete_block(&self, args: &Value) -> Result<Value> {
        let block_id = args["block_id"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'block_id' parameter".into()))?;

        let req = self.notion_request(reqwest::Method::DELETE, &format!("/blocks/{block_id}"))?;
        self.send_and_parse(req).await?;

        Ok(json!({
            "block_id": block_id,
            "message": "Block deleted successfully.",
        }))
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Fetch all child blocks of a given block/page with pagination.
    fn get_block_children<'a>(
        &'a self,
        block_id: &'a str,
        max_depth: Option<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            let depth = max_depth.unwrap_or(2);
            let mut all_blocks = Vec::new();
            let mut cursor: Option<String> = None;

            loop {
                let mut path = format!("/blocks/{block_id}/children?page_size=100");
                if let Some(ref c) = cursor {
                    path.push_str(&format!("&start_cursor={c}"));
                }

                let req = self.notion_request(reqwest::Method::GET, &path)?;
                let data = self.send_and_parse(req).await?;

                if let Some(results) = data["results"].as_array() {
                    for block in results {
                        let mut b = summarize_block(block);
                        // Recurse into children if the block has them and we have depth left.
                        if depth > 0 && block["has_children"].as_bool().unwrap_or(false) {
                            if let Some(id) = block["id"].as_str() {
                                b["children"] =
                                    self.get_block_children(id, Some(depth - 1)).await?;
                            }
                        }
                        all_blocks.push(b);
                    }
                }

                if data["has_more"].as_bool().unwrap_or(false) {
                    cursor = data["next_cursor"].as_str().map(|s| s.to_string());
                } else {
                    break;
                }
            }

            Ok(Value::Array(all_blocks))
        })
    }

    /// Append blocks in batches of MAX_BLOCKS_PER_REQUEST.
    async fn append_blocks_batched(&self, page_id: &str, blocks: &[Value]) -> Result<()> {
        for chunk in blocks.chunks(MAX_BLOCKS_PER_REQUEST) {
            let body = json!({
                "children": chunk,
            });
            let req = self
                .notion_request(
                    reqwest::Method::PATCH,
                    &format!("/blocks/{page_id}/children"),
                )?
                .json(&body);
            self.send_and_parse(req).await?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tool trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Tool for NotionTool {
    fn name(&self) -> &str {
        "notion"
    }

    fn description(&self) -> &str {
        "Create and manage beautiful Notion pages and databases. \
         Supports rich content: headings, callouts, toggles, tables, checklists, \
         bookmarks, quotes, dividers, images, and more. \
         Perfect for trip plans, project docs, reports, and knowledge bases."
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
                            "search",
                            "get_page",
                            "create_page",
                            "append_blocks",
                            "create_database",
                            "add_database_entry",
                            "update_page",
                            "delete_block"
                        ],
                        "description": "The Notion action to perform"
                    },
                    "api_token": {
                        "type": "string",
                        "description": "(setup) Notion integration token (starts with 'ntn_' or 'secret_')"
                    },
                    "default_parent_page_id": {
                        "type": "string",
                        "description": "(setup) Default parent page ID for new pages"
                    },
                    "query": {
                        "type": "string",
                        "description": "(search) Search query text"
                    },
                    "filter": {
                        "type": "string",
                        "enum": ["page", "database"],
                        "description": "(search) Filter results by type"
                    },
                    "page_size": {
                        "type": "integer",
                        "description": "(search) Number of results (1-100, default 10)"
                    },
                    "start_cursor": {
                        "type": "string",
                        "description": "(search/get_page) Pagination cursor"
                    },
                    "page_id": {
                        "type": "string",
                        "description": "(get_page/append_blocks/update_page) The page ID"
                    },
                    "include_content": {
                        "type": "boolean",
                        "description": "(get_page) Also fetch page blocks (default true)"
                    },
                    "title": {
                        "type": "string",
                        "description": "(create_page/create_database) Page or database title"
                    },
                    "parent_page_id": {
                        "type": "string",
                        "description": "(create_page/create_database) Parent page ID (uses default if omitted)"
                    },
                    "icon": {
                        "type": "string",
                        "description": "(create_page/create_database/update_page) Emoji icon, e.g. '✈️'"
                    },
                    "cover_url": {
                        "type": "string",
                        "description": "(create_page/update_page) Cover image URL"
                    },
                    "children": {
                        "description": "(create_page) Array of Notion block objects, or a markdown string to auto-convert",
                    },
                    "blocks": {
                        "description": "(append_blocks) Array of Notion block objects to append"
                    },
                    "content": {
                        "type": "string",
                        "description": "(append_blocks) Markdown string to auto-convert to blocks and append"
                    },
                    "properties": {
                        "description": "(create_database/add_database_entry/update_page) Property schema or values"
                    },
                    "database_id": {
                        "type": "string",
                        "description": "(add_database_entry) Target database ID"
                    },
                    "block_id": {
                        "type": "string",
                        "description": "(delete_block) Block ID to delete"
                    },
                    "archived": {
                        "type": "boolean",
                        "description": "(update_page) Set true to archive/trash the page"
                    },
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
            "search" => self.action_search(&args).await,
            "get_page" => self.action_get_page(&args).await,
            "create_page" => self.action_create_page(&args).await,
            "append_blocks" => self.action_append_blocks(&args).await,
            "create_database" => self.action_create_database(&args).await,
            "add_database_entry" => self.action_add_database_entry(&args).await,
            "update_page" => self.action_update_page(&args).await,
            "delete_block" => self.action_delete_block(&args).await,
            _ => Err(Error::ToolExecution(
                format!("unknown notion action: '{action}'").into(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Block helpers
// ---------------------------------------------------------------------------

/// Extract the title from a Notion page or database object.
fn extract_title(obj: &Value) -> String {
    // Try page-style properties.title
    if let Some(title_prop) = obj["properties"]["title"]["title"].as_array() {
        let text: String = title_prop
            .iter()
            .filter_map(|t| t["plain_text"].as_str())
            .collect();
        if !text.is_empty() {
            return text;
        }
    }

    // Try page-style "Name" property (common in databases).
    if let Some(props) = obj["properties"].as_object() {
        for (_key, val) in props {
            if let Some(title_arr) = val["title"].as_array() {
                let text: String = title_arr
                    .iter()
                    .filter_map(|t| t["plain_text"].as_str())
                    .collect();
                if !text.is_empty() {
                    return text;
                }
            }
        }
    }

    // Try database-style top-level title.
    if let Some(title_arr) = obj["title"].as_array() {
        let text: String = title_arr
            .iter()
            .filter_map(|t| t["plain_text"].as_str())
            .collect();
        if !text.is_empty() {
            return text;
        }
    }

    String::new()
}

/// Summarize a Notion block into a compact representation.
fn summarize_block(block: &Value) -> Value {
    let block_type = block["type"].as_str().unwrap_or("unknown");
    let id = block["id"].as_str().unwrap_or("");

    let mut summary = json!({
        "id": id,
        "type": block_type,
    });

    // Extract text content from the block's type-specific data.
    if let Some(type_data) = block.get(block_type) {
        // Most text blocks have a "rich_text" array.
        if let Some(rich_text) = type_data["rich_text"].as_array() {
            let text: String = rich_text
                .iter()
                .filter_map(|t| t["plain_text"].as_str())
                .collect();
            if !text.is_empty() {
                summary["text"] = json!(text);
            }
        }

        // Checkbox state for to_do blocks.
        if let Some(checked) = type_data["checked"].as_bool() {
            summary["checked"] = json!(checked);
        }

        // URL for bookmarks, embeds, images, etc.
        if let Some(url) = type_data["url"].as_str() {
            summary["url"] = json!(url);
        }
        if let Some(url) = type_data["external"]["url"].as_str() {
            summary["url"] = json!(url);
        }
        if let Some(url) = type_data["file"]["url"].as_str() {
            summary["url"] = json!(url);
        }

        // Caption for images/embeds.
        if let Some(caption) = type_data["caption"].as_array() {
            let cap_text: String = caption
                .iter()
                .filter_map(|t| t["plain_text"].as_str())
                .collect();
            if !cap_text.is_empty() {
                summary["caption"] = json!(cap_text);
            }
        }

        // Language for code blocks.
        if let Some(lang) = type_data["language"].as_str() {
            summary["language"] = json!(lang);
        }

        // Color for callout blocks.
        if let Some(color) = type_data["color"].as_str() {
            summary["color"] = json!(color);
        }
        if let Some(icon) = type_data["icon"]["emoji"].as_str() {
            summary["icon"] = json!(icon);
        }

        // Table dimensions.
        if let Some(width) = type_data["table_width"].as_u64() {
            summary["table_width"] = json!(width);
        }
    }

    if block["has_children"].as_bool().unwrap_or(false) {
        summary["has_children"] = json!(true);
    }

    summary
}

// ---------------------------------------------------------------------------
// Markdown-to-blocks converter
// ---------------------------------------------------------------------------

/// Convert a simple markdown string into Notion block objects.
///
/// Supports: headings (#, ##, ###), bullet lists (- / *), numbered lists (1.),
/// checklists (- [ ] / - [x]), blockquotes (>), horizontal rules (---),
/// code fences (```), images (![alt](url)), and paragraphs.
///
/// This isn't a full markdown parser — it handles the most useful cases for
/// producing beautiful Notion documents from the agent.
fn markdown_to_blocks(md: &str) -> Vec<Value> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = md.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        // Skip blank lines.
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Code fence.
        if trimmed.starts_with("```") {
            let lang = trimmed.trim_start_matches('`').trim();
            let lang = if lang.is_empty() { "plain text" } else { lang };
            let mut code_lines = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim().starts_with("```") {
                code_lines.push(lines[i]);
                i += 1;
            }
            i += 1; // skip closing ```
            blocks.push(json!({
                "type": "code",
                "code": {
                    "rich_text": [{ "text": { "content": code_lines.join("\n") } }],
                    "language": lang,
                }
            }));
            continue;
        }

        // Horizontal rule.
        if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            blocks.push(json!({ "type": "divider", "divider": {} }));
            i += 1;
            continue;
        }

        // Headings.
        if let Some(rest) = trimmed.strip_prefix("### ") {
            blocks.push(json!({
                "type": "heading_3",
                "heading_3": { "rich_text": rich_text_from_inline(rest) }
            }));
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            blocks.push(json!({
                "type": "heading_2",
                "heading_2": { "rich_text": rich_text_from_inline(rest) }
            }));
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            blocks.push(json!({
                "type": "heading_1",
                "heading_1": { "rich_text": rich_text_from_inline(rest) }
            }));
            i += 1;
            continue;
        }

        // Checklist items.
        if let Some(rest) = trimmed
            .strip_prefix("- [x] ")
            .or_else(|| trimmed.strip_prefix("- [X] "))
        {
            blocks.push(json!({
                "type": "to_do",
                "to_do": {
                    "rich_text": rich_text_from_inline(rest),
                    "checked": true,
                }
            }));
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("- [ ] ") {
            blocks.push(json!({
                "type": "to_do",
                "to_do": {
                    "rich_text": rich_text_from_inline(rest),
                    "checked": false,
                }
            }));
            i += 1;
            continue;
        }

        // Bulleted list items.
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            blocks.push(json!({
                "type": "bulleted_list_item",
                "bulleted_list_item": { "rich_text": rich_text_from_inline(rest) }
            }));
            i += 1;
            continue;
        }

        // Numbered list items.
        if let Some(pos) = trimmed.find(". ") {
            let prefix = &trimmed[..pos];
            if prefix.chars().all(|c| c.is_ascii_digit()) {
                let rest = &trimmed[pos + 2..];
                blocks.push(json!({
                    "type": "numbered_list_item",
                    "numbered_list_item": { "rich_text": rich_text_from_inline(rest) }
                }));
                i += 1;
                continue;
            }
        }

        // Blockquote.
        if let Some(rest) = trimmed.strip_prefix("> ") {
            blocks.push(json!({
                "type": "quote",
                "quote": { "rich_text": rich_text_from_inline(rest) }
            }));
            i += 1;
            continue;
        }

        // Image.
        if trimmed.starts_with("![") {
            if let Some((alt, url)) = parse_image_markdown(trimmed) {
                blocks.push(json!({
                    "type": "image",
                    "image": {
                        "type": "external",
                        "external": { "url": url },
                        "caption": [{ "text": { "content": alt } }],
                    }
                }));
                i += 1;
                continue;
            }
        }

        // Callout — special syntax: > 💡 text or > ⚠️ text (emoji after >).
        // (Already handled as quote above; agent can use block JSON for callouts.)

        // Default: paragraph.
        blocks.push(json!({
            "type": "paragraph",
            "paragraph": { "rich_text": rich_text_from_inline(trimmed) }
        }));
        i += 1;
    }

    blocks
}

/// Parse inline markdown into Notion rich_text segments.
///
/// Handles **bold**, *italic*, `code`, [links](url), and ~~strikethrough~~.
fn rich_text_from_inline(text: &str) -> Vec<Value> {
    let mut segments = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        // Find the earliest inline marker.
        let markers: &[(&str, &str, &str)] = &[
            ("**", "**", "bold"),
            ("*", "*", "italic"),
            ("`", "`", "code"),
            ("~~", "~~", "strikethrough"),
            ("[", ")", "link"),
        ];

        let mut earliest: Option<(usize, &str, &str, &str)> = None;
        for &(open, close, kind) in markers {
            if let Some(start) = remaining.find(open) {
                if earliest.is_none() || start < earliest.unwrap().0 {
                    earliest = Some((start, open, close, kind));
                }
            }
        }

        let Some((start, open, close, kind)) = earliest else {
            // No more markers — push the rest as plain text.
            if !remaining.is_empty() {
                segments.push(json!({
                    "text": { "content": remaining }
                }));
            }
            break;
        };

        // Push plain text before this marker.
        if start > 0 {
            segments.push(json!({
                "text": { "content": &remaining[..start] }
            }));
        }

        let after_open = &remaining[start + open.len()..];

        if kind == "link" {
            // Parse [text](url)
            if let Some(close_bracket) = after_open.find("](") {
                let link_text = &after_open[..close_bracket];
                let after_bracket = &after_open[close_bracket + 2..];
                if let Some(close_paren) = after_bracket.find(')') {
                    let url = &after_bracket[..close_paren];
                    segments.push(json!({
                        "text": {
                            "content": link_text,
                            "link": { "url": url },
                        },
                        "annotations": { "color": "blue" },
                    }));
                    remaining = &after_bracket[close_paren + 1..];
                    continue;
                }
            }
            // Malformed link — treat opening bracket as text.
            segments.push(json!({
                "text": { "content": &remaining[start..start + open.len()] }
            }));
            remaining = after_open;
            continue;
        }

        // For bold/italic/code/strikethrough: find the closing marker.
        if let Some(end) = after_open.find(close) {
            let inner = &after_open[..end];
            let mut seg = json!({
                "text": { "content": inner }
            });
            match kind {
                "bold" => seg["annotations"] = json!({ "bold": true }),
                "italic" => seg["annotations"] = json!({ "italic": true }),
                "code" => seg["annotations"] = json!({ "code": true }),
                "strikethrough" => seg["annotations"] = json!({ "strikethrough": true }),
                _ => {}
            }
            segments.push(seg);
            remaining = &after_open[end + close.len()..];
        } else {
            // No closing marker — treat as plain text.
            segments.push(json!({
                "text": { "content": &remaining[start..start + open.len()] }
            }));
            remaining = after_open;
        }
    }

    if segments.is_empty() {
        vec![json!({ "text": { "content": "" } })]
    } else {
        segments
    }
}

/// Parse `![alt](url)` markdown image syntax.
fn parse_image_markdown(s: &str) -> Option<(&str, &str)> {
    let s = s.strip_prefix("![")?;
    let close_bracket = s.find("](")?;
    let alt = &s[..close_bracket];
    let rest = &s[close_bracket + 2..];
    let close_paren = rest.find(')')?;
    let url = &rest[..close_paren];
    Some((alt, url))
}
