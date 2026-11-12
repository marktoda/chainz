//! Signer key material: backends, provisioning, unlock and the signer
//! lifecycle ([`Chainz::install_signer`], [`Chainz::remove_signer`]).
//!
//! The rule: chainz unlocks a key only for `--expose-key` (and when moving a
//! key between backends). Everything else uses the cached address.
//!
//! [`KeyBackend`] is the seam to external storage and [`Prompter`] the seam
//! to the person at the terminal. Production uses [`SystemKeyBackend`] (OS
//! keyring, `op`) and the binary's terminal prompter; this crate's tests use
//! in-memory ones and never touch the developer's keyring.

use crate::address::{Address, cached_address_problem};
use crate::envelope::{self, Envelope};
use crate::model::{Backend, KEYRING_SERVICE, Signer};
use crate::secrets::{Secrets, SignerSecret};
use crate::store::Chainz;
use crate::tx::Users;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use rand::Rng;
use zeroize::Zeroizing;

/// The environment variable that makes the OS keyring unavailable (the
/// test harness and the perf gate set it).
pub const DISABLE_KEYRING_VAR: &str = "CHAINZ_DISABLE_KEYRING";

/// The one wording for a signer that has no cached address, which
/// `@wallet`, `@<signer>`, `ETH_FROM` and `fork --fund` need.
pub fn no_cached_address(name: &str) -> String {
    format!(
        "signer '{name}' has no cached address; re-add it with `chainz signer rm {name}` and `chainz signer add {name}`"
    )
}

/// External key storage.
pub trait KeyBackend {
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

/// Hidden prompts for passwords and keys.
pub trait Prompter {
    /// Whether a hidden prompt can be shown.
    fn is_interactive(&self) -> bool;
    /// Prompt for a hidden value.
    fn secret(&self, prompt: &str) -> Result<Zeroizing<String>>;
}

/// The real OS keyring and `op` CLI.
///
/// `CHAINZ_DISABLE_KEYRING` (any value) makes the keyring unavailable; the
/// test harness always sets it.
pub struct SystemKeyBackend;

impl KeyBackend for SystemKeyBackend {
    fn keyring_available(&self) -> bool {
        if std::env::var_os(DISABLE_KEYRING_VAR).is_some() {
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
        if std::env::var_os(DISABLE_KEYRING_VAR).is_some() {
            bail!("the OS keyring is disabled ({DISABLE_KEYRING_VAR} is set)");
        }
        Ok(())
    }
}

/// In-memory key storage and scripted prompts for tests: never touches the
/// OS keyring, `op` or the terminal.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryKeyBackend {
    /// Keyring entries by (service, username).
    pub entries: std::cell::RefCell<std::collections::HashMap<(String, String), String>>,
    /// Scripted answers for prompts, consumed front to back.
    pub prompts: std::cell::RefCell<Vec<String>>,
    /// Whether the fake keyring is available.
    pub keyring: bool,
    /// Whether prompts are allowed.
    pub interactive: bool,
    /// Make `keyring_set` fail (to exercise rollback).
    pub fail_set: bool,
}

#[cfg(test)]
impl Prompter for MemoryKeyBackend {
    fn is_interactive(&self) -> bool {
        self.interactive
    }
    fn secret(&self, _: &str) -> Result<Zeroizing<String>> {
        let mut prompts = self.prompts.borrow_mut();
        if prompts.is_empty() {
            bail!("no scripted prompt answer");
        }
        Ok(Zeroizing::new(prompts.remove(0)))
    }
}

#[cfg(test)]
impl KeyBackend for MemoryKeyBackend {
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

/// A signer ready to be written: the record for config.toml, the key
/// material for secrets.toml, and what to undo if that write fails.
#[derive(Debug)]
pub struct Provision {
    /// The `[signers.NAME]` record for config.toml.
    pub signer: Signer,
    /// Key material for secrets.toml (plaintext and encrypted backends).
    pub secret: Option<SignerSecret>,
    /// Whether an external keyring entry was created (and must be deleted
    /// on rollback).
    created_external: bool,
}

impl Provision {
    /// A reference signer (keystore, 1Password): nothing stored, nothing
    /// to undo.
    pub fn reference(signer: Signer) -> Self {
        Self {
            signer,
            secret: None,
            created_external: false,
        }
    }

