//! Private-key storage and resolution.
//!
//! `KeyVault` is the seam between key lifecycle logic and external storage.
//! Production uses the OS keyring/1Password adapters; tests use an in-memory
//! adapter and never touch developer credentials.

use crate::{
    config::Chainz,
    opt::{KeyCommand, KeyTypeArg, MigrationTargetArg},
};
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use alloy::{
    primitives::Address,
    signers::{Signer, local::PrivateKeySigner},
};
use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use keyring::Entry;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::{fmt, io::IsTerminal, process::Command, sync::OnceLock};
use zeroize::{Zeroize, Zeroizing};

const KEYRING_SERVICE: &str = "chainz";
const ENVELOPE_VERSION: u8 = 1;
// These are Argon2 0.5's defaults. Persisting them makes encrypted records
// independent from future library-default changes.
const KDF_MEMORY_KIB: u32 = 19_456;
const KDF_ITERATIONS: u32 = 2;
const KDF_PARALLELISM: u32 = 1;

#[derive(Serialize, Deserialize, Clone)]
pub struct Key {
    pub name: String,
    /// Public address cached at creation/migration so wallet-only commands do
    /// not need to unlock the private-key backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(flatten)]
    pub kind: KeyType,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum KeyType {
    #[serde(rename = "PrivateKey")]
    PrivateKey { value: String },
    #[serde(rename = "EncryptedKey")]
    EncryptedKey {
        value: String,
        nonce: String,
        salt: String,
        #[serde(default = "default_envelope_version")]
        version: u8,
        #[serde(default = "default_kdf_memory")]
        kdf_memory_kib: u32,
        #[serde(default = "default_kdf_iterations")]
        kdf_iterations: u32,
        #[serde(default = "default_kdf_parallelism")]
        kdf_parallelism: u32,
    },
    #[serde(rename = "OnePassword")]
    OnePassword { vault: String, item: String },
    #[serde(rename = "Keyring")]
    Keyring { service: String, username: String },
}

const fn default_envelope_version() -> u8 {
    ENVELOPE_VERSION
}
const fn default_kdf_memory() -> u32 {
    KDF_MEMORY_KIB
}
const fn default_kdf_iterations() -> u32 {
    KDF_ITERATIONS
}
const fn default_kdf_parallelism() -> u32 {
    KDF_PARALLELISM
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Key")
            .field("name", &self.name)
            .field("address", &self.address)
            .field("kind", &self.kind)
            .finish()
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        if let KeyType::PrivateKey { value } = &mut self.kind {
            value.zeroize();
        }
    }
}

impl fmt::Debug for KeyType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrivateKey { .. } => f.write_str("PrivateKey { value: [REDACTED] }"),
            Self::EncryptedKey { version, .. } => f
                .debug_struct("EncryptedKey")
                .field("version", version)
                .field("value", &"[REDACTED]")
                .finish(),
            Self::OnePassword { vault, item } => f
                .debug_struct("OnePassword")
                .field("vault", vault)
                .field("item", item)
                .finish(),
            Self::Keyring { service, username } => f
                .debug_struct("Keyring")
                .field("service", service)
                .field("username", username)
                .finish(),
        }
    }
}

trait KeyBackend {
    fn is_interactive(&self) -> bool;
    fn prompt_secret(&self, prompt: &str) -> Result<Zeroizing<String>>;
    fn keyring_available(&self) -> bool;
    fn keyring_get(&self, service: &str, username: &str) -> Result<Zeroizing<String>>;
    /// Store a credential, returning whether this call created the entry.
    fn keyring_set(&self, service: &str, username: &str, value: &str) -> Result<bool>;
    fn keyring_delete(&self, service: &str, username: &str) -> Result<()>;
    fn one_password_get(&self, vault: &str, item: &str) -> Result<Zeroizing<String>>;
}

struct SystemKeyBackend;

impl KeyBackend for SystemKeyBackend {
    fn is_interactive(&self) -> bool {
        // Secret input may intentionally arrive on stdin while the password
        // prompt still uses the controlling terminal via stderr.
        std::io::stderr().is_terminal()
    }

