// module for storing configurations of encrypted private keys

#[cfg(test)]
use self::tests::mock_password_prompt as prompt_password;
#[cfg(not(test))]
use rpassword::prompt_password;

use crate::{
    config::Chainz,
    opt::{KeyCommand, KeyTypeArg},
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
use std::fmt;
use zeroize::Zeroizing;

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
}

impl KeyCommand {
    pub async fn handle(self, config: &mut Chainz) -> Result<()> {
        match self {
            KeyCommand::Add {
                name,
                key,
                key_type,
            } => {
                let choice = match key_type {
                    Some(t) => t,
                    // --key without --type implies a plain private key, so
                    // `chainz key add <name> --key <key>` works without a TTY
                    None if key.is_some() => KeyTypeArg::PrivateKey,
                    None => {
                        let variants = <KeyTypeArg as clap::ValueEnum>::value_variants();
                        let selection = dialoguer::Select::new()
                            .with_prompt("Select key type")
                            .items(variants)
                            .default(0)
                            .interact()?;
                        variants[selection]
                    }
                };

                let get_pk = |key: Option<String>| -> Result<String> {
                    let pk = match key {
                        Some(k) => k,
                        None => prompt_password("Enter private key: ")?,
                    };
                    Key::validate_private_key(&pk)?;
                    Ok(pk)
                };

                let kind = match choice {
                    KeyTypeArg::PrivateKey => KeyType::PrivateKey {
                        value: get_pk(key)?,
                    },
                    KeyTypeArg::Encrypted => {
                        let pk = get_pk(key)?;
                        let password = prompt_password("Enter encryption password: ")?;
                        Key::encrypt(name.clone(), &pk, &password)?.kind
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
                        let service: String = dialoguer::Input::new()
                            .with_prompt("Enter service name")
                            .default("chainz".into())
                            .interact()?;
                        let username: String = dialoguer::Input::new()
                            .with_prompt("Enter username")
                            .interact()?;
                        let pk = get_pk(key)?;
                        // Store in system keyring
                        let entry = Entry::new(&service, &username)?;
                        entry.set_password(&pk)?;
                        KeyType::Keyring { service, username }
                    }
                };

                let key = Key::new(name.clone(), kind);
                config.add_key(&name, key)?;
                println!("Added key '{}'", name);
                config.save().await?;
            }
            KeyCommand::List => {
                let keys = config.list_keys();
                if keys.is_empty() {
                    println!("No stored keys");
                } else {
                    println!("Stored keys:");
                    for (name, key) in keys {
                        // print error if there is one
                        match key.address() {
                            Ok(addr) => {
                                println!("- {}: {}", name, addr);
                            }
                            Err(e) => {
                                eprintln!("- {}: {}", name, e);
                            }
                        }
                    }
                }
            }
            KeyCommand::Remove { name } => {
                config.remove_key(&name)?;
                println!("Removed key '{}'", name);
                config.save().await?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let display = match &self.kind {
            KeyType::PrivateKey { value } => {
                // Only try to get address for unencrypted keys
                let addr = Key::new(
                    self.name.clone(),
                    KeyType::PrivateKey {
                        value: value.clone(),
                    },
                )
                .address()
                .map(|a| a.to_string())
                .unwrap_or("Invalid key".to_string());
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

    const TEST_PRIVATE_KEY: &str =
        "0000000000000000000000000000000000000000000000000000000000000001";
    const TEST_ADDRESS: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";

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
