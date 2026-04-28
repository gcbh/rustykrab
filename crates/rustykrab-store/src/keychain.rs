//! macOS Keychain integration for credential storage.
//!
//! Uses the **Data Protection Keychain** (`kSecUseDataProtectionKeychain`)
//! available on macOS 10.15+. Unlike the legacy keychain, the Data Protection
//! Keychain does **not** use per-application ACLs, so credentials are
//! accessible to any process running as the current user without password
//! prompts or code-signing requirements. Items are protected by the user's
//! login session — they are available after first unlock (i.e. once the user
//! logs in after boot) and do not require further interaction.
//!
//! This means:
//! - No password prompts when the binary is rebuilt during development
//! - No Touch ID / biometric gates on credential reads
//! - Credentials survive restarts without environment variables
//! - Items are still encrypted at rest by macOS and tied to the user account

use rustykrab_core::Error;

#[cfg(target_os = "macos")]
const SERVICE_NAME: &str = "com.rustykrab.master-key";
#[cfg(target_os = "macos")]
const ACCOUNT_NAME: &str = "rustykrab-encryption-key";

// ---------------------------------------------------------------------------
// Internal helpers — Data Protection Keychain read/write
// ---------------------------------------------------------------------------

/// Read a generic password from the keychain.
///
/// Tries the Data Protection Keychain first; falls back to the legacy
/// keychain if the entitlement required for the Data Protection Keychain
/// is absent (ad-hoc or unsigned binaries without a Developer Team ID).
///
/// Returns `Ok(None)` when the item does not exist (errSecItemNotFound).
#[cfg(target_os = "macos")]
fn dp_get(service: &str, account: &str) -> Result<Option<Vec<u8>>, Error> {
    use security_framework::passwords::{generic_password, get_generic_password, PasswordOptions};

    let mut opts = PasswordOptions::new_generic_password(service, account);
    opts.use_protected_keychain();

    match generic_password(opts) {
        Ok(bytes) => return Ok(Some(bytes)),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("could not be found") || msg.contains("errSecItemNotFound") {
                // Not in the Data Protection Keychain; try legacy below.
            } else if msg.contains("entitlement") || msg.contains("-34018") {
                // No Data Protection Keychain entitlement — fall through to legacy.
                tracing::debug!("Data Protection Keychain not available, using legacy keychain");
            } else {
                return Err(Error::Storage(format!(
                    "keychain read failed for {service}/{account}: {e}"
                )));
            }
        }
    }

    // Legacy keychain fallback.
    match get_generic_password(service, account) {
        Ok(bytes) => {
            // Auto-migrate: write to Data Protection Keychain so future reads
            // skip the legacy keychain (and its per-app ACL prompts).
            if let Err(e) = dp_set(service, account, &bytes) {
                tracing::debug!(
                    "auto-migrate to Data Protection Keychain failed for \
                     {service}/{account}: {e}"
                );
            } else {
                tracing::info!(
                    "migrated {service}/{account} from legacy to Data Protection Keychain"
                );
            }
            Ok(Some(bytes))
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("could not be found") || msg.contains("errSecItemNotFound") {
                Ok(None)
            } else {
                Err(Error::Storage(format!(
                    "keychain read failed for {service}/{account}: {e}"
                )))
            }
        }
    }
}

