use async_trait::async_trait;
use futures::TryStreamExt;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, SandboxRequirements, Tool};
use rustykrab_store::SecretStore;
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

type ImapSession = async_imap::Session<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;

// ---------------------------------------------------------------------------
// Cached IMAP session
// ---------------------------------------------------------------------------

/// An authenticated IMAP session cached across tool calls, plus the state
/// needed to reuse it safely. Establishing a Gmail IMAP session costs a full
/// TCP + TLS + LOGIN round trip (300ms–1s) and Gmail throttles rapid
/// re-authentication, so the session is kept open between operations.
struct CachedSession {
    session: ImapSession,
    /// Account the session is authenticated as; a credential change
    /// invalidates the cache.
    email: String,
    /// Mailbox currently SELECTed, so repeat operations on the same mailbox
    /// skip the redundant SELECT round trip.
    selected_mailbox: Option<String>,
}

impl std::ops::Deref for CachedSession {
    type Target = ImapSession;
    fn deref(&self) -> &ImapSession {
        &self.session
    }
}

impl std::ops::DerefMut for CachedSession {
    fn deref_mut(&mut self) -> &mut ImapSession {
        &mut self.session
    }
}

/// Return a live, authenticated session for `email`, reusing the cached one
/// when it belongs to the same account and still answers a NOOP; otherwise
/// (re)connect lazily.
async fn ensure_session<'a>(
    cache: &'a mut Option<CachedSession>,
    email: &str,
    password: &str,
) -> Result<&'a mut CachedSession> {
    let reusable = match cache.as_mut() {
        // NOOP doubles as a liveness probe and lets the server deliver
        // pending mailbox updates (RFC 3501 §6.1.2).
        Some(cached) if cached.email == email => cached.session.noop().await.is_ok(),
        _ => false,
    };
    if !reusable {
        // Drop rather than LOGOUT: the old connection is dead or belongs to
        // another account, and a LOGOUT on a broken socket can stall.
        *cache = None;
        let session = connect_imap(email, password).await?;
        *cache = Some(CachedSession {
            session,
            email: email.to_string(),
            selected_mailbox: None,
        });
    }
    Ok(cache.as_mut().expect("session was just ensured"))
}

/// SELECT `mailbox` unless it is already the selected mailbox of this session.
async fn select_mailbox(cached: &mut CachedSession, mailbox: &str) -> Result<()> {
    if cached.selected_mailbox.as_deref() == Some(mailbox) {
        return Ok(());
    }
    // Clear first so a failed SELECT never leaves a stale mailbox recorded
    // (RFC 3501: a failed SELECT leaves no mailbox selected).
    cached.selected_mailbox = None;
    cached
        .session
        .select(mailbox)
        .await
        .map_err(|e| Error::ToolExecution(format!("select {mailbox} failed: {e}").into()))?;
    cached.selected_mailbox = Some(mailbox.to_string());
    Ok(())
}

// ---------------------------------------------------------------------------
// Tool struct
// ---------------------------------------------------------------------------

pub struct GmailTool {
    secrets: SecretStore,
    /// Cached IMAP session, reused across calls. The lock is held for the
    /// duration of an operation — IMAP is stateful, so operations on one
    /// connection must not interleave.
    imap: tokio::sync::Mutex<Option<CachedSession>>,
}

impl GmailTool {
    pub fn new(secrets: SecretStore) -> Self {
        Self {
            secrets,
            imap: tokio::sync::Mutex::new(None),
        }
    }

