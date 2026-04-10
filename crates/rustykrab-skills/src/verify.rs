use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rustykrab_core::Error;

/// Verifies ed25519 signatures on skill packages.
///
/// Every skill loaded from ClawHub (or any external source) must carry
/// a valid signature from a trusted publisher key. This prevents the
/// ClawHavoc class of supply-chain attacks where malicious skills
/// were installed without any integrity verification.
pub struct SkillVerifier {
    trusted_keys: Vec<VerifyingKey>,
}

impl SkillVerifier {
    /// Create a verifier with a set of trusted publisher public keys.
    pub fn new(trusted_keys: Vec<VerifyingKey>) -> Self {
        Self { trusted_keys }
    }

    /// Parse a hex-encoded ed25519 public key and add it to the trust set.
    pub fn add_trusted_key_hex(&mut self, hex_key: &str) -> Result<(), Error> {
        let bytes = hex::decode(hex_key)
            .map_err(|e| Error::Config(format!("invalid hex key: {e}")))?;
        let key_bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| Error::Config("ed25519 public key must be 32 bytes".into()))?;
        let key = VerifyingKey::from_bytes(&key_bytes)
            .map_err(|e| Error::Config(format!("invalid ed25519 key: {e}")))?;
        self.trusted_keys.push(key);
        Ok(())
    }

    /// Verify that `content` was signed by one of the trusted keys.
    ///
    /// `signature_bytes` should be a 64-byte ed25519 signature.
    pub fn verify(&self, content: &[u8], signature_bytes: &[u8]) -> Result<(), Error> {
        let signature = Signature::from_slice(signature_bytes)
            .map_err(|e| Error::Auth(format!("invalid signature format: {e}")))?;

        for key in &self.trusted_keys {
            if key.verify(content, &signature).is_ok() {
                return Ok(());
            }
        }

        Err(Error::Auth(
            "skill signature does not match any trusted publisher key".into(),
        ))
    }

    /// Convenience: verify a skill manifest + bundled code.
    ///
    /// The signed payload is `manifest_bytes || code_bytes` concatenated.
    pub fn verify_skill_bundle(
        &self,
        manifest_bytes: &[u8],
        code_bytes: &[u8],
        signature_bytes: &[u8],
    ) -> Result<(), Error> {
        let mut payload = Vec::with_capacity(manifest_bytes.len() + code_bytes.len());
        payload.extend_from_slice(manifest_bytes);
        payload.extend_from_slice(code_bytes);
        self.verify(&payload, signature_bytes)
    }

    /// Return how many trusted keys are configured.
    pub fn trusted_key_count(&self) -> usize {
        self.trusted_keys.len()
    }
}

/// Utility: generate a new ed25519 signing keypair (for skill publishers).
pub fn generate_signing_keypair() -> (ed25519_dalek::SigningKey, VerifyingKey) {
    let mut csprng = rand_core::OsRng;
    let signing_key = ed25519_dalek::SigningKey::generate(&mut csprng);
    let verifying_key = signing_key.verifying_key();
    (signing_key, verifying_key)
}