    fn prompt_secret(&self, prompt: &str) -> Result<Zeroizing<String>> {
        Ok(Zeroizing::new(rpassword::prompt_password(prompt)?))
    }

    fn keyring_available(&self) -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        if std::env::var_os("CHAINZ_DISABLE_KEYRING").is_some() {
            return false;
        }
        *AVAILABLE.get_or_init(|| {
            let username = format!("__probe__{}", std::process::id());
            let Ok(entry) = Entry::new(KEYRING_SERVICE, &username) else {
                return false;
            };
            let usable = entry.set_password("chainz-keyring-probe").is_ok()
                && matches!(entry.get_password(), Ok(value) if value == "chainz-keyring-probe");
            let _ = entry.delete_credential();
            usable
        })
    }

    fn keyring_get(&self, service: &str, username: &str) -> Result<Zeroizing<String>> {
        Ok(Zeroizing::new(
            Entry::new(service, username)?.get_password()?,
        ))
    }

    fn keyring_set(&self, service: &str, username: &str, value: &str) -> Result<bool> {
        let entry = Entry::new(service, username)?;
        match entry.get_password() {
            Ok(existing) if existing == value => Ok(false),
            Ok(_) => anyhow::bail!(
                "A different credential already exists in the OS keyring for '{}/{}'; refusing to overwrite it",
                service,
                username
            ),
            Err(keyring::Error::NoEntry) => {
                entry.set_password(value)?;
                Ok(true)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn keyring_delete(&self, service: &str, username: &str) -> Result<()> {
        match Entry::new(service, username)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn one_password_get(&self, vault: &str, item: &str) -> Result<Zeroizing<String>> {
        let output = Command::new("op")
            .args(["read", &format!("op://{}/{}", vault, item)])
            .output()
            .context("Failed to run the 1Password CLI")?;
        if !output.status.success() {
            anyhow::bail!(
                "Failed to read from 1Password: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let mut value = String::from_utf8(output.stdout)?;
        let trimmed = Zeroizing::new(value.trim().to_string());
        value.zeroize();
        Ok(trimmed)
    }
}

struct KeyVault<B> {
    backend: B,
}

#[derive(Debug)]
pub(crate) struct KeyProvision {
    key: Key,
    created_external: bool,
}

impl KeyProvision {
    pub(crate) fn key(&self) -> &Key {
        &self.key
    }
}

impl<B: KeyBackend> KeyVault<B> {
    fn new(backend: B) -> Self {
        Self { backend }
    }

    fn resolve(&self, key: &Key) -> Result<Zeroizing<String>> {
        let value = match &key.kind {
            KeyType::PrivateKey { value } => Zeroizing::new(value.clone()),
            KeyType::EncryptedKey {
                value,
                nonce,
                salt,
                version,
                kdf_memory_kib,
                kdf_iterations,
                kdf_parallelism,
            } => {
                if *version != ENVELOPE_VERSION {
                    anyhow::bail!("Unsupported encrypted-key version {}", version);
                }
                if !self.backend.is_interactive() {
                    anyhow::bail!(
                        "Key '{}' is encrypted and needs an interactive password prompt",
                        key.name
                    );
                }
                let password = self
                    .backend
                    .prompt_secret(&format!("Enter decryption password for {}: ", key.name))?;
                let salt_bytes = BASE64.decode(salt)?;
                let mut derived = derive_key(
                    &password,
                    &salt_bytes,
                    *kdf_memory_kib,
                    *kdf_iterations,
                    *kdf_parallelism,
                )?;
                let cipher = Aes256Gcm::new_from_slice(&derived)
                    .map_err(|_| anyhow!("Failed to initialize decryption"))?;
                derived.zeroize();
                let nonce_bytes = BASE64.decode(nonce)?;
                if nonce_bytes.len() != 12 {
                    anyhow::bail!("Invalid encrypted-key nonce");
                }
                let ciphertext = BASE64.decode(value)?;
                let plaintext = cipher
                    .decrypt(Nonce::from_slice(&nonce_bytes), ciphertext.as_ref())
                    .map_err(|_| anyhow!("Failed to decrypt key '{}'", key.name))?;
                Zeroizing::new(String::from_utf8(plaintext)?)
            }
            KeyType::OnePassword { vault, item } => self.backend.one_password_get(vault, item)?,
            KeyType::Keyring { service, username } => {
                self.backend.keyring_get(service, username)?
            }
        };
        Key::validate_private_key(&value)?;
        Ok(value)
    }

    fn safe_default(&self) -> MigrationTargetArg {
        if self.backend.keyring_available() {
            MigrationTargetArg::Keyring
        } else {
            MigrationTargetArg::Encrypted
        }
    }

    fn provision_private_key(
        &self,
        name: &str,
        private_key: &str,
        requested: Option<KeyTypeArg>,
    ) -> Result<KeyProvision> {
        Key::validate_private_key(private_key)?;
        let storage = match requested {
            Some(KeyTypeArg::PrivateKey) => {
                eprintln!(
                    "Warning: storing '{}' as plaintext; migrate with `chainz key migrate {}`",
                    name, name
                );
                return Ok(KeyProvision {
                    key: Key::new(
                        name.to_string(),
                        KeyType::PrivateKey {
                            value: private_key.to_string(),
                        },
                    ),
                    created_external: false,
                });
            }
            Some(KeyTypeArg::Encrypted) => MigrationTargetArg::Encrypted,
            Some(KeyTypeArg::Keyring) => MigrationTargetArg::Keyring,
            Some(KeyTypeArg::OnePassword) => {
                anyhow::bail!("1Password keys are references and cannot be populated from a value")
            }
            None => self.safe_default(),
        };
        self.provision_target(name, private_key, storage)
    }

    fn provision_target(
        &self,
        name: &str,
        private_key: &str,
        target: MigrationTargetArg,
    ) -> Result<KeyProvision> {
        match target {
            MigrationTargetArg::Keyring => {
                if !self.backend.keyring_available() {
                    anyhow::bail!("The OS keyring is unavailable; use --to encrypted instead");
                }
                self.store_keyring(name, name, private_key)
            }
            MigrationTargetArg::Encrypted => {
                if !self.backend.is_interactive() {
                    anyhow::bail!(
                        "No OS keyring is available and encrypted storage needs an interactive password prompt; use `--type private-key --stdin` only if plaintext storage is intentional"
                    );
                }
                let password = self
                    .backend
                    .prompt_secret(&format!("Enter encryption password for {}: ", name))?;
                let confirmation = self
                    .backend
                    .prompt_secret("Confirm encryption password: ")?;
                if password.as_str() != confirmation.as_str() {
                    anyhow::bail!("Encryption passwords do not match");
                }
                Ok(KeyProvision {
                    key: encrypt_with_password(name.to_string(), private_key, &password)?,
                    created_external: false,
                })
            }
        }
    }

    fn provision_replacement_private_key(
        &self,
        name: &str,
        private_key: &str,
    ) -> Result<KeyProvision> {
        Key::validate_private_key(private_key)?;
        match self.safe_default() {
            MigrationTargetArg::Keyring => {
                let suffix: u64 = rand::rng().random();
                let username = format!("{}-replacement-{suffix:016x}", name);
                self.store_keyring(name, &username, private_key)
            }
            MigrationTargetArg::Encrypted => {
                self.provision_target(name, private_key, MigrationTargetArg::Encrypted)
            }
        }
    }

    fn store_keyring(&self, name: &str, username: &str, private_key: &str) -> Result<KeyProvision> {
        let created_external = self
            .backend
            .keyring_set(KEYRING_SERVICE, username, private_key)?;
        Ok(KeyProvision {
            key: Key::new(
                name.to_string(),
                KeyType::Keyring {
                    service: KEYRING_SERVICE.to_string(),
                    username: username.to_string(),
                },
            )
            .with_public_address(private_key),
            created_external,
        })
    }

    fn provision_migration(
        &self,
        key: &Key,
        target: Option<MigrationTargetArg>,
    ) -> Result<KeyProvision> {
        let private_key = self.resolve(key)?;
        self.provision_target(
            &key.name,
            &private_key,
            target.unwrap_or_else(|| self.safe_default()),
        )
    }

    fn cleanup_external(&self, key: &Key) -> Result<()> {
        if let KeyType::Keyring { service, username } = &key.kind {
            self.backend.keyring_delete(service, username)?;
        }
        Ok(())
    }

    fn rollback(&self, provision: &KeyProvision) -> Result<()> {
        if provision.created_external {
            self.cleanup_external(&provision.key)?;
        }
        Ok(())
    }
}

impl Key {
    pub fn new(name: String, kind: KeyType) -> Self {
        let address = match &kind {
            KeyType::PrivateKey { value } => Self::address_from_private_key(value)
                .ok()
                .map(|address| address.to_string()),
            _ => None,
        };
        Self {
            name,
            address,
            kind,
        }
    }

    fn with_public_address(mut self, private_key: &str) -> Self {
        self.address = Self::address_from_private_key(private_key)
            .ok()
            .map(|address| address.to_string());
        self
    }

    pub fn private_key(&self) -> Result<Zeroizing<String>> {
        KeyVault::new(SystemKeyBackend).resolve(self)
    }

    pub fn encrypt(name: String, private_key: &str, password: &str) -> Result<Self> {
        Key::validate_private_key(private_key)?;
        encrypt_with_password(name, private_key, password)
    }

    pub fn signer(&self) -> Result<Box<dyn Signer>> {
        Ok(Box::new(self.private_key()?.parse::<PrivateKeySigner>()?))
    }

    pub fn address(&self) -> Result<Address> {
        Ok(self.signer()?.address())
    }

    pub(crate) fn address_from_private_key(private_key: &str) -> Result<Address> {
        Ok(private_key.parse::<PrivateKeySigner>()?.address())
    }

    pub fn validate_private_key(key: &str) -> Result<()> {
        key.parse::<PrivateKeySigner>()
            .map(|_| ())
            .map_err(|e| anyhow!("Invalid private key: {}", e))
    }

    pub(crate) fn validate_record(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            anyhow::bail!("Key names cannot be empty");
        }
        if let Some(address) = &self.address {
            let parsed = address
                .parse::<Address>()
                .map_err(|error| anyhow!("Invalid cached address: {}", error))?;
            if let KeyType::PrivateKey { value } = &self.kind
                && parsed != Self::address_from_private_key(value)?
            {
                anyhow::bail!("Cached address does not match the private key");
            }
        }
        match &self.kind {
            KeyType::PrivateKey { value } => Self::validate_private_key(value),
            KeyType::EncryptedKey {
                value,
                nonce,
                salt,
                version,
                kdf_memory_kib,
                kdf_iterations,
                kdf_parallelism,
                ..
            } => {
                if *version != ENVELOPE_VERSION {
                    anyhow::bail!("Unsupported encrypted-key version {}", version);
                }
                if BASE64.decode(value)?.is_empty()
                    || BASE64.decode(nonce)?.len() != 12
                    || BASE64.decode(salt)?.len() < 16
                {
                    anyhow::bail!("Invalid encrypted-key parameters");
                }
                let _ = argon2_params(*kdf_memory_kib, *kdf_iterations, *kdf_parallelism)?;
                Ok(())
            }
            KeyType::OnePassword { vault, item } => {
                if vault.trim().is_empty() || item.trim().is_empty() {
                    anyhow::bail!("1Password vault and item cannot be empty");
                }
                Ok(())
            }
            KeyType::Keyring { service, username } => {
                if service.trim().is_empty() || username.trim().is_empty() {
                    anyhow::bail!("Keyring service and username cannot be empty");
                }
                Ok(())
            }
        }
    }

    fn kind_name(&self) -> &'static str {
        match self.kind {
            KeyType::PrivateKey { .. } => "PrivateKey",
            KeyType::EncryptedKey { .. } => "EncryptedKey",
            KeyType::OnePassword { .. } => "OnePassword",
            KeyType::Keyring { .. } => "Keyring",
        }
    }

    pub(crate) fn address_noninteractive(&self) -> Option<String> {
        self.address.clone().or_else(|| match self.kind {
            KeyType::PrivateKey { .. } => self.address().ok().map(|a| a.to_string()),
            _ => None,
        })
    }
}

fn argon2_params(memory_kib: u32, iterations: u32, parallelism: u32) -> Result<argon2::Params> {
    if memory_kib > 262_144 || iterations > 10 || parallelism > 16 {
        anyhow::bail!("Key-derivation parameters exceed safety limits");
    }
    argon2::Params::new(memory_kib, iterations, parallelism, Some(32))
        .map_err(|e| anyhow!("Invalid key-derivation parameters: {}", e))
}

fn derive_key(
    password: &str,
    salt: &[u8],
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
) -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    let params = argon2_params(memory_kib, iterations, parallelism)?;
    let argon2 = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow!("Key derivation failed: {}", e))?;
    Ok(key)
}

fn encrypt_with_password(name: String, private_key: &str, password: &str) -> Result<Key> {
    if password.is_empty() {
        anyhow::bail!("Encryption password cannot be empty");
    }
    let mut rng = rand::rng();
    let mut salt_bytes = [0u8; 16];
    rng.fill(&mut salt_bytes);
    let mut derived = derive_key(
        password,
        &salt_bytes,
        KDF_MEMORY_KIB,
        KDF_ITERATIONS,
        KDF_PARALLELISM,
    )?;
    let cipher = Aes256Gcm::new_from_slice(&derived)
        .map_err(|_| anyhow!("Failed to initialize encryption"))?;
    derived.zeroize();
    let mut nonce_bytes = [0u8; 12];
    rng.fill(&mut nonce_bytes);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), private_key.as_bytes())
        .map_err(|_| anyhow!("Failed to encrypt private key"))?;

    Ok(Key::new(
        name,
        KeyType::EncryptedKey {
            value: BASE64.encode(ciphertext),
            nonce: BASE64.encode(nonce_bytes),
            salt: BASE64.encode(salt_bytes),
            version: ENVELOPE_VERSION,
            kdf_memory_kib: KDF_MEMORY_KIB,
            kdf_iterations: KDF_ITERATIONS,
            kdf_parallelism: KDF_PARALLELISM,
        },
    )
    .with_public_address(private_key))
}

