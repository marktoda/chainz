//! Signer key material: backends, provisioning, unlock and the encrypted
//! envelope.
//!
//! The rule: chainz unlocks a key only for `--expose-key` (and when moving a
//! key between backends). Everything else uses the cached address.
//!
//! [`KeyBackend`] is the seam to external storage. Production uses
//! [`SystemKeyBackend`] (OS keyring, `op`, terminal prompts); this crate's
//! tests use an in-memory backend and never touch the developer's keyring.

use crate::address::Address;
use crate::model::{Backend, KEYRING_SERVICE, Signer};
use crate::secrets::{Secrets, SignerSecret};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rand::Rng;
use zeroize::Zeroizing;

const ENVELOPE_VERSION: u8 = 1;
// Argon2 0.5 defaults, persisted per record so library default changes
// never strand an existing key.
const KDF_MEMORY_KIB: u32 = 19_456;
const KDF_ITERATIONS: u32 = 2;
const KDF_PARALLELISM: u32 = 1;

/// External key storage and prompting.
pub trait KeyBackend {
    /// Whether a password prompt can be shown.
    fn is_interactive(&self) -> bool;
    /// Prompt for a hidden value.
    fn prompt_secret(&self, prompt: &str) -> Result<Zeroizing<String>>;
    /// Whether the OS keyring is usable.
    fn keyring_available(&self) -> bool;
    /// Read a keyring entry.
    fn keyring_get(&self, service: &str, username: &str) -> Result<Zeroizing<String>>;
    /// Store a keyring entry; returns whether this call created it. Refuses
    /// to overwrite a different existing value.
    fn keyring_set(&self, service: &str, username: &str, value: &str) -> Result<bool>;
    /// Delete a keyring entry (missing is fine).
    fn keyring_delete(&self, service: &str, username: &str) -> Result<()>;
    /// Read a 1Password item through `op`.
    fn one_password_get(&self, vault: &str, item: &str) -> Result<Zeroizing<String>>;
}

/// The real OS keyring, `op` CLI and terminal prompts.
///
/// `CHAINZ_DISABLE_KEYRING` (any value) makes the keyring unavailable; the
/// test harness always sets it.
pub struct SystemKeyBackend;

impl KeyBackend for SystemKeyBackend {
    fn is_interactive(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stderr().is_terminal()
    }

    fn prompt_secret(&self, prompt: &str) -> Result<Zeroizing<String>> {
        Ok(Zeroizing::new(rpassword_prompt(prompt)?))
    }