    /// Give up on this provision after `original` went wrong: delete the
    /// external entry it created, and attach a failure to do so to
    /// `original` as context (the original error stays the cause).
    pub fn abandon(
        &self,
        keys: &dyn KeyBackend,
        name: &str,
        original: anyhow::Error,
    ) -> anyhow::Error {
        let rollback = if self.created_external && self.signer.backend == Backend::Keyring {
            let (service, username) = self.signer.keyring_entry(name);
            keys.keyring_delete(service, username)
        } else {
            Ok(())
        };
        match rollback {
            Ok(()) => original,
            Err(e) => original.context(format!(
                "rolling back the new key entry also failed ({e:#}); remove it by hand"
            )),
        }
    }
}

/// The safe default backend: the keyring when available, else encrypted.
pub fn safe_default(keys: &dyn KeyBackend) -> Backend {
    if keys.keyring_available() {
        Backend::Keyring
    } else {
        Backend::Encrypted
    }
}

/// Store `private_key` for signer `name` in `target` (keyring, encrypted or
/// plaintext).
pub fn provision(
    keys: &dyn KeyBackend,
    prompter: &dyn Prompter,
    name: &str,
    private_key: &str,
    target: Backend,
) -> Result<Provision> {
    let address = Address::from_private_key(private_key)?;
    let key = normalize_key(private_key);
    match target {
        Backend::Plaintext => Ok(Provision {
            signer: Signer::new(Backend::Plaintext, Some(address)),
            secret: Some({
                let mut secret = SignerSecret::default();
                secret.private_key = Some(key.to_string());
                secret
            }),
            created_external: false,
        }),
        Backend::Keyring => {
            if !keys.keyring_available() {
                bail!("the OS keyring is unavailable; use --backend encrypted instead");
            }
            let created = keys.keyring_set(KEYRING_SERVICE, name, &key)?;
            Ok(Provision {
                signer: Signer::new(Backend::Keyring, Some(address)),
                secret: None,
                created_external: created,
            })
        }
        Backend::Encrypted => {
            if !prompter.is_interactive() {
                bail!(
                    "encrypted storage needs an interactive password prompt; use --backend plaintext --stdin only if plaintext storage is intentional"
                );
            }
            let password = prompter.secret(&format!("Encryption password for {name}: "))?;
            let confirm = prompter.secret("Confirm encryption password: ")?;
            if password.as_str() != confirm.as_str() {
                bail!("encryption passwords do not match");
            }
            Ok(Provision {
                signer: Signer::new(Backend::Encrypted, Some(address)),
                secret: Some(encrypt(&key, &password)?),
                created_external: false,
            })
        }
        Backend::Keystore | Backend::OnePassword => {
            bail!("{target} signers are references; they cannot be populated from a key value")
        }
    }
}

/// What [`Chainz::remove_signer`] did.
#[derive(Debug)]
pub struct Removed {
    /// The chains the signer was unassigned from (they have no signer
    /// now), in config order.
    pub unassigned: Vec<String>,
    /// Deleting the signer's external key material, if it has any. The
    /// signer is gone from the config either way.
    pub external: Result<()>,
}

impl Chainz {
    /// Write signer `name` from `provision` (record and key material, in
    /// one transaction). An existing signer of that name is replaced; its
    /// old keyring entry is deleted once nothing references it, and a
    /// failure to do so is returned as a warning. If the write fails, the
    /// provision is [abandoned](Provision::abandon).
    pub fn install_signer(
        &mut self,
        keys: &dyn KeyBackend,
        name: &str,
        provision: &Provision,
    ) -> Result<Option<String>> {
        let old = self.config().signers.get(name).cloned();
        let written = self.transact(|tx| {
            tx.set_signer(name, &provision.signer)?;
            tx.set_signer_secret(name, provision.secret.as_ref())
        });
        if let Err(e) = written {
            return Err(provision.abandon(keys, name, e));
        }
        let new = &provision.signer;
        let Some(old) = old.filter(|old| {
            old.backend == Backend::Keyring
                && !(new.backend == Backend::Keyring
                    && new.keyring_entry(name) == old.keyring_entry(name))
        }) else {
            return Ok(None);
        };
        Ok(delete_external(keys, name, &old)
            .err()
            .map(|e| format!("the old keyring entry for '{name}' remains: {e:#}")))
    }

