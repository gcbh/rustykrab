use hmac::{Hmac, Mac};
use openclaw_core::Error;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Encrypted credential store backed by a sled tree.
///
/// Secrets are encrypted at rest using HMAC-SHA256 as a key derivation
/// function to produce per-key encryption keys with a unique nonce,
/// then XOR'd with the derived keystream. An HMAC authentication tag
/// is appended to each ciphertext to detect tampering.
///
/// Wire format: [nonce (16 bytes)] [ciphertext (N bytes)] [auth tag (32 bytes)]
///
/// This prevents plaintext API keys on disk and detects tampering
/// (addressing the `~/.clawdbot/.env` plaintext credential class of bugs).
#[derive(Clone)]
pub struct SecretStore {
    tree: sled::Tree,
    master_key: Vec<u8>,
}

/// Length of the random nonce prepended to each ciphertext.
const NONCE_LEN: usize = 16;
/// Length of the HMAC-SHA256 authentication tag.
const TAG_LEN: usize = 32;

impl SecretStore {
    pub(crate) fn new(tree: sled::Tree, master_key: Vec<u8>) -> Self {
        Self { tree, master_key }
    }

    /// Store a secret value under the given name.
    pub fn set(&self, name: &str, value: &str) -> Result<(), Error> {
        // Validate secret name
        Self::validate_name(name)?;

        let encrypted = self.encrypt(name, value.as_bytes());
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
            let name = String::from_utf8(key.to_vec())
                .map_err(|e| Error::Storage(e.to_string()))?;
            names.push(name);
        }
        Ok(names)
    }

    /// Validate that a secret name is well-formed.
    fn validate_name(name: &str) -> Result<(), Error> {
        if name.is_empty() || name.len() > 256 {
            return Err(Error::Storage("secret name must be 1-256 characters".into()));
        }
        // Allow alphanumeric, underscore, hyphen, and dot
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.') {
            return Err(Error::Storage(
                "secret name must contain only alphanumeric characters, underscores, hyphens, and dots".into(),
            ));
        }
        Ok(())
    }

    /// Encrypt data with a per-key keystream and authentication tag.
    ///
    /// Format: [nonce (16 bytes)] [ciphertext] [HMAC tag (32 bytes)]
    fn encrypt(&self, key_name: &str, data: &[u8]) -> Vec<u8> {
        // Generate random nonce for this encryption
        use rand::RngCore;
        let mut nonce = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce);

        // Derive keystream using nonce for uniqueness
        let keystream = self.derive_keystream(key_name, &nonce, data.len());
        let ciphertext: Vec<u8> = data
            .iter()
            .zip(keystream.iter())
            .map(|(a, b)| a ^ b)
            .collect();

        // Compute authentication tag over nonce + ciphertext
        let mut mac =
            HmacSha256::new_from_slice(&self.master_key).expect("HMAC accepts any key size");
        mac.update(b"auth:");
        mac.update(key_name.as_bytes());
        mac.update(&nonce);
        mac.update(&ciphertext);
        let tag = mac.finalize().into_bytes();

        // Wire format: nonce || ciphertext || tag
        let mut result = Vec::with_capacity(NONCE_LEN + ciphertext.len() + TAG_LEN);
        result.extend_from_slice(&nonce);
        result.extend_from_slice(&ciphertext);
        result.extend_from_slice(&tag);
        result
    }

    /// Decrypt data, verifying the authentication tag first.
    fn decrypt(&self, key_name: &str, data: &[u8]) -> Result<Vec<u8>, Error> {
        if data.len() < NONCE_LEN + TAG_LEN {
            return Err(Error::Storage("encrypted data too short".into()));
        }

        let nonce = &data[..NONCE_LEN];
        let ciphertext = &data[NONCE_LEN..data.len() - TAG_LEN];
        let stored_tag = &data[data.len() - TAG_LEN..];

        // Verify authentication tag BEFORE decryption
        let mut mac =
            HmacSha256::new_from_slice(&self.master_key).expect("HMAC accepts any key size");
        mac.update(b"auth:");
        mac.update(key_name.as_bytes());
        mac.update(nonce);
        mac.update(ciphertext);

        mac.verify_slice(stored_tag)
            .map_err(|_| Error::Storage("secret integrity check failed — data may be tampered".into()))?;

        // Decrypt
        let keystream = self.derive_keystream(key_name, nonce, ciphertext.len());
        let plaintext: Vec<u8> = ciphertext
            .iter()
            .zip(keystream.iter())
            .map(|(a, b)| a ^ b)
            .collect();

        Ok(plaintext)
    }

    /// Produce a keystream of `len` bytes using HMAC-SHA256 in counter mode.
    ///
    /// Uses key_name + nonce + counter to derive unique keystream per
    /// encryption operation.
    fn derive_keystream(&self, key_name: &str, nonce: &[u8], len: usize) -> Vec<u8> {
        let mut stream = Vec::with_capacity(len);
        let mut counter: u32 = 0;

        while stream.len() < len {
            let mut mac =
                HmacSha256::new_from_slice(&self.master_key).expect("HMAC accepts any key size");
            mac.update(b"derive:");
            mac.update(key_name.as_bytes());
            mac.update(nonce);
            mac.update(&counter.to_le_bytes());
            let block = mac.finalize().into_bytes();
            stream.extend_from_slice(&block);
            counter += 1;
        }

        stream.truncate(len);
        stream
    }
}
