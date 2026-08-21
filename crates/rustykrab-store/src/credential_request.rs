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

use rusqlite::params;
use rustykrab_core::Error;
use uuid::Uuid;

use crate::secret::{SecretStore, WriteAuthority};

/// How long a pending request survives before it is swept.
pub const REQUEST_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestAction {
    Update,
    Delete,
}

impl RequestAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            RequestAction::Update => "update",
            RequestAction::Delete => "delete",
        }
    }

    fn parse(raw: &str) -> Result<Self, Error> {
        match raw {
            "update" => Ok(RequestAction::Update),
            "delete" => Ok(RequestAction::Delete),
            other => Err(Error::Storage(format!("unknown request action '{other}'"))),
        }
    }
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
                     status, created_at, target_version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8)",
                params![
                    id,
                    name,
                    action.as_str(),
                    proposed,
                    reason,
                    conversation,
                    now_ms(),
                    target_version
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
    pub async fn pending(&self) -> Result<Vec<CredentialRequest>, Error> {
        self.sweep_expired().await?;
        crate::with_conn(&self.conn, |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, action, reason, conversation_id, status, created_at
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
                    ))
                })
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut out = Vec::new();
            for row in rows {
                let (id, name, action, reason, conversation_id, status, created_at) =
                    row.map_err(|e| Error::Storage(e.to_string()))?;
                out.push(CredentialRequest {
                    id,
                    name,
                    action: RequestAction::parse(&action)?,
                    reason,
                    conversation_id,
                    status,
                    created_at,
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
                    "SELECT name, action, proposed_data, status, target_version
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
}
