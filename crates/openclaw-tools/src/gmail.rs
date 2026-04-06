use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Error, Result, Tool};
use openclaw_store::SecretStore;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const IMAP_HOST: &str = "imap.gmail.com";
const IMAP_PORT: u16 = 993;
const SMTP_HOST: &str = "smtp.gmail.com";

// SecretStore keys
const KEY_EMAIL: &str = "gmail_email";
const KEY_APP_PASSWORD: &str = "gmail_app_password";

/// Maximum messages to return from a search.
const MAX_SEARCH_RESULTS: usize = 50;

// ---------------------------------------------------------------------------
// Tool struct
// ---------------------------------------------------------------------------

pub struct GmailTool {
    secrets: SecretStore,
}

impl GmailTool {
    pub fn new(secrets: SecretStore) -> Self {
        Self { secrets }
    }

    /// Get email and app password from the credential store.
    fn get_credentials(&self) -> Result<(String, String)> {
        let email = self.secrets.get(KEY_EMAIL).map_err(|_| {
            Error::ToolExecution(
                "gmail_email not found. Store it with: \
                 credential_write(action='set', name='gmail_email', value='you@gmail.com')"
                    .into(),
            )
        })?;
        let password = self.secrets.get(KEY_APP_PASSWORD).map_err(|_| {
            Error::ToolExecution(
                "gmail_app_password not found. Store it with: \
                 credential_write(action='set', name='gmail_app_password', value='YOUR_APP_PASSWORD')"
                    .into(),
            )
        })?;
        Ok((email, password))
    }

    /// Connect to Gmail IMAP with TLS.
    fn connect_imap(&self) -> Result<imap::Session<native_tls::TlsStream<std::net::TcpStream>>> {
        let (email, password) = self.get_credentials()?;
        let tls = native_tls::TlsConnector::builder()
            .build()
            .map_err(|e| Error::ToolExecution(format!("TLS setup failed: {e}")))?;
        let client = imap::connect((IMAP_HOST, IMAP_PORT), IMAP_HOST, &tls)
            .map_err(|e| Error::ToolExecution(format!("IMAP connect failed: {e}")))?;
        let session = client
            .login(&email, &password)
            .map_err(|e| Error::ToolExecution(format!("IMAP login failed: {}", e.0)))?;
        Ok(session)
    }

    // -----------------------------------------------------------------------
    // Action: setup
    // -----------------------------------------------------------------------

