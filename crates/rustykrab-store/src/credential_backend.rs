//! Where a live credential is actually kept.
//!
//! A credential the user hands over does not belong in the database. It
//! belongs in whatever this machine offers that is purpose-built to hold
//! secrets — the macOS Keychain here, the Secret Service on a Linux
//! desktop, a password manager's CLI, a remote vault on a server.
//!
//! Those differ enough that the rest of the codebase should not know which
//! one it has. This is the seam: [`CredentialBackend`] is the whole
//! contract, and everything above it — the credential page, the fulfil
//! path, the tools that read credentials back — is written against the
//! trait rather than against a keychain.
//!
//! ## Adding a backend
//!
//! Implement five methods. There is no registration step and nothing to
//! change elsewhere; construct it and hand it to [`crate::Store::with_credential_backend`].
//!
//! ```ignore
//! struct VaultBackend { client: VaultClient }
//!
//! impl CredentialBackend for VaultBackend {
//!     fn name(&self) -> &str { "HashiCorp Vault" }
//!     fn available(&self) -> bool { self.client.reachable() }
//!     fn get(&self, account: &str) -> Result<Option<String>, Error> { … }
//!     fn set(&self, account: &str, value: &str) -> Result<(), Error> { … }
//!     fn delete(&self, account: &str) -> Result<(), Error> { … }
//! }
//! ```
//!
//! Three things a backend must honour, because callers rely on them:
//!
//! - **`available()` is the gate.** Reporting `true` is a promise that a
//!   credential written here is stored as securely as the backend claims.
//!   Writes are refused outright when it is `false` — there is deliberately
//!   no database fallback, because falling back would put the credential
//!   exactly where the user asked it not to go.
//! - **`account` is an opaque key**, already namespaced by the caller. A
//!   backend that needs its own prefix, path or collection adds it
//!   internally; it must not reinterpret the key.
//! - **Absent is `Ok(None)`, not an error.** "No credential yet" is the
//!   normal state before the user supplies one, and callers branch on it.

use rustykrab_core::Error;
use std::collections::HashMap;
use std::sync::Mutex;

/// A store for live credential values.
pub trait CredentialBackend: Send + Sync + 'static {
    /// Human-readable, for logs and operator-facing errors — "macOS
    /// Keychain", not "keychain_v2". It appears in the message a user sees
    /// when a write is refused.
    fn name(&self) -> &str;

    /// Whether this backend can hold a credential right now.
    ///
    /// A promise, not a guess: writes are refused when this is `false`, and
    /// accepted with no second line of defence when it is `true`.
    fn available(&self) -> bool;

    fn get(&self, account: &str) -> Result<Option<String>, Error>;
    fn set(&self, account: &str, value: &str) -> Result<(), Error>;
    fn delete(&self, account: &str) -> Result<(), Error>;
}

/// The macOS Keychain, via the Data Protection Keychain.
#[derive(Debug, Default, Clone, Copy)]
pub struct KeychainBackend;

impl CredentialBackend for KeychainBackend {
    fn name(&self) -> &str {
        "macOS Keychain"
    }

    fn available(&self) -> bool {
        crate::keychain::keychain_available()
    }

    fn get(&self, account: &str) -> Result<Option<String>, Error> {
        Ok(
            crate::keychain::get_credential(crate::registry::keychain_service(), account)?
                .map(|c| c.value),
        )
    }

    fn set(&self, account: &str, value: &str) -> Result<(), Error> {
        crate::keychain::set_credential(crate::registry::keychain_service(), account, value)
    }

    fn delete(&self, account: &str) -> Result<(), Error> {
        crate::keychain::delete_credential(crate::registry::keychain_service(), account)
    }
}

/// A backend that holds nothing and says so.
///
/// The default where no secure store has been chosen — a Linux server with
/// no Secret Service, a container. It reports unavailable, so credential
/// writes are refused with an explanation rather than silently landing in
/// the database.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoBackend;

impl CredentialBackend for NoBackend {
    fn name(&self) -> &str {
        "no secure credential store"
    }
    fn available(&self) -> bool {
        false
    }
    fn get(&self, _: &str) -> Result<Option<String>, Error> {
        Ok(None)
    }
    fn set(&self, _: &str, _: &str) -> Result<(), Error> {
        Err(Error::Storage(
            "no secure credential store is configured".to_string(),
        ))
    }
    fn delete(&self, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

/// An in-process backend for tests and for harnesses that must not touch
/// the machine's real credential store.
///
/// This exists because the alternative — letting tests write to the real
/// Keychain — was not hypothetical: an early run of the suite deposited
/// `gmail-app-password` into the developer's own login keychain. Nothing
/// here outlives the process.
///
/// It is deliberately not chosen by any default. A deployment gets one by
/// asking for it explicitly, so a machine cannot end up keeping real
/// credentials in memory because a fallback picked this.
#[derive(Debug, Default)]
pub struct MemoryBackend {
    items: Mutex<HashMap<String, String>>,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialBackend for MemoryBackend {
    fn name(&self) -> &str {
        "in-memory (test)"
    }
    fn available(&self) -> bool {
        true
    }
    fn get(&self, account: &str) -> Result<Option<String>, Error> {
        Ok(self
            .items
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account)
            .cloned())
    }
    fn set(&self, account: &str, value: &str) -> Result<(), Error> {
        self.items
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(account.to_string(), value.to_string());
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<(), Error> {
        self.items
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(account);
        Ok(())
    }
}

/// What a deployment gets when it has not chosen: the Keychain on macOS,
/// and nothing anywhere else.
pub fn default_backend() -> std::sync::Arc<dyn CredentialBackend> {
    #[cfg(target_os = "macos")]
    {
        std::sync::Arc::new(KeychainBackend)
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::sync::Arc::new(NoBackend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_backend_round_trips_and_forgets() {
        let b = MemoryBackend::new();
        assert!(b.available());
        assert_eq!(b.get("a").unwrap(), None);
        b.set("a", "secret").unwrap();
        assert_eq!(b.get("a").unwrap().as_deref(), Some("secret"));
        b.delete("a").unwrap();
        assert_eq!(b.get("a").unwrap(), None);
    }

    #[test]
    fn absent_is_not_an_error() {
        // Callers branch on `None`; turning "not supplied yet" into an error
        // would make the normal case look like a failure.
        assert_eq!(MemoryBackend::new().get("missing").unwrap(), None);
        assert_eq!(NoBackend.get("missing").unwrap(), None);
    }

    #[test]
    fn an_unavailable_backend_refuses_writes_rather_than_pretending() {
        assert!(!NoBackend.available());
        assert!(NoBackend.set("a", "secret").is_err());
    }
}
