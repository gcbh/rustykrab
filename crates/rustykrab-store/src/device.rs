//! Paired devices and the one-time codes that create them.
//!
//! A device token is what the phone holds instead of the master token: it
//! is per-device, attributable in the audit trail, and revocable on its own
//! — the lost-phone story. Tokens and pairing codes are stored only as
//! SHA-256 hashes, so reading the database does not yield anything that can
//! authenticate.

use std::sync::Arc;
use std::sync::Mutex;

use rand::TryRngCore;
use rusqlite::params;
use rustykrab_core::crypto::constant_time_eq;
use rustykrab_core::Error;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// How long a pairing code is usable. Short on purpose: it is typed or
/// scanned within seconds of being shown.
pub const PAIRING_CODE_TTL_MS: i64 = 5 * 60 * 1000;

/// Characters used in pairing codes. No look-alikes (0/O, 1/I/L), because
/// these get read off a screen and typed by hand.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const CODE_LEN: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
}

/// Who a request authenticated as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// The master token from the environment or keychain.
    Master,
    /// A paired device.
    Device { id: String, name: String },
}

impl Principal {
    /// Label recorded against decisions this principal makes.
    pub fn describe(&self) -> String {
        match self {
            Principal::Master => "master".to_string(),
            Principal::Device { name, .. } => name.clone(),
        }
    }
}

#[derive(Clone)]
pub struct DeviceStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn hash(value: &str) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .expect("OS RNG failed");
    hex::encode(bytes)
}