    fn keyring_available(&self) -> bool {
        if std::env::var_os("CHAINZ_DISABLE_KEYRING").is_some() {
            return false;
        }
        // Read-only probe: a missing entry proves the store answers. v1
        // wrote and deleted a probe entry; v2 never writes to probe.
        let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, "__chainz_probe__") else {
            return false;
        };
        matches!(entry.get_password(), Ok(_) | Err(keyring::Error::NoEntry))
    }

    fn keyring_get(&self, service: &str, username: &str) -> Result<Zeroizing<String>> {
        self.guard()?;
        Ok(Zeroizing::new(
            keyring::Entry::new(service, username)?.get_password()?,
        ))
    }

    fn keyring_set(&self, service: &str, username: &str, value: &str) -> Result<bool> {
        self.guard()?;
        let entry = keyring::Entry::new(service, username)?;
        match entry.get_password().map(Zeroizing::new) {
            Ok(existing) if existing.as_str() == value => Ok(false),
            Ok(_) => bail!(
                "a different credential already exists in the OS keyring for '{service}/{username}'; refusing to overwrite it"
            ),
            Err(keyring::Error::NoEntry) => {
                entry.set_password(value)?;
                Ok(true)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn keyring_delete(&self, service: &str, username: &str) -> Result<()> {
        self.guard()?;
        match keyring::Entry::new(service, username)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn one_password_get(&self, vault: &str, item: &str) -> Result<Zeroizing<String>> {
        let output = std::process::Command::new("op")
            .args(["read", &format!("op://{vault}/{item}")])
            .output()
            .context("failed to run the 1Password CLI (`op`)")?;
        if !output.status.success() {
            bail!(
                "failed to read from 1Password: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let stdout = Zeroizing::new(output.stdout);
        let value = std::str::from_utf8(&stdout)
            .map_err(|_| anyhow!("1Password returned a value that is not UTF-8"))?;
        Ok(Zeroizing::new(value.trim().to_string()))
    }
}

impl SystemKeyBackend {
    fn guard(&self) -> Result<()> {
        if std::env::var_os("CHAINZ_DISABLE_KEYRING").is_some() {
            bail!("the OS keyring is disabled (CHAINZ_DISABLE_KEYRING is set)");
        }
        Ok(())
    }
}

fn rpassword_prompt(prompt: &str) -> Result<String> {
    rpassword::prompt_password(prompt).context("failed to read the password")
}

/// In-memory backend for tests: never touches the OS keyring or `op`.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryKeyBackend {
    /// Keyring entries by (service, username).
    pub entries: std::cell::RefCell<std::collections::HashMap<(String, String), String>>,
    /// Scripted answers for `prompt_secret`, consumed front to back.
    pub prompts: std::cell::RefCell<Vec<String>>,
    /// Whether the fake keyring is available.
    pub keyring: bool,
    /// Whether prompts are allowed.
    pub interactive: bool,
    /// Make `keyring_set` fail (to exercise rollback).
    pub fail_set: bool,
}

#[cfg(test)]
impl KeyBackend for MemoryKeyBackend {
    fn is_interactive(&self) -> bool {
        self.interactive
    }
    fn prompt_secret(&self, _: &str) -> Result<Zeroizing<String>> {
        let mut prompts = self.prompts.borrow_mut();
        if prompts.is_empty() {
            bail!("no scripted prompt answer");
        }
        Ok(Zeroizing::new(prompts.remove(0)))
    }
    fn keyring_available(&self) -> bool {
        self.keyring
    }
    fn keyring_get(&self, service: &str, username: &str) -> Result<Zeroizing<String>> {
        self.entries
            .borrow()
            .get(&(service.into(), username.into()))
            .map(|v| Zeroizing::new(v.clone()))
            .ok_or_else(|| anyhow!("no keyring entry for '{service}/{username}'"))
    }
    fn keyring_set(&self, service: &str, username: &str, value: &str) -> Result<bool> {
        if self.fail_set {
            bail!("keyring write failed");
        }
        let mut entries = self.entries.borrow_mut();
        match entries.get(&(service.into(), username.into())) {
            Some(existing) if existing == value => Ok(false),
            Some(_) => bail!("a different credential already exists for '{service}/{username}'"),
            None => {
                entries.insert((service.into(), username.into()), value.into());
                Ok(true)
            }
        }
    }
    fn keyring_delete(&self, service: &str, username: &str) -> Result<()> {
        self.entries
            .borrow_mut()
            .remove(&(service.into(), username.into()));
        Ok(())
    }
    fn one_password_get(&self, vault: &str, item: &str) -> Result<Zeroizing<String>> {
        self.keyring_get(&format!("op:{vault}"), item)
    }
}

/// The outcome of storing a key: what to write to config and secrets, and
/// what to undo if that write fails.
#[derive(Debug)]
pub struct Provision {
    /// The `[signers.NAME]` record for config.toml.
    pub signer: Signer,
    /// Key material for secrets.toml (plaintext and encrypted backends).
    pub secret: Option<SignerSecret>,
    /// Whether an external keyring entry was created (and must be deleted
    /// on rollback).
    pub created_external: bool,
}

fn blank_signer(backend: Backend, address: Option<Address>) -> Signer {
    Signer {
        backend,
        address,
        service: None,
        username: None,
        path: None,
        vault: None,
        item: None,
    }
}

/// The safe default backend: the keyring when available, else encrypted.
pub fn safe_default(backend: &dyn KeyBackend) -> Backend {
    if backend.keyring_available() {
        Backend::Keyring
    } else {
        Backend::Encrypted
    }
}

/// Store `private_key` for signer `name` in `target` (keyring, encrypted or
/// plaintext). `keyring_username` overrides the entry name (migration uses
/// a fresh one to avoid clobbering the entry still in use).
pub fn provision(
    backend: &dyn KeyBackend,
    name: &str,
    private_key: &str,
    target: Backend,
    keyring_username: Option<&str>,
) -> Result<Provision> {
    let address = Address::from_private_key(private_key)?;
    match target {
        Backend::Plaintext => Ok(Provision {
            signer: blank_signer(Backend::Plaintext, Some(address)),
            secret: Some({
                let mut secret = SignerSecret::default();
                secret.private_key = Some(normalize_key(private_key));
                secret
            }),
            created_external: false,
        }),
        Backend::Keyring => {
            if !backend.keyring_available() {
                bail!("the OS keyring is unavailable; use --backend encrypted instead");
            }
            let username = keyring_username.unwrap_or(name);
            let created =
                backend.keyring_set(KEYRING_SERVICE, username, &normalize_key(private_key))?;
            let mut signer = blank_signer(Backend::Keyring, Some(address));
            if username != name {
                signer.username = Some(username.to_string());
            }
            Ok(Provision {
                signer,
                secret: None,
                created_external: created,
            })
        }
        Backend::Encrypted => {
            if !backend.is_interactive() {
                bail!(
                    "encrypted storage needs an interactive password prompt; use --backend plaintext --stdin only if plaintext storage is intentional"
                );
            }
            let password = backend.prompt_secret(&format!("Encryption password for {name}: "))?;
            let confirm = backend.prompt_secret("Confirm encryption password: ")?;
            if password.as_str() != confirm.as_str() {
                bail!("encryption passwords do not match");
            }
            Ok(Provision {
                signer: blank_signer(Backend::Encrypted, Some(address)),
                secret: Some(encrypt(&normalize_key(private_key), &password)?),
                created_external: false,
            })
        }
        Backend::Keystore | Backend::OnePassword => {
            bail!("{target} signers are references; they cannot be populated from a key value")
        }
    }
}

/// Undo the external side effect of a provision whose config write failed.
pub fn rollback(backend: &dyn KeyBackend, name: &str, provision: &Provision) -> Result<()> {
    if provision.created_external && provision.signer.backend == Backend::Keyring {
        let (service, username) = provision.signer.keyring_entry(name);
        backend.keyring_delete(service, username)?;
    }
    Ok(())
}

/// Delete a signer's external keyring entry (after it left config).
pub fn delete_external(backend: &dyn KeyBackend, name: &str, signer: &Signer) -> Result<()> {
    if signer.backend == Backend::Keyring {
        let (service, username) = signer.keyring_entry(name);
        backend.keyring_delete(service, username)?;
    }
    Ok(())
}

/// Unlock a signer's private key. Only `--expose-key` and backend migration
/// call this.
pub fn unlock(
    backend: &dyn KeyBackend,
    name: &str,
    signer: &Signer,
    secrets: &Secrets,
) -> Result<Zeroizing<String>> {
    let key = match signer.backend {
        Backend::Plaintext => Zeroizing::new(
            secrets
                .signers
                .get(name)
                .and_then(|s| s.private_key.clone())
                .ok_or_else(|| anyhow!("signer '{name}' has no key in secrets.toml"))?,
        ),
        Backend::Encrypted => {
            let secret = secrets
                .signers
                .get(name)
                .ok_or_else(|| anyhow!("signer '{name}' has no encrypted key in secrets.toml"))?;
            if !backend.is_interactive() {
                bail!("signer '{name}' is encrypted and needs an interactive password prompt");
            }
            let password = backend.prompt_secret(&format!("Decryption password for {name}: "))?;
            decrypt(secret, &password)
                .with_context(|| format!("failed to decrypt signer '{name}'"))?
        }
        Backend::Keyring => {
            let (service, username) = signer.keyring_entry(name);
            backend.keyring_get(service, username)?
        }
        Backend::OnePassword => backend.one_password_get(
            signer.vault.as_deref().unwrap_or_default(),
            signer.item.as_deref().unwrap_or_default(),
        )?,
        Backend::Keystore => bail!(
            "signer '{name}' is a Foundry keystore; chainz never decrypts it. Foundry reads it via ETH_KEYSTORE"
        ),
    };
    let address = Address::from_private_key(&key)
        .map_err(|_| anyhow!("signer '{name}' holds an invalid private key"))?;
    if let Some(cached) = signer.address
        && cached != address
    {
        bail!("signer '{name}' key does not match its cached address");
    }
    Ok(key)
}

fn normalize_key(key: &str) -> String {
    key.trim().to_string()
}

fn argon2_params(memory_kib: u32, iterations: u32, parallelism: u32) -> Result<argon2::Params> {
    if memory_kib > 262_144 || iterations > 10 || parallelism > 16 {
        bail!("key-derivation parameters exceed safety limits");
    }
    argon2::Params::new(memory_kib, iterations, parallelism, Some(32))
        .map_err(|e| anyhow!("invalid key-derivation parameters: {e}"))
}

fn derive(password: &str, salt: &[u8], m: u32, t: u32, p: u32) -> Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0u8; 32]);
    argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2_params(m, t, p)?,
    )
    .hash_password_into(password.as_bytes(), salt, &mut key[..])
    .map_err(|e| anyhow!("key derivation failed: {e}"))?;
    Ok(key)
}

