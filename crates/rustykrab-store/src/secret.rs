use std::sync::Arc;

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use argon2::Argon2;
use rand::TryRngCore;
use rusqlite::params;
use rustykrab_core::Error;
use std::sync::Mutex;
use zeroize::Zeroizing;

use crate::run_blocking;

/// The salt length used for Argon2 key derivation.
const SALT_LEN: usize = 16;
/// The nonce length for AES-256-GCM (96 bits).
const NONCE_LEN: usize = 12;

/// Who is performing a credential write.
///
/// The whole point of the guard is that "the agent changed it" and "the user
/// changed it" are different events, so authority is a parameter of the write
/// rather than something inferred from context. `Agent` is refused by every
/// destructive operation at this layer — not by a check in each tool — so a
/// new tool cannot forget to ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteAuthority {
    /// An explicit human action through an authenticated client.
    User { device: Option<String> },
    /// Startup mirroring: env var → keychain → store.
    System,
    /// A tool call. Create-only.
    Agent { conversation_id: Option<uuid::Uuid> },
}

impl WriteAuthority {
    /// Short description recorded in the audit trail.
    pub fn describe(&self) -> String {
        match self {
            WriteAuthority::User { device: Some(d) } => format!("user:{d}"),
            WriteAuthority::User { device: None } => "user".to_string(),
            WriteAuthority::System => "system".to_string(),
            WriteAuthority::Agent {
                conversation_id: Some(c),
            } => format!("agent:{c}"),
            WriteAuthority::Agent {
                conversation_id: None,
            } => "agent".to_string(),
        }
    }

    pub fn is_agent(&self) -> bool {
        matches!(self, WriteAuthority::Agent { .. })
    }
}

/// Per-secret metadata. Values are never included — no endpoint returns one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretMeta {
    pub name: String,
    /// Epoch milliseconds; `None` for rows written before versioning existed.
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub version: i64,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Encrypted credential store backed by SQLite.
///
/// Secrets are encrypted at rest using AES-256-GCM (authenticated encryption
/// with associated data). Each secret gets its own random salt and nonce,
/// stored alongside the ciphertext. The encryption key is derived from the
/// master key + a per-secret salt using Argon2id.
///
/// Storage format per entry: `[salt (16 bytes)][nonce (12 bytes)][ciphertext+tag]`
///
/// Properties:
/// - **Confidentiality**: AES-256 encryption
/// - **Integrity**: GCM authentication tag detects any tampering
/// - **Key hardening**: Argon2id makes brute-forcing the master key expensive
/// - **Unique keys**: Per-secret salt ensures identical plaintexts produce
///   different ciphertexts and compromising one key doesn't reveal others
#[derive(Clone)]
pub struct SecretStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
    master_key: Arc<Zeroizing<Vec<u8>>>,
}

impl SecretStore {
    pub(crate) fn new(
        conn: Arc<Mutex<rusqlite::Connection>>,
        master_key: Zeroizing<Vec<u8>>,
    ) -> Self {
        Self {
            conn,
            master_key: Arc::new(master_key),
        }
    }

