//! The `encrypted` signer backend's envelope format in `secrets.toml`:
//! base64 AES-256-GCM ciphertext, a 12-byte nonce, an Argon2id salt and the
//! KDF parameters. Decoding and checking it needs no password and no
//! cipher, so validation reads it without the signer crypto
//! ([`crate::signer`] encrypts and decrypts).

use crate::secrets::SignerSecret;
use anyhow::{Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

/// The envelope version this build reads and writes.
pub(crate) const VERSION: u8 = 1;
// Argon2 0.5 defaults, persisted per record so library default changes
// never strand an existing key.
pub(crate) const KDF_MEMORY_KIB: u32 = 19_456;
pub(crate) const KDF_ITERATIONS: u32 = 2;
pub(crate) const KDF_PARALLELISM: u32 = 1;

/// An envelope's decoded parts.
pub(crate) struct Envelope {
    pub(crate) ciphertext: Vec<u8>,
    pub(crate) nonce: Vec<u8>,
    pub(crate) salt: Vec<u8>,
    pub(crate) kdf: argon2::Params,
}

impl Envelope {
    /// Decode and check `secret` (no password needed).
    pub(crate) fn read(secret: &SignerSecret) -> Result<Self> {
        if secret.version.unwrap_or(VERSION) != VERSION {
            bail!("unsupported encrypted-key version");
        }
        let decode = |field: &str, v: &Option<String>| {
            BASE64
                .decode(v.as_deref().unwrap_or_default())
                .map_err(|_| anyhow!("invalid encrypted-key parameters ({field} is not base64)"))
        };
        let ciphertext = decode("ciphertext", &secret.ciphertext)?;
        let nonce = decode("nonce", &secret.nonce)?;
        let salt = decode("salt", &secret.salt)?;
        if ciphertext.is_empty() || nonce.len() != 12 || salt.len() < 16 {
            bail!("invalid encrypted-key parameters");
        }
        let envelope = Envelope {
            ciphertext,
            nonce,
            salt,
            kdf: kdf_params(
                secret.kdf_memory_kib.unwrap_or(KDF_MEMORY_KIB),
                secret.kdf_iterations.unwrap_or(KDF_ITERATIONS),
                secret.kdf_parallelism.unwrap_or(KDF_PARALLELISM),
            )?,
        };
        Ok(envelope)
    }

    /// The `secrets.toml` record for freshly encrypted parts, with this
    /// build's KDF parameters.
    pub(crate) fn record(ciphertext: &[u8], nonce: &[u8], salt: &[u8]) -> SignerSecret {
        let mut secret = SignerSecret::default();
        secret.ciphertext = Some(BASE64.encode(ciphertext));
        secret.nonce = Some(BASE64.encode(nonce));
        secret.salt = Some(BASE64.encode(salt));
        secret.version = Some(VERSION);
        secret.kdf_memory_kib = Some(KDF_MEMORY_KIB);
        secret.kdf_iterations = Some(KDF_ITERATIONS);
        secret.kdf_parallelism = Some(KDF_PARALLELISM);
        secret
    }
}

/// Argon2id parameters, within safety limits.
pub(crate) fn kdf_params(
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
) -> Result<argon2::Params> {
    if memory_kib > 262_144 || iterations > 10 || parallelism > 16 {
        bail!("key-derivation parameters exceed safety limits");
    }
    argon2::Params::new(memory_kib, iterations, parallelism, Some(32))
        .map_err(|e| anyhow!("invalid key-derivation parameters: {e}"))
}