    async fn action_setup(&self, args: &Value) -> Result<Value> {
        let email = args["email"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'email' parameter".into()))?;
        let app_password = args["app_password"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'app_password' parameter".into()))?;

        self.secrets
            .set(KEY_EMAIL, email)
            .map_err(|e| Error::ToolExecution(format!("failed to store email: {e}")))?;
        self.secrets
            .set(KEY_APP_PASSWORD, app_password)
            .map_err(|e| Error::ToolExecution(format!("failed to store app password: {e}")))?;

        // Verify credentials by connecting to IMAP.
        let mut session = self.connect_imap()?;
        let _ = session.logout();

        Ok(json!({
            "status": "authenticated",
            "email": email,
            "message": "Gmail credentials stored and verified via IMAP."
        }))
    }

    // -----------------------------------------------------------------------
    // Action: search
    // -----------------------------------------------------------------------

    async fn action_search(&self, args: &Value) -> Result<Value> {
        let query = args["query"].as_str().unwrap_or("ALL");
        let max_results = args["max_results"]
            .as_u64()
            .unwrap_or(20)
            .min(MAX_SEARCH_RESULTS as u64) as usize;
        let mailbox = args["mailbox"].as_str().unwrap_or("INBOX");

        // IMAP is blocking, run in spawn_blocking.
        let secrets = self.secrets.clone();
        let query = query.to_string();
        let mailbox = mailbox.to_string();

        tokio::task::spawn_blocking(move || {
            let (email, password) = get_creds(&secrets)?;
            let mut session = connect_imap_blocking(&email, &password)?;

            session
                .select(&mailbox)
                .map_err(|e| Error::ToolExecution(format!("select {mailbox} failed: {e}")))?;

            // Use Gmail's X-GM-RAW extension for full search syntax,
            // falling back to standard IMAP SEARCH.
            let uids = session
                .uid_search(&format!("X-GM-RAW \"{query}\""))
                .or_else(|_| session.uid_search(&query))
                .map_err(|e| Error::ToolExecution(format!("search failed: {e}")))?;

            // Take the most recent UIDs (highest numbers = newest).
            let mut uid_list: Vec<u32> = uids.into_iter().collect();
            uid_list.sort_unstable_by(|a, b| b.cmp(a));
            uid_list.truncate(max_results);

            if uid_list.is_empty() {
                let _ = session.logout();
                return Ok(json!({ "messages": [], "count": 0 }));
            }

            let uid_set = uid_list
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>()
                .join(",");

            let fetches = session
                .uid_fetch(&uid_set, "(UID ENVELOPE FLAGS)")
                .map_err(|e| Error::ToolExecution(format!("fetch failed: {e}")))?;

            let mut messages = Vec::new();
            for fetch in fetches.iter() {
                let envelope = match fetch.envelope() {
                    Some(e) => e,
                    None => continue,
                };

                let subject = envelope
                    .subject
                    .as_ref()
                    .map(|s| decode_imap_string(s))
                    .unwrap_or_default();
                let from = envelope
                    .from
                    .as_ref()
                    .and_then(|addrs| addrs.first())
                    .map(|a| format_address(a))
                    .unwrap_or_default();
                let date = envelope
                    .date
                    .as_ref()
                    .map(|d| decode_imap_string(d))
                    .unwrap_or_default();

                let flags: Vec<String> = fetch
                    .flags()
                    .iter()
                    .map(|f| format!("{f:?}"))
                    .collect();

                messages.push(json!({
                    "uid": fetch.uid.unwrap_or(0),
                    "subject": subject,
                    "from": from,
                    "date": date,
                    "flags": flags,
                }));
            }

            let _ = session.logout();

            Ok(json!({
                "messages": messages,
                "count": messages.len(),
            }))
        })
        .await
        .map_err(|e| Error::ToolExecution(format!("task join failed: {e}")))?
    }

    // -----------------------------------------------------------------------
    // Action: read
    // -----------------------------------------------------------------------

    async fn action_read(&self, args: &Value) -> Result<Value> {
        let uid: u32 = args["uid"]
            .as_u64()
            .ok_or_else(|| Error::ToolExecution("missing 'uid' (use search to find UIDs)".into()))?
            as u32;
        let mailbox = args["mailbox"].as_str().unwrap_or("INBOX");

        let secrets = self.secrets.clone();
        let mailbox = mailbox.to_string();

        tokio::task::spawn_blocking(move || {
            let (email, password) = get_creds(&secrets)?;
            let mut session = connect_imap_blocking(&email, &password)?;

            session
                .select(&mailbox)
                .map_err(|e| Error::ToolExecution(format!("select {mailbox} failed: {e}")))?;

            let fetches = session
                .uid_fetch(uid.to_string(), "(UID RFC822 FLAGS ENVELOPE)")
                .map_err(|e| Error::ToolExecution(format!("fetch uid {uid} failed: {e}")))?;

            let fetch = fetches
                .iter()
                .next()
                .ok_or_else(|| Error::ToolExecution(format!("message uid {uid} not found")))?;

            let raw_body = fetch.body().unwrap_or_default();
            let parsed = mail_parser::MessageParser::default()
                .parse(raw_body)
                .ok_or_else(|| Error::ToolExecution("failed to parse email".into()))?;

            let subject = parsed.subject().unwrap_or("").to_string();
            let from = parsed
                .from()
                .and_then(|a| a.first())
                .map(|a| {
                    a.name()
                        .map(|n| format!("{n} <{}>", a.address().unwrap_or("")))
                        .unwrap_or_else(|| a.address().unwrap_or("").to_string())
                })
                .unwrap_or_default();
            let to = parsed
                .to()
                .and_then(|a| a.first())
                .and_then(|a| a.address())
                .unwrap_or("")
                .to_string();
            let date = parsed.date().map(|d| d.to_string()).unwrap_or_default();
            let body_text = parsed
                .body_text(0)
                .unwrap_or_else(|| {
                    parsed
                        .body_html(0)
                        .map(|h| strip_html_tags(&h).into())
                        .unwrap_or_default()
                })
                .to_string();

            // Truncate very long bodies to avoid blowing up context.
            let body_text = if body_text.len() > 8000 {
                format!("{}…\n[truncated, {} chars total]", &body_text[..body_text.floor_char_boundary(8000)], body_text.len())
            } else {
                body_text
            };

            let flags: Vec<String> = fetch.flags().iter().map(|f| format!("{f:?}")).collect();

            let _ = session.logout();

            Ok(json!({
                "uid": uid,
                "subject": subject,
                "from": from,
                "to": to,
                "date": date,
                "body": body_text,
                "flags": flags,
            }))
        })
        .await
        .map_err(|e| Error::ToolExecution(format!("task join failed: {e}")))?
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
        let in_reply_to = args["in_reply_to"].as_str();

        let (email, password) = self.get_credentials()?;

        let mut message_builder = lettre::message::Message::builder()
            .from(email.parse().map_err(|e| Error::ToolExecution(format!("invalid from address: {e}")))?)
            .to(to.parse().map_err(|e| Error::ToolExecution(format!("invalid to address: {e}")))?)
            .subject(subject);

        if let Some(cc_addr) = cc {
            message_builder = message_builder.cc(
                cc_addr.parse().map_err(|e| Error::ToolExecution(format!("invalid cc address: {e}")))?
            );
        }
        if let Some(reply_id) = in_reply_to {
            message_builder = message_builder.in_reply_to(reply_id.to_string());
        }

        let email_msg = message_builder
            .body(body.to_string())
            .map_err(|e| Error::ToolExecution(format!("failed to build email: {e}")))?;

        let creds = lettre::transport::smtp::authentication::Credentials::new(
            email.clone(),
            password,
        );

        // SMTP is blocking in lettre's sync transport; use async transport.
        use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

        let mailer = AsyncSmtpTransport::<Tokio1Executor>::relay(SMTP_HOST)
            .map_err(|e| Error::ToolExecution(format!("SMTP relay setup failed: {e}")))?
            .credentials(creds)
            .build();

        mailer
            .send(email_msg)
            .await
            .map_err(|e| Error::ToolExecution(format!("send failed: {e}")))?;

        Ok(json!({
            "status": "sent",
            "to": to,
            "subject": subject,
        }))
    }

    // -----------------------------------------------------------------------
    // Action: labels (list mailboxes)
    // -----------------------------------------------------------------------

    async fn action_labels(&self) -> Result<Value> {
        let secrets = self.secrets.clone();

        tokio::task::spawn_blocking(move || {
            let (email, password) = get_creds(&secrets)?;
            let mut session = connect_imap_blocking(&email, &password)?;

            let mailboxes = session
                .list(Some(""), Some("*"))
                .map_err(|e| Error::ToolExecution(format!("list mailboxes failed: {e}")))?;

            let labels: Vec<Value> = mailboxes
                .iter()
                .map(|mb| {
                    json!({
                        "name": mb.name(),
                        "delimiter": mb.delimiter().map(|c| c.to_string()),
                    })
                })
                .collect();

            let _ = session.logout();

            Ok(json!({ "labels": labels }))
        })
        .await
        .map_err(|e| Error::ToolExecution(format!("task join failed: {e}")))?
    }

    // -----------------------------------------------------------------------
    // Action: move (move message to a different mailbox/label)
    // -----------------------------------------------------------------------

    async fn action_move(&self, args: &Value) -> Result<Value> {
        let uid: u32 = args["uid"]
            .as_u64()
            .ok_or_else(|| Error::ToolExecution("missing 'uid'".into()))?
            as u32;
        let from_mailbox = args["mailbox"].as_str().unwrap_or("INBOX");
        let to_mailbox = args["to_mailbox"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'to_mailbox'".into()))?;

        let secrets = self.secrets.clone();
        let from_mailbox = from_mailbox.to_string();
        let to_mailbox = to_mailbox.to_string();

        tokio::task::spawn_blocking(move || {
            let (email, password) = get_creds(&secrets)?;
            let mut session = connect_imap_blocking(&email, &password)?;

            session
                .select(&from_mailbox)
                .map_err(|e| Error::ToolExecution(format!("select {from_mailbox} failed: {e}")))?;

            session
                .uid_mv(uid.to_string(), &to_mailbox)
                .map_err(|e| Error::ToolExecution(format!("move failed: {e}")))?;

            let _ = session.logout();

            Ok(json!({
                "status": "moved",
                "uid": uid,
                "from": from_mailbox,
                "to": to_mailbox,
            }))
        })
        .await
        .map_err(|e| Error::ToolExecution(format!("task join failed: {e}")))?
    }

    // -----------------------------------------------------------------------
    // Action: trash
    // -----------------------------------------------------------------------

    async fn action_trash(&self, args: &Value) -> Result<Value> {
        let uid: u32 = args["uid"]
            .as_u64()
            .ok_or_else(|| Error::ToolExecution("missing 'uid'".into()))?
            as u32;
        let mailbox = args["mailbox"].as_str().unwrap_or("INBOX");

        let secrets = self.secrets.clone();
        let mailbox = mailbox.to_string();

        tokio::task::spawn_blocking(move || {
            let (email, password) = get_creds(&secrets)?;
            let mut session = connect_imap_blocking(&email, &password)?;

            session
                .select(&mailbox)
                .map_err(|e| Error::ToolExecution(format!("select {mailbox} failed: {e}")))?;

            session
                .uid_mv(uid.to_string(), "[Gmail]/Trash")
                .map_err(|e| Error::ToolExecution(format!("trash failed: {e}")))?;

            let _ = session.logout();

            Ok(json!({ "status": "trashed", "uid": uid }))
        })
        .await
        .map_err(|e| Error::ToolExecution(format!("task join failed: {e}")))?
    }

    // -----------------------------------------------------------------------
    // Action: mark_read / mark_unread
    // -----------------------------------------------------------------------

    async fn action_mark(&self, args: &Value, read: bool) -> Result<Value> {
        let uid: u32 = args["uid"]
            .as_u64()
            .ok_or_else(|| Error::ToolExecution("missing 'uid'".into()))?
            as u32;
        let mailbox = args["mailbox"].as_str().unwrap_or("INBOX");

        let secrets = self.secrets.clone();
        let mailbox = mailbox.to_string();

        tokio::task::spawn_blocking(move || {
            let (email, password) = get_creds(&secrets)?;
            let mut session = connect_imap_blocking(&email, &password)?;

            session
                .select(&mailbox)
                .map_err(|e| Error::ToolExecution(format!("select {mailbox} failed: {e}")))?;

            if read {
                session
                    .uid_store(uid.to_string(), "+FLAGS (\\Seen)")
                    .map_err(|e| Error::ToolExecution(format!("mark read failed: {e}")))?;
            } else {
                session
                    .uid_store(uid.to_string(), "-FLAGS (\\Seen)")
                    .map_err(|e| Error::ToolExecution(format!("mark unread failed: {e}")))?;
            }

            let _ = session.logout();

            let status = if read { "marked_read" } else { "marked_unread" };
            Ok(json!({ "status": status, "uid": uid }))
        })
        .await
        .map_err(|e| Error::ToolExecution(format!("task join failed: {e}")))?
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
        "Interact with Gmail via IMAP/SMTP using an app password. Supports searching, \
         reading, sending, listing labels, moving messages, marking read/unread, and trashing. \
         Requires gmail_email and gmail_app_password credentials to be stored first."
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
                        "enum": ["setup", "search", "read", "send", "labels", "move", "trash", "mark_read", "mark_unread"],
                        "description": "Action to perform"
                    },
                    "email": {
                        "type": "string",
                        "description": "Gmail address (for 'setup' action)"
                    },
                    "app_password": {
                        "type": "string",
                        "description": "Gmail app password (for 'setup' action)"
                    },
                    "query": {
                        "type": "string",
                        "description": "Search query (for 'search' action). Supports Gmail search syntax e.g. 'is:unread from:boss@example.com'"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Max messages to return (search, default 20, max 50)"
                    },
                    "mailbox": {
                        "type": "string",
                        "description": "Mailbox/label to operate on (default 'INBOX')"
                    },
                    "uid": {
                        "type": "integer",
                        "description": "Message UID (for read/move/trash/mark_read/mark_unread)"
                    },
                    "to": {
                        "type": "string",
                        "description": "Recipient email address (for send)"
                    },
                    "subject": {
                        "type": "string",
                        "description": "Email subject (for send)"
                    },
                    "body": {
                        "type": "string",
                        "description": "Email body text (for send)"
                    },
                    "cc": {
                        "type": "string",
                        "description": "CC recipient (for send)"
                    },
                    "in_reply_to": {
                        "type": "string",
                        "description": "Message-ID to reply to (for send)"
                    },
                    "to_mailbox": {
                        "type": "string",
                        "description": "Destination mailbox/label (for 'move' action)"
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
            "setup" => self.action_setup(&args).await,
            "search" => self.action_search(&args).await,
            "read" => self.action_read(&args).await,
            "send" => self.action_send(&args).await,
            "labels" => self.action_labels().await,
            "move" => self.action_move(&args).await,
            "trash" => self.action_trash(&args).await,
            "mark_read" => self.action_mark(&args, true).await,
            "mark_unread" => self.action_mark(&args, false).await,
            other => Err(Error::ToolExecution(format!(
                "unknown action '{other}', expected one of: setup, search, read, send, labels, move, trash, mark_read, mark_unread"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Module-private helpers
// ---------------------------------------------------------------------------

/// Get credentials from SecretStore (for use in spawn_blocking closures).
fn get_creds(secrets: &SecretStore) -> Result<(String, String)> {
    let email = secrets.get(KEY_EMAIL).map_err(|_| {
        Error::ToolExecution("gmail_email not configured. Run gmail(action='setup') first.".into())
    })?;
    let password = secrets.get(KEY_APP_PASSWORD).map_err(|_| {
        Error::ToolExecution(
            "gmail_app_password not configured. Run gmail(action='setup') first.".into(),
        )
    })?;
    Ok((email, password))
}

/// Connect to Gmail IMAP (blocking, for use in spawn_blocking).
fn connect_imap_blocking(
    email: &str,
    password: &str,
) -> Result<imap::Session<native_tls::TlsStream<std::net::TcpStream>>> {
    let tls = native_tls::TlsConnector::builder()
        .build()
        .map_err(|e| Error::ToolExecution(format!("TLS setup failed: {e}")))?;
    let client = imap::connect((IMAP_HOST, IMAP_PORT), IMAP_HOST, &tls)
        .map_err(|e| Error::ToolExecution(format!("IMAP connect failed: {e}")))?;
    let session = client
        .login(email, password)
        .map_err(|e| Error::ToolExecution(format!("IMAP login failed: {}", e.0)))?;
    Ok(session)
}

/// Decode an IMAP string (may be UTF-7 encoded).
fn decode_imap_string(s: &[u8]) -> String {
    String::from_utf8_lossy(s).to_string()
}

/// Format an IMAP address into a readable string.
fn format_address(addr: &imap_proto::types::Address) -> String {
    let name = addr.name.as_ref().map(|n| decode_imap_string(n));
    let mailbox = addr.mailbox.as_ref().map(|m| decode_imap_string(m));
    let host = addr.host.as_ref().map(|h| decode_imap_string(h));

    let email_addr = match (mailbox, host) {
        (Some(m), Some(h)) => format!("{m}@{h}"),
        (Some(m), None) => m,
        _ => String::new(),
    };

    match name {
        Some(ref n) if !n.is_empty() => format!("{n} <{email_addr}>"),
        _ => email_addr,
    }
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

    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}