impl DeviceStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }

    /// Mint a single-use pairing code, returning the plaintext to display.
    /// Only its hash is stored.
    pub async fn mint_pairing_code(&self) -> Result<String, Error> {
        let mut bytes = [0u8; CODE_LEN];
        rand::rngs::OsRng
            .try_fill_bytes(&mut bytes)
            .expect("OS RNG failed");
        let code: String = bytes
            .iter()
            .map(|b| CODE_ALPHABET[*b as usize % CODE_ALPHABET.len()] as char)
            .collect();

        let stored = code.clone();
        crate::with_conn(&self.conn, move |conn| {
            conn.execute(
                "INSERT INTO pairing_codes (code_hash, expires_at) VALUES (?1, ?2)",
                params![hash(&stored), now_ms() + PAIRING_CODE_TTL_MS],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await?;
        Ok(code)
    }

    /// Redeem a pairing code for a new device identity.
    ///
    /// The code is consumed whether or not it was valid for long, and the
    /// returned token is the only time the plaintext exists — the caller
    /// must hand it straight to the device.
    pub async fn redeem_pairing_code(
        &self,
        code: &str,
        device_name: &str,
    ) -> Result<(Device, String), Error> {
        let name = device_name.trim().to_string();
        if name.is_empty() || name.len() > 128 {
            return Err(Error::Auth("device name must be 1-128 characters".into()));
        }
        // Uppercase so a code typed in lower case still works.
        let code_hash = hash(&code.trim().to_uppercase());
        let token = random_token();
        let token_hash = hash(&token);
        let id = Uuid::new_v4().to_string();
        let device = Device {
            id: id.clone(),
            name: name.clone(),
            created_at: now_ms(),
            last_seen_at: None,
        };
        let created_at = device.created_at;

        crate::with_conn(&self.conn, move |conn| {
            // Single use: consuming the row and checking expiry in one
            // statement means two racing redemptions cannot both win.
            let consumed = conn
                .execute(
                    "DELETE FROM pairing_codes WHERE code_hash = ?1 AND expires_at > ?2",
                    params![code_hash, now_ms()],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            if consumed == 0 {
                return Err(Error::Auth(
                    "pairing code is invalid, already used, or expired".into(),
                ));
            }
            conn.execute(
                "INSERT INTO devices (id, name, token_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![id, name, token_hash, created_at],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await?;

        Ok((device, token))
    }

    /// Resolve a bearer token to a device, if it belongs to one that is
    /// still active. Also stamps `last_seen_at`.
    pub async fn authenticate(&self, token: &str) -> Result<Option<Principal>, Error> {
        let candidate = hash(token);
        crate::with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare("SELECT id, name, token_hash FROM devices WHERE revoked_at IS NULL")
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                })
                .map_err(|e| Error::Storage(e.to_string()))?;

            for row in rows {
                let (id, name, stored) = row.map_err(|e| Error::Storage(e.to_string()))?;
                // Constant-time compare of the hashes, so a timing signal
                // can't be used to search for a valid token.
                if stored.len() == candidate.len()
                    && constant_time_eq(&hex::encode(&stored), &hex::encode(&candidate))
                {
                    conn.execute(
                        "UPDATE devices SET last_seen_at = ?2 WHERE id = ?1",
                        params![id, now_ms()],
                    )
                    .map_err(|e| Error::Storage(e.to_string()))?;
                    return Ok(Some(Principal::Device { id, name }));
                }
            }
            Ok(None)
        })
        .await
    }

    pub async fn list(&self) -> Result<Vec<Device>, Error> {
        crate::with_conn(&self.conn, |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, created_at, last_seen_at FROM devices
                     WHERE revoked_at IS NULL ORDER BY created_at",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(Device {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        created_at: row.get(2)?,
                        last_seen_at: row.get(3)?,
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

    /// Revoke a device. Its token stops authenticating immediately.
    ///
    /// The row is kept (marked revoked) rather than deleted so audit
    /// entries naming the device still resolve.
    pub async fn revoke(&self, id: &str) -> Result<(), Error> {
        let id = id.to_string();
        crate::with_conn(&self.conn, move |conn| {
            let n = conn
                .execute(
                    "UPDATE devices SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
                    params![id, now_ms()],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            if n == 0 {
                return Err(Error::NotFound(format!("device '{id}'")));
            }
            Ok(())
        })
        .await
    }

    /// Record the APNs token a device wants notifications on.
    ///
    /// Replaces any previous token for that device: iOS reissues them, and
    /// keeping a stale one only produces failed sends.
    pub async fn set_push_token(&self, id: &str, push_token: &str) -> Result<(), Error> {
        let id = id.to_string();
        let push_token = push_token.to_string();
        crate::with_conn(&self.conn, move |conn| {
            let n = conn
                .execute(
                    "UPDATE devices SET push_token = ?2 WHERE id = ?1 AND revoked_at IS NULL",
                    params![id, push_token],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            if n == 0 {
                return Err(Error::NotFound(format!("device '{id}'")));
            }
            Ok(())
        })
        .await
    }

    /// Forget a push token Apple has told us is dead.
    pub async fn clear_push_token(&self, id: &str) -> Result<(), Error> {
        let id = id.to_string();
        crate::with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE devices SET push_token = NULL WHERE id = ?1",
                params![id],
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(())
        })
        .await
    }

    /// Active devices that have registered for notifications, as
    /// `(device id, push token)`.
    pub async fn with_push_tokens(&self) -> Result<Vec<(String, String)>, Error> {
        crate::with_conn(&self.conn, |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, push_token FROM devices
                     WHERE revoked_at IS NULL AND push_token IS NOT NULL",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(|e| Error::Storage(e.to_string()))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| Error::Storage(e.to_string()))?);
            }
            Ok(out)
        })
        .await
    }

    /// Drop expired pairing codes. Called opportunistically on mint.
    pub async fn sweep_expired_codes(&self) -> Result<usize, Error> {
        crate::with_conn(&self.conn, |conn| {
            let n = conn
                .execute(
                    "DELETE FROM pairing_codes WHERE expires_at <= ?1",
                    params![now_ms()],
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            Ok(n)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    fn devices() -> (tempfile::TempDir, DeviceStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path(), vec![3u8; 32]).expect("open");
        let devices = store.devices();
        (dir, devices)
    }

    #[tokio::test]
    async fn pairing_mints_a_working_device_token() {
        let (_dir, devices) = devices();
        let code = devices.mint_pairing_code().await.unwrap();
        assert_eq!(code.len(), CODE_LEN);

        let (device, token) = devices.redeem_pairing_code(&code, "Phone").await.unwrap();
        assert_eq!(device.name, "Phone");

        let principal = devices.authenticate(&token).await.unwrap();
        assert_eq!(
            principal,
            Some(Principal::Device {
                id: device.id.clone(),
                name: "Phone".to_string()
            })
        );
    }

    #[tokio::test]
    async fn a_code_works_exactly_once() {
        let (_dir, devices) = devices();
        let code = devices.mint_pairing_code().await.unwrap();
        devices.redeem_pairing_code(&code, "First").await.unwrap();

        let second = devices.redeem_pairing_code(&code, "Second").await;
        assert!(matches!(second, Err(Error::Auth(_))), "{second:?}");
        assert_eq!(devices.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_codes_and_tokens_are_rejected() {
        let (_dir, devices) = devices();
        assert!(devices
            .redeem_pairing_code("NOTACODE", "Phone")
            .await
            .is_err());
        assert_eq!(devices.authenticate("not-a-token").await.unwrap(), None);
    }

    #[tokio::test]
    async fn revoking_stops_the_token_working() {
        let (_dir, devices) = devices();
        let code = devices.mint_pairing_code().await.unwrap();
        let (device, token) = devices.redeem_pairing_code(&code, "Lost").await.unwrap();
        assert!(devices.authenticate(&token).await.unwrap().is_some());

        devices.revoke(&device.id).await.unwrap();

        // The lost-phone story: the same token no longer authenticates.
        assert_eq!(devices.authenticate(&token).await.unwrap(), None);
        assert!(devices.list().await.unwrap().is_empty());
        // Revoking twice is a NotFound rather than a silent success.
        assert!(matches!(
            devices.revoke(&device.id).await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn push_tokens_are_recorded_replaced_and_cleared() {
        let (_dir, devices) = devices();
        let code = devices.mint_pairing_code().await.unwrap();
        let (device, _) = devices.redeem_pairing_code(&code, "Phone").await.unwrap();

        // Nothing registered yet.
        assert!(devices.with_push_tokens().await.unwrap().is_empty());

        devices
            .set_push_token(&device.id, "apns-token-1")
            .await
            .unwrap();
        assert_eq!(
            devices.with_push_tokens().await.unwrap(),
            vec![(device.id.clone(), "apns-token-1".to_string())]
        );

        // iOS reissues tokens; the new one replaces the old.
        devices
            .set_push_token(&device.id, "apns-token-2")
            .await
            .unwrap();
        assert_eq!(
            devices.with_push_tokens().await.unwrap(),
            vec![(device.id.clone(), "apns-token-2".to_string())]
        );

        devices.clear_push_token(&device.id).await.unwrap();
        assert!(devices.with_push_tokens().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_revoked_device_stops_receiving_pushes() {
        let (_dir, devices) = devices();
        let code = devices.mint_pairing_code().await.unwrap();
        let (device, _) = devices.redeem_pairing_code(&code, "Lost").await.unwrap();
        devices
            .set_push_token(&device.id, "apns-token")
            .await
            .unwrap();

        devices.revoke(&device.id).await.unwrap();

        // A revoked phone must not keep getting approval prompts.
        assert!(devices.with_push_tokens().await.unwrap().is_empty());
        assert!(matches!(
            devices.set_push_token(&device.id, "new-token").await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn codes_are_case_insensitive_when_redeemed() {
        let (_dir, devices) = devices();
        let code = devices.mint_pairing_code().await.unwrap();
        // Typed in lower case off a screen.
        let redeemed = devices
            .redeem_pairing_code(&code.to_lowercase(), "Phone")
            .await;
        assert!(redeemed.is_ok(), "{redeemed:?}");
    }

    #[tokio::test]
    async fn last_seen_is_stamped_on_use() {
        let (_dir, devices) = devices();
        let code = devices.mint_pairing_code().await.unwrap();
        let (device, token) = devices.redeem_pairing_code(&code, "Phone").await.unwrap();
        assert!(devices.list().await.unwrap()[0].last_seen_at.is_none());

        devices.authenticate(&token).await.unwrap();

        let listed = devices.list().await.unwrap();
        assert_eq!(listed[0].id, device.id);
        assert!(listed[0].last_seen_at.is_some());
    }
}
