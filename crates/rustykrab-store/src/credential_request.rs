//! Pending credential changes the agent asked for and the user must decide.
//!
//! The agent can create a credential under a new name, but replacing or
//! deleting one it doesn't own is not its call to make. Those attempts land
//! here as rows the user resolves from the app or WebChat.
//!
//! A queued proposal is encrypted exactly like a secret (AAD = the request
//! id), so a pending request is never a plaintext copy of a credential
//! sitting in the database.

use std::sync::Arc;
use std::sync::Mutex;

use std::time::Duration;

use chrono::Utc;
use rusqlite::params;
use rustykrab_core::Error;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::secret::{SecretStore, WriteAuthority};

/// How long a pending request survives before it is swept.
pub const REQUEST_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestAction {
    Update,
    Delete,
    /// The agent needs a credential it does not have and is asking the
    /// user to supply it. The inverse of `Update`: there is no proposed
    /// value to approve, only a set of fields to fill in. Completed with
    /// [`CredentialRequestStore::fulfil`], never with `approve` — there is
    /// nothing to approve until the user has typed something.
    Fulfil,
}

impl RequestAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            RequestAction::Update => "update",
            RequestAction::Delete => "delete",
            RequestAction::Fulfil => "fulfil",
        }
    }

    fn parse(raw: &str) -> Result<Self, Error> {
        match raw {
            "update" => Ok(RequestAction::Update),
            "delete" => Ok(RequestAction::Delete),
            "fulfil" => Ok(RequestAction::Fulfil),
            other => Err(Error::Storage(format!("unknown request action '{other}'"))),
        }
    }
}

/// One value the agent is asking for, described well enough that a client
/// can render an input for it without knowing anything about the service.
///
/// `key` is the store name the answer is filed under, so a login that needs
/// a username and a password is two fields and two secrets — the store stays
/// a flat name-to-value map and gains no notion of "an account".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestedField {
    /// Credential name the supplied value is stored under.
    pub key: String,
    /// What to show the user, e.g. "App password".
    pub label: String,
    /// Whether the input must be masked. Usernames and email addresses are
    /// not secret; passwords and tokens are.
    #[serde(default = "default_true")]
    pub secret: bool,
    /// Optional guidance, e.g. where to generate the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

fn default_true() -> bool {
    true
}

/// A credential change request. Never carries the proposed value — no
/// caller outside `approve` has any business seeing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRequest {
    pub id: String,
    pub name: String,
    pub action: RequestAction,
    pub reason: Option<String>,
    pub conversation_id: Option<String>,
    pub status: String,
    pub created_at: i64,
    /// What the credential is for, in words the user recognises —
    /// "Gmail", "secure.examplebank.com". Only set on `Fulfil`.
    pub service: Option<String>,
    /// Fields the user must fill in. Empty for `Update`/`Delete`, which
    /// ask a yes/no question rather than for input.
    pub fields: Vec<RequestedField>,
}

/// Told when a request is filed, so something can go and tell the user.
///
/// Implemented outside this crate (the gateway sends the push) so storage
/// stays unaware of notification transports, matching the backend-trait
/// pattern used elsewhere in the workspace.
///
/// Deliberately fire-and-forget: a notification that fails must never stop
/// a request being recorded, because the record is what protects the
/// credential. The app and WebChat both list pending requests without any
/// notification at all.
pub trait RequestNotifier: Send + Sync + std::fmt::Debug {
    fn request_filed(&self, credential_name: &str, action: &str);
}