/// Write a generic password to the keychain.
///
/// Tries the Data Protection Keychain first; falls back to the legacy
/// keychain if the entitlement is absent (ad-hoc signed binaries).
#[cfg(target_os = "macos")]
fn dp_set(service: &str, account: &str, password: &[u8]) -> Result<(), Error> {
    use security_framework::passwords::{
        delete_generic_password, delete_generic_password_options, set_generic_password,
        set_generic_password_options, PasswordOptions,
    };

    // Try Data Protection Keychain first.
    let mut del_opts = PasswordOptions::new_generic_password(service, account);
    del_opts.use_protected_keychain();
    let _ = delete_generic_password_options(del_opts);

    let mut opts = PasswordOptions::new_generic_password(service, account);
    opts.use_protected_keychain();
    match set_generic_password_options(password, opts) {
        Ok(()) => return Ok(()),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("entitlement") || msg.contains("-34018") {
                tracing::debug!("Data Protection Keychain not available, using legacy keychain");
            } else {
                return Err(Error::Storage(format!(
                    "keychain write failed for {service}/{account}: {e}"
                )));
            }
        }
    }

    // Legacy keychain fallback: delete then add to avoid duplicate errors.
    let _ = delete_generic_password(service, account);
    set_generic_password(service, account, password).map_err(|e| {
        Error::Storage(format!(
            "keychain write failed for {service}/{account}: {e}"
        ))
    })
}

/// Delete a generic password from the keychain (Data Protection, then legacy).
#[cfg(target_os = "macos")]
fn dp_delete(service: &str, account: &str) -> Result<(), Error> {
    use security_framework::passwords::{
        delete_generic_password, delete_generic_password_options, PasswordOptions,
    };

    let mut opts = PasswordOptions::new_generic_password(service, account);
    opts.use_protected_keychain();
    match delete_generic_password_options(opts) {
        Ok(()) => return Ok(()),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("entitlement") || msg.contains("-34018") {
                tracing::debug!("Data Protection Keychain not available, using legacy keychain");
            } else {
                return Err(Error::Storage(format!(
                    "keychain delete failed for {service}/{account}: {e}"
                )));
            }
        }
    }

    delete_generic_password(service, account).map_err(|e| {
        Error::Storage(format!(
            "keychain delete failed for {service}/{account}: {e}"
        ))
    })
}

// ---------------------------------------------------------------------------
// Master key — the encryption key for the local SecretStore
// ---------------------------------------------------------------------------

/// Retrieve the master key from the macOS Keychain.
///
/// Returns `None` if no key is stored yet.
#[cfg(target_os = "macos")]
pub fn get_master_key() -> Result<Option<Vec<u8>>, Error> {
    match dp_get(SERVICE_NAME, ACCOUNT_NAME)? {
        Some(bytes) => {
            let hex_str = String::from_utf8(bytes)
                .map_err(|e| Error::Storage(format!("keychain: invalid utf-8: {e}")))?;
            let key = hex::decode(hex_str.trim())
                .map_err(|e| Error::Storage(format!("keychain: invalid hex: {e}")))?;
            Ok(Some(key))
        }
        None => Ok(None),
    }
}

/// Store the master key in the macOS Keychain (as hex).
#[cfg(target_os = "macos")]
pub fn set_master_key(key: &[u8]) -> Result<(), Error> {
    dp_set(SERVICE_NAME, ACCOUNT_NAME, hex::encode(key).as_bytes())
}

/// Delete the master key from the Keychain.
#[cfg(target_os = "macos")]
pub fn delete_master_key() -> Result<(), Error> {
    dp_delete(SERVICE_NAME, ACCOUNT_NAME)
}

/// Retrieve or generate the master key using the macOS Keychain.
///
/// 1. Try env var `RUSTYKRAB_MASTER_KEY`
/// 2. Try macOS Keychain (Data Protection Keychain — no password prompt)
/// 3. Generate a new random key and store it in the Keychain
///
/// This is the primary entry point for CLI startup.
#[cfg(target_os = "macos")]
pub fn resolve_master_key() -> Result<Vec<u8>, Error> {
    // Priority 1: environment variable (for CI, Docker, non-macOS deployments).
    if let Ok(env_key) = std::env::var("RUSTYKRAB_MASTER_KEY") {
        tracing::info!("using master key from RUSTYKRAB_MASTER_KEY env var");
        return hex::decode(env_key.trim()).map_err(|e| {
            Error::Storage(format!(
                "RUSTYKRAB_MASTER_KEY must be a hex-encoded string: {e}"
            ))
        });
    }

    // Priority 2: macOS Data Protection Keychain (no password prompt).
    if let Some(key) = get_master_key()? {
        tracing::info!("master key loaded from macOS Keychain (Data Protection)");
        return Ok(key);
    }

    // Priority 3: generate and store a new key.
    tracing::info!("no master key found — generating and storing in macOS Keychain");
    let mut key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut key);
    set_master_key(&key)?;
    tracing::info!(
        "master key stored in macOS Keychain under '{SERVICE_NAME}' \
         (Data Protection Keychain — no password prompt on access)."
    );
    Ok(key.to_vec())
}

