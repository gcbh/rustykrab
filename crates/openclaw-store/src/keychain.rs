//! macOS Keychain integration for credential storage.
//!
//! Uses the **login keychain** — the default keychain that macOS unlocks
//! automatically when the user logs in. This avoids the
//! `keychain-access-groups` entitlement requirement imposed by the Data
//! Protection Keychain (`kSecUseDataProtectionKeychain`), which only works
//! for properly signed and entitled binaries — not cargo-built debug/release
//! builds.
//!
//! This means:
//! - No entitlements or codesigning required for development builds
//! - No password prompts when the binary is rebuilt during development
//! - No Touch ID / biometric gates on credential reads
//! - Credentials survive restarts without environment variables
//! - Items are still encrypted at rest by macOS and tied to the user account

use openclaw_core::Error;

#[cfg(target_os = "macos")]
const SERVICE_NAME: &str = "com.openclaw.master-key";
#[cfg(target_os = "macos")]
const ACCOUNT_NAME: &str = "openclaw-encryption-key";

// ---------------------------------------------------------------------------
// Internal helpers — login keychain read/write
// ---------------------------------------------------------------------------

/// Read a generic password from the login keychain.
///
/// Returns `Ok(None)` when the item does not exist (errSecItemNotFound).
#[cfg(target_os = "macos")]
fn kc_get(service: &str, account: &str) -> Result<Option<Vec<u8>>, Error> {
    use security_framework::passwords::{generic_password, PasswordOptions};

    let opts = PasswordOptions::new_generic_password(service, account);

    match generic_password(opts) {
        Ok(bytes) => Ok(Some(bytes)),
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

/// Write a generic password to the login keychain.
///
/// Deletes any existing item first to avoid duplicate-item errors.
#[cfg(target_os = "macos")]
fn kc_set(service: &str, account: &str, password: &[u8]) -> Result<(), Error> {
    use security_framework::passwords::{
        delete_generic_password_options, set_generic_password_options, PasswordOptions,
    };

    // Delete any existing item (ignore "not found").
    let del_opts = PasswordOptions::new_generic_password(service, account);
    let _ = delete_generic_password_options(del_opts);

    let opts = PasswordOptions::new_generic_password(service, account);
    set_generic_password_options(password, opts)
        .map_err(|e| Error::Storage(format!("keychain write failed for {service}/{account}: {e}")))
}

/// Delete a generic password from the login keychain.
#[cfg(target_os = "macos")]
fn kc_delete(service: &str, account: &str) -> Result<(), Error> {
    use security_framework::passwords::{delete_generic_password_options, PasswordOptions};

    let opts = PasswordOptions::new_generic_password(service, account);
    delete_generic_password_options(opts)
        .map_err(|e| Error::Storage(format!("keychain delete failed for {service}/{account}: {e}")))
}

// ---------------------------------------------------------------------------
// Master key — the encryption key for the local SecretStore
// ---------------------------------------------------------------------------

/// Retrieve the master key from the macOS Keychain.
///
/// Returns `None` if no key is stored yet.
#[cfg(target_os = "macos")]
pub fn get_master_key() -> Result<Option<Vec<u8>>, Error> {
    match kc_get(SERVICE_NAME, ACCOUNT_NAME)? {
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
    kc_set(SERVICE_NAME, ACCOUNT_NAME, hex::encode(key).as_bytes())
}

/// Delete the master key from the Keychain.
#[cfg(target_os = "macos")]
pub fn delete_master_key() -> Result<(), Error> {
    kc_delete(SERVICE_NAME, ACCOUNT_NAME)
}

/// Retrieve or generate the master key using the macOS Keychain.
///
/// 1. Try env var `OPENCLAW_MASTER_KEY`
/// 2. Try macOS login keychain (no password prompt)
/// 3. Generate a new random key and store it in the Keychain
///
/// This is the primary entry point for CLI startup.
#[cfg(target_os = "macos")]
pub fn resolve_master_key() -> Result<Vec<u8>, Error> {
    // Priority 1: environment variable (for CI, Docker, non-macOS deployments).
    if let Ok(env_key) = std::env::var("OPENCLAW_MASTER_KEY") {
        tracing::info!("using master key from OPENCLAW_MASTER_KEY env var");
        return hex::decode(env_key.trim()).map_err(|e| {
            Error::Storage(format!(
                "OPENCLAW_MASTER_KEY must be a hex-encoded string: {e}"
            ))
        });
    }

    // Priority 2: macOS login keychain (no password prompt).
    if let Some(key) = get_master_key()? {
        tracing::info!("master key loaded from macOS Keychain");
        return Ok(key);
    }

    // Priority 3: generate and store a new key.
    tracing::info!("no master key found — generating and storing in macOS Keychain");
    let mut key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut key);
    set_master_key(&key)?;
    tracing::info!(
        "master key stored in macOS Keychain under '{SERVICE_NAME}'"
    );
    Ok(key.to_vec())
}

/// Non-macOS fallback: use env var or generate an ephemeral key.
#[cfg(not(target_os = "macos"))]
pub fn resolve_master_key() -> Result<Vec<u8>, Error> {
    if let Ok(env_key) = std::env::var("OPENCLAW_MASTER_KEY") {
        tracing::info!("using master key from OPENCLAW_MASTER_KEY env var");
        return hex::decode(env_key.trim()).map_err(|e| {
            Error::Storage(format!(
                "OPENCLAW_MASTER_KEY must be a hex-encoded string: {e}"
            ))
        });
    }

    tracing::warn!(
        "OPENCLAW_MASTER_KEY not set and macOS Keychain not available — \
         generating ephemeral key. Secrets will not survive restart."
    );
    let mut key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut key);
    Ok(key.to_vec())
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

/// Returns `true` when the current platform supports Keychain credential
/// lookups (i.e. the binary was compiled for macOS).
pub fn keychain_available() -> bool {
    cfg!(target_os = "macos")
}

/// Retrieve a credential from the macOS Keychain by service and account.
///
/// Uses the login keychain — no entitlements or codesigning required.
/// `service` corresponds to the "Where" field visible in Keychain Access,
/// and `account` to the "Account" field.
///
/// Returns `Ok(None)` when the item does not exist.
#[cfg(target_os = "macos")]
pub fn get_credential(service: &str, account: &str) -> Result<Option<KeychainCredential>, Error> {
    match kc_get(service, account)? {
        Some(bytes) => {
            let value = String::from_utf8(bytes)
                .map_err(|e| Error::Storage(format!("keychain: credential is not valid utf-8: {e}")))?;
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
    Err(Error::Storage(
        "macOS Keychain is not available on this platform".into(),
    ))
}

/// Store a credential in the macOS Keychain under the given service/account.
///
/// Uses the login keychain — no entitlements or codesigning required.
/// If a credential already exists for this service/account pair, it is
/// replaced.
#[cfg(target_os = "macos")]
pub fn set_credential(service: &str, account: &str, value: &str) -> Result<(), Error> {
    kc_set(service, account, value.as_bytes())
}

#[cfg(not(target_os = "macos"))]
pub fn set_credential(_service: &str, _account: &str, _value: &str) -> Result<(), Error> {
    Err(Error::Storage(
        "macOS Keychain is not available on this platform".into(),
    ))
}

/// Delete a credential from the macOS Keychain.
#[cfg(target_os = "macos")]
pub fn delete_credential(service: &str, account: &str) -> Result<(), Error> {
    kc_delete(service, account)
}

#[cfg(not(target_os = "macos"))]
pub fn delete_credential(_service: &str, _account: &str) -> Result<(), Error> {
    Err(Error::Storage(
        "macOS Keychain is not available on this platform".into(),
    ))
}
