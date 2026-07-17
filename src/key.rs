// module for storing configurations of encrypted private keys

#[cfg(test)]
use self::tests::mock_password_prompt as prompt_password;
#[cfg(not(test))]
use rpassword::prompt_password;

use crate::{
    config::Chainz,
    opt::{KeyCommand, KeyTypeArg, SafeKeyTypeArg},
    ui,
};
use alloy::{
    primitives::Address,
    signers::{Signer, local::PrivateKeySigner},
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use keyring::Entry;
use rand::Rng;
#[cfg(not(test))]
use std::io::IsTerminal;
use std::{fmt, sync::OnceLock};
use zeroize::Zeroizing;

const KEYRING_SERVICE: &str = "chainz";
static KEYRING_AVAILABLE: OnceLock<bool> = OnceLock::new();

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Key {
    pub name: String,
    #[serde(flatten)]
    pub kind: KeyType,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum KeyType {
    #[serde(rename = "PrivateKey")]
    PrivateKey { value: String },
    #[serde(rename = "EncryptedKey")]
    EncryptedKey {
        value: String,
        nonce: String,
        salt: String,
    },
    #[serde(rename = "OnePassword")]
    OnePassword { vault: String, item: String },
    #[serde(rename = "Keyring")]
    Keyring { service: String, username: String },
}

impl Key {
    pub fn new(name: String, kind: KeyType) -> Self {
        Self { name, kind }
    }

    pub fn private_key(&self) -> Result<Zeroizing<String>> {
        match &self.kind {
            KeyType::PrivateKey { value } => Ok(Zeroizing::new(value.clone())),
            KeyType::EncryptedKey { value, nonce, salt } => {
                let password = Zeroizing::new(prompt_password(
                    format!("Enter decryption password for {}: ", self.name).as_str(),
                )?);
                let salt_bytes = BASE64.decode(salt)?;
                let key = Self::derive_key(&password, &salt_bytes)?;
                let cipher = Aes256Gcm::new(&key.into());
                let nonce_bytes = BASE64.decode(nonce)?;
                let nonce = Nonce::from_slice(&nonce_bytes);
                let ciphertext = BASE64.decode(value)?;
                let plaintext = cipher
                    .decrypt(nonce, ciphertext.as_ref())
                    .map_err(|_| anyhow!("Failed to decrypt"))?;
                Ok(Zeroizing::new(String::from_utf8(plaintext)?))
            }
            KeyType::OnePassword { vault, item } => {
                let output = std::process::Command::new("op")
                    .args(["read", &format!("op://{}/{}", vault, item)])
                    .output();
                match output {
                    Ok(output) => {
                        if !output.status.success() {
                            anyhow::bail!(
                                "Failed to read from 1Password: {}",
                                String::from_utf8_lossy(&output.stderr)
                            );
                        } else {
                            Ok(Zeroizing::new(
                                String::from_utf8(output.stdout)?.trim().to_string(),
                            ))
                        }
                    }
                    Err(e) => {
                        anyhow::bail!("Failed to read from 1Password: {}", e);
                    }
                }
            }
            KeyType::Keyring { service, username } => {
                let entry = Entry::new(service, username)?;
                Ok(Zeroizing::new(entry.get_password()?))
            }
        }
    }

    pub fn encrypt(name: String, private_key: &str, password: &str) -> Result<Self> {
        let mut rng = rand::rng();
        let mut salt_bytes = [0u8; 16];
        rng.fill(&mut salt_bytes);
        let key = Self::derive_key(password, &salt_bytes)?;
        let cipher = Aes256Gcm::new(&key.into());
        let mut nonce_bytes = [0u8; 12];
        rng.fill(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, private_key.as_bytes())
            .map_err(|_| anyhow!("Failed to encrypt private key"))?;

        Ok(Key::new(
            name,
            KeyType::EncryptedKey {
                value: BASE64.encode(ciphertext),
                nonce: BASE64.encode(nonce_bytes),
                salt: BASE64.encode(salt_bytes),
            },
        ))
    }

    fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; 32]> {
        let mut key = [0u8; 32];
        argon2::Argon2::default()
            .hash_password_into(password.as_bytes(), salt, &mut key)
            .map_err(|e| anyhow!("Key derivation failed: {}", e))?;
        Ok(key)
    }

    pub fn signer(&self) -> Result<Box<dyn Signer>> {
        Ok(Box::new(self.private_key()?.parse::<PrivateKeySigner>()?))
    }

    pub fn address(&self) -> Result<Address> {
        Ok(self.signer()?.address())
    }

    pub fn validate_private_key(key: &str) -> Result<()> {
        key.parse::<PrivateKeySigner>()
            .map(|_| ())
            .map_err(|e| anyhow!("Invalid private key: {}", e))
    }

    /// Storage backend name, matching the serialized `type` tag.
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            KeyType::PrivateKey { .. } => "PrivateKey",
            KeyType::EncryptedKey { .. } => "EncryptedKey",
            KeyType::OnePassword { .. } => "OnePassword",
            KeyType::Keyring { .. } => "Keyring",
        }
    }

    /// Wallet address, only when derivable without prompting the user
    /// (i.e. plaintext keys). Returns None for all other backends.
    pub fn address_noninteractive(&self) -> Option<String> {
        match self.kind {
            KeyType::PrivateKey { .. } => self.address().ok().map(|a| a.to_string()),
            _ => None,
        }
    }
}