/// Non-macOS (Linux/Docker): the master key must come from the
/// `RUSTYKRAB_MASTER_KEY` environment variable.
///
/// There is no OS credential store integration on this platform — headless
/// Linux and Docker containers don't have a session-persistent secret backend
/// that's safe to rely on. If the env var is unset, the daemon refuses to
/// start rather than silently using an ephemeral key (which would render
/// every encrypted secret in `store.db` unrecoverable on the next boot).
#[cfg(not(target_os = "macos"))]
pub fn resolve_master_key() -> Result<Vec<u8>, Error> {
    if let Ok(env_key) = std::env::var("RUSTYKRAB_MASTER_KEY") {
        let trimmed = env_key.trim();
        if !trimmed.is_empty() {
            tracing::info!("using master key from RUSTYKRAB_MASTER_KEY env var");
            return hex::decode(trimmed).map_err(|e| {
                Error::Storage(format!(
                    "RUSTYKRAB_MASTER_KEY must be a hex-encoded string: {e}"
                ))
            });
        }
    }

    Err(Error::Storage(
        "RUSTYKRAB_MASTER_KEY is not set. On Linux/Docker the master key must \
         be provided via the RUSTYKRAB_MASTER_KEY environment variable \
         (generate one with `openssl rand -hex 32`). See the README section \
         \"Linux / Docker setup\" for systemd and Docker examples."
            .to_string(),
    ))
}

/// No OS credential store on this platform.
#[cfg(not(target_os = "macos"))]
pub fn get_master_key() -> Result<Option<Vec<u8>>, Error> {
    Ok(None)
}

/// No OS credential store on this platform.
#[cfg(not(target_os = "macos"))]
pub fn set_master_key(_key: &[u8]) -> Result<(), Error> {
    Err(Error::Storage(
        "OS credential store is not supported on this platform — set \
         RUSTYKRAB_MASTER_KEY in the environment instead"
            .to_string(),
    ))
}

/// No OS credential store on this platform.
#[cfg(not(target_os = "macos"))]
pub fn delete_master_key() -> Result<(), Error> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Generic credential access — read arbitrary credentials from macOS Keychain
// for use during remote deployment.
// ---------------------------------------------------------------------------

/// A credential retrieved from the macOS Keychain.
#[derive(Debug, Clone)]
pub struct KeychainCredential {
    pub service: String,
    pub account: String,
    /// The raw password / secret value.
    pub value: String,
}

/// Returns `true` on macOS (Data Protection Keychain), `false` elsewhere.
#[cfg(target_os = "macos")]
pub fn keychain_available() -> bool {
    true
}

/// No OS credential store on this platform.
///
/// rustykrab targets headless Linux and Docker, where there is no
/// session-persistent secret backend that's safe to rely on. Credentials
/// must come from `RUSTYKRAB_MASTER_KEY` + the encrypted SQLite store
/// (or per-credential `RUSTYKRAB_*` env vars).
#[cfg(not(target_os = "macos"))]
pub fn keychain_available() -> bool {
    false
}

