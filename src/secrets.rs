//! Password hashing and secret encryption.
//!
//! Two different problems, deliberately handled differently:
//!
//! * **IMAP passwords are hashed** with Argon2id. We only ever verify them, so
//!   there is no reason to be able to recover one — and every reason not to be.
//! * **Source mailbox passwords are encrypted**, because we must present them
//!   to rebel and to the twoducks server. Hashing would make them useless.
//!
//! ## What the encryption is and is not for
//!
//! The key lives in `config.toml`, not in the database. That is what makes this
//! worth doing rather than theatre:
//!
//! * database backups — which OVH takes automatically and which live outside
//!   our control — no longer contain plaintext source passwords
//! * Terraform state no longer carries them at all
//!
//! It does **not** protect against anyone who can read both the config file and
//! the database. That is the archiver itself, and root on the instance. Anyone
//! claiming otherwise is selling obfuscation as security.

use anyhow::{Context, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand::RngCore;

/// Hash an IMAP password for storage. Argon2id with the crate defaults, which
/// are the OWASP-recommended parameters at time of writing.
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut rand::rngs::OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))
}

/// Verify a password against a stored hash.
///
/// Returns `false` for a malformed or placeholder hash rather than erroring, so
/// an account that has never had a password set simply cannot be logged into.
pub fn verify_password(password: &str, stored: &str) -> bool {
    match PasswordHash::new(stored) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// The symmetric key used for source-password encryption.
pub struct SecretKey(Key);

/// Redacting, so the key cannot reach a log or an error chain by accident —
/// which would defeat the entire point of keeping it out of the database.
impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

impl SecretKey {
    /// Parse a base64-encoded 32-byte key.
    pub fn from_base64(encoded: &str) -> Result<Self> {
        let bytes = B64
            .decode(encoded.trim())
            .context("encryption key is not valid base64")?;
        anyhow::ensure!(
            bytes.len() == 32,
            "encryption key must be 32 bytes, got {}. Generate one with: \
             email-archiver generate-key",
            bytes.len()
        );
        Ok(Self(*Key::from_slice(&bytes)))
    }

    pub fn generate() -> Result<String> {
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Ok(B64.encode(bytes))
    }

    /// Encrypt, returning `base64(nonce || ciphertext)`.
    ///
    /// A fresh random nonce per value: XChaCha20's 24-byte nonce is large
    /// enough that random generation cannot realistically collide, which avoids
    /// having to track a counter across processes and restarts.
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let cipher = XChaCha20Poly1305::new(&self.0);
        let mut nonce_bytes = [0u8; 24];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = XNonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|e| anyhow::anyhow!("encrypting: {e}"))?;

        let mut out = nonce_bytes.to_vec();
        out.extend_from_slice(&ciphertext);
        Ok(B64.encode(out))
    }

    pub fn decrypt(&self, encoded: &str) -> Result<String> {
        let raw = B64
            .decode(encoded.trim())
            .context("stored secret is not base64")?;
        anyhow::ensure!(
            raw.len() > 24,
            "stored secret is too short to contain a nonce"
        );
        let (nonce_bytes, ciphertext) = raw.split_at(24);

        let cipher = XChaCha20Poly1305::new(&self.0);
        let plaintext = cipher
            .decrypt(XNonce::from_slice(nonce_bytes), ciphertext)
            .map_err(|_| {
                anyhow::anyhow!(
                    "could not decrypt stored secret — wrong encryption key, or the value \
                     was tampered with"
                )
            })?;
        String::from_utf8(plaintext).context("decrypted secret is not valid UTF-8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trips() {
        let hash = hash_password("correct horse").unwrap();
        assert!(verify_password("correct horse", &hash));
        assert!(!verify_password("wrong horse", &hash));
    }

    #[test]
    fn hash_is_salted() {
        // Two hashes of the same password must differ, or the store leaks which
        // users share a password.
        let a = hash_password("same").unwrap();
        let b = hash_password("same").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn placeholder_hash_never_verifies() {
        // Accounts start with '!' as a deliberate non-hash. Nothing should be
        // able to log into one before a real password is set.
        assert!(!verify_password("", "!"));
        assert!(!verify_password("anything", "!"));
        assert!(!verify_password("anything", ""));
    }

    #[test]
    fn secret_round_trips() {
        let key = SecretKey::from_base64(&SecretKey::generate().unwrap()).unwrap();
        let sealed = key.encrypt("hunter2").unwrap();
        assert!(!sealed.contains("hunter2"));
        assert_eq!(key.decrypt(&sealed).unwrap(), "hunter2");
    }

    #[test]
    fn same_plaintext_encrypts_differently() {
        // Without a fresh nonce, identical passwords would produce identical
        // ciphertext and the table would reveal which accounts share one.
        let key = SecretKey::from_base64(&SecretKey::generate().unwrap()).unwrap();
        assert_ne!(key.encrypt("same").unwrap(), key.encrypt("same").unwrap());
    }

    #[test]
    fn wrong_key_fails_loudly() {
        let a = SecretKey::from_base64(&SecretKey::generate().unwrap()).unwrap();
        let b = SecretKey::from_base64(&SecretKey::generate().unwrap()).unwrap();
        let sealed = a.encrypt("secret").unwrap();
        let err = b.decrypt(&sealed).unwrap_err().to_string();
        assert!(err.contains("wrong encryption key"), "{err}");
    }

    #[test]
    fn tampering_is_detected() {
        // AEAD: a modified ciphertext must fail rather than decrypt to garbage.
        let key = SecretKey::from_base64(&SecretKey::generate().unwrap()).unwrap();
        let sealed = key.encrypt("secret").unwrap();
        let mut raw = B64.decode(&sealed).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        assert!(key.decrypt(&B64.encode(raw)).is_err());
    }

    #[test]
    fn rejects_a_key_of_the_wrong_length() {
        let err = SecretKey::from_base64(&B64.encode([0u8; 16]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("32 bytes"), "{err}");
    }
}