fn read_stdin_secret(label: &str) -> Result<Zeroizing<String>> {
    use std::io::Read;
    let mut value = String::new();
    std::io::stdin()
        .read_to_string(&mut value)
        .with_context(|| format!("Failed to read {} from stdin", label))?;
    let trimmed = value.trim().to_string();
    value.zeroize();
    if trimmed.is_empty() {
        anyhow::bail!("{} from stdin was empty", label);
    }
    Ok(Zeroizing::new(trimmed))
}

pub(crate) fn provision_safe_key(name: &str, private_key: &str) -> Result<KeyProvision> {
    KeyVault::new(SystemKeyBackend).provision_private_key(name, private_key, None)
}

/// Provision a key for an in-progress config replacement without colliding
/// with credentials still referenced by the current config.
pub(crate) fn provision_safe_replacement_key(
    name: &str,
    private_key: &str,
) -> Result<KeyProvision> {
    KeyVault::new(SystemKeyBackend).provision_replacement_private_key(name, private_key)
}

pub(crate) fn rollback_key_provision(provision: &KeyProvision) -> Result<()> {
    KeyVault::new(SystemKeyBackend).rollback(provision)
}

/// Convert newly staged plaintext keys to safe storage and commit the config
/// as one logical transaction. External credentials are rolled back if any
/// provision or the config write fails.
pub(crate) async fn save_with_safe_new_keys(
    chainz: &mut Chainz,
    names: impl IntoIterator<Item = String>,
) -> Result<()> {
    let vault = KeyVault::new(SystemKeyBackend);
    let mut provisions: Vec<(String, Key, KeyProvision)> = Vec::new();
    for name in names {
        let staged = chainz.get_key(&name)?;
        if !matches!(staged.kind, KeyType::PrivateKey { .. }) {
            continue;
        }
        let private_key = staged.private_key()?;
        let provision = match vault.provision_private_key(&name, &private_key, None) {
            Ok(provision) => provision,
            Err(error) => {
                rollback_staged_provisions(chainz, &vault, &provisions);
                return Err(error);
            }
        };
        chainz
            .config
            .keys
            .insert(name.clone(), provision.key().clone());
        provisions.push((name, staged, provision));
    }
    if let Err(error) = chainz.save().await {
        rollback_staged_provisions(chainz, &vault, &provisions);
        return Err(error);
    }
    Ok(())
}