/// Encrypt a private key with a password (AES-256-GCM, Argon2id).
pub fn encrypt(private_key: &str, password: &str) -> Result<SignerSecret> {
    if password.is_empty() {
        bail!("encryption password cannot be empty");
    }
    let mut rng = rand::rng();
    let mut salt = [0u8; 16];
    rng.fill(&mut salt);
    let mut nonce = [0u8; 12];
    rng.fill(&mut nonce);
    let key = derive(
        password,
        &salt,
        KDF_MEMORY_KIB,
        KDF_ITERATIONS,
        KDF_PARALLELISM,
    )?;
    let cipher = Aes256Gcm::new_from_slice(&key[..])
        .map_err(|_| anyhow!("failed to initialize encryption"))?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), private_key.as_bytes())
        .map_err(|_| anyhow!("failed to encrypt the private key"))?;
    let mut secret = SignerSecret::default();
    secret.ciphertext = Some(BASE64.encode(ciphertext));
    secret.nonce = Some(BASE64.encode(nonce));
    secret.salt = Some(BASE64.encode(salt));
    secret.version = Some(ENVELOPE_VERSION);
    secret.kdf_memory_kib = Some(KDF_MEMORY_KIB);
    secret.kdf_iterations = Some(KDF_ITERATIONS);
    secret.kdf_parallelism = Some(KDF_PARALLELISM);
    Ok(secret)
}