fn default_key_type(keyring_available: bool) -> KeyTypeArg {
    if keyring_available {
        KeyTypeArg::Keyring
    } else {
        KeyTypeArg::Encrypted
    }
}

fn keyring_available() -> bool {
    if std::env::var_os("CHAINZ_DISABLE_KEYRING").is_some() {
        return false;
    }
    *KEYRING_AVAILABLE.get_or_init(probe_keyring)
}

fn probe_keyring() -> bool {
    let username = format!("__chainz_probe_{}", rand::random::<u64>());
    let Ok(entry) = Entry::new(KEYRING_SERVICE, &username) else {
        return false;
    };
    let result = entry
        .set_password("chainz-keyring-probe")
        .and_then(|_| entry.get_password())
        .is_ok_and(|value| value == "chainz-keyring-probe");
    let _ = entry.delete_credential();
    result
}

fn ensure_password_prompt_available() -> Result<()> {
    #[cfg(test)]
    return Ok(());

    #[cfg(not(test))]
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "encrypted storage needs a terminal for its password prompt; use --type private-key only if plaintext storage is intentional"
        );
    }

    #[cfg(not(test))]
    Ok(())
}

fn read_private_key(key: Option<String>) -> Result<String> {
    let private_key = match key {
        Some(key) => key,
        None => prompt_password("Enter private key: ")?,
    };
    Key::validate_private_key(&private_key)?;
    Ok(private_key)
}

fn create_key(name: &str, key: Option<String>, key_type: KeyTypeArg) -> Result<Key> {
    let kind = match key_type {
        KeyTypeArg::PrivateKey => {
            let private_key = read_private_key(key)?;
            eprintln!(
                "{}",
                ui::warn(&format!(
                    "'{}' will be stored as plaintext; migrate later with `chainz key migrate {}`",
                    name, name
                ))
            );
            KeyType::PrivateKey { value: private_key }
        }
        KeyTypeArg::Encrypted => {
            let private_key = read_private_key(key)?;
            ensure_password_prompt_available()?;
            let password = Zeroizing::new(prompt_password("Enter encryption password: ")?);
            if password.is_empty() {
                anyhow::bail!("Encryption password cannot be empty");
            }
            let confirmation = Zeroizing::new(prompt_password("Confirm encryption password: ")?);
            if password.as_str() != confirmation.as_str() {
                anyhow::bail!("Encryption passwords do not match");
            }
            Key::encrypt(name.to_string(), &private_key, &password)?.kind
        }
        KeyTypeArg::OnePassword => {
            let vault: String = dialoguer::Input::new()
                .with_prompt("Enter 1Password vault name")
                .interact()?;
            let item: String = dialoguer::Input::new()
                .with_prompt("Enter 1Password item name")
                .interact()?;
            KeyType::OnePassword { vault, item }
        }
        KeyTypeArg::Keyring => {
            let service = KEYRING_SERVICE.to_string();
            let username = name.to_string();
            let private_key = read_private_key(key)?;
            Entry::new(&service, &username)?.set_password(&private_key)?;
            KeyType::Keyring { service, username }
        }
    };

    Ok(Key::new(name.to_string(), kind))
}