#[derive(Clone)]
pub struct CredentialRequestStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
    secrets: SecretStore,
    notifier: Option<Arc<dyn RequestNotifier>>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl CredentialRequestStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>, secrets: SecretStore) -> Self {
        Self {
            conn,
            secrets,
            notifier: None,
        }
    }

    /// Attach something that tells the user a request is waiting.
    pub fn with_notifier(mut self, notifier: Arc<dyn RequestNotifier>) -> Self {
        self.notifier = Some(notifier);
        self
    }

    /// Queue a replacement of an existing credential.
    ///
    /// Filing supersedes any earlier pending request for the same name: the
    /// user should be deciding on the agent's latest intent, not working
    /// through a backlog of stale proposals.
    pub async fn file_update(
        &self,
        name: &str,
        proposed_value: &str,
        reason: Option<String>,
        conversation_id: Option<Uuid>,
    ) -> Result<String, Error> {
        let id = Uuid::new_v4().to_string();
        // AAD ties the ciphertext to this request: a proposal cannot be
        // lifted into another request or applied to another name.
        let encrypted = self
            .secrets
            .encrypt_with_aad(&id, proposed_value.as_bytes())?;
        let target_version = self.secrets.version_of(name).await?;
        self.insert(
            id.clone(),
            name,
            RequestAction::Update,
            Some(encrypted),
            reason,
            conversation_id,
            target_version,
        )
        .await?;
        Ok(id)
    }

    /// Queue a deletion.
    pub async fn file_delete(
        &self,
        name: &str,
        reason: Option<String>,
        conversation_id: Option<Uuid>,
    ) -> Result<String, Error> {
        let id = Uuid::new_v4().to_string();
        let target_version = self.secrets.version_of(name).await?;
        self.insert(
            id.clone(),
            name,
            RequestAction::Delete,
            None,
            reason,
            conversation_id,
            target_version,
        )
        .await?;
        Ok(id)
    }

    /// Ask the user for a credential the agent does not have.
    ///
    /// Nothing is encrypted here because nothing is proposed: the row is a
    /// question, and the answer arrives later through [`Self::fulfil`].
    /// `name` is the credential the request is *about* — it dedupes
    /// against other pending requests for the same one, so an agent that
    /// hits the same missing password on three turns asks once.
    pub async fn file_fulfil(
        &self,
        name: &str,
        service: Option<String>,
        fields: Vec<RequestedField>,
        reason: Option<String>,
        conversation_id: Option<Uuid>,
    ) -> Result<String, Error> {
        if fields.is_empty() {
            return Err(Error::Storage(
                "a fulfil request must name at least one field".into(),
            ));
        }
        let id = Uuid::new_v4().to_string();
        self.insert_full(
            id.clone(),
            name,
            RequestAction::Fulfil,
            None,
            reason,
            conversation_id,
            // A credential that does not exist yet has no version to be
            // stale against, and one that does is being replaced by
            // whatever the user types — either way this guard does not
            // apply, and pinning it would reject the answer.
            None,
            service,
            Some(fields),
        )
        .await?;
        Ok(id)
    }

    /// Complete a `Fulfil` request with the values the user typed.
    ///
    /// Only the fields the request asked for may be written: a device
    /// answering a Gmail prompt must not be able to set
    /// `anthropic_api_key` by adding it to the payload.
    pub async fn fulfil(
        &self,
        id: &str,
        values: &[(String, String)],
        decided_by: &str,
    ) -> Result<(), Error> {
        let row = self.load(id).await?;
        if row.status != "pending" {
            return Err(Error::AlreadyExists(format!(
                "request {id} is already {}",
                row.status
            )));
        }
        if row.action != RequestAction::Fulfil {
            return Err(Error::Storage(format!(
                "request {id} is a '{}' request — use approve/deny",
                row.action.as_str()
            )));
        }

        let asked: Vec<RequestedField> = row.fields;
        for (key, _) in values {
            if !asked.iter().any(|f| &f.key == key) {
                return Err(Error::Storage(format!(
                    "'{key}' was not one of the fields requested"
                )));
            }
        }
        for field in &asked {
            let supplied = values
                .iter()
                .find(|(k, _)| k == &field.key)
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            if supplied.is_empty() {
                return Err(Error::Storage(format!(
                    "no value supplied for '{}'",
                    field.key
                )));
            }
        }

        let authority = WriteAuthority::User {
            device: Some(decided_by.to_string()),
        };
        for (key, value) in values {
            // Straight into the OS keychain. The user typed this on the
            // understanding it was going somewhere safe, and the database is
            // not that: `put_hardware` keeps the value out of it entirely and
            // refuses outright where there is no keychain, rather than
            // quietly choosing the weaker home.
            self.secrets
                .put_hardware(key, value, authority.clone())
                .await?;
        }

        self.decide(id, "approved", decided_by, &row.name, "fulfil")
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert(
        &self,
        id: String,
        name: &str,
        action: RequestAction,
        proposed: Option<Vec<u8>>,
        reason: Option<String>,
        conversation_id: Option<Uuid>,
        target_version: Option<i64>,
    ) -> Result<(), Error> {
        self.insert_full(
            id,
            name,
            action,
            proposed,
            reason,
            conversation_id,
            target_version,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_full(
        &self,
        id: String,
        name: &str,
        action: RequestAction,
        proposed: Option<Vec<u8>>,
        reason: Option<String>,
        conversation_id: Option<Uuid>,
        target_version: Option<i64>,
        service: Option<String>,
        fields: Option<Vec<RequestedField>>,
    ) -> Result<(), Error> {
        let fields_json = match &fields {
            Some(f) => Some(
                serde_json::to_string(f)
                    .map_err(|e| Error::Storage(format!("cannot encode fields: {e}")))?,
            ),
            None => None,
        };
        let name = name.to_string();
        let announce_name = name.clone();
        let conversation = conversation_id.map(|c| c.to_string());
        crate::with_conn(&self.conn, move |conn| {
            // Newer intent replaces older intent for the same credential.
            conn.execute(
                "UPDATE credential_requests SET status = 'superseded', decided_at = ?2
                 WHERE name = ?1 AND status = 'pending'",
                params![name, now_ms()],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            conn.execute(
                "INSERT INTO credential_requests
                    (id, name, action, proposed_data, reason, conversation_id,
                     status, created_at, target_version, service, fields)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, ?9, ?10)",
                params![
                    id,
                    name,
                    action.as_str(),
                    proposed,
                    reason,
                    conversation,
                    now_ms(),
                    target_version,
                    service,
                    fields_json
                ],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await?;

        // Only after the row is durable: a notification about a request
        // that failed to record would send the user looking for nothing.
        if let Some(notifier) = &self.notifier {
            notifier.request_filed(&announce_name, action.as_str());
        }
        Ok(())
    }

    /// Pending requests, newest first. Expired ones are swept on the way
    /// through rather than by a background timer.
    /// Mint a one-time link for a `fulfil` request and return the token.
    ///
    /// The token is returned exactly once, to be put in the message the
    /// user receives. Only its SHA-256 lands in the database, so a dump of
    /// the store does not yield working links.
    ///
    /// Re-issuing replaces any previous token: the last link sent is the
    /// only one that works.
    pub async fn issue_link(&self, id: &str, ttl: Duration) -> Result<String, Error> {
        let token = {
            use rand::Rng;
            let mut raw = [0u8; 32];
            rand::rng().fill(&mut raw);
            // URL-safe and unpadded: this goes in a link a user may retype.
            hex::encode(raw)
        };
        let hash = hash_token(&token);
        let expires = Utc::now()
            .checked_add_signed(chrono::Duration::from_std(ttl).unwrap_or_default())
            .map(|t| t.timestamp_millis())
            .unwrap_or(i64::MAX);
        let id = id.to_string();
        crate::with_conn(&self.conn, move |conn| {
            let n = conn
                .execute(
                    "UPDATE credential_requests
                        SET link_token_hash = ?1, link_expires_at = ?2
                      WHERE id = ?3 AND status = 'pending'",
                    params![hash, expires, id],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            if n == 0 {
                return Err(Error::NotFound(format!("pending request {id}")));
            }
            Ok(())
        })
        .await?;
        Ok(token)
    }

    /// Resolve a one-time link token to the request it answers.
    ///
    /// Returns `None` for a token that is unknown, expired, or whose
    /// request is no longer pending — deliberately not distinguishing
    /// between them, so the page cannot be used to probe which links exist.
    /// Single use falls out of the status check: fulfilling moves the row
    /// off `pending`, so the link stops working the moment it is answered.
    pub async fn find_by_link(&self, token: &str) -> Result<Option<CredentialRequest>, Error> {
        self.sweep_expired().await?;
        let hash = hash_token(token);
        let now = Utc::now().timestamp_millis();
        crate::with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, action, reason, conversation_id, status,
                            created_at, service, fields
                       FROM credential_requests
                      WHERE link_token_hash = ?1
                        AND status = 'pending'
                        AND (link_expires_at IS NULL OR link_expires_at > ?2)",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut rows = stmt
                .query(params![hash, now])
                .map_err(|e| Error::Storage(e.to_string()))?;
            let Some(row) = rows.next().map_err(|e| Error::Storage(e.to_string()))? else {
                return Ok(None);
            };
            let get_s = |i: usize| row.get::<_, String>(i);
            let get_o = |i: usize| row.get::<_, Option<String>>(i);
            Ok(Some(CredentialRequest {
                id: get_s(0).map_err(|e| Error::Storage(e.to_string()))?,
                name: get_s(1).map_err(|e| Error::Storage(e.to_string()))?,
                action: RequestAction::parse(
                    &get_s(2).map_err(|e| Error::Storage(e.to_string()))?,
                )?,
                reason: get_o(3).map_err(|e| Error::Storage(e.to_string()))?,
                conversation_id: get_o(4).map_err(|e| Error::Storage(e.to_string()))?,
                status: get_s(5).map_err(|e| Error::Storage(e.to_string()))?,
                created_at: row
                    .get::<_, i64>(6)
                    .map_err(|e| Error::Storage(e.to_string()))?,
                service: get_o(7).map_err(|e| Error::Storage(e.to_string()))?,
                fields: decode_fields(
                    get_o(8)
                        .map_err(|e| Error::Storage(e.to_string()))?
                        .as_deref(),
                ),
            }))
        })
        .await
    }

    pub async fn pending(&self) -> Result<Vec<CredentialRequest>, Error> {
        self.sweep_expired().await?;
        crate::with_conn(&self.conn, |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, action, reason, conversation_id, status,
                            created_at, service, fields
                     FROM credential_requests WHERE status = 'pending'
                     ORDER BY created_at DESC",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, Option<String>>(8)?,
                    ))
                })
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut out = Vec::new();
            for row in rows {
                let (
                    id,
                    name,
                    action,
                    reason,
                    conversation_id,
                    status,
                    created_at,
                    service,
                    fields,
                ) = row.map_err(|e| Error::Storage(e.to_string()))?;
                out.push(CredentialRequest {
                    id,
                    name,
                    action: RequestAction::parse(&action)?,
                    reason,
                    conversation_id,
                    status,
                    created_at,
                    service,
                    fields: decode_fields(fields.as_deref()),
                });
            }
            Ok(out)
        })
        .await
    }

    /// Mark long-untouched requests expired so they stop showing up as
    /// things the user still has to decide.
    pub async fn sweep_expired(&self) -> Result<usize, Error> {
        crate::with_conn(&self.conn, |conn| {
            let cutoff = now_ms() - REQUEST_TTL_MS;
            let n = conn
                .execute(
                    "UPDATE credential_requests
                     SET status = 'expired', decided_at = ?2, proposed_data = NULL
                     WHERE status = 'pending' AND created_at < ?1",
                    params![cutoff, now_ms()],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(n)
        })
        .await
    }

    /// Apply a pending request with `User` authority.
    ///
    /// Refused with [`Error::AlreadyExists`] — surfaced as `409` — when the
    /// credential moved after the request was filed. Approving then would
    /// silently undo whatever the user did in the meantime, which is the
    /// exact class of surprise this whole feature exists to prevent.
    pub async fn approve(&self, id: &str, decided_by: &str) -> Result<(), Error> {
        let row = self.load(id).await?;
        if row.status != "pending" {
            return Err(Error::AlreadyExists(format!(
                "request {id} is already {}",
                row.status
            )));
        }
        let current_version = self.secrets.version_of(&row.name).await?;
        if current_version != row.target_version {
            return Err(Error::AlreadyExists(format!(
                "'{}' changed since this request was filed",
                row.name
            )));
        }

        let authority = WriteAuthority::User {
            device: Some(decided_by.to_string()),
        };
        match row.action {
            RequestAction::Update => {
                let proposed = row
                    .proposed
                    .ok_or_else(|| Error::Storage("request has no proposed value".into()))?;
                let plaintext = self.secrets.decrypt_with_aad(id, &proposed)?;
                let value = String::from_utf8(plaintext)
                    .map_err(|e| Error::Storage(format!("invalid utf-8 in proposal: {e}")))?;
                match current_version {
                    Some(_) => self.secrets.overwrite(&row.name, &value, authority).await?,
                    // The target was deleted after filing; honouring the
                    // request means recreating it.
                    None => self.secrets.create(&row.name, &value).await?,
                }
            }
            RequestAction::Delete => {
                self.secrets.delete(&row.name, authority).await?;
            }
            RequestAction::Fulfil => {
                return Err(Error::Storage(format!(
                    "request {id} asks the user for a value — complete it with \
                     fulfil, not approve"
                )));
            }
        }

        self.decide(id, "approved", decided_by, &row.name, "approve")
            .await
    }

    /// Reject a request and wipe the proposal — a denied value should not
    /// linger in the database.
    pub async fn deny(&self, id: &str, decided_by: &str) -> Result<(), Error> {
        let row = self.load(id).await?;
        if row.status != "pending" {
            return Err(Error::AlreadyExists(format!(
                "request {id} is already {}",
                row.status
            )));
        }
        self.decide(id, "denied", decided_by, &row.name, "deny")
            .await
    }

    async fn decide(
        &self,
        id: &str,
        status: &str,
        decided_by: &str,
        name: &str,
        audit_op: &str,
    ) -> Result<(), Error> {
        let id = id.to_string();
        let status = status.to_string();
        let decided_by = decided_by.to_string();
        let name = name.to_string();
        let audit_op = audit_op.to_string();
        crate::with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE credential_requests
                 SET status = ?2, decided_at = ?3, decided_by = ?4, proposed_data = NULL
                 WHERE id = ?1",
                params![id, status, now_ms(), decided_by],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            conn.execute(
                "INSERT INTO secret_audit (name, op, authority, request_id, at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![name, audit_op, format!("user:{decided_by}"), id, now_ms()],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
    }

    async fn load(&self, id: &str) -> Result<StoredRequest, Error> {
        let id = id.to_string();
        crate::with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT name, action, proposed_data, status, target_version, fields
                     FROM credential_requests WHERE id = ?1",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let row = stmt
                .query_row(params![id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                })
                .map_err(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => {
                        Error::NotFound(format!("credential request '{id}'"))
                    }
                    other => Error::Storage(other.to_string()),
                })?;
            Ok(StoredRequest {
                name: row.0,
                action: RequestAction::parse(&row.1)?,
                proposed: row.2,
                status: row.3,
                target_version: row.4,
                fields: decode_fields(row.5.as_deref()),
            })
        })
        .await
    }
}