fn rollback_staged_provisions<B: KeyBackend>(
    chainz: &mut Chainz,
    vault: &KeyVault<B>,
    provisions: &[(String, Key, KeyProvision)],
) {
    for (name, staged, provision) in provisions.iter().rev() {
        chainz.config.keys.insert(name.clone(), staged.clone());
        if let Err(error) = vault.rollback(provision) {
            eprintln!(
                "Warning: could not roll back credential for '{}': {error}",
                name
            );
        }
    }
}

pub(crate) async fn migrate_plaintext_keys(chainz: &mut Chainz) -> Result<usize> {
    let names: Vec<String> = chainz
        .list_keys()
        .into_iter()
        .filter(|(_, key)| matches!(key.kind, KeyType::PrivateKey { .. }))
        .map(|(name, _)| name.to_string())
        .collect();
    migrate_names(chainz, names, None, true).await
}

async fn migrate_names(
    chainz: &mut Chainz,
    names: Vec<String>,
    target: Option<MigrationTargetArg>,
    continue_on_error: bool,
) -> Result<usize> {
    let vault = KeyVault::new(SystemKeyBackend);
    let mut migrated: Vec<(String, Key, KeyProvision)> = Vec::new();
    for name in names {
        let source = chainz.get_key(&name)?;
        match vault.provision_migration(&source, target) {
            Ok(provision) => {
                chainz
                    .config
                    .keys
                    .insert(name.clone(), provision.key().clone());
                migrated.push((name, source, provision));
            }
            Err(error) if continue_on_error => {
                eprintln!("Failed to migrate '{}': {:#}", name, error);
            }
            Err(error) => {
                rollback_migrations(chainz, &vault, &migrated);
                return Err(error);
            }
        }
    }
    if !migrated.is_empty() {
        if let Err(error) = chainz.save().await {
            rollback_migrations(chainz, &vault, &migrated);
            return Err(error);
        }
        for (name, source, provision) in &migrated {
            if same_external_location(source, provision.key()) {
                continue;
            }
            if let Err(error) = vault.cleanup_external(source) {
                eprintln!(
                    "Warning: migrated '{}' but could not remove the old credential: {error}",
                    name
                );
            }
        }
    }
    Ok(migrated.len())
}