    /// Store a **new** secret. Fails with [`Error::AlreadyExists`] if the
    /// name is taken.
    ///
    /// There is deliberately no `set()` that upserts: silently replacing a
    /// credential is exactly the behaviour the guard exists to prevent, so
    /// the API forces callers to say whether they mean create or overwrite,
    /// and overwrite demands an authority.
    ///
    /// The Argon2id key derivation and SQLite write both run on tokio's
    /// blocking pool — neither may occupy an async worker thread.
    pub async fn create(&self, name: &str, value: &str) -> Result<(), Error> {
        Self::validate_name(name)?;

        let store = self.clone();
        let name = name.to_string();
        let value = Zeroizing::new(value.to_string());
        run_blocking(move || {
            let encrypted = store.encrypt(&name, value.as_bytes())?;
            let conn = store.conn.lock().unwrap();
            let now = now_ms();
            let rows = conn
                .execute(
                    "INSERT INTO secrets (name, data, created_at, updated_at, version)
                     VALUES (?1, ?2, ?3, ?3, 1)
                     ON CONFLICT(name) DO NOTHING",
                    params![name, encrypted, now],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            if rows == 0 {
                return Err(Error::AlreadyExists(format!("secret '{name}'")));
            }
            Self::audit(&conn, &name, "create", "create", None)?;
            Ok(())
        })
        .await
    }

    /// Replace an existing secret, archiving the superseded value.
    ///
    /// Refuses [`WriteAuthority::Agent`]: an agent that wants to replace a
    /// credential must file a request instead (see `GuardedSecrets`).
    pub async fn overwrite(
        &self,
        name: &str,
        value: &str,
        authority: WriteAuthority,
    ) -> Result<(), Error> {
        Self::validate_name(name)?;
        if authority.is_agent() {
            return Err(Error::Auth(format!(
                "the agent cannot overwrite '{name}' directly"
            )));
        }

        let store = self.clone();
        let name = name.to_string();
        let value = Zeroizing::new(value.to_string());
        run_blocking(move || {
            let encrypted = store.encrypt(&name, value.as_bytes())?;
            let mut conn = store.conn.lock().unwrap();
            let tx = conn
                .transaction()
                .map_err(|e| Error::Storage(e.to_string()))?;
            let (previous, version) = Self::current_row(&tx, &name)?
                .ok_or_else(|| Error::NotFound(format!("secret '{name}'")))?;
            Self::archive(&tx, &name, version, &previous, &authority)?;
            tx.execute(
                "UPDATE secrets SET data = ?2, updated_at = ?3, version = ?4 WHERE name = ?1",
                params![name, encrypted, now_ms(), version + 1],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Self::audit(&tx, &name, "overwrite", &authority.describe(), None)?;
            tx.commit().map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
    }

    /// Create the secret if absent, replace it if the value actually
    /// changed, and do nothing when it already matches.
    ///
    /// This is the startup-mirroring path (env var → keychain → store).
    /// Skipping no-op writes matters: the registry runs on every boot, and
    /// rewriting an unchanged value would inflate the version and fill the
    /// audit trail with events nobody performed.
    pub async fn upsert_system(&self, name: &str, value: &str) -> Result<(), Error> {
        match self.get(name).await {
            Ok(existing) if existing == value => Ok(()),
            Ok(_) => self.overwrite(name, value, WriteAuthority::System).await,
            Err(Error::NotFound(_)) => match self.create(name, value).await {
                // Lost a race with another writer; the value is present.
                Err(Error::AlreadyExists(_)) => Ok(()),
                other => other,
            },
            Err(e) => Err(e),
        }
    }

    /// Retrieve and decrypt a secret by name.
    ///
    /// Runs on tokio's blocking pool — see [`SecretStore::set`].
    pub async fn get(&self, name: &str) -> Result<String, Error> {
        let store = self.clone();
        let name = name.to_string();
        run_blocking(move || {
            let encrypted: Vec<u8> = {
                let conn = store.conn.lock().unwrap();
                let mut stmt = conn
                    .prepare("SELECT data FROM secrets WHERE name = ?1")
                    .map_err(|e| Error::Storage(e.to_string()))?;
                stmt.query_row(params![name], |row| row.get(0))
                    .map_err(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => {
                            Error::NotFound(format!("secret '{name}'"))
                        }
                        other => Error::Storage(other.to_string()),
                    })?
            };
            let plaintext = store.decrypt(&name, &encrypted)?;
            String::from_utf8(plaintext)
                .map_err(|e| Error::Storage(format!("invalid utf-8 in secret: {e}")))
        })
        .await
    }

    /// Delete a secret, archiving the value first.
    ///
    /// Refuses [`WriteAuthority::Agent`], like [`overwrite`](Self::overwrite).
    /// The archived row means an accidental approval is recoverable.
    pub async fn delete(&self, name: &str, authority: WriteAuthority) -> Result<(), Error> {
        if authority.is_agent() {
            return Err(Error::Auth(format!(
                "the agent cannot delete '{name}' directly"
            )));
        }
        let store = self.clone();
        let name = name.to_string();
        run_blocking(move || {
            let mut conn = store.conn.lock().unwrap();
            let tx = conn
                .transaction()
                .map_err(|e| Error::Storage(e.to_string()))?;
            // Deleting something that isn't there is not an error, but
            // there is also nothing to archive or audit.
            if let Some((previous, version)) = Self::current_row(&tx, &name)? {
                Self::archive(&tx, &name, version, &previous, &authority)?;
                tx.execute("DELETE FROM secrets WHERE name = ?1", params![name])
                    .map_err(|e| Error::Storage(e.to_string()))?;
                Self::audit(&tx, &name, "delete", &authority.describe(), None)?;
            }
            tx.commit().map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
    }

    /// Names plus timestamps and version — never values.
    pub async fn metadata(&self) -> Result<Vec<SecretMeta>, Error> {
        crate::with_conn(&self.conn, |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT name, created_at, updated_at, COALESCE(version, 1)
                     FROM secrets ORDER BY name",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(SecretMeta {
                        name: row.get(0)?,
                        created_at: row.get(1)?,
                        updated_at: row.get(2)?,
                        version: row.get(3)?,
                    })
                })
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| Error::Storage(e.to_string()))?);
            }
            Ok(out)
        })
        .await
    }

    /// The current version number for a secret, or `None` if absent. Used to
    /// detect a credential that moved under a pending request.
    pub async fn version_of(&self, name: &str) -> Result<Option<i64>, Error> {
        let name = name.to_string();
        crate::with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare("SELECT COALESCE(version, 1) FROM secrets WHERE name = ?1")
                .map_err(|e| Error::Storage(e.to_string()))?;
            match stmt.query_row(params![name], |row| row.get::<_, i64>(0)) {
                Ok(v) => Ok(Some(v)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(Error::Storage(e.to_string())),
            }
        })
        .await
    }

    // -- internals shared by the write paths -----------------------------

    /// Current ciphertext and version for a name.
    fn current_row(
        conn: &rusqlite::Connection,
        name: &str,
    ) -> Result<Option<(Vec<u8>, i64)>, Error> {
        let mut stmt = conn
            .prepare("SELECT data, COALESCE(version, 1) FROM secrets WHERE name = ?1")
            .map_err(|e| Error::Storage(e.to_string()))?;
        match stmt.query_row(params![name], |row| Ok((row.get(0)?, row.get(1)?))) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(Error::Storage(e.to_string())),
        }
    }

    /// Copy the superseded ciphertext into `secret_versions`. Stored still
    /// encrypted — archiving must not turn history into plaintext.
    fn archive(
        conn: &rusqlite::Connection,
        name: &str,
        version: i64,
        data: &[u8],
        authority: &WriteAuthority,
    ) -> Result<(), Error> {
        conn.execute(
            "INSERT OR REPLACE INTO secret_versions
                (name, version, data, replaced_at, replaced_by)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, version, data, now_ms(), authority.describe()],
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    fn audit(
        conn: &rusqlite::Connection,
        name: &str,
        op: &str,
        authority: &str,
        request_id: Option<&str>,
    ) -> Result<(), Error> {
        conn.execute(
            "INSERT INTO secret_audit (name, op, authority, request_id, at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, op, authority, request_id, now_ms()],
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// List all secret names (does not decrypt values).
    pub async fn list_names(&self) -> Result<Vec<String>, Error> {
        crate::with_conn(&self.conn, |conn| {
            let mut stmt = conn
                .prepare("SELECT name FROM secrets")
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| row.get(0))
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut names = Vec::new();
            for row in rows {
                names.push(row.map_err(|e| Error::Storage(e.to_string()))?);
            }
            Ok(names)
        })
        .await
    }

    /// Validate that a secret name is well-formed.
    fn validate_name(name: &str) -> Result<(), Error> {
        if name.is_empty() || name.len() > 256 {
            return Err(Error::Storage(
                "secret name must be 1-256 characters".into(),
            ));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        {
            return Err(Error::Storage(
                "secret name must contain only alphanumeric characters, underscores, hyphens, and dots".into(),
            ));
        }
        Ok(())
    }

    /// Encrypt a payload that isn't a secret row, binding it to `aad`.
    ///
    /// Used for queued credential proposals, where the AAD is the request
    /// id — so a proposal cannot be lifted into a different request or
    /// applied to a different name.
    pub(crate) fn encrypt_with_aad(&self, aad: &str, data: &[u8]) -> Result<Vec<u8>, Error> {
        self.encrypt(aad, data)
    }

    pub(crate) fn decrypt_with_aad(&self, aad: &str, data: &[u8]) -> Result<Vec<u8>, Error> {
        self.decrypt(aad, data)
    }

    /// Encrypt data with AES-256-GCM. Returns `salt || nonce || ciphertext+tag`.
    fn encrypt(&self, key_name: &str, data: &[u8]) -> Result<Vec<u8>, Error> {
        let mut salt = [0u8; SALT_LEN];
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rngs::OsRng
            .try_fill_bytes(&mut salt)
            .expect("OS RNG failed");
        rand::rngs::OsRng
            .try_fill_bytes(&mut nonce_bytes)
            .expect("OS RNG failed");

        let derived_key = self.derive_key(&salt)?;
        let cipher = Aes256Gcm::new_from_slice(&*derived_key)
            .map_err(|e| Error::Storage(format!("cipher init: {e}")))?;

        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(
                nonce,
                aes_gcm::aead::Payload {
                    msg: data,
                    aad: key_name.as_bytes(),
                },
            )
            .map_err(|e| Error::Storage(format!("encryption failed: {e}")))?;

        let mut packed = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
        packed.extend_from_slice(&salt);
        packed.extend_from_slice(&nonce_bytes);
        packed.extend_from_slice(&ciphertext);
        Ok(packed)
    }

    /// Decrypt data. Input format: `salt || nonce || ciphertext+tag`.
    fn decrypt(&self, key_name: &str, data: &[u8]) -> Result<Vec<u8>, Error> {
        if data.len() < SALT_LEN + NONCE_LEN {
            return Err(Error::Storage("ciphertext too short".into()));
        }

        let salt = &data[..SALT_LEN];
        let nonce_bytes = &data[SALT_LEN..SALT_LEN + NONCE_LEN];
        let ciphertext = &data[SALT_LEN + NONCE_LEN..];

        let derived_key = self.derive_key(salt)?;
        let cipher = Aes256Gcm::new_from_slice(&*derived_key)
            .map_err(|e| Error::Storage(format!("cipher init: {e}")))?;

        let nonce = Nonce::from_slice(nonce_bytes);

        cipher
            .decrypt(
                nonce,
                aes_gcm::aead::Payload {
                    msg: ciphertext,
                    aad: key_name.as_bytes(),
                },
            )
            .map_err(|e| {
                Error::Storage(format!(
                    "decryption failed (wrong key or tampered data): {e}"
                ))
            })
    }

    /// Derive a 256-bit encryption key from the master key + salt using Argon2id.
    fn derive_key(&self, salt: &[u8]) -> Result<Zeroizing<[u8; 32]>, Error> {
        let mut key = Zeroizing::new([0u8; 32]);
        Argon2::default()
            .hash_password_into(&self.master_key, salt, &mut *key)
            .map_err(|e| Error::Storage(format!("key derivation failed: {e}")))?;
        Ok(key)
    }
}

/// Test-only views over the guard's bookkeeping tables. Production code
/// reads these through the request/approval API, not raw rows.
#[cfg(test)]
impl SecretStore {
    /// The most recently archived ciphertext for a name, and how many
    /// archived versions exist.
    async fn archived(&self, name: &str) -> (Vec<u8>, usize) {
        let name = name.to_string();
        crate::with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare("SELECT data FROM secret_versions WHERE name = ?1 ORDER BY version")
                .unwrap();
            let rows: Vec<Vec<u8>> = stmt
                .query_map(params![name], |row| row.get::<_, Vec<u8>>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            Ok((rows.last().cloned().unwrap_or_default(), rows.len()))
        })
        .await
        .unwrap()
    }

    async fn audit_ops(&self, name: &str) -> Vec<String> {
        let name = name.to_string();
        crate::with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare("SELECT op FROM secret_audit WHERE name = ?1 ORDER BY id")
                .unwrap();
            let rows: Vec<String> = stmt
                .query_map(params![name], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            Ok(rows)
        })
        .await
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    fn test_store() -> (tempfile::TempDir, SecretStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path(), vec![7u8; 32]).expect("open store");
        let secrets = store.secrets();
        (dir, secrets)
    }

    fn user() -> WriteAuthority {
        WriteAuthority::User { device: None }
    }

    #[tokio::test]
    async fn create_is_create_only() {
        let (_dir, secrets) = test_store();
        secrets.create("token", "first").await.unwrap();

        let again = secrets.create("token", "second").await;
        assert!(
            matches!(again, Err(Error::AlreadyExists(_))),
            "second create should be refused, got {again:?}"
        );
        // The refusal must not have touched the stored value.
        assert_eq!(secrets.get("token").await.unwrap(), "first");
    }

    #[tokio::test]
    async fn overwrite_archives_the_previous_value() {
        let (_dir, secrets) = test_store();
        secrets.create("token", "v1").await.unwrap();
        secrets.overwrite("token", "v2", user()).await.unwrap();

        assert_eq!(secrets.get("token").await.unwrap(), "v2");
        assert_eq!(secrets.version_of("token").await.unwrap(), Some(2));

        // The superseded value is archived, still encrypted.
        let (data, count) = secrets.archived("token").await;
        assert_eq!(count, 1, "expected one archived version");
        assert!(
            !String::from_utf8_lossy(&data).contains("v1"),
            "archived value must not be plaintext"
        );
    }

    #[tokio::test]
    async fn agent_authority_cannot_overwrite_or_delete() {
        let (_dir, secrets) = test_store();
        secrets.create("token", "original").await.unwrap();
        let agent = WriteAuthority::Agent {
            conversation_id: None,
        };

        let overwrite = secrets.overwrite("token", "hijacked", agent.clone()).await;
        assert!(matches!(overwrite, Err(Error::Auth(_))), "{overwrite:?}");

        let delete = secrets.delete("token", agent).await;
        assert!(matches!(delete, Err(Error::Auth(_))), "{delete:?}");

        // Neither attempt changed anything.
        assert_eq!(secrets.get("token").await.unwrap(), "original");
        assert_eq!(secrets.version_of("token").await.unwrap(), Some(1));
    }

    #[tokio::test]
    async fn delete_archives_then_removes() {
        let (_dir, secrets) = test_store();
        secrets.create("token", "bye").await.unwrap();
        secrets.delete("token", user()).await.unwrap();

        assert!(matches!(
            secrets.get("token").await,
            Err(Error::NotFound(_))
        ));
        assert_eq!(secrets.archived("token").await.1, 1);
        // Deleting again is a no-op rather than an error.
        secrets.delete("token", user()).await.unwrap();
    }

    #[tokio::test]
    async fn metadata_reports_version_and_timestamps_but_no_values() {
        let (_dir, secrets) = test_store();
        secrets.create("a", "value-a").await.unwrap();
        secrets.overwrite("a", "value-a2", user()).await.unwrap();
        secrets.create("b", "value-b").await.unwrap();

        let meta = secrets.metadata().await.unwrap();
        let names: Vec<_> = meta.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);

        let a = &meta[0];
        assert_eq!(a.version, 2);
        assert!(a.created_at.is_some() && a.updated_at.is_some());
        assert!(a.updated_at >= a.created_at);
        assert_eq!(meta[1].version, 1);
    }

    #[tokio::test]
    async fn system_upsert_skips_unchanged_values() {
        let (_dir, secrets) = test_store();
        // First boot creates it.
        secrets.upsert_system("mirrored", "same").await.unwrap();
        assert_eq!(secrets.version_of("mirrored").await.unwrap(), Some(1));

        // Later boots with an identical value must not churn the version
        // or fill the audit trail with writes nobody made.
        secrets.upsert_system("mirrored", "same").await.unwrap();
        assert_eq!(secrets.version_of("mirrored").await.unwrap(), Some(1));

        // A genuinely changed value does write.
        secrets.upsert_system("mirrored", "rotated").await.unwrap();
        assert_eq!(secrets.version_of("mirrored").await.unwrap(), Some(2));
        assert_eq!(secrets.get("mirrored").await.unwrap(), "rotated");
    }

    #[tokio::test]
    async fn writes_are_audited() {
        let (_dir, secrets) = test_store();
        secrets.create("token", "v1").await.unwrap();
        secrets.overwrite("token", "v2", user()).await.unwrap();
        secrets.delete("token", user()).await.unwrap();

        let ops = secrets.audit_ops("token").await;
        assert_eq!(ops, vec!["create", "overwrite", "delete"]);
    }
}
