//! macOS Keychain integration for master key storage.
//!
//! On macOS, the master encryption key is stored in the system Keychain
//! rather than in an environment variable. The Keychain is protected by:
//!
//! - The user's login password (required to unlock)
//! - The Secure Enclave on Apple Silicon Macs (hardware-backed protection)
//! - Touch ID (if enabled, macOS prompts biometric auth for keychain access)
//! - Per-app ACLs (only OpenClaw can read its own keychain item)
//!
//! On first launch, if no master key exists in the Keychain or env var,
//! a random 32-byte key is generated and stored in the Keychain. On
//! subsequent launches, the key is retrieved from the Keychain — macOS
//! may prompt for Touch ID or password depending on system settings.

use openclaw_core::Error;

#[cfg(target_os = "macos")]
const SERVICE_NAME: &str = "com.openclaw.master-key";
#[cfg(target_os = "macos")]
const ACCOUNT_NAME: &str = "openclaw-encryption-key";

/// Retrieve the master key from the macOS Keychain.
///
/// Returns `None` if no key is stored yet.
#[cfg(target_os = "macos")]
pub fn get_master_key() -> Result<Option<Vec<u8>>, Error> {
    use security_framework::passwords::get_generic_password;

    match get_generic_password(SERVICE_NAME, ACCOUNT_NAME) {
        Ok(bytes) => {
            // The key is stored as hex — decode it.
            let hex_str = String::from_utf8(bytes.to_vec())
                .map_err(|e| Error::Storage(format!("keychain: invalid utf-8: {e}")))?;
            let key = hex::decode(hex_str.trim())
                .map_err(|e| Error::Storage(format!("keychain: invalid hex: {e}")))?;
            Ok(Some(key))
        }
        Err(e) => {
            let msg = e.to_string();
            // "The specified item could not be found in the keychain" means
            // no key has been stored yet — not an error.
            if msg.contains("could not be found") || msg.contains("errSecItemNotFound") {
                Ok(None)
            } else {
                Err(Error::Storage(format!("keychain read failed: {e}")))
            }
        }
    }
}

/// Store the master key in the macOS Keychain.
///
/// If a key already exists, it is updated. The key is stored as hex.
#[cfg(target_os = "macos")]
pub fn set_master_key(key: &[u8]) -> Result<(), Error> {
    use security_framework::passwords::{delete_generic_password, set_generic_password};

    let hex_key = hex::encode(key);

    // Try to delete any existing item first (set_generic_password errors on duplicates).
    let _ = delete_generic_password(SERVICE_NAME, ACCOUNT_NAME);

    set_generic_password(SERVICE_NAME, ACCOUNT_NAME, hex_key.as_bytes())
        .map_err(|e| Error::Storage(format!("keychain write failed: {e}")))?;

    Ok(())
}

/// Delete the master key from the Keychain.
#[cfg(target_os = "macos")]
pub fn delete_master_key() -> Result<(), Error> {
    use security_framework::passwords::delete_generic_password;

    delete_generic_password(SERVICE_NAME, ACCOUNT_NAME)
        .map_err(|e| Error::Storage(format!("keychain delete failed: {e}")))?;
    Ok(())
}

/// Retrieve or generate the master key using the macOS Keychain.
///
/// 1. Try env var `OPENCLAW_MASTER_KEY`
/// 2. Try macOS Keychain
/// 3. Generate a new random key and store it in the Keychain
///
/// This is the primary entry point for CLI startup.
#[cfg(target_os = "macos")]
pub fn resolve_master_key() -> Result<Vec<u8>, Error> {
    // Priority 1: environment variable (for CI, Docker, non-macOS deployments).
    if let Ok(env_key) = std::env::var("OPENCLAW_MASTER_KEY") {
        tracing::info!("using master key from OPENCLAW_MASTER_KEY env var");
        return hex::decode(env_key.trim())
            .map_err(|e| Error::Storage(format!(
                "OPENCLAW_MASTER_KEY must be a hex-encoded string: {e}"
            )));
    }

    // Priority 2: macOS Keychain.
    if let Some(key) = get_master_key()? {
        tracing::info!("master key loaded from macOS Keychain (Secure Enclave backed)");
        return Ok(key);
    }

    // Priority 3: generate and store a new key.
    tracing::info!("no master key found — generating and storing in macOS Keychain");
    let mut key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut key);
    set_master_key(&key)?;
    tracing::info!(
        "master key stored in macOS Keychain under '{SERVICE_NAME}'. \
         It is protected by your login password and Touch ID."
    );
    Ok(key.to_vec())
}

/// Non-macOS fallback: use env var or generate an ephemeral key.
#[cfg(not(target_os = "macos"))]
pub fn resolve_master_key() -> Result<Vec<u8>, Error> {
    if let Ok(env_key) = std::env::var("OPENCLAW_MASTER_KEY") {
        tracing::info!("using master key from OPENCLAW_MASTER_KEY env var");
        return hex::decode(env_key.trim())
            .map_err(|e| Error::Storage(format!(
                "OPENCLAW_MASTER_KEY must be a hex-encoded string: {e}"
            )));
    }

    tracing::warn!(
        "OPENCLAW_MASTER_KEY not set and macOS Keychain not available — \
         generating ephemeral key. Secrets will not survive restart."
    );
    let mut key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut key);
    Ok(key.to_vec())
}