fn rollback_migrations<B: KeyBackend>(
    chainz: &mut Chainz,
    vault: &KeyVault<B>,
    migrated: &[(String, Key, KeyProvision)],
) {
    for (name, source, provision) in migrated.iter().rev() {
        chainz.config.keys.insert(name.clone(), source.clone());
        if let Err(error) = vault.rollback(provision) {
            eprintln!(
                "Warning: could not roll back credential for '{}': {error}",
                name
            );
        }
    }
}

fn same_external_location(left: &Key, right: &Key) -> bool {
    matches!(
        (&left.kind, &right.kind),
        (
            KeyType::Keyring {
                service: left_service,
                username: left_username,
            },
            KeyType::Keyring {
                service: right_service,
                username: right_username,
            }
        ) if left_service == right_service && left_username == right_username
    )
}

impl KeyCommand {
    pub async fn handle(self, chainz: &mut Chainz) -> Result<()> {
        match self {
            KeyCommand::Add {
                name,
                key,
                stdin,
                key_type,
            } => {
                if chainz.get_key(&name).is_ok() {
                    anyhow::bail!("Key '{}' already exists", name);
                }
                if key.is_some() {
                    eprintln!(
                        "Warning: --key can be visible in shell history and process listings; prefer --stdin"
                    );
                }
                let vault = KeyVault::new(SystemKeyBackend);
                let provision = if key_type == Some(KeyTypeArg::OnePassword) {
                    if key.is_some() || stdin {
                        anyhow::bail!("1Password references do not accept private-key input");
                    }
                    let vault_name: String = dialoguer::Input::new()
                        .with_prompt("Enter 1Password vault name")
                        .interact_text()?;
                    let item: String = dialoguer::Input::new()
                        .with_prompt("Enter 1Password item reference")
                        .interact_text()?;
                    KeyProvision {
                        key: Key::new(
                            name.clone(),
                            KeyType::OnePassword {
                                vault: vault_name,
                                item,
                            },
                        ),
                        created_external: false,
                    }
                } else {
                    let private_key = match (key, stdin) {
                        (Some(value), false) => Zeroizing::new(value),
                        (None, true) => read_stdin_secret("private key")?,
                        (None, false) if vault.backend.is_interactive() => {
                            vault.backend.prompt_secret("Enter private key: ")?
                        }
                        (None, false) => anyhow::bail!(
                            "No private key provided; use --stdin for scripts or run interactively"
                        ),
                        (Some(_), true) => unreachable!("clap rejects conflicting inputs"),
                    };
                    vault.provision_private_key(&name, &private_key, key_type)?
                };
                if let Err(error) = chainz.add_key(&name, provision.key().clone()) {
                    let _ = vault.rollback(&provision);
                    return Err(error);
                }
                if let Err(error) = chainz.save().await {
                    chainz.config.keys.remove(&name);
                    if let Err(rollback_error) = vault.rollback(&provision) {
                        eprintln!(
                            "Warning: could not roll back credential for '{}': {rollback_error}",
                            name
                        );
                    }
                    return Err(error);
                }
                println!("Added key '{}'", name);
            }
            KeyCommand::List { json } => {
                let keys = chainz.list_keys();
                if json {
                    let entries: Vec<_> = keys
                        .iter()
                        .map(|(name, key)| {
                            serde_json::json!({
                                "name": name,
                                "type": key.kind_name(),
                                "address": key.address_noninteractive(),
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&entries)?);
                } else if keys.is_empty() {
                    println!("No stored keys");
                } else {
                    println!("Stored keys:");
                    for (_, key) in keys {
                        println!("- {}", key);
                    }
                }
            }
            KeyCommand::Remove { name, force } => {
                let removed = chainz.get_key(&name)?;
                let references = chainz.chains_using_key(&name);
                if !references.is_empty() && !force {
                    anyhow::bail!(
                        "Key '{}' is used by chain(s): {}. Use --force to detach it.",
                        name,
                        references.join(", ")
                    );
                }
                let detached = if force { chainz.detach_key(&name) } else { 0 };
                chainz.remove_key(&name)?;
                chainz.save().await?;
                if let Err(error) = KeyVault::new(SystemKeyBackend).cleanup_external(&removed) {
                    eprintln!(
                        "Warning: removed '{}' from config but could not delete its external credential: {error}",
                        name
                    );
                }
                println!("Removed key '{}'", name);
                if detached > 0 {
                    println!("Detached from {} chain(s)", detached);
                }
            }
            KeyCommand::Migrate { name, all, to } => {
                let names = if all {
                    chainz
                        .list_keys()
                        .into_iter()
                        .filter(|(_, key)| matches!(key.kind, KeyType::PrivateKey { .. }))
                        .map(|(name, _)| name.to_string())
                        .collect()
                } else {
                    vec![name.ok_or_else(|| anyhow!("Provide a key name or use --all"))?]
                };
                let count = migrate_names(chainz, names, to, all).await?;
                println!("Migrated {} key(s)", count);
            }
        }
        Ok(())
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let storage = match self.kind {
            KeyType::PrivateKey { .. } => "plaintext",
            KeyType::EncryptedKey { .. } => "encrypted",
            KeyType::OnePassword { .. } => "1password",
            KeyType::Keyring { .. } => "keyring",
        };
        match self.address_noninteractive() {
            Some(address) => write!(f, "{} ({}, {})", self.name, address, storage),
            None => write!(f, "{} ({})", self.name, storage),
        }
    }
}

#[cfg(test)]
mod tests;