    /// Remove signer `name` (record and key material in one transaction),
    /// then delete its external key material.
    pub fn remove_signer(
        &mut self,
        keys: &dyn KeyBackend,
        name: &str,
        users: Users,
    ) -> Result<Removed> {
        let record = self
            .config()
            .signers
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("signer '{name}' not found"))?;
        let unassigned = self.transact(|tx| tx.remove_signer(name, users))?;
        Ok(Removed {
            unassigned,
            external: delete_external(keys, name, &record),
        })
    }
}

/// Delete a signer's external keyring entry (after it left config).
fn delete_external(keys: &dyn KeyBackend, name: &str, signer: &Signer) -> Result<()> {
    if signer.backend == Backend::Keyring {
        let (service, username) = signer.keyring_entry(name);
        keys.keyring_delete(service, username)?;
    }
    Ok(())
}

/// Unlock a signer's private key. Only `--expose-key` and backend migration
/// call this.
pub fn unlock(
    keys: &dyn KeyBackend,
    prompter: &dyn Prompter,
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
            if !prompter.is_interactive() {
                bail!("signer '{name}' is encrypted and needs an interactive password prompt");
            }
            let password = prompter.secret(&format!("Decryption password for {name}: "))?;
            decrypt(secret, &password)
                .with_context(|| format!("failed to decrypt signer '{name}'"))?
        }
        Backend::Keyring => {
            let (service, username) = signer.keyring_entry(name);
            keys.keyring_get(service, username)?
        }
        Backend::OnePassword => {
            let (Some(vault), Some(item)) = (signer.vault.as_deref(), signer.item.as_deref())
            else {
                bail!("signer '{name}' (backend '1password') needs a 'vault' and an 'item'");
            };
            keys.one_password_get(vault, item)?
        }
        Backend::Keystore => bail!(
            "signer '{name}' is a Foundry keystore; chainz never decrypts it. Foundry reads it via ETH_KEYSTORE"
        ),
    };
    match cached_address_problem(name, &key, signer.address) {
        Some(problem) => bail!("{problem}"),
        None => Ok(key),
    }
}

fn normalize_key(key: &str) -> Zeroizing<String> {
    Zeroizing::new(key.trim().to_string())
}

fn derive(password: &str, salt: &[u8], params: argon2::Params) -> Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0u8; 32]);
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, &mut key[..])
        .map_err(|e| anyhow!("key derivation failed: {e}"))?;
    Ok(key)
}

/// Encrypt a private key with a password (AES-256-GCM, Argon2id).
pub(crate) fn encrypt(private_key: &str, password: &str) -> Result<SignerSecret> {
    if password.is_empty() {
        bail!("encryption password cannot be empty");
    }
    let mut rng = rand::rng();
    let mut salt = [0u8; 16];
    rng.fill(&mut salt);
    let mut nonce = [0u8; 12];
    rng.fill(&mut nonce);
    let params = envelope::kdf_params(
        envelope::KDF_MEMORY_KIB,
        envelope::KDF_ITERATIONS,
        envelope::KDF_PARALLELISM,
    )?;
    let key = derive(password, &salt, params)?;
    let cipher = Aes256Gcm::new_from_slice(&key[..])
        .map_err(|_| anyhow!("failed to initialize encryption"))?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), private_key.as_bytes())
        .map_err(|_| anyhow!("failed to encrypt the private key"))?;
    Ok(Envelope::record(&ciphertext, &nonce, &salt))
}

/// Decrypt an envelope made by [`encrypt`] (or v1's `EncryptedKey`).
pub(crate) fn decrypt(secret: &SignerSecret, password: &str) -> Result<Zeroizing<String>> {
    let envelope = Envelope::read(secret)?;
    let key = derive(password, &envelope.salt, envelope.kdf)?;
    let cipher = Aes256Gcm::new_from_slice(&key[..])
        .map_err(|_| anyhow!("failed to initialize decryption"))?;
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&envelope.nonce),
            envelope.ciphertext.as_ref(),
        )
        .map_err(|_| anyhow!("wrong password or corrupted key"))?;
    Ok(Zeroizing::new(String::from_utf8(plaintext)?))
}

