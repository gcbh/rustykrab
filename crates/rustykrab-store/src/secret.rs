use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use argon2::Argon2;
use rand::TryRngCore;
use rustykrab_core::Error;
use std::sync::Arc;
use zeroize::Zeroizing;

/// The salt length used for Argon2 key derivation.
const SALT_LEN: usize = 16;
/// The nonce length for AES-256-GCM (96 bits).
const NONCE_LEN: usize = 12;

/// Encrypted credential store backed by a sled tree.
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
    tree: sled::Tree,
    master_key: Arc<Zeroizing<Vec<u8>>>,
}

impl SecretStore {
    pub(crate) fn new(tree: sled::Tree, master_key: Zeroizing<Vec<u8>>) -> Self {
        Self {
            tree,
            master_key: Arc::new(master_key),
        }
    }

    /// Store a secret value under the given name.
    pub fn set(&self, name: &str, value: &str) -> Result<(), Error> {
        // Validate secret name to prevent injection and normalization attacks
        Self::validate_name(name)?;

        let encrypted = self.encrypt(name, value.as_bytes())?;
        self.tree
            .insert(name.as_bytes(), encrypted)
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Retrieve and decrypt a secret by name.
    pub fn get(&self, name: &str) -> Result<String, Error> {
        let encrypted = self
            .tree
            .get(name.as_bytes())
            .map_err(|e| Error::Storage(e.to_string()))?
            .ok_or_else(|| Error::NotFound(format!("secret '{name}'")))?;
        let plaintext = self.decrypt(name, &encrypted)?;
        String::from_utf8(plaintext)
            .map_err(|e| Error::Storage(format!("invalid utf-8 in secret: {e}")))
    }

    /// Delete a secret.
    pub fn delete(&self, name: &str) -> Result<(), Error> {
        self.tree
            .remove(name.as_bytes())
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// List all secret names (does not decrypt values).
    pub fn list_names(&self) -> Result<Vec<String>, Error> {
        let mut names = Vec::new();
        for entry in self.tree.iter() {
            let (key, _) = entry.map_err(|e| Error::Storage(e.to_string()))?;
            let name =
                String::from_utf8(key.to_vec()).map_err(|e| Error::Storage(e.to_string()))?;
            names.push(name);
        }
        Ok(names)
    }

    /// Validate that a secret name is well-formed.
    ///
    /// Prevents Unicode normalization attacks and ensures key names
    /// are safe for use as HMAC/AAD inputs.
    fn validate_name(name: &str) -> Result<(), Error> {
        if name.is_empty() || name.len() > 256 {
            return Err(Error::Storage(
                "secret name must be 1-256 characters".into(),
            ));
        }
        // Allow alphanumeric, underscore, hyphen, and dot
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

    /// Encrypt data with AES-256-GCM. Returns `salt || nonce || ciphertext+tag`.
    ///
    /// The secret name is used as associated data (AAD), binding the
    /// ciphertext to its key name — moving a ciphertext to a different
    /// key name will fail authentication.
    fn encrypt(&self, key_name: &str, data: &[u8]) -> Result<Vec<u8>, Error> {
        // Generate random salt and nonce using OsRng for explicit
        // cryptographic intent.
        let mut salt = [0u8; SALT_LEN];
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rngs::OsRng
            .try_fill_bytes(&mut salt)
            .expect("OS RNG failed");
        rand::rngs::OsRng
            .try_fill_bytes(&mut nonce_bytes)
            .expect("OS RNG failed");

        // Derive a per-secret encryption key via Argon2id.
        let derived_key = self.derive_key(&salt)?;
        let cipher = Aes256Gcm::new_from_slice(derived_key.as_ref())
            .map_err(|e| Error::Storage(format!("cipher init: {e}")))?;

        let nonce = Nonce::from_slice(&nonce_bytes);

        // Encrypt with the secret name as AAD.
        let ciphertext = cipher
            .encrypt(
                nonce,
                aes_gcm::aead::Payload {
                    msg: data,
                    aad: key_name.as_bytes(),
                },
            )
            .map_err(|e| Error::Storage(format!("encryption failed: {e}")))?;

        // Pack: salt || nonce || ciphertext+tag
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
        let cipher = Aes256Gcm::new_from_slice(derived_key.as_ref())
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
    ///
    /// Argon2id is resistant to both side-channel and GPU/ASIC brute-force
    /// attacks. Even if the database is stolen, the attacker must spend
    /// significant time/memory per guess of the master key.
    ///
    /// The returned key is wrapped in `Zeroizing` so it is securely erased
    /// from memory when dropped (fixes #176).
    fn derive_key(&self, salt: &[u8]) -> Result<Zeroizing<[u8; 32]>, Error> {
        let mut key = Zeroizing::new([0u8; 32]);
        Argon2::default()
            .hash_password_into(&self.master_key, salt, key.as_mut())
            .map_err(|e| Error::Storage(format!("key derivation failed: {e}")))?;
        Ok(key)
    }
}