fn create_key_with_safe_default(name: &str, key: String) -> Result<Key> {
    if keyring_available() {
        match create_key(name, Some(key.clone()), KeyTypeArg::Keyring) {
            Ok(stored) => return Ok(stored),
            Err(error) => eprintln!(
                "{}",
                ui::warn(&format!(
                    "OS keyring unavailable ({error}); falling back to encrypted storage"
                ))
            ),
        }
    }
    create_key(name, Some(key), KeyTypeArg::Encrypted)
}

fn select_key_type() -> Result<KeyTypeArg> {
    let variants = <KeyTypeArg as clap::ValueEnum>::value_variants();
    let preferred = default_key_type(keyring_available());
    let default = variants
        .iter()
        .position(|value| *value == preferred)
        .unwrap_or(0);
    let selection = dialoguer::Select::new()
        .with_prompt("Select key storage")
        .items(variants)
        .default(default)
        .interact()?;
    Ok(variants[selection])
}

pub(crate) fn prompt_for_new_key(name: &str) -> Result<Key> {
    let key_type = select_key_type()?;
    create_key(name, None, key_type)
}

pub(crate) fn create_default_key(name: &str, private_key: String) -> Result<Key> {
    create_key_with_safe_default(name, private_key)
}

fn migration_target(target: Option<SafeKeyTypeArg>) -> KeyTypeArg {
    match target {
        Some(SafeKeyTypeArg::Encrypted) => KeyTypeArg::Encrypted,
        Some(SafeKeyTypeArg::Keyring) => KeyTypeArg::Keyring,
        None => default_key_type(keyring_available()),
    }
}

fn migrate_one(source: &Key, target: KeyTypeArg) -> Result<Key> {
    let private_key = source.private_key()?.to_string();
    create_key(&source.name, Some(private_key), target)
}

impl KeyCommand {
    pub async fn handle(self, config: &mut Chainz) -> Result<()> {
        match self {
            KeyCommand::Add {
                name,
                key,
                key_type,
            } => {
                if config.config.keys.contains_key(&name) {
                    anyhow::bail!("Key '{}' already exists", name);
                }
                let key = match (key_type, key) {
                    (Some(key_type), key) => create_key(&name, key, key_type)?,
                    (None, Some(key)) => create_key_with_safe_default(&name, key)?,
                    (None, None) => {
                        let choice = select_key_type()?;
                        create_key(&name, None, choice)?
                    }
                };
                config.add_key(&name, key)?;
                println!("Added key '{}'", name);
                config.save().await?;
            }
            KeyCommand::List { json } => {
                let keys = config.list_keys();
                if json {
                    // Addresses only where derivable without prompting;
                    // never includes key material.
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
                        // Display derives the address only for plaintext keys,
                        // so listing never prompts for decryption
                        println!("- {}", key);
                    }
                }
            }
            KeyCommand::Remove { name } => {
                config.remove_key(&name)?;
                println!("Removed key '{}'", name);
                config.save().await?;
            }
            KeyCommand::Migrate { name, all, to } => {
                if !all && name.is_none() {
                    anyhow::bail!("Provide a key name or use --all");
                }
                let target = migration_target(to);
                let names: Vec<String> = if all {
                    let mut names: Vec<_> = config
                        .config
                        .keys
                        .iter()
                        .filter(|(_, key)| matches!(key.kind, KeyType::PrivateKey { .. }))
                        .map(|(name, _)| name.clone())
                        .collect();
                    names.sort();
                    names
                } else {
                    vec![name.expect("validated above")]
                };

                if names.is_empty() {
                    println!("No plaintext keys to migrate");
                    return Ok(());
                }

                let mut migrated = 0;
                for name in names {
                    let source = config.get_key(&name)?;
                    match migrate_one(&source, target) {
                        Ok(key) => {
                            config.config.keys.insert(name.clone(), key);
                            println!("{}", ui::success(&format!("Migrated key '{}'", name)));
                            migrated += 1;
                        }
                        Err(error) if all => {
                            println!(
                                "{}",
                                ui::fail(&format!("Could not migrate '{}': {}", name, error))
                            );
                        }
                        Err(error) => return Err(error),
                    }
                }
                if migrated > 0 {
                    config.save().await?;
                }
            }
        }
        Ok(())
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let display = match &self.kind {
            KeyType::PrivateKey { .. } => {
                let addr = self
                    .address_noninteractive()
                    .unwrap_or_else(|| "Invalid key".to_string());
                format!("{} ({})", self.name, addr)
            }
            KeyType::EncryptedKey { .. } => {
                format!("{} (encrypted)", self.name)
            }
            KeyType::OnePassword { .. } => {
                format!("{} (1password)", self.name)
            }
            KeyType::Keyring { .. } => {
                format!("{} (keyring)", self.name)
            }
        };
        write!(f, "{}", display)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::Mutex;