/// Decrypt an envelope made by [`encrypt`] (or v1's `EncryptedKey`).
pub fn decrypt(secret: &SignerSecret, password: &str) -> Result<Zeroizing<String>> {
    validate_envelope(secret)?;
    let salt = BASE64.decode(secret.salt.as_deref().unwrap_or_default())?;
    let nonce = BASE64.decode(secret.nonce.as_deref().unwrap_or_default())?;
    let ciphertext = BASE64.decode(secret.ciphertext.as_deref().unwrap_or_default())?;
    let key = derive(
        password,
        &salt,
        secret.kdf_memory_kib.unwrap_or(KDF_MEMORY_KIB),
        secret.kdf_iterations.unwrap_or(KDF_ITERATIONS),
        secret.kdf_parallelism.unwrap_or(KDF_PARALLELISM),
    )?;
    let cipher = Aes256Gcm::new_from_slice(&key[..])
        .map_err(|_| anyhow!("failed to initialize decryption"))?;
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| anyhow!("wrong password or corrupted key"))?;
    Ok(Zeroizing::new(String::from_utf8(plaintext)?))
}

/// Structural checks on an encrypted envelope (no password needed).
pub fn validate_envelope(secret: &SignerSecret) -> Result<()> {
    if secret.version.unwrap_or(ENVELOPE_VERSION) != ENVELOPE_VERSION {
        bail!("unsupported encrypted-key version");
    }
    let decode = |v: &Option<String>| {
        BASE64
            .decode(v.as_deref().unwrap_or_default())
            .unwrap_or_default()
    };
    if decode(&secret.ciphertext).is_empty()
        || decode(&secret.nonce).len() != 12
        || decode(&secret.salt).len() < 16
    {
        bail!("invalid encrypted-key parameters");
    }
    argon2_params(
        secret.kdf_memory_kib.unwrap_or(KDF_MEMORY_KIB),
        secret.kdf_iterations.unwrap_or(KDF_ITERATIONS),
        secret.kdf_parallelism.unwrap_or(KDF_PARALLELISM),
    )?;
    Ok(())
}