    /// Get email and app password from the credential store.
    async fn get_credentials(&self) -> Result<(String, String)> {
        let email = self.secrets.get(KEY_EMAIL).await.map_err(|e| {
            Error::ToolExecution(
                format!(
                    "gmail_email not available: {e}. Store it with: \
                     credential_write(action='set', name='gmail_email', value='you@gmail.com'). \
                     If you already stored it, the master encryption key may have changed \
                     (set RUSTYKRAB_MASTER_KEY for persistence across restarts)."
                )
                .into(),
            )
        })?;
        let password = self.secrets.get(KEY_APP_PASSWORD).await.map_err(|e| {
            Error::ToolExecution(
                format!(
                    "gmail_app_password not available: {e}. Store it with: \
                     credential_write(action='set', name='gmail_app_password', \
                     value='YOUR_APP_PASSWORD'). If you already stored it, the master \
                     encryption key may have changed (set RUSTYKRAB_MASTER_KEY for \
                     persistence across restarts)."
                )
                .into(),
            )
        })?;
        Ok((email, password))
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
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to store email: {e}").into()))?;
        self.secrets
            .set(KEY_APP_PASSWORD, app_password)
            .await
            .map_err(|e| {
                Error::ToolExecution(format!("failed to store app password: {e}").into())
            })?;

        // Verify the new credentials by connecting to IMAP, dropping any
        // session cached for the previous account and seeding the cache with
        // the freshly verified one.
        let mut guard = self.imap.lock().await;
        *guard = None;
        let session = connect_imap(email, app_password).await?;
        *guard = Some(CachedSession {
            session,
            email: email.to_string(),
            selected_mailbox: None,
        });

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

        let (email, password) = self.get_credentials().await?;
        let mut guard = self.imap.lock().await;
        let session = ensure_session(&mut guard, &email, &password).await?;
        select_mailbox(session, mailbox).await?;

        // Use Gmail's X-GM-RAW extension for full search syntax,
        // falling back to standard IMAP SEARCH.
        // Strip CRLF sequences to prevent IMAP command injection, then
        // escape inner double quotes so the IMAP command parses correctly
        // (e.g. query `from:"foo@bar.com"` becomes `X-GM-RAW "from:\"foo@bar.com\""`).
        let sanitized_query = query.replace(['\r', '\n', '\0'], "");
        let escaped_query = sanitized_query.replace('\\', "\\\\").replace('"', "\\\"");
        let uids = match session
            .uid_search(format!("X-GM-RAW \"{escaped_query}\""))
            .await
        {
            Ok(set) => set,
            Err(_) => session
                .uid_search(query)
                .await
                .map_err(|e| Error::ToolExecution(format!("search failed: {e}").into()))?,
        };

        // Take the most recent UIDs (highest numbers = newest).
        let mut uid_list: Vec<u32> = uids.into_iter().collect();
        uid_list.sort_unstable_by(|a, b| b.cmp(a));
        uid_list.truncate(max_results);

        if uid_list.is_empty() {
            return Ok(json!({ "messages": [], "count": 0 }));
        }

        let uid_set = uid_list
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let fetches: Vec<async_imap::types::Fetch> = session
            .uid_fetch(&uid_set, "(UID ENVELOPE FLAGS)")
            .await
            .map_err(|e| Error::ToolExecution(format!("fetch failed: {e}").into()))?
            .try_collect()
            .await
            .map_err(|e| Error::ToolExecution(format!("fetch stream failed: {e}").into()))?;

        let mut messages = Vec::new();
        for fetch in &fetches {
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
                .map(format_address)
                .unwrap_or_default();
            let date = envelope
                .date
                .as_ref()
                .map(|d| decode_imap_string(d))
                .unwrap_or_default();

            let flags: Vec<String> = fetch.flags().map(|f| format!("{f:?}")).collect();

            messages.push(json!({
                "uid": fetch.uid.unwrap_or(0),
                "subject": subject,
                "from": from,
                "date": date,
                "flags": flags,
            }));
        }

        Ok(json!({
            "messages": messages,
            "count": messages.len(),
        }))
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

        let (email, password) = self.get_credentials().await?;
        let mut guard = self.imap.lock().await;
        let session = ensure_session(&mut guard, &email, &password).await?;
        select_mailbox(session, mailbox).await?;

        let fetches: Vec<async_imap::types::Fetch> = session
            .uid_fetch(uid.to_string(), "(UID RFC822 FLAGS ENVELOPE)")
            .await
            .map_err(|e| Error::ToolExecution(format!("fetch uid {uid} failed: {e}").into()))?
            .try_collect()
            .await
            .map_err(|e| Error::ToolExecution(format!("fetch stream failed: {e}").into()))?;

        let fetch = fetches
            .first()
            .ok_or_else(|| Error::ToolExecution(format!("message uid {uid} not found").into()))?;

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
        let to: Vec<String> = parsed
            .to()
            .map(|addrs| {
                addrs
                    .iter()
                    .map(|a| {
                        a.name()
                            .map(|n| format!("{n} <{}>", a.address().unwrap_or("")))
                            .unwrap_or_else(|| a.address().unwrap_or("").to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let cc: Vec<String> = parsed
            .cc()
            .map(|addrs| {
                addrs
                    .iter()
                    .map(|a| {
                        a.name()
                            .map(|n| format!("{n} <{}>", a.address().unwrap_or("")))
                            .unwrap_or_else(|| a.address().unwrap_or("").to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let date = parsed.date().map(|d| d.to_string()).unwrap_or_default();
        let message_id = parsed.message_id().unwrap_or("").to_string();
        let reply_to = parsed
            .reply_to()
            .and_then(|a| a.first())
            .and_then(|a| a.address())
            .unwrap_or("")
            .to_string();
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
            format!(
                "{}…\n[truncated, {} chars total]",
                &body_text[..body_text.floor_char_boundary(8000)],
                body_text.len()
            )
        } else {
            body_text
        };

        // Attachment metadata
        use mail_parser::MimeHeaders;
        let attachments: Vec<Value> = parsed
            .attachments()
            .enumerate()
            .map(|(i, part)| {
                let filename = part
                    .content_disposition()
                    .and_then(|cd| cd.attribute("filename"))
                    .or_else(|| part.content_type().and_then(|ct| ct.attribute("name")))
                    .unwrap_or("unnamed")
                    .to_string();
                let content_type = part
                    .content_type()
                    .map(|ct| {
                        let sub = ct.subtype().unwrap_or("octet-stream");
                        format!("{}/{sub}", ct.ctype())
                    })
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                let size = part.contents().len();
                json!({
                    "index": i,
                    "filename": filename,
                    "content_type": content_type,
                    "size_bytes": size,
                })
            })
            .collect();

        let flags: Vec<String> = fetch.flags().map(|f| format!("{f:?}")).collect();

        let mut result = json!({
            "uid": uid,
            "subject": subject,
            "from": from,
            "to": to,
            "date": date,
            "body": body_text,
            "flags": flags,
            "message_id": message_id,
        });
        if !cc.is_empty() {
            result["cc"] = json!(cc);
        }
        if !reply_to.is_empty() {
            result["reply_to"] = json!(reply_to);
        }
        if !attachments.is_empty() {
            let count = attachments.len();
            result["attachments"] = json!(attachments);
            result["attachment_count"] = json!(count);
        }
        Ok(result)
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

        let (email, password) = self.get_credentials().await?;

        let mut message_builder =
            lettre::message::Message::builder()
                .from(email.parse().map_err(|e| {
                    Error::ToolExecution(format!("invalid from address: {e}").into())
                })?)
                .to(to
                    .parse()
                    .map_err(|e| Error::ToolExecution(format!("invalid to address: {e}").into()))?)
                .subject(subject);

        if let Some(cc_addr) = cc {
            message_builder = message_builder.cc(cc_addr
                .parse()
                .map_err(|e| Error::ToolExecution(format!("invalid cc address: {e}").into()))?);
        }
        if let Some(reply_id) = in_reply_to {
            message_builder = message_builder.in_reply_to(reply_id.to_string());
        }

        let email_msg = message_builder
            .body(body.to_string())
            .map_err(|e| Error::ToolExecution(format!("failed to build email: {e}").into()))?;

        let creds =
            lettre::transport::smtp::authentication::Credentials::new(email.clone(), password);

        // SMTP is blocking in lettre's sync transport; use async transport.
        use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

        let mailer = AsyncSmtpTransport::<Tokio1Executor>::relay(SMTP_HOST)
            .map_err(|e| Error::ToolExecution(format!("SMTP relay setup failed: {e}").into()))?
            .credentials(creds)
            .build();

        mailer
            .send(email_msg)
            .await
            .map_err(|e| Error::ToolExecution(format!("send failed: {e}").into()))?;

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
        let (email, password) = self.get_credentials().await?;
        let mut guard = self.imap.lock().await;
        let session = ensure_session(&mut guard, &email, &password).await?;

        let names: Vec<async_imap::types::Name> = session
            .list(Some(""), Some("*"))
            .await
            .map_err(|e| Error::ToolExecution(format!("list mailboxes failed: {e}").into()))?
            .try_collect()
            .await
            .map_err(|e| Error::ToolExecution(format!("list stream failed: {e}").into()))?;

        let labels: Vec<Value> = names
            .iter()
            .map(|mb| {
                json!({
                    "name": mb.name(),
                    "delimiter": mb.delimiter(),
                })
            })
            .collect();

        Ok(json!({ "labels": labels }))
    }

    // -----------------------------------------------------------------------
    // Action: move (move message to a different mailbox/label)
    // -----------------------------------------------------------------------

    async fn action_move(&self, args: &Value) -> Result<Value> {
        let uid: u32 = args["uid"]
            .as_u64()
            .ok_or_else(|| Error::ToolExecution("missing 'uid'".into()))?
            as u32;
        let from_mailbox = args["mailbox"].as_str().unwrap_or("INBOX").to_string();
        let to_mailbox = args["to_mailbox"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'to_mailbox'".into()))?
            .to_string();

        let (email, password) = self.get_credentials().await?;
        let mut guard = self.imap.lock().await;
        let session = ensure_session(&mut guard, &email, &password).await?;
        select_mailbox(session, &from_mailbox).await?;

        session
            .uid_mv(uid.to_string(), &to_mailbox)
            .await
            .map_err(|e| Error::ToolExecution(format!("move failed: {e}").into()))?;

        Ok(json!({
            "status": "moved",
            "uid": uid,
            "from": from_mailbox,
            "to": to_mailbox,
        }))
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

        let (email, password) = self.get_credentials().await?;
        let mut guard = self.imap.lock().await;
        let session = ensure_session(&mut guard, &email, &password).await?;
        select_mailbox(session, mailbox).await?;

        session
            .uid_mv(uid.to_string(), "[Gmail]/Trash")
            .await
            .map_err(|e| Error::ToolExecution(format!("trash failed: {e}").into()))?;

        Ok(json!({ "status": "trashed", "uid": uid }))
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

        let (email, password) = self.get_credentials().await?;
        let mut guard = self.imap.lock().await;
        let session = ensure_session(&mut guard, &email, &password).await?;
        select_mailbox(session, mailbox).await?;

        // uid_store returns a stream of FETCH responses; drain it so the server
        // response is consumed before the next command.
        let store_query = if read {
            "+FLAGS (\\Seen)"
        } else {
            "-FLAGS (\\Seen)"
        };
        let _drain: Vec<async_imap::types::Fetch> = session
            .uid_store(uid.to_string(), store_query)
            .await
            .map_err(|e| {
                let action = if read { "mark read" } else { "mark unread" };
                Error::ToolExecution(format!("{action} failed: {e}").into())
            })?
            .try_collect()
            .await
            .map_err(|e| Error::ToolExecution(format!("store stream failed: {e}").into()))?;

        let status = if read { "marked_read" } else { "marked_unread" };
        Ok(json!({ "status": status, "uid": uid }))
    }

    // -----------------------------------------------------------------------
    // Action: thread (fetch full reply chain for a message)
    // -----------------------------------------------------------------------

    async fn action_thread(&self, args: &Value) -> Result<Value> {
        let uid: u32 = args["uid"]
            .as_u64()
            .ok_or_else(|| Error::ToolExecution("missing 'uid'".into()))?
            as u32;
        let mailbox = args["mailbox"].as_str().unwrap_or("INBOX");

        let (email, password) = self.get_credentials().await?;
        let mut guard = self.imap.lock().await;
        let session = ensure_session(&mut guard, &email, &password).await?;

        // First, fetch the target message to get its References/In-Reply-To/Message-ID.
        select_mailbox(session, mailbox).await?;

        let fetches: Vec<async_imap::types::Fetch> = session
            .uid_fetch(uid.to_string(), "(UID RFC822)")
            .await
            .map_err(|e| Error::ToolExecution(format!("fetch uid {uid} failed: {e}").into()))?
            .try_collect()
            .await
            .map_err(|e| Error::ToolExecution(format!("fetch stream failed: {e}").into()))?;

        let fetch = fetches
            .first()
            .ok_or_else(|| Error::ToolExecution(format!("message uid {uid} not found").into()))?;

        let raw = fetch.body().unwrap_or_default();
        let parsed = mail_parser::MessageParser::default()
            .parse(raw)
            .ok_or_else(|| Error::ToolExecution("failed to parse email".into()))?;

        // Collect all Message-IDs in the thread: this message + References header.
        let mut msg_ids: Vec<String> = Vec::new();
        if let Some(mid) = parsed.message_id() {
            msg_ids.push(mid.to_string());
        }
        // References header contains the full chain of Message-IDs.
        let refs_header = parsed.header_raw("References").unwrap_or("");
        for token in refs_header.split_whitespace() {
            let id = token.trim_matches(|c| c == '<' || c == '>');
            if !id.is_empty() && !msg_ids.contains(&id.to_string()) {
                msg_ids.push(id.to_string());
            }
        }
        if let Some(irt) = parsed.header_raw("In-Reply-To") {
            let id = irt.trim().trim_matches(|c| c == '<' || c == '>');
            if !id.is_empty() && !msg_ids.contains(&id.to_string()) {
                msg_ids.push(id.to_string());
            }
        }

        if msg_ids.is_empty() {
            return Ok(json!({
                "thread": [],
                "count": 0,
                "note": "no threading headers found on this message"
            }));
        }

        // Search [Gmail]/All Mail for all messages matching any of these IDs.
        select_mailbox(session, "[Gmail]/All Mail").await?;

        let mut all_uids = std::collections::HashSet::new();
        for mid in &msg_ids {
            // Escape message IDs for IMAP query safety.
            let escaped = mid.replace('\\', "\\\\").replace('"', "\\\"");
            // Search for messages that reference this ID or have this ID.
            let query = format!("X-GM-RAW \"rfc822msgid:{escaped} OR references:{escaped}\"");
            if let Ok(uids) = session.uid_search(&query).await {
                all_uids.extend(uids);
            }
            // Also try standard HEADER search as fallback.
            let query2 =
                format!("OR HEADER Message-ID \"<{escaped}>\" HEADER References \"<{escaped}>\"");
            if let Ok(uids) = session.uid_search(&query2).await {
                all_uids.extend(uids);
            }
        }

        if all_uids.is_empty() {
            return Ok(json!({
                "thread": [],
                "count": 0,
                "note": "could not find thread messages"
            }));
        }

        // Fetch all thread messages.
        let mut uid_list: Vec<u32> = all_uids.into_iter().collect();
        uid_list.sort_unstable(); // chronological (oldest first)

        let uid_set = uid_list
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let fetches: Vec<async_imap::types::Fetch> = session
            .uid_fetch(&uid_set, "(UID RFC822 FLAGS)")
            .await
            .map_err(|e| Error::ToolExecution(format!("fetch thread failed: {e}").into()))?
            .try_collect()
            .await
            .map_err(|e| Error::ToolExecution(format!("fetch stream failed: {e}").into()))?;

        let mut messages = Vec::new();
        for fetch in &fetches {
            let raw_body = fetch.body().unwrap_or_default();
            let msg = match mail_parser::MessageParser::default().parse(raw_body) {
                Some(m) => m,
                None => continue,
            };

            let subject = msg.subject().unwrap_or("").to_string();
            let from = msg
                .from()
                .and_then(|a| a.first())
                .map(|a| {
                    a.name()
                        .map(|n| format!("{n} <{}>", a.address().unwrap_or("")))
                        .unwrap_or_else(|| a.address().unwrap_or("").to_string())
                })
                .unwrap_or_default();
            let to: Vec<String> = msg
                .to()
                .map(|addrs| {
                    addrs
                        .iter()
                        .map(|a| {
                            a.name()
                                .map(|n| format!("{n} <{}>", a.address().unwrap_or("")))
                                .unwrap_or_else(|| a.address().unwrap_or("").to_string())
                        })
                        .collect()
                })
                .unwrap_or_default();
            let date = msg.date().map(|d| d.to_string()).unwrap_or_default();
            let message_id = msg.message_id().unwrap_or("").to_string();
            let body_text = msg
                .body_text(0)
                .unwrap_or_else(|| {
                    msg.body_html(0)
                        .map(|h| strip_html_tags(&h).into())
                        .unwrap_or_default()
                })
                .to_string();

            // Truncate long bodies.
            let body_text = if body_text.len() > 4000 {
                format!(
                    "{}…\n[truncated, {} chars total]",
                    &body_text[..body_text.floor_char_boundary(4000)],
                    body_text.len()
                )
            } else {
                body_text
            };

            messages.push(json!({
                "uid": fetch.uid.unwrap_or(0),
                "subject": subject,
                "from": from,
                "to": to,
                "date": date,
                "message_id": message_id,
                "body": body_text,
            }));
        }

        Ok(json!({
            "thread": messages,
            "count": messages.len(),
        }))
    }

    // -----------------------------------------------------------------------
    // Action: download_attachment
    // -----------------------------------------------------------------------

    async fn action_download_attachment(&self, args: &Value) -> Result<Value> {
        let uid: u32 = args["uid"]
            .as_u64()
            .ok_or_else(|| Error::ToolExecution("missing 'uid'".into()))?
            as u32;
        let attachment_index: usize = args["attachment_index"].as_u64().ok_or_else(|| {
            Error::ToolExecution(
                "missing 'attachment_index' (use read action to list attachments)".into(),
            )
        })? as usize;
        let mailbox = args["mailbox"].as_str().unwrap_or("INBOX");

        let (email, password) = self.get_credentials().await?;
        let mut guard = self.imap.lock().await;
        let session = ensure_session(&mut guard, &email, &password).await?;
        select_mailbox(session, mailbox).await?;

        let fetches: Vec<async_imap::types::Fetch> = session
            .uid_fetch(uid.to_string(), "(UID RFC822)")
            .await
            .map_err(|e| Error::ToolExecution(format!("fetch uid {uid} failed: {e}").into()))?
            .try_collect()
            .await
            .map_err(|e| Error::ToolExecution(format!("fetch stream failed: {e}").into()))?;

        let fetch = fetches
            .first()
            .ok_or_else(|| Error::ToolExecution(format!("message uid {uid} not found").into()))?;

        let raw_body = fetch.body().unwrap_or_default();
        let parsed = mail_parser::MessageParser::default()
            .parse(raw_body)
            .ok_or_else(|| Error::ToolExecution("failed to parse email".into()))?;

        use mail_parser::MimeHeaders;
        let part = parsed.attachment(attachment_index).ok_or_else(|| {
            Error::ToolExecution(
                format!(
                    "attachment index {attachment_index} not found (message has {} attachments)",
                    parsed.attachment_count()
                )
                .into(),
            )
        })?;

        let raw_filename = part
            .content_disposition()
            .and_then(|cd| cd.attribute("filename"))
            .or_else(|| part.content_type().and_then(|ct| ct.attribute("name")))
            .unwrap_or("attachment.bin")
            .to_string();

        // Sanitize filename to prevent path traversal attacks.
        // Strip path separators and parent-directory components,
        // keeping only the final filename component.
        let filename = {
            let name = std::path::Path::new(&raw_filename)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("attachment.bin");
            // Reject empty or dot-only filenames
            if name.is_empty() || name == "." || name == ".." {
                "attachment.bin".to_string()
            } else {
                // Prefix with a UUID to avoid collisions and ensure uniqueness
                format!("{}_{}", uuid::Uuid::new_v4(), name)
            }
        };

        let contents = part.contents();

        // Save to a temp directory
        let download_dir = std::env::temp_dir().join("rustykrab-attachments");
        std::fs::create_dir_all(&download_dir)
            .map_err(|e| Error::ToolExecution(format!("create dir failed: {e}").into()))?;

        let dest = download_dir.join(&filename);

        // Final safety check: ensure the destination is within download_dir
        if let Ok(canonical_dest) = dest.canonicalize().or_else(|_| {
            // For new files, check parent
            dest.parent()
                .and_then(|p| p.canonicalize().ok())
                .map(|p| p.join(dest.file_name().unwrap_or_default()))
                .ok_or(std::io::Error::other("cannot resolve"))
        }) {
            let canonical_dir = download_dir
                .canonicalize()
                .map_err(|e| Error::ToolExecution(format!("resolve dir failed: {e}").into()))?;
            if !canonical_dest.starts_with(&canonical_dir) {
                return Err(Error::ToolExecution(
                    "attachment filename escapes download directory".into(),
                ));
            }
        }
        std::fs::write(&dest, contents)
            .map_err(|e| Error::ToolExecution(format!("write failed: {e}").into()))?;

        Ok(json!({
            "status": "downloaded",
            "filename": filename,
            "path": dest.to_string_lossy(),
            "size_bytes": contents.len(),
        }))
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

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_net: true,
            ..SandboxRequirements::default()
        }
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
                        "enum": ["setup", "search", "read", "send", "labels", "move", "trash", "mark_read", "mark_unread", "download_attachment", "thread"],
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
                        "description": "IMAP mailbox name (default 'INBOX'). Gmail folders: '[Gmail]/Sent Mail', '[Gmail]/All Mail', '[Gmail]/Drafts', '[Gmail]/Spam', '[Gmail]/Trash', '[Gmail]/Starred'. Custom labels use their name directly."
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
                    },
                    "attachment_index": {
                        "type": "integer",
                        "description": "Attachment index to download (for 'download_attachment' action, 0-based, from 'read' response)"
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
            "download_attachment" => self.action_download_attachment(&args).await,
            "thread" => self.action_thread(&args).await,
            other => Err(Error::ToolExecution(format!(
                "unknown action '{other}', expected one of: setup, search, read, send, labels, move, trash, mark_read, mark_unread, download_attachment, thread"
            ).into())),
        }
    }
}

// ---------------------------------------------------------------------------
// Module-private helpers
// ---------------------------------------------------------------------------

/// Connect to Gmail IMAP over rustls and authenticate.
async fn connect_imap(email: &str, password: &str) -> Result<ImapSession> {
    let native_certs = rustls_native_certs::load_native_certs();
    let mut root_store = rustls::RootCertStore::empty();
    root_store.add_parsable_certificates(native_certs.certs);

    let config = std::sync::Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth(),
    );
    let server_name: rustls::pki_types::ServerName<'static> = IMAP_HOST
        .to_string()
        .try_into()
        .map_err(|e| Error::ToolExecution(format!("invalid server name: {e}").into()))?;

    let tcp = tokio::net::TcpStream::connect((IMAP_HOST, IMAP_PORT))
        .await
        .map_err(|e| Error::ToolExecution(format!("IMAP connect failed: {e}").into()))?;
    let connector = tokio_rustls::TlsConnector::from(config);
    let tls_stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| Error::ToolExecution(format!("TLS handshake failed: {e}").into()))?;

    let mut client = async_imap::Client::new(tls_stream);
    // Read the server greeting so the protocol stream stays in sync.
    let _greeting = client
        .read_response()
        .await
        .map_err(|e| Error::ToolExecution(format!("IMAP greeting read failed: {e}").into()))?
        .ok_or_else(|| {
            Error::ToolExecution("IMAP server closed connection before greeting".into())
        })?;

    let session = client
        .login(email, password)
        .await
        .map_err(|(e, _client)| Error::ToolExecution(format!("IMAP login failed: {e}").into()))?;
    Ok(session)
}

/// Decode an IMAP string (may be UTF-7 encoded).
fn decode_imap_string(s: &[u8]) -> String {
    String::from_utf8_lossy(s).to_string()
}

/// Format an IMAP address into a readable string.
fn format_address(addr: &imap_proto::types::Address<'_>) -> String {
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