    const TEST_PRIVATE_KEY: &str =
        "0000000000000000000000000000000000000000000000000000000000000001";
    const TEST_ADDRESS: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";
    static PASSWORD_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn safe_default_prefers_keyring_then_encrypted() {
        assert_eq!(default_key_type(true), KeyTypeArg::Keyring);
        assert_eq!(default_key_type(false), KeyTypeArg::Encrypted);
    }

    #[test]
    fn migrate_plaintext_to_encrypted_round_trips() {
        let _guard = PASSWORD_ENV_LOCK.lock().unwrap();
        let source = Key::new(
            "deployer".to_string(),
            KeyType::PrivateKey {
                value: TEST_PRIVATE_KEY.to_string(),
            },
        );
        // SAFETY: the test prompt reads this process-local fixture value.
        unsafe { env::set_var("CLITEST_PASSWORD", "migration-password") };

        let migrated = migrate_one(&source, KeyTypeArg::Encrypted).unwrap();

        assert!(matches!(migrated.kind, KeyType::EncryptedKey { .. }));
        assert_eq!(migrated.private_key().unwrap().as_str(), TEST_PRIVATE_KEY);
    }

    #[test]
    fn test_plain_private_key() -> Result<()> {
        let key = Key::new(
            "test".to_string(),
            KeyType::PrivateKey {
                value: TEST_PRIVATE_KEY.to_string(),
            },
        );
        assert_eq!(key.private_key()?.as_str(), TEST_PRIVATE_KEY);
        assert_eq!(key.address()?.to_string(), TEST_ADDRESS);
        Ok(())
    }

    #[test]
    fn test_encrypted_key() -> Result<()> {
        let _guard = PASSWORD_ENV_LOCK.lock().unwrap();
        let password = "test_password";
        let encrypted = Key::encrypt("test".to_string(), TEST_PRIVATE_KEY, password)?;

        // Ensure the encrypted value is different from the original
        if let KeyType::EncryptedKey { value, .. } = &encrypted.kind {
            assert_ne!(value, TEST_PRIVATE_KEY);
        } else {
            panic!("Expected EncryptedKey variant");
        }

        // Test decryption fails with wrong password
        // SAFETY: tests that touch process env run single-threaded per binary here;
        // this var is only read by the mock password prompt below.
        unsafe { env::set_var("CLITEST_PASSWORD", "wrong_password") };
        assert!(encrypted.private_key().is_err());

        // Test decryption succeeds with correct password
        unsafe { env::set_var("CLITEST_PASSWORD", password) };
        assert_eq!(encrypted.private_key()?.as_str(), TEST_PRIVATE_KEY);
        assert_eq!(encrypted.address()?.to_string(), TEST_ADDRESS);
        Ok(())
    }

