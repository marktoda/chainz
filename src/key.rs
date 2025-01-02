// module for storing configurations of encrypted private keys

#[cfg(test)]
use self::tests::mock_password_prompt as prompt_password;
#[cfg(not(test))]
use rpassword::prompt_password;

use crate::{config::Chainz, opt::KeyCommand};
use alloy::{
    primitives::Address,
    signers::{local::PrivateKeySigner, Signer},
};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use strum::{EnumIter, IntoEnumIterator};

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use keyring::Entry;
use rand::Rng;
use std::fmt;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Key {
    pub name: String,
    #[serde(flatten)]
    pub kind: KeyType,
}

#[derive(Serialize, Deserialize, Clone, Debug, strum::Display, EnumIter)]
#[serde(tag = "type")]
#[strum(serialize_all = "title_case")]
pub enum KeyType {
    #[serde(rename = "PrivateKey")]
    PrivateKey { value: String },
    #[serde(rename = "EncryptedKey")]
    EncryptedKey { value: String, nonce: String },
    #[serde(rename = "OnePassword")]
    OnePassword { vault: String, item: String },
    #[serde(rename = "Keyring")]
    Keyring { service: String, username: String },
}

impl Key {
    pub fn new(name: String, kind: KeyType) -> Self {
        Self { name, kind }
    }

    pub fn private_key(&self) -> Result<String> {
        match &self.kind {
            KeyType::PrivateKey { value } => Ok(value.clone()),
            KeyType::EncryptedKey { value, nonce } => {
                let password =
                    prompt_password(&format!("Enter decryption password for {}: ", self.name))?;
                let key = Self::derive_key(&password);
                let cipher = Aes256Gcm::new(&key.into());
                let nonce_bytes = BASE64.decode(nonce)?;
                let nonce = Nonce::from_slice(&nonce_bytes);
                let ciphertext = BASE64.decode(value)?;
                let plaintext = cipher
                    .decrypt(nonce, ciphertext.as_ref())
                    .map_err(|_| anyhow!("Failed to decrypt"))?;
                Ok(String::from_utf8(plaintext)?)
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
                            Ok(String::from_utf8(output.stdout)?.trim().to_string())
                        }
                    }
                    Err(e) => {
                        anyhow::bail!("Failed to read from 1Password: {}", e);
                    }
                }
            }
            KeyType::Keyring { service, username } => {
                let entry = Entry::new(service, username)?;
                Ok(entry.get_password()?)
            }
        }
    }

    pub fn encrypt(name: String, private_key: &str, password: &str) -> Result<Self> {
        let key = Self::derive_key(password);
        let cipher = Aes256Gcm::new(&key.into());
        let mut rng = rand::thread_rng();
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
            },
        ))
    }

    fn derive_key(password: &str) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(password.as_bytes());
        hasher.finalize().into()
    }

    pub fn signer(&self) -> Result<Box<dyn Signer>> {
        Ok(Box::new(self.private_key()?.parse::<PrivateKeySigner>()?))
    }

    pub fn address(&self) -> Result<Address> {
        Ok(self.signer()?.address())
    }
}

impl KeyCommand {
    pub async fn handle(self, config: &mut Chainz) -> Result<()> {
        match self {
            KeyCommand::Add { name, key } => {
                let key_types: Vec<_> = KeyType::iter().collect();

                let choice = dialoguer::Select::new()
                    .with_prompt("Select key type")
                    .items(&key_types)
                    .default(0)
                    .interact()?;

                let kind = match choice {
                    // raw private key
                    0 => {
                        let pk = if let Some(k) = key {
                            k
                        } else {
                            prompt_password("Enter private key: ")?
                        };
                        KeyType::PrivateKey { value: pk }
                    }
                    // encrypted private key
                    1 => {
                        let pk = if let Some(k) = key {
                            k
                        } else {
                            prompt_password("Enter private key: ")?
                        };
                        let password = prompt_password("Enter encryption password: ")?;
                        Key::encrypt(name.clone(), &pk, &password)?.kind
                    }
                    // 1password private key
                    2 => {
                        let vault: String = dialoguer::Input::new()
                            .with_prompt("Enter 1Password vault name")
                            .interact()?;
                        let item: String = dialoguer::Input::new()
                            .with_prompt("Enter 1Password item name")
                            .interact()?;
                        KeyType::OnePassword { vault, item }
                    }
                    // keyring private key
                    3 => {
                        let service: String = dialoguer::Input::new()
                            .with_prompt("Enter service name")
                            .default("chainz".into())
                            .interact()?;
                        let username: String = dialoguer::Input::new()
                            .with_prompt("Enter username")
                            .interact()?;
                        let pk = if let Some(k) = key {
                            k
                        } else {
                            prompt_password("Enter private key: ")?
                        };
                        // Store in system keyring
                        let entry = Entry::new(&service, &username)?;
                        entry.set_password(&pk)?;
                        KeyType::Keyring { service, username }
                    }
                    _ => anyhow::bail!("Invalid choice"),
                };

                let key = Key::new(name.clone(), kind);
                config.add_key(&name, key).await?;
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
        assert_eq!(key.private_key()?, TEST_PRIVATE_KEY);
        assert_eq!(key.address()?.to_string(), TEST_ADDRESS);
        Ok(())
    }

    #[test]
    fn test_encrypted_key() -> Result<()> {
        let password = "test_password";
        let encrypted = Key::encrypt("test".to_string(), TEST_PRIVATE_KEY, password)?;

        // Ensure the encrypted value is different from the original
        if let KeyType::EncryptedKey { value, nonce: _ } = &encrypted.kind {
            assert_ne!(value, TEST_PRIVATE_KEY);
        } else {
            panic!("Expected EncryptedKey variant");
        }

        // Test decryption fails with wrong password
        env::set_var("CLITEST_PASSWORD", "wrong_password");
        assert!(encrypted.private_key().is_err());

        // Test decryption succeeds with correct password
        env::set_var("CLITEST_PASSWORD", password);
        assert_eq!(encrypted.private_key()?, TEST_PRIVATE_KEY);
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
                let _ = entry.delete_password();

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

                        assert_eq!(key.private_key()?, TEST_PRIVATE_KEY);
                        assert_eq!(key.address()?.to_string(), TEST_ADDRESS);

                        // Cleanup
                        let _ = entry.delete_password();
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
    fn test_key_display() {
        let key_types: Vec<String> = KeyType::iter().map(|k| k.to_string()).collect();
        assert_eq!(
            key_types,
            vec!["Private Key", "Encrypted Key", "One Password", "Keyring"]
        );
    }

    #[test]
    fn test_derive_key() {
        let password = "test_password";
        let key1 = Key::derive_key(password);
        let key2 = Key::derive_key(password);
        let key3 = Key::derive_key("different_password");

        assert_eq!(key1, key2);
        assert_ne!(key1, key3);
    }

    // Helper function for testing password prompts in integration tests
    #[cfg(test)]
    pub fn mock_password_prompt(_prompt: &str) -> Result<String> {
        Ok(env::var("CLITEST_PASSWORD").unwrap_or_else(|_| "test_password".to_string()))
    }
}