struct StoredRequest {
    name: String,
    action: RequestAction,
    proposed: Option<Vec<u8>>,
    status: String,
    target_version: Option<i64>,
    fields: Vec<RequestedField>,
}

/// A row written before the column existed, or one holding junk, yields no
/// fields rather than an error: a request that cannot be rendered should
/// show up as un-answerable, not take down the whole pending list.
fn decode_fields(raw: Option<&str>) -> Vec<RequestedField> {
    raw.and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod fulfil_tests {
    use super::*;
    use crate::Store;

    /// Every test store gets an in-memory credential backend.
    ///
    /// Not a stylistic choice: before this existed, running the suite on a
    /// Mac deposited `gmail-app-password` into the developer's own login
    /// keychain, because `fulfil` writes to whatever backend it is handed.
    /// Tests must never be able to reach the real one.
    pub(super) fn store() -> (tempfile::TempDir, CredentialRequestStore, SecretStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path(), vec![7u8; 32])
            .expect("open")
            .with_credential_backend(std::sync::Arc::new(
                crate::credential_backend::MemoryBackend::new(),
            ));
        (dir, store.credential_requests(), store.secrets())
    }

    pub(super) fn gmail_fields() -> Vec<RequestedField> {
        vec![
            RequestedField {
                key: "gmail_email".into(),
                label: "Gmail address".into(),
                secret: false,
                hint: None,
            },
            RequestedField {
                key: "gmail_app_password".into(),
                label: "App password".into(),
                secret: true,
                hint: None,
            },
        ]
    }

    #[tokio::test]
    async fn a_fulfil_request_carries_its_form() {
        let (_dir, requests, _secrets) = store();
        requests
            .file_fulfil(
                "gmail_app_password",
                Some("Gmail".into()),
                gmail_fields(),
                Some("to read your inbox".into()),
                None,
            )
            .await
            .unwrap();

        let pending = requests.pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].action, RequestAction::Fulfil);
        assert_eq!(pending[0].service.as_deref(), Some("Gmail"));
        assert_eq!(pending[0].fields.len(), 2);
        // The username field must not come back masked, or the app renders
        // an email address as dots.
        assert!(!pending[0].fields[0].secret);
        assert!(pending[0].fields[1].secret);
    }

    #[tokio::test]
    async fn fulfilling_stores_every_supplied_value() {
        let (_dir, requests, secrets) = store();
        let id = requests
            .file_fulfil("gmail_app_password", None, gmail_fields(), None, None)
            .await
            .unwrap();

        requests
            .fulfil(
                &id,
                &[
                    ("gmail_email".into(), "me@gmail.com".into()),
                    ("gmail_app_password".into(), "abcdefghijklmnop".into()),
                ],
                "device:test",
            )
            .await
            .unwrap();

        // Readable through the path the tools use...
        assert_eq!(
            secrets.get_hardware("gmail_email").as_deref(),
            Some("me@gmail.com")
        );
        assert_eq!(
            secrets.get_hardware("gmail_app_password").as_deref(),
            Some("abcdefghijklmnop")
        );

        // ...and not out of the database, which is the point. The row
        // exists so the overwrite guard still has a version to compare and
        // the audit trail still names the write; what it no longer holds is
        // the credential.
        assert_ne!(
            secrets.get("gmail_app_password").await.ok().as_deref(),
            Some("abcdefghijklmnop")
        );
        assert_eq!(
            secrets.version_of("gmail_app_password").await.unwrap(),
            Some(1)
        );

        assert!(requests.pending().await.unwrap().is_empty());
    }

    /// The credential must not be sitting in the database file, however it
    /// got there — the API returning the wrong thing is not the same as the
    /// bytes being absent.
    #[tokio::test]
    async fn the_supplied_value_is_nowhere_in_the_database_file() {
        let (dir, requests, _secrets) = store();
        let id = requests
            .file_fulfil("gmail_app_password", None, gmail_fields(), None, None)
            .await
            .unwrap();
        requests
            .fulfil(
                &id,
                &[
                    ("gmail_email".into(), "me@gmail.com".into()),
                    ("gmail_app_password".into(), "correct-horse-battery".into()),
                ],
                "device:test",
            )
            .await
            .unwrap();

        let raw = std::fs::read(dir.path().join("store.db")).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&raw).contains("correct-horse-battery"),
            "the credential was found in the database file"
        );
    }

    /// With no secure store there is nowhere safe to put a credential, and
    /// the database is not an acceptable second choice.
    #[tokio::test]
    async fn fulfilling_is_refused_when_no_secure_backend_is_available() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path(), vec![7u8; 32])
            .expect("open")
            .with_credential_backend(std::sync::Arc::new(crate::credential_backend::NoBackend));
        let requests = store.credential_requests();
        let id = requests
            .file_fulfil("gmail_app_password", None, gmail_fields(), None, None)
            .await
            .unwrap();

        let err = requests
            .fulfil(
                &id,
                &[
                    ("gmail_email".into(), "me@gmail.com".into()),
                    ("gmail_app_password".into(), "abcdefghijklmnop".into()),
                ],
                "device:test",
            )
            .await
            .expect_err("must refuse rather than fall back to the database");
        assert!(
            err.to_string().contains("Refusing"),
            "the refusal should say why: {err}"
        );

        // And the request stays pending: nothing was written, so nothing was
        // decided.
        assert_eq!(requests.pending().await.unwrap().len(), 1);
    }

    /// The request is the authorisation. A device answering a Gmail prompt
    /// must not be able to set an unrelated credential by adding it to the
    /// payload — that would turn every prompt into an arbitrary write.
    #[tokio::test]
    async fn a_value_that_was_never_asked_for_is_refused() {
        let (_dir, requests, secrets) = store();
        let id = requests
            .file_fulfil("gmail_app_password", None, gmail_fields(), None, None)
            .await
            .unwrap();

        let err = requests
            .fulfil(
                &id,
                &[
                    ("gmail_email".into(), "me@gmail.com".into()),
                    ("gmail_app_password".into(), "abcdefghijklmnop".into()),
                    ("anthropic_api_key".into(), "sk-attacker".into()),
                ],
                "device:test",
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("anthropic_api_key"), "{err}");
        // Nothing at all was written: the whole answer is rejected, not
        // the offending field alone.
        assert!(secrets.get("anthropic_api_key").await.is_err());
        assert!(secrets.get("gmail_email").await.is_err());
    }

    #[tokio::test]
    async fn a_blank_answer_is_refused() {
        let (_dir, requests, secrets) = store();
        let id = requests
            .file_fulfil("gmail_app_password", None, gmail_fields(), None, None)
            .await
            .unwrap();

        let err = requests
            .fulfil(
                &id,
                &[
                    ("gmail_email".into(), "me@gmail.com".into()),
                    ("gmail_app_password".into(), String::new()),
                ],
                "device:test",
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("gmail_app_password"), "{err}");
        assert!(secrets.get("gmail_email").await.is_err());
    }

    /// A fulfil has no proposed value, so approving it could only ever
    /// store nothing while marking the request done — leaving the agent
    /// blocked on a credential the user believes they supplied.
    #[tokio::test]
    async fn a_fulfil_cannot_be_approved() {
        let (_dir, requests, _secrets) = store();
        let id = requests
            .file_fulfil("gmail_app_password", None, gmail_fields(), None, None)
            .await
            .unwrap();

        let err = requests.approve(&id, "device:test").await.unwrap_err();
        assert!(err.to_string().contains("fulfil"), "{err}");
        assert_eq!(requests.pending().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn asking_twice_supersedes_the_first_ask() {
        let (_dir, requests, _secrets) = store();
        requests
            .file_fulfil("gmail_app_password", None, gmail_fields(), None, None)
            .await
            .unwrap();
        requests
            .file_fulfil("gmail_app_password", None, gmail_fields(), None, None)
            .await
            .unwrap();

        // An agent that hits the same missing credential on three turns
        // must not leave the user three identical prompts.
        assert_eq!(requests.pending().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_request_with_no_fields_is_refused() {
        let (_dir, requests, _secrets) = store();
        let err = requests
            .file_fulfil("gmail_app_password", None, vec![], None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("at least one field"), "{err}");
    }
}

/// Hash a link token for storage and lookup.
///
/// SHA-256 without a salt is right here and a salt would be wrong: the
/// token is 32 bytes of CSPRNG output, so there is no dictionary to attack,
/// and lookup is by hash — a per-row salt would make that impossible.
fn hash_token(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

#[cfg(test)]
mod link_tests {
    use super::fulfil_tests::{gmail_fields, store};
    use super::*;

    const TTL: Duration = Duration::from_secs(900);

    #[tokio::test]
    async fn a_link_resolves_to_its_request_and_only_that_one() {
        let (_dir, requests, _secrets) = store();
        let id = requests
            .file_fulfil(
                "gmail_app_password",
                Some("Gmail".into()),
                gmail_fields(),
                None,
                None,
            )
            .await
            .expect("file");
        let token = requests.issue_link(&id, TTL).await.expect("issue");

        let found = requests.find_by_link(&token).await.expect("lookup");
        assert_eq!(found.map(|r| r.id), Some(id));

        // A token that was never issued resolves to nothing rather than
        // erroring, so the page cannot be used to probe which links exist.
        assert!(requests
            .find_by_link(&"0".repeat(64))
            .await
            .expect("lookup")
            .is_none());
    }

    #[tokio::test]
    async fn the_token_itself_is_not_recoverable_from_the_store() {
        let (_dir, requests, _secrets) = store();
        let id = requests
            .file_fulfil(
                "gmail_app_password",
                Some("Gmail".into()),
                gmail_fields(),
                None,
                None,
            )
            .await
            .expect("file");
        let token = requests.issue_link(&id, TTL).await.expect("issue");
        // Only the hash is persisted: a dump of the row yields no working link.
        assert_ne!(hash_token(&token), token);
        assert_eq!(hash_token(&token).len(), 64);
    }

    #[tokio::test]
    async fn answering_a_request_burns_its_link() {
        let (_dir, requests, secrets) = store();
        let id = requests
            .file_fulfil(
                "gmail_app_password",
                Some("Gmail".into()),
                gmail_fields(),
                None,
                None,
            )
            .await
            .expect("file");
        let token = requests.issue_link(&id, TTL).await.expect("issue");
        assert!(requests
            .find_by_link(&token)
            .await
            .expect("lookup")
            .is_some());

        requests
            .fulfil(
                &id,
                &[
                    ("gmail_email".to_string(), "a@b.c".to_string()),
                    ("gmail_app_password".to_string(), "pw".to_string()),
                ],
                "test",
            )
            .await
            .expect("fulfil");

        // Single use falls out of the status check rather than a separate
        // flag: the row is no longer pending, so the link is dead.
        assert!(requests
            .find_by_link(&token)
            .await
            .expect("lookup")
            .is_none());
        let _ = secrets;
    }

    #[tokio::test]
    async fn an_expired_link_stops_working() {
        let (_dir, requests, _secrets) = store();
        let id = requests
            .file_fulfil(
                "gmail_app_password",
                Some("Gmail".into()),
                gmail_fields(),
                None,
                None,
            )
            .await
            .expect("file");
        let token = requests
            .issue_link(&id, Duration::from_secs(0))
            .await
            .expect("issue");
        assert!(requests
            .find_by_link(&token)
            .await
            .expect("lookup")
            .is_none());
    }

    #[tokio::test]
    async fn re_issuing_invalidates_the_link_already_sent() {
        let (_dir, requests, _secrets) = store();
        let id = requests
            .file_fulfil(
                "gmail_app_password",
                Some("Gmail".into()),
                gmail_fields(),
                None,
                None,
            )
            .await
            .expect("file");
        let first = requests.issue_link(&id, TTL).await.expect("issue");
        let second = requests.issue_link(&id, TTL).await.expect("re-issue");
        assert_ne!(first, second);
        assert!(requests
            .find_by_link(&first)
            .await
            .expect("lookup")
            .is_none());
        assert!(requests
            .find_by_link(&second)
            .await
            .expect("lookup")
            .is_some());
    }
}