    #[test]
    fn test_keyring() -> Result<()> {
        let service = "chainz_test";
        let username = "test_user";

        // Try to create a keyring entry
        match Entry::new(service, username) {
            Ok(entry) => {
                // Clean up any existing test key
                let _ = entry.delete_credential();

                // Attempt to set password
                match entry.set_password(TEST_PRIVATE_KEY) {
                    Ok(_) => {
                        // If we successfully set the password, run the full test
                        let key = Key::new(
                            "test".to_string(),
                            KeyType::Keyring {
                                service: service.to_string(),
                                username: username.to_string(),
                            },
                        );

                        assert_eq!(key.private_key()?.as_str(), TEST_PRIVATE_KEY);
                        assert_eq!(key.address()?.to_string(), TEST_ADDRESS);

                        // Cleanup
                        let _ = entry.delete_credential();
                    }
                    Err(e) => {
                        println!("Skipping keyring test (failed to set password: {})", e);
                    }
                }
            }
            Err(e) => {
                println!("Skipping keyring test (no keyring access: {})", e);
            }
        }
        Ok(())
    }

    #[test]
    fn test_one_password() -> Result<()> {
        let key = Key::new(
            "test".to_string(),
            KeyType::OnePassword {
                vault: "test_vault".to_string(),
                item: "test_item".to_string(),
            },
        );

        // This test will fail if 1Password CLI is not installed or not authenticated
        // We'll make it a soft failure with a warning
        match key.private_key() {
            Ok(pk) => {
                println!("1Password integration test succeeded");
                assert!(!pk.is_empty());
            }
            Err(e) => {
                println!("Skipping 1Password test ({})", e);
            }
        }
        Ok(())
    }

    /// kind_name hand-maintains the serde `type` tag names; this pins them
    /// together so a rename in one place can't silently desync the other.
    #[test]
    fn test_kind_name_matches_serde_tag() {
        let keys = [
            Key::new(
                "a".into(),
                KeyType::PrivateKey {
                    value: TEST_PRIVATE_KEY.into(),
                },
            ),
            Key::encrypt("b".into(), TEST_PRIVATE_KEY, "pw").unwrap(),
            Key::new(
                "c".into(),
                KeyType::OnePassword {
                    vault: "v".into(),
                    item: "i".into(),
                },
            ),
            Key::new(
                "d".into(),
                KeyType::Keyring {
                    service: "s".into(),
                    username: "u".into(),
                },
            ),
        ];
        for key in keys {
            let serialized = serde_json::to_value(&key.kind).unwrap();
            assert_eq!(serialized["type"], key.kind_name());
        }
    }

    #[test]
    fn test_key_type_picker_labels() {
        let labels: Vec<String> = <KeyTypeArg as clap::ValueEnum>::value_variants()
            .iter()
            .map(|k| k.to_string())
            .collect();
        assert_eq!(
            labels,
            vec!["Private Key", "Encrypted Key", "One Password", "Keyring"]
        );
    }

    #[test]
    fn test_derive_key() {
        let salt = [0u8; 16];
        let password = "test_password";
        let key1 = Key::derive_key(password, &salt).unwrap();
        let key2 = Key::derive_key(password, &salt).unwrap();
        let key3 = Key::derive_key("different_password", &salt).unwrap();

        assert_eq!(key1, key2);
        assert_ne!(key1, key3);

        // Different salt produces different key
        let other_salt = [1u8; 16];
        let key4 = Key::derive_key(password, &other_salt).unwrap();
        assert_ne!(key1, key4);
    }

    // Helper function for testing password prompts in integration tests
    #[cfg(test)]
    pub fn mock_password_prompt(_prompt: &str) -> Result<String> {
        Ok(env::var("CLITEST_PASSWORD").unwrap_or_else(|_| "test_password".to_string()))
    }
}
