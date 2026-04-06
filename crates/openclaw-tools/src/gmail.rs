use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use openclaw_core::types::ToolSchema;
use openclaw_core::{Error, Result, Tool};
use openclaw_store::SecretStore;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GMAIL_API_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

const GMAIL_SCOPES: &str =
    "https://www.googleapis.com/auth/gmail.modify \
     https://www.googleapis.com/auth/gmail.send \
     https://www.googleapis.com/auth/gmail.readonly";

// SecretStore keys
const KEY_CLIENT_ID: &str = "gmail_client_id";
const KEY_CLIENT_SECRET: &str = "gmail_client_secret";
const KEY_ACCESS_TOKEN: &str = "gmail_access_token";
const KEY_REFRESH_TOKEN: &str = "gmail_refresh_token";
const KEY_TOKEN_EXPIRES_AT: &str = "gmail_token_expires_at";

/// Maximum messages to enrich with metadata after a search.
const MAX_SEARCH_RESULTS: usize = 50;

// ---------------------------------------------------------------------------
// Tool struct
// ---------------------------------------------------------------------------

pub struct GmailTool {
    secrets: SecretStore,
    client: reqwest::Client,
}

impl GmailTool {
    pub fn new(secrets: SecretStore) -> Self {
        Self {
            secrets,
            client: reqwest::Client::new(),
        }
    }

    // -----------------------------------------------------------------------
    // OAuth helpers
    // -----------------------------------------------------------------------

    /// Retrieve a valid access token, refreshing if expired.
    async fn get_valid_token(&self) -> Result<String> {
        let access_token = self
            .secrets
            .get(KEY_ACCESS_TOKEN)
            .map_err(|_| Error::ToolExecution("Gmail not authenticated. Run gmail(action='auth') first.".into()))?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let expires_at: u64 = self
            .secrets
            .get(KEY_TOKEN_EXPIRES_AT)
            .unwrap_or_else(|_| "0".to_string())
            .parse()
            .unwrap_or(0);

        if now >= expires_at.saturating_sub(60) {
            debug!("Gmail access token expired, refreshing");
            return self.refresh_token().await;
        }

        Ok(access_token)
    }

    /// Exchange the refresh token for a new access token.
    async fn refresh_token(&self) -> Result<String> {
        let client_id = self.secrets.get(KEY_CLIENT_ID)
            .map_err(|_| Error::ToolExecution("gmail_client_id not set".into()))?;
        let client_secret = self.secrets.get(KEY_CLIENT_SECRET)
            .map_err(|_| Error::ToolExecution("gmail_client_secret not set".into()))?;
        let refresh_token = self.secrets.get(KEY_REFRESH_TOKEN)
            .map_err(|_| Error::ToolExecution("gmail_refresh_token not found — re-run auth".into()))?;

        let params = [
            ("grant_type", "refresh_token"),
            ("client_id", &client_id),
            ("client_secret", &client_secret),
            ("refresh_token", &refresh_token),
        ];

        let resp = self
            .client
            .post(GOOGLE_TOKEN_URL)
            .form(&params)
            .send()
            .await
            .map_err(|e| Error::ToolExecution(format!("token refresh request failed: {e}")))?;

        let body: Value = parse_gmail_response(resp).await?;

        let new_access = body["access_token"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("no access_token in refresh response".into()))?;
        let expires_in = body["expires_in"].as_u64().unwrap_or(3600);

        let expires_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + expires_in;

        self.secrets.set(KEY_ACCESS_TOKEN, new_access)
            .map_err(|e| Error::ToolExecution(format!("failed to store access token: {e}")))?;
        self.secrets.set(KEY_TOKEN_EXPIRES_AT, &expires_at.to_string())
            .map_err(|e| Error::ToolExecution(format!("failed to store token expiry: {e}")))?;

        // If Google returns a new refresh token, update it
        if let Some(new_refresh) = body["refresh_token"].as_str() {
            self.secrets.set(KEY_REFRESH_TOKEN, new_refresh)
                .map_err(|e| Error::ToolExecution(format!("failed to store refresh token: {e}")))?;
        }

        Ok(new_access.to_string())
    }

