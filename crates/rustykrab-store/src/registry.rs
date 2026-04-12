//! Central secret registry — the single source of truth for every credential
//! the application requires or can use.
//!
//! Each secret has three resolution sources checked in priority order:
//! 1. Environment variable (highest priority — explicit override)
//! 2. OS credential store (macOS Keychain / Linux Secret Service)
//! 3. Encrypted local SecretStore (SQLite + AES-256-GCM)
//!
//! Required secrets are validated at startup. If any required secret is missing
//! from *all* sources the application refuses to start.

use crate::keychain;
use crate::SecretStore;

/// The keychain service name shared across all RustyKrab credentials.
const KEYCHAIN_SERVICE: &str = "com.rustykrab.credentials";

/// Describes a single credential the application knows about.
#[derive(Debug, Clone, Copy)]
pub struct SecretSpec {
    /// Key in the encrypted `SecretStore` (e.g. `"notion_api_token"`).
    pub store_name: &'static str,
    /// Environment variable override (e.g. `"NOTION_API_TOKEN"`).
    pub env_var: &'static str,
    /// Account name under the shared keychain service.
    pub keychain_account: &'static str,
    /// Human-readable label for error / status output.
    pub description: &'static str,
    /// When `true`, the application **must** refuse to start if this secret
    /// cannot be resolved from any source.
    pub required: bool,
}

/// The authoritative list of every credential the application uses.
///
/// Add new entries here — tools, CLI startup, and the `keychain` subcommand
/// all derive their behaviour from this list.
pub static REGISTRY: &[SecretSpec] = &[
    SecretSpec {
        store_name: "anthropic_api_key",
        env_var: "ANTHROPIC_API_KEY",
        keychain_account: "anthropic-api-key",
        description: "Anthropic Claude API key",
        required: false, // not needed when provider is Ollama
    },
    SecretSpec {
        store_name: "notion_api_token",
        env_var: "NOTION_API_TOKEN",
        keychain_account: "notion-api-token",
        description: "Notion integration API token",
        required: true,
    },
    SecretSpec {
        store_name: "obsidian_api_key",
        env_var: "OBSIDIAN_API_KEY",
        keychain_account: "obsidian-api-key",
        description: "Obsidian Local REST API key",
        required: true,
    },
    SecretSpec {
        store_name: "gmail_email",
        env_var: "GMAIL_EMAIL",
        keychain_account: "gmail-email",
        description: "Gmail email address (for IMAP/SMTP)",
        required: false,
    },
    SecretSpec {
        store_name: "gmail_app_password",
        env_var: "GMAIL_APP_PASSWORD",
        keychain_account: "gmail-app-password",
        description: "Gmail app password (for IMAP/SMTP)",
        required: false,
    },
    SecretSpec {
        store_name: "rustykrab_auth_token",
        env_var: "RUSTYKRAB_AUTH_TOKEN",
        keychain_account: "auth-token",
        description: "Gateway bearer auth token",
        required: false, // auto-generated when absent
    },
];

/// A secret that could not be found in any resolution source.
#[derive(Debug)]
pub struct MissingSecret {
    pub spec: &'static SecretSpec,
}

/// Validate every secret in [`REGISTRY`] and return those that are absent.
///
/// This does **not** mutate any store — it is a read-only check suitable for
/// startup validation.
pub fn validate(secrets: &SecretStore) -> Vec<MissingSecret> {
    REGISTRY
        .iter()
        .filter(|spec| !is_present(spec, secrets))
        .map(|spec| MissingSecret { spec })
        .collect()
}

/// Resolve a secret from all sources in priority order.
///
/// When found in a higher-priority source the value is persisted downward
/// so future runs can find it without the env var.
///
/// Returns `None` only when the secret is absent from every source.
pub fn resolve(spec: &SecretSpec, secrets: &SecretStore) -> Option<String> {
    // 1. Environment variable (highest priority).
    if let Ok(val) = std::env::var(spec.env_var) {
        let val = val.trim().to_string();
        if !val.is_empty() {
            // Persist downward.
            if keychain::keychain_available() {
                let _ = keychain::set_credential(KEYCHAIN_SERVICE, spec.keychain_account, &val);
            }
            let _ = secrets.set(spec.store_name, &val);
            return Some(val);
        }
    }

    // 2. OS credential store.
    if keychain::keychain_available() {
        if let Ok(Some(cred)) = keychain::get_credential(KEYCHAIN_SERVICE, spec.keychain_account) {
            let _ = secrets.set(spec.store_name, &cred.value);
            return Some(cred.value);
        }
    }

    // 3. Encrypted local store.
    if let Ok(val) = secrets.get(spec.store_name) {
        // Back-fill into keychain if available.
        if keychain::keychain_available() {
            let _ = keychain::set_credential(KEYCHAIN_SERVICE, spec.keychain_account, &val);
        }
        return Some(val);
    }

    None
}

/// Look up a [`SecretSpec`] by its store name.
pub fn lookup(store_name: &str) -> Option<&'static SecretSpec> {
    REGISTRY.iter().find(|s| s.store_name == store_name)
}

/// Look up a [`SecretSpec`] by its keychain account name.
pub fn lookup_by_account(account: &str) -> Option<&'static SecretSpec> {
    REGISTRY.iter().find(|s| s.keychain_account == account)
}

/// Return the shared keychain service name.
pub fn keychain_service() -> &'static str {
    KEYCHAIN_SERVICE
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn is_present(spec: &SecretSpec, secrets: &SecretStore) -> bool {
    // env var
    if let Ok(val) = std::env::var(spec.env_var) {
        if !val.trim().is_empty() {
            return true;
        }
    }
    // keychain
    if keychain::keychain_available() {
        if let Ok(Some(_)) = keychain::get_credential(KEYCHAIN_SERVICE, spec.keychain_account) {
            return true;
        }
    }
    // store
    secrets.get(spec.store_name).is_ok()
}