/// Read the address field of a Foundry keystore file without decrypting
/// it: `None` when it has none (`cast wallet new` writes none). A missing
/// or unreadable file, or one that is not a keystore, is an error.
pub(crate) fn keystore_address(path: &std::path::Path) -> Result<Option<Address>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("keystore not found: {}", path.display())
        }
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read keystore {}", path.display()));
        }
    };
    let json: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("keystore {} is not JSON", path.display()))?;
    if !json.is_object() {
        bail!("keystore {} is not a keystore", path.display());
    }
    let Some(raw) = json.get("address").and_then(|a| a.as_str()) else {
        return Ok(None);
    };
    let hex = raw.strip_prefix("0x").unwrap_or(raw).to_ascii_lowercase();
    format!("0x{hex}")
        .parse()
        .map(Some)
        .map_err(|_| anyhow!("keystore address is malformed"))
}

/// The address to cache for a keystore signer at `path` (`given` is
/// `--address`): the keystore's own, which `given` must match, else
/// `given`. A keystore with no address field and no `--address` is an
/// error that says how to get it; chainz never decrypts a keystore.
pub fn keystore_signer_address(path: &std::path::Path, given: Option<Address>) -> Result<Address> {
    match (keystore_address(path)?, given) {
        (Some(found), Some(given)) if found != given => {
            bail!("--address does not match the keystore's address {found}")
        }
        (Some(found), _) | (None, Some(found)) => Ok(found),
        (None, None) => bail!(
            "keystore {path} has no address field (`cast wallet new` writes none), and chainz never decrypts it; pass --address \"$(cast wallet address --keystore {path})\"",
            path = path.display()
        ),
    }
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
        let p = provision(&b, &b, "default", KEY, Backend::Keyring).unwrap();
        assert_eq!(p.signer.address.unwrap().to_string(), ADDR);
        assert!(p.created_external);
        assert_eq!(b.entries.borrow().len(), 1);
        let err = p.abandon(&b, "default", anyhow!("config write failed"));
        assert_eq!(err.to_string(), "config write failed");
        assert!(b.entries.borrow().is_empty());
    }

    #[test]
    fn keyring_unavailable_or_noninteractive_fails_safe() {
        let b = mem(false, false);
        assert!(provision(&b, &b, "k", KEY, Backend::Keyring).is_err());
        let err = provision(&b, &b, "k", KEY, Backend::Encrypted).unwrap_err();
        assert!(err.to_string().contains("interactive"));
        assert_eq!(safe_default(&b), Backend::Encrypted);
    }

    #[test]
    fn encrypted_round_trip_through_unlock() {
        let b = mem(false, true);
        b.prompts
            .borrow_mut()
            .extend(["pw".to_string(), "pw".to_string(), "pw".to_string()]);
        let p = provision(&b, &b, "cold", KEY, Backend::Encrypted).unwrap();
        let mut secrets = Secrets::default();
        secrets
            .signers
            .insert("cold".into(), p.secret.clone().unwrap());
        let key = unlock(&b, &b, "cold", &p.signer, &secrets).unwrap();
        assert_eq!(key.as_str(), KEY);
        b.prompts.borrow_mut().push("wrong".into());
        assert!(unlock(&b, &b, "cold", &p.signer, &secrets).is_err());
    }

    #[test]
    fn keystore_addresses_come_from_the_file_or_address() {
        let dir = tempfile::tempdir().unwrap();
        let with = dir.path().join("with.json");
        std::fs::write(
            &with,
            r#"{"address":"7e5f4552091a69125d5dfcb7b8c2659029395bdf"}"#,
        )
        .unwrap();
        let without = dir.path().join("without.json");
        std::fs::write(&without, r#"{"crypto":{}}"#).unwrap();
        let addr: Address = ADDR.parse().unwrap();
        let other: Address = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
            .parse()
            .unwrap();

        assert_eq!(keystore_signer_address(&with, None).unwrap(), addr);
        assert_eq!(keystore_signer_address(&with, Some(addr)).unwrap(), addr);
        let err = keystore_signer_address(&with, Some(other)).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");

        assert_eq!(
            keystore_signer_address(&without, Some(other)).unwrap(),
            other
        );
        let err = keystore_signer_address(&without, None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no address field") && err.contains("cast wallet address --keystore"),
            "{err}"
        );

        let missing = dir.path().join("missing.json");
        let err = keystore_signer_address(&missing, Some(addr)).unwrap_err();
        assert!(err.to_string().starts_with("keystore not found: "), "{err}");
        std::fs::write(&missing, "[]").unwrap();
        assert!(keystore_signer_address(&missing, Some(addr)).is_err());
    }

    #[test]
    fn keystore_is_never_unlocked() {
        let b = mem(true, true);
        let s = Signer::keystore("/x".into(), None);
        let err = unlock(&b, &b, "dep", &s, &Secrets::default()).unwrap_err();
        assert!(err.to_string().contains("ETH_KEYSTORE"));
    }

    #[test]
    fn unlock_rejects_a_key_that_does_not_match_the_cached_address() {
        let b = mem(true, false);
        b.keyring_set(KEYRING_SERVICE, "x", &"2".repeat(64))
            .unwrap();
        let s = Signer::keyring("x", None, None, Some(ADDR.parse().unwrap()));
        assert!(unlock(&b, &b, "x", &s, &Secrets::default()).is_err());
    }

    #[test]
    fn a_failed_rollback_is_context_on_the_original_error() {
        let b = MemoryKeyBackend {
            keyring: true,
            ..Default::default()
        };
        let p = provision(&b, &b, "default", KEY, Backend::Keyring).unwrap();
        b.entries.borrow_mut().clear();
        // The memory backend's delete never fails; a failing one is a
        // keyring that went away.
        struct Gone;
        impl KeyBackend for Gone {
            fn keyring_available(&self) -> bool {
                false
            }
            fn keyring_get(&self, _: &str, _: &str) -> Result<Zeroizing<String>> {
                bail!("gone")
            }
            fn keyring_set(&self, _: &str, _: &str, _: &str) -> Result<bool> {
                bail!("gone")
            }
            fn keyring_delete(&self, _: &str, _: &str) -> Result<()> {
                bail!("keyring locked")
            }
            fn one_password_get(&self, _: &str, _: &str) -> Result<Zeroizing<String>> {
                bail!("gone")
            }
        }
        let both = p.abandon(&Gone, "default", anyhow!("config write failed"));
        let text = format!("{both:#}");
        assert!(text.contains("keyring locked"), "{text}");
        assert!(text.ends_with("config write failed"), "{text}");
        assert_eq!(both.root_cause().to_string(), "config write failed");
    }

    #[test]
    fn install_and_remove_a_signer_with_its_keyring_entry() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::StorePaths::in_dir(dir.path().to_path_buf());
        let mut chainz = Chainz::open_at(paths, crate::LoadMode::Strict).unwrap();
        let b = mem(true, true);
        let p = provision(&b, &b, "hot", KEY, Backend::Keyring).unwrap();
        assert_eq!(chainz.install_signer(&b, "hot", &p).unwrap(), None);
        assert_eq!(chainz.config().signers["hot"].backend, Backend::Keyring);

        // Moving it out of the keyring deletes the old entry.
        let p = provision(&b, &b, "hot", KEY, Backend::Plaintext).unwrap();
        assert_eq!(chainz.install_signer(&b, "hot", &p).unwrap(), None);
        assert!(b.entries.borrow().is_empty());
        assert!(chainz.secrets().unwrap().signers.contains_key("hot"));

        // A failed write undoes the keyring entry it created.
        let p = provision(&b, &b, "Bad Name", KEY, Backend::Keyring).unwrap();
        assert_eq!(b.entries.borrow().len(), 1);
        assert!(chainz.install_signer(&b, "Bad Name", &p).is_err());
        assert!(b.entries.borrow().is_empty());

        let removed = chainz.remove_signer(&b, "hot", Users::Keep).unwrap();
        assert!(removed.unassigned.is_empty());
        assert!(removed.external.is_ok());
        assert!(chainz.config().signers.is_empty());
        assert!(chainz.secrets().unwrap().signers.is_empty());
    }
}