    /// Make an authenticated Gmail API request with one 401 retry.
    async fn gmail_request(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let token = self.get_valid_token().await?;

        let mut req = self.client.request(method.clone(), url)
            .bearer_auth(&token);
        if let Some(b) = body {
            req = req.json(b);
        }

        let resp = req.send().await
            .map_err(|e| Error::ToolExecution(format!("Gmail request failed: {e}")))?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            debug!("Gmail 401 — refreshing token and retrying");
            let new_token = self.refresh_token().await?;

            let mut retry = self.client.request(method, url)
                .bearer_auth(&new_token);
            if let Some(b) = body {
                retry = retry.json(b);
            }

            let resp2 = retry.send().await
                .map_err(|e| Error::ToolExecution(format!("Gmail retry failed: {e}")))?;
            return parse_gmail_response(resp2).await;
        }

        parse_gmail_response(resp).await
    }

    // -----------------------------------------------------------------------
    // Action: auth
    // -----------------------------------------------------------------------

    async fn action_auth(&self) -> Result<Value> {
        let client_id = self.secrets.get(KEY_CLIENT_ID).map_err(|_| {
            Error::ToolExecution(
                "gmail_client_id not found. Store it with: \
                 credential_write(action='set', name='gmail_client_id', value='YOUR_CLIENT_ID')"
                    .into(),
            )
        })?;
        let client_secret = self.secrets.get(KEY_CLIENT_SECRET).map_err(|_| {
            Error::ToolExecution(
                "gmail_client_secret not found. Store it with: \
                 credential_write(action='set', name='gmail_client_secret', value='YOUR_SECRET')"
                    .into(),
            )
        })?;

        // Bind a TCP listener on an ephemeral port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to bind listener: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| Error::ToolExecution(format!("failed to get local addr: {e}")))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}");

        // Build the authorization URL
        let auth_url = format!(
            "{GOOGLE_AUTH_URL}?{}",
            urlencoded(&[
                ("client_id", client_id.as_str()),
                ("redirect_uri", redirect_uri.as_str()),
                ("response_type", "code"),
                ("scope", GMAIL_SCOPES),
                ("access_type", "offline"),
                ("prompt", "consent"),
            ])
        );

        // Try to open browser
        open_browser(&auth_url);

        let auth_msg = format!(
            "Opening browser for Gmail authorization.\n\
             If the browser didn't open, visit:\n{auth_url}"
        );

        // Wait for the callback (120s timeout)
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            accept_oauth_callback(&listener),
        )
        .await
        .map_err(|_| Error::ToolExecution("OAuth callback timed out after 120s".into()))?
        .map_err(|e| Error::ToolExecution(format!("OAuth callback failed: {e}")))?;

        // Exchange code for tokens
        let params = [
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
        ];

        let resp = self
            .client
            .post(GOOGLE_TOKEN_URL)
            .form(&params)
            .send()
            .await
            .map_err(|e| Error::ToolExecution(format!("token exchange failed: {e}")))?;

        let body: Value = parse_gmail_response(resp).await?;

        let access_token = body["access_token"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("no access_token in response".into()))?;
        let refresh_token = body["refresh_token"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("no refresh_token in response".into()))?;
        let expires_in = body["expires_in"].as_u64().unwrap_or(3600);

        let expires_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + expires_in;

        // Store all tokens
        self.secrets.set(KEY_ACCESS_TOKEN, access_token)
            .map_err(|e| Error::ToolExecution(format!("failed to store access token: {e}")))?;
        self.secrets.set(KEY_REFRESH_TOKEN, refresh_token)
            .map_err(|e| Error::ToolExecution(format!("failed to store refresh token: {e}")))?;
        self.secrets.set(KEY_TOKEN_EXPIRES_AT, &expires_at.to_string())
            .map_err(|e| Error::ToolExecution(format!("failed to store expiry: {e}")))?;

        Ok(json!({
            "status": "authenticated",
            "message": auth_msg,
            "expires_in_seconds": expires_in,
        }))
    }

    // -----------------------------------------------------------------------
    // Action: search
    // -----------------------------------------------------------------------

    async fn action_search(&self, args: &Value) -> Result<Value> {
        let query = args["query"].as_str().unwrap_or("is:inbox");
        let max_results = args["max_results"].as_u64().unwrap_or(20).min(MAX_SEARCH_RESULTS as u64);
        let page_token = args["page_token"].as_str();

        let mut url = format!(
            "{GMAIL_API_BASE}/messages?q={}&maxResults={max_results}",
            urlencoded_value(query),
        );
        if let Some(pt) = page_token {
            url.push_str(&format!("&pageToken={pt}"));
        }

        let list: Value = self.gmail_request(reqwest::Method::GET, &url, None).await?;

        let messages = list["messages"].as_array();
        if messages.is_none() || messages.unwrap().is_empty() {
            return Ok(json!({
                "messages": [],
                "result_size_estimate": 0,
                "next_page_token": list.get("nextPageToken"),
            }));
        }

        let msg_ids: Vec<&str> = messages
            .unwrap()
            .iter()
            .filter_map(|m| m["id"].as_str())
            .collect();

        // Fetch metadata for each message
        let mut enriched = Vec::with_capacity(msg_ids.len());
        for id in &msg_ids {
            let meta_url = format!(
                "{GMAIL_API_BASE}/messages/{id}?format=metadata&metadataHeaders=Subject&metadataHeaders=From&metadataHeaders=Date"
            );
            match self.gmail_request(reqwest::Method::GET, &meta_url, None).await {
                Ok(meta) => {
                    let headers = extract_headers(&meta);
                    enriched.push(json!({
                        "id": id,
                        "thread_id": meta.get("threadId"),
                        "subject": headers.get("Subject").unwrap_or(&String::new()),
                        "from": headers.get("From").unwrap_or(&String::new()),
                        "date": headers.get("Date").unwrap_or(&String::new()),
                        "snippet": meta.get("snippet").and_then(|s| s.as_str()).unwrap_or(""),
                        "label_ids": meta.get("labelIds"),
                    }));
                }
                Err(e) => {
                    warn!("Failed to fetch metadata for message {id}: {e}");
                    enriched.push(json!({ "id": id, "error": e.to_string() }));
                }
            }
        }

        Ok(json!({
            "messages": enriched,
            "result_size_estimate": list.get("resultSizeEstimate"),
            "next_page_token": list.get("nextPageToken"),
        }))
    }

    // -----------------------------------------------------------------------
    // Action: read
    // -----------------------------------------------------------------------

    async fn action_read(&self, args: &Value) -> Result<Value> {
        let message_id = args["message_id"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing message_id".into()))?;

        let url = format!("{GMAIL_API_BASE}/messages/{message_id}?format=full");
        let msg: Value = self.gmail_request(reqwest::Method::GET, &url, None).await?;

        let headers = extract_headers(&msg);
        let body_text = extract_body(&msg);

        Ok(json!({
            "id": msg.get("id"),
            "thread_id": msg.get("threadId"),
            "label_ids": msg.get("labelIds"),
            "snippet": msg.get("snippet"),
            "subject": headers.get("Subject").unwrap_or(&String::new()),
            "from": headers.get("From").unwrap_or(&String::new()),
            "to": headers.get("To").unwrap_or(&String::new()),
            "cc": headers.get("Cc").unwrap_or(&String::new()),
            "date": headers.get("Date").unwrap_or(&String::new()),
            "message_id_header": headers.get("Message-ID").or_else(|| headers.get("Message-Id")).unwrap_or(&String::new()),
            "body": body_text,
        }))
    }

    // -----------------------------------------------------------------------
    // Action: send
    // -----------------------------------------------------------------------

    async fn action_send(&self, args: &Value) -> Result<Value> {
        let to = args["to"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'to' address".into()))?;
        let subject = args["subject"].as_str().unwrap_or("(no subject)");
        let body = args["body"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'body'".into()))?;
        let cc = args["cc"].as_str();
        let bcc = args["bcc"].as_str();
        let thread_id = args["thread_id"].as_str();
        let in_reply_to = args["in_reply_to"].as_str();

        let raw = build_rfc2822_message(to, subject, body, cc, bcc, in_reply_to);
        let encoded = base64_url_encode(raw.as_bytes());

        let mut payload = json!({ "raw": encoded });
        if let Some(tid) = thread_id {
            payload["threadId"] = json!(tid);
        }

        let url = format!("{GMAIL_API_BASE}/messages/send");
        let result = self.gmail_request(reqwest::Method::POST, &url, Some(&payload)).await?;

        Ok(json!({
            "status": "sent",
            "id": result.get("id"),
            "thread_id": result.get("threadId"),
            "label_ids": result.get("labelIds"),
        }))
    }

    // -----------------------------------------------------------------------
    // Action: draft
    // -----------------------------------------------------------------------

    async fn action_draft(&self, args: &Value) -> Result<Value> {
        let to = args["to"].as_str().unwrap_or("");
        let subject = args["subject"].as_str().unwrap_or("(no subject)");
        let body = args["body"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'body'".into()))?;
        let cc = args["cc"].as_str();
        let bcc = args["bcc"].as_str();
        let thread_id = args["thread_id"].as_str();
        let in_reply_to = args["in_reply_to"].as_str();

        let raw = build_rfc2822_message(to, subject, body, cc, bcc, in_reply_to);
        let encoded = base64_url_encode(raw.as_bytes());

        let mut message = json!({ "raw": encoded });
        if let Some(tid) = thread_id {
            message["threadId"] = json!(tid);
        }

        let payload = json!({ "message": message });
        let url = format!("{GMAIL_API_BASE}/drafts");
        let result = self.gmail_request(reqwest::Method::POST, &url, Some(&payload)).await?;

        Ok(json!({
            "status": "draft_created",
            "draft_id": result.get("id"),
            "message_id": result.get("message").and_then(|m| m.get("id")),
            "thread_id": result.get("message").and_then(|m| m.get("threadId")),
        }))
    }

    // -----------------------------------------------------------------------
    // Action: labels
    // -----------------------------------------------------------------------

    async fn action_labels(&self) -> Result<Value> {
        let url = format!("{GMAIL_API_BASE}/labels");
        let result = self.gmail_request(reqwest::Method::GET, &url, None).await?;

        let labels = result["labels"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|l| {
                        json!({
                            "id": l.get("id"),
                            "name": l.get("name"),
                            "type": l.get("type"),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(json!({ "labels": labels }))
    }

    // -----------------------------------------------------------------------
    // Action: modify
    // -----------------------------------------------------------------------

    async fn action_modify(&self, args: &Value) -> Result<Value> {
        let message_id = args["message_id"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing message_id".into()))?;

        let mut payload = json!({});

        if let Some(add) = args.get("add_labels") {
            payload["addLabelIds"] = add.clone();
        }
        if let Some(remove) = args.get("remove_labels") {
            payload["removeLabelIds"] = remove.clone();
        }

        let url = format!("{GMAIL_API_BASE}/messages/{message_id}/modify");
        let result = self.gmail_request(reqwest::Method::POST, &url, Some(&payload)).await?;

        Ok(json!({
            "status": "modified",
            "id": result.get("id"),
            "label_ids": result.get("labelIds"),
        }))
    }

    // -----------------------------------------------------------------------
    // Action: trash / untrash
    // -----------------------------------------------------------------------

    async fn action_trash(&self, args: &Value) -> Result<Value> {
        let message_id = args["message_id"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing message_id".into()))?;

        let url = format!("{GMAIL_API_BASE}/messages/{message_id}/trash");
        self.gmail_request(reqwest::Method::POST, &url, None).await?;

        Ok(json!({ "status": "trashed", "message_id": message_id }))
    }

    async fn action_untrash(&self, args: &Value) -> Result<Value> {
        let message_id = args["message_id"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing message_id".into()))?;

        let url = format!("{GMAIL_API_BASE}/messages/{message_id}/untrash");
        self.gmail_request(reqwest::Method::POST, &url, None).await?;

        Ok(json!({ "status": "untrashed", "message_id": message_id }))
    }
}

// ---------------------------------------------------------------------------
// Tool trait impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Tool for GmailTool {
    fn name(&self) -> &str {
        "gmail"
    }

    fn description(&self) -> &str {
        "Interact with Gmail via the REST API. Supports OAuth2 authentication, searching, \
         reading, sending, drafting, labelling, and trashing messages. Requires gmail_client_id \
         and gmail_client_secret credentials to be stored first."
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
                        "enum": ["auth", "search", "read", "send", "draft", "labels", "modify", "trash", "untrash"],
                        "description": "Action to perform"
                    },
                    "query": {
                        "type": "string",
                        "description": "Gmail search query (for 'search' action, e.g. 'is:unread from:boss@example.com')"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Max messages to return (search, default 20, max 50)"
                    },
                    "page_token": {
                        "type": "string",
                        "description": "Pagination token from a previous search"
                    },
                    "message_id": {
                        "type": "string",
                        "description": "Gmail message ID (for read/modify/trash/untrash)"
                    },
                    "to": {
                        "type": "string",
                        "description": "Recipient email address (for send/draft)"
                    },
                    "subject": {
                        "type": "string",
                        "description": "Email subject (for send/draft)"
                    },
                    "body": {
                        "type": "string",
                        "description": "Email body text (for send/draft)"
                    },
                    "cc": {
                        "type": "string",
                        "description": "CC recipients, comma-separated (for send/draft)"
                    },
                    "bcc": {
                        "type": "string",
                        "description": "BCC recipients, comma-separated (for send/draft)"
                    },
                    "thread_id": {
                        "type": "string",
                        "description": "Thread ID to reply within (for send/draft)"
                    },
                    "in_reply_to": {
                        "type": "string",
                        "description": "Message-ID header of the message being replied to (for send/draft)"
                    },
                    "add_labels": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Label IDs to add (for modify)"
                    },
                    "remove_labels": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Label IDs to remove (for modify)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing action".into()))?;

        match action {
            "auth" => self.action_auth().await,
            "search" => self.action_search(&args).await,
            "read" => self.action_read(&args).await,
            "send" => self.action_send(&args).await,
            "draft" => self.action_draft(&args).await,
            "labels" => self.action_labels().await,
            "modify" => self.action_modify(&args).await,
            "trash" => self.action_trash(&args).await,
            "untrash" => self.action_untrash(&args).await,
            other => Err(Error::ToolExecution(format!(
                "unknown action '{other}', expected one of: auth, search, read, send, draft, labels, modify, trash, untrash"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Module-private helpers
// ---------------------------------------------------------------------------

/// Parse a Gmail API response: check status, decode JSON.
async fn parse_gmail_response(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Error::ToolExecution(format!("failed to read response body: {e}")))?;

    if !status.is_success() {
        return Err(Error::ToolExecution(format!(
            "Gmail API error (HTTP {status}): {body}"
        )));
    }

    if body.is_empty() {
        return Ok(json!({}));
    }

    serde_json::from_str(&body)
        .map_err(|e| Error::ToolExecution(format!("failed to parse Gmail response: {e}")))
}

/// Accept the OAuth redirect on the loopback listener, extract the `code` parameter.
async fn accept_oauth_callback(listener: &tokio::net::TcpListener) -> Result<String> {
    let (mut stream, _) = listener
        .accept()
        .await
        .map_err(|e| Error::ToolExecution(format!("accept failed: {e}")))?;

    let mut buf = vec![0u8; 4096];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| Error::ToolExecution(format!("read failed: {e}")))?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // Extract the GET path
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| Error::ToolExecution("invalid HTTP request from OAuth callback".into()))?;

    // Parse query parameters
    let query_string = path.split('?').nth(1).unwrap_or("");
    let params: HashMap<String, String> =
        url::form_urlencoded::parse(query_string.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

    // Check for error
    if let Some(err) = params.get("error") {
        let html = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
             <html><body><h2>Authorization Failed</h2><p>{err}</p>\
             <p>You can close this window.</p></body></html>"
        );
        let _ = stream.write_all(html.as_bytes()).await;
        return Err(Error::ToolExecution(format!("OAuth error: {err}")));
    }

    let code = params
        .get("code")
        .ok_or_else(|| Error::ToolExecution("no 'code' parameter in OAuth callback".into()))?
        .clone();

    // Send success page
    let html = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
                <html><body><h2>Gmail Authorization Successful</h2>\
                <p>You can close this window and return to the agent.</p></body></html>";
    let _ = stream.write_all(html.as_bytes()).await;

    Ok(code)
}

/// Open a URL in the default browser.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "linux")]
    let cmd = "xdg-open";
    #[cfg(target_os = "windows")]
    let cmd = "start";

    if let Err(e) = std::process::Command::new(cmd).arg(url).spawn() {
        warn!("Failed to open browser: {e}");
    }
}

/// Extract headers from a Gmail message payload into a HashMap.
fn extract_headers(msg: &Value) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(headers) = msg
        .get("payload")
        .and_then(|p| p.get("headers"))
        .and_then(|h| h.as_array())
    {
        for h in headers {
            if let (Some(name), Some(value)) = (h["name"].as_str(), h["value"].as_str()) {
                map.insert(name.to_string(), value.to_string());
            }
        }
    }
    map
}

/// Extract the best text body from a Gmail message payload.
///
/// Prefers text/plain, falls back to text/html with tag stripping.
/// Handles multipart messages by recursing through MIME parts.
fn extract_body(msg: &Value) -> String {
    let payload = match msg.get("payload") {
        Some(p) => p,
        None => return String::new(),
    };

    // Try to find body in parts first (multipart messages)
    if let Some(text) = find_body_in_parts(payload, "text/plain") {
        return text;
    }
    if let Some(html) = find_body_in_parts(payload, "text/html") {
        return strip_html_tags(&html);
    }

    // Single-part message: body is directly on payload
    if let Some(data) = payload
        .get("body")
        .and_then(|b| b.get("data"))
        .and_then(|d| d.as_str())
    {
        if let Ok(decoded) = base64_url_decode(data) {
            let mime_type = payload
                .get("mimeType")
                .and_then(|m| m.as_str())
                .unwrap_or("");
            if mime_type.contains("html") {
                return strip_html_tags(&decoded);
            }
            return decoded;
        }
    }

    String::new()
}

/// Recursively search MIME parts for a body with the given MIME type.
fn find_body_in_parts(part: &Value, target_mime: &str) -> Option<String> {
    let mime_type = part.get("mimeType").and_then(|m| m.as_str()).unwrap_or("");

    if mime_type == target_mime {
        if let Some(data) = part.get("body").and_then(|b| b.get("data")).and_then(|d| d.as_str()) {
            if let Ok(decoded) = base64_url_decode(data) {
                return Some(decoded);
            }
        }
    }

    // Recurse into sub-parts
    if let Some(parts) = part.get("parts").and_then(|p| p.as_array()) {
        for p in parts {
            if let Some(found) = find_body_in_parts(p, target_mime) {
                return Some(found);
            }
        }
    }

    None
}

/// Build a minimal RFC 2822 message.
fn build_rfc2822_message(
    to: &str,
    subject: &str,
    body: &str,
    cc: Option<&str>,
    bcc: Option<&str>,
    in_reply_to: Option<&str>,
) -> String {
    let mut msg = String::with_capacity(256 + body.len());
    msg.push_str(&format!("To: {to}\r\n"));
    if let Some(cc_val) = cc {
        msg.push_str(&format!("Cc: {cc_val}\r\n"));
    }
    if let Some(bcc_val) = bcc {
        msg.push_str(&format!("Bcc: {bcc_val}\r\n"));
    }
    msg.push_str(&format!("Subject: {subject}\r\n"));
    if let Some(reply_to) = in_reply_to {
        msg.push_str(&format!("In-Reply-To: {reply_to}\r\n"));
        msg.push_str(&format!("References: {reply_to}\r\n"));
    }
    msg.push_str("Content-Type: text/plain; charset=UTF-8\r\n");
    msg.push_str("\r\n");
    msg.push_str(body);
    msg
}

/// Base64 URL-safe encode (no padding).
fn base64_url_encode(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

/// Base64 URL-safe decode.
fn base64_url_decode(data: &str) -> std::result::Result<String, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(data)
        .map_err(|e| format!("base64 decode error: {e}"))?;
    String::from_utf8(bytes).map_err(|e| format!("UTF-8 decode error: {e}"))
}

/// Strip HTML tags and decode common entities.
fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;

    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }

    // Decode common HTML entities
    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// URL-encode a set of key-value pairs.
fn urlencoded(params: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params)
        .finish()
}

/// URL-encode a single value.
fn urlencoded_value(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