/// Read the address field of a Foundry keystore file without decrypting it.
pub fn keystore_address(path: &std::path::Path) -> Result<Address> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read keystore {}", path.display()))?;
    let json: serde_json::Value = serde_json::from_str(&text).context("keystore is not JSON")?;
    let raw = json
        .get("address")
        .and_then(|a| a.as_str())
        .ok_or_else(|| anyhow!("keystore has no address field; pass --address"))?;
    let hex = raw.strip_prefix("0x").unwrap_or(raw).to_ascii_lowercase();
    format!("0x{hex}")
        .parse()
        .map_err(|_| anyhow!("keystore address is malformed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const ADDR: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";

    fn mem(keyring: bool, interactive: bool) -> MemoryKeyBackend {
        MemoryKeyBackend {
            keyring,
            interactive,
            ..Default::default()
        }
    }

    #[test]
    fn keyring_provision_caches_address_and_rolls_back() {
        let b = mem(true, false);
        let p = provision(&b, "default", KEY, Backend::Keyring, None).unwrap();
        assert_eq!(p.signer.address.unwrap().to_string(), ADDR);
        assert!(p.created_external);
        assert_eq!(b.entries.borrow().len(), 1);
        rollback(&b, "default", &p).unwrap();
        assert!(b.entries.borrow().is_empty());
    }

    #[test]
    fn keyring_unavailable_or_noninteractive_fails_safe() {
        let b = mem(false, false);
        assert!(provision(&b, "k", KEY, Backend::Keyring, None).is_err());
        let err = provision(&b, "k", KEY, Backend::Encrypted, None).unwrap_err();
        assert!(err.to_string().contains("interactive"));
        assert_eq!(safe_default(&b), Backend::Encrypted);
    }

    #[test]
    fn encrypted_round_trip_through_unlock() {
        let b = mem(false, true);
        b.prompts
            .borrow_mut()
            .extend(["pw".to_string(), "pw".to_string(), "pw".to_string()]);
        let p = provision(&b, "cold", KEY, Backend::Encrypted, None).unwrap();
        let mut secrets = Secrets::default();
        secrets
            .signers
            .insert("cold".into(), p.secret.clone().unwrap());
        let key = unlock(&b, "cold", &p.signer, &secrets).unwrap();
        assert_eq!(key.as_str(), KEY);
        b.prompts.borrow_mut().push("wrong".into());
        assert!(unlock(&b, "cold", &p.signer, &secrets).is_err());
    }

    #[test]
    fn keystore_is_never_unlocked() {
        let b = mem(true, true);
        let mut s = blank_signer(Backend::Keystore, None);
        s.path = Some("/x".into());
        let err = unlock(&b, "dep", &s, &Secrets::default()).unwrap_err();
        assert!(err.to_string().contains("ETH_KEYSTORE"));
    }

    #[test]
    fn unlock_rejects_a_key_that_does_not_match_the_cached_address() {
        let b = mem(true, false);
        b.keyring_set(KEYRING_SERVICE, "x", &"2".repeat(64))
            .unwrap();
        let mut s = blank_signer(Backend::Keyring, Some(ADDR.parse().unwrap()));
        s.username = Some("x".into());
        assert!(unlock(&b, "x", &s, &Secrets::default()).is_err());
    }
}