/// Retrieve a credential from the macOS Keychain by service and account.
///
/// Uses the Data Protection Keychain — no password prompt or per-app ACL.
/// `service` corresponds to the "Where" field visible in Keychain Access,
/// and `account` to the "Account" field.
///
/// Returns `Ok(None)` when the item does not exist.
#[cfg(target_os = "macos")]
pub fn get_credential(service: &str, account: &str) -> Result<Option<KeychainCredential>, Error> {
    match dp_get(service, account)? {
        Some(bytes) => {
            let value = String::from_utf8(bytes).map_err(|e| {
                Error::Storage(format!("keychain: credential is not valid utf-8: {e}"))
            })?;
            Ok(Some(KeychainCredential {
                service: service.to_string(),
                account: account.to_string(),
                value,
            }))
        }
        None => Ok(None),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn get_credential(_service: &str, _account: &str) -> Result<Option<KeychainCredential>, Error> {
    Ok(None)
}

/// Store a credential in the macOS Keychain under the given service/account.
///
/// Uses the Data Protection Keychain — no password prompt or per-app ACL.
/// If a credential already exists for this service/account pair, it is
/// replaced.
#[cfg(target_os = "macos")]
pub fn set_credential(service: &str, account: &str, value: &str) -> Result<(), Error> {
    dp_set(service, account, value.as_bytes())
}

#[cfg(not(target_os = "macos"))]
pub fn set_credential(_service: &str, _account: &str, _value: &str) -> Result<(), Error> {
    Err(Error::Storage(
        "OS credential store is not supported on this platform — use \
         RUSTYKRAB_* environment variables or the encrypted secrets store"
            .to_string(),
    ))
}

/// Delete a credential from the macOS Keychain.
#[cfg(target_os = "macos")]
pub fn delete_credential(service: &str, account: &str) -> Result<(), Error> {
    dp_delete(service, account)
}

#[cfg(not(target_os = "macos"))]
pub fn delete_credential(_service: &str, _account: &str) -> Result<(), Error> {
    Ok(())
}

#[cfg(all(test, not(target_os = "macos")))]
mod non_mac_tests {
    use super::*;
    use std::sync::Mutex;

    // Tests in this module mutate `RUSTYKRAB_MASTER_KEY`. Run them serially
    // so they don't race each other when cargo test parallelises.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn keychain_available_is_false() {
        assert!(!keychain_available());
    }

    #[test]
    fn get_credential_returns_none() {
        assert!(get_credential("anything", "anything").unwrap().is_none());
        assert!(get_master_key().unwrap().is_none());
    }

    #[test]
    fn set_credential_returns_error() {
        assert!(set_credential("svc", "acct", "value").is_err());
        assert!(set_master_key(&[0u8; 32]).is_err());
    }

    #[test]
    fn delete_is_noop() {
        assert!(delete_credential("svc", "acct").is_ok());
        assert!(delete_master_key().is_ok());
    }

    #[test]
    fn resolve_master_key_uses_env_var() {
        let _g = ENV_LOCK.lock().unwrap();
        let hex = "0".repeat(64);
        std::env::set_var("RUSTYKRAB_MASTER_KEY", &hex);
        let key = resolve_master_key().expect("env-var path should succeed");
        std::env::remove_var("RUSTYKRAB_MASTER_KEY");
        assert_eq!(key, vec![0u8; 32]);
    }

    #[test]
    fn resolve_master_key_errors_when_unset() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("RUSTYKRAB_MASTER_KEY");
        let err = resolve_master_key().expect_err("unset env var must error");
        assert!(
            err.to_string().contains("RUSTYKRAB_MASTER_KEY"),
            "error message should name the env var: {err}"
        );
    }

    #[test]
    fn resolve_master_key_errors_when_empty() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("RUSTYKRAB_MASTER_KEY", "   ");
        let err = resolve_master_key().expect_err("blank env var must error");
        std::env::remove_var("RUSTYKRAB_MASTER_KEY");
        assert!(err.to_string().contains("RUSTYKRAB_MASTER_KEY"));
    }

    #[test]
    fn resolve_master_key_rejects_invalid_hex() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("RUSTYKRAB_MASTER_KEY", "not-hex");
        let err = resolve_master_key().expect_err("non-hex env var must error");
        std::env::remove_var("RUSTYKRAB_MASTER_KEY");
        assert!(err.to_string().contains("hex"));
    }
}
