use super::*;
use std::{cell::RefCell, collections::HashMap};

const TEST_PRIVATE_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const TEST_ADDRESS: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";

struct MemoryBackend {
    interactive: bool,
    available: bool,
    prompts: RefCell<Vec<String>>,
    keyring: RefCell<HashMap<(String, String), String>>,
    one_password: RefCell<HashMap<(String, String), String>>,
}

impl MemoryBackend {
    fn new(interactive: bool, available: bool, prompts: &[&str]) -> Self {
        Self {
            interactive,
            available,
            prompts: RefCell::new(prompts.iter().rev().map(|s| s.to_string()).collect()),
            keyring: RefCell::new(HashMap::new()),
            one_password: RefCell::new(HashMap::new()),
        }
    }
}

impl KeyBackend for MemoryBackend {
    fn is_interactive(&self) -> bool {
        self.interactive
    }
    fn prompt_secret(&self, _prompt: &str) -> Result<Zeroizing<String>> {
        Ok(Zeroizing::new(
            self.prompts
                .borrow_mut()
                .pop()
                .ok_or_else(|| anyhow!("no prompt value"))?,
        ))
    }
    fn keyring_available(&self) -> bool {
        self.available
    }
    fn keyring_get(&self, service: &str, username: &str) -> Result<Zeroizing<String>> {
        Ok(Zeroizing::new(
            self.keyring
                .borrow()
                .get(&(service.to_string(), username.to_string()))
                .cloned()
                .ok_or_else(|| anyhow!("missing keyring entry"))?,
        ))
    }
    fn keyring_set(&self, service: &str, username: &str, value: &str) -> Result<bool> {
        let location = (service.to_string(), username.to_string());
        let mut keyring = self.keyring.borrow_mut();
        match keyring.get(&location) {
            Some(existing) if existing == value => Ok(false),
            Some(_) => anyhow::bail!("credential already exists"),
            None => {
                keyring.insert(location, value.to_string());
                Ok(true)
            }
        }
    }
    fn keyring_delete(&self, service: &str, username: &str) -> Result<()> {
        self.keyring
            .borrow_mut()
            .remove(&(service.to_string(), username.to_string()));
        Ok(())
    }
    fn one_password_get(&self, vault: &str, item: &str) -> Result<Zeroizing<String>> {
        Ok(Zeroizing::new(
            self.one_password
                .borrow()
                .get(&(vault.to_string(), item.to_string()))
                .cloned()
                .ok_or_else(|| anyhow!("missing 1Password entry"))?,
        ))
    }
}

#[test]
fn plaintext_and_encrypted_round_trip() -> Result<()> {
    let plaintext = Key::new(
        "test".into(),
        KeyType::PrivateKey {
            value: TEST_PRIVATE_KEY.into(),
        },
    );
    let plain_backend = MemoryBackend::new(true, false, &[]);
    assert_eq!(
        KeyVault::new(plain_backend).resolve(&plaintext)?.as_str(),
        TEST_PRIVATE_KEY
    );

    let encrypted = encrypt_with_password("test".into(), TEST_PRIVATE_KEY, "password")?;
    let encrypted_backend = MemoryBackend::new(true, false, &["password"]);
    assert_eq!(
        KeyVault::new(encrypted_backend)
            .resolve(&encrypted)?
            .as_str(),
        TEST_PRIVATE_KEY
    );
    assert_eq!(
        Key::address_from_private_key(TEST_PRIVATE_KEY)?.to_string(),
        TEST_ADDRESS
    );
    Ok(())
}

#[test]
fn encrypted_key_rejects_wrong_password() -> Result<()> {
    let encrypted = encrypt_with_password("test".into(), TEST_PRIVATE_KEY, "correct")?;
    let error = KeyVault::new(MemoryBackend::new(true, false, &["wrong"]))
        .resolve(&encrypted)
        .unwrap_err()
        .to_string();
    assert!(error.contains("Failed to decrypt"));
    Ok(())
}

#[test]
fn one_password_adapter_is_hermetic() -> Result<()> {
    let backend = MemoryBackend::new(false, false, &[]);
    backend.one_password.borrow_mut().insert(
        ("vault".to_string(), "item/field".to_string()),
        TEST_PRIVATE_KEY.to_string(),
    );
    let key = Key::new(
        "deployer".into(),
        KeyType::OnePassword {
            vault: "vault".into(),
            item: "item/field".into(),
        },
    );
    assert_eq!(
        KeyVault::new(backend).resolve(&key)?.as_str(),
        TEST_PRIVATE_KEY
    );
    Ok(())
}

#[test]
fn default_ladder_prefers_keyring_and_uses_standard_shape() -> Result<()> {
    let backend = MemoryBackend::new(false, true, &[]);
    let vault = KeyVault::new(backend);
    let key = vault
        .provision_private_key("deployer", TEST_PRIVATE_KEY, None)?
        .key()
        .clone();
    assert!(matches!(
        key.kind,
        KeyType::Keyring { ref service, ref username }
            if service == KEYRING_SERVICE && username == "deployer"
    ));
    assert_eq!(vault.resolve(&key)?.as_str(), TEST_PRIVATE_KEY);
    Ok(())
}

#[test]
fn replacement_keyring_entry_avoids_the_standard_location() -> Result<()> {
    let backend = MemoryBackend::new(false, true, &[]);
    let vault = KeyVault::new(backend);
    let key = vault
        .provision_replacement_private_key("default", TEST_PRIVATE_KEY)?
        .key()
        .clone();
    assert!(matches!(
        key.kind,
        KeyType::Keyring { ref service, ref username }
            if service == KEYRING_SERVICE
                && username.starts_with("default-replacement-")
                && username != "default"
    ));
    assert_eq!(vault.resolve(&key)?.as_str(), TEST_PRIVATE_KEY);
    Ok(())
}

#[test]
fn rollback_removes_only_credentials_created_by_the_provision() -> Result<()> {
    let backend = MemoryBackend::new(false, true, &[]);
    let vault = KeyVault::new(backend);
    let provision = vault.provision_private_key("deployer", TEST_PRIVATE_KEY, None)?;
    assert!(vault.resolve(provision.key()).is_ok());

    vault.rollback(&provision)?;
    assert!(vault.resolve(provision.key()).is_err());

    vault.backend.keyring.borrow_mut().insert(
        (KEYRING_SERVICE.to_string(), "deployer".to_string()),
        TEST_PRIVATE_KEY.to_string(),
    );
    let reused = vault.provision_private_key("deployer", TEST_PRIVATE_KEY, None)?;
    vault.rollback(&reused)?;
    assert_eq!(vault.resolve(reused.key())?.as_str(), TEST_PRIVATE_KEY);
    Ok(())
}

#[test]
fn default_ladder_encrypts_with_confirmation() -> Result<()> {
    let backend = MemoryBackend::new(true, false, &["password", "password"]);
    let vault = KeyVault::new(backend);
    let key = vault
        .provision_private_key("deployer", TEST_PRIVATE_KEY, None)?
        .key()
        .clone();
    assert!(matches!(key.kind, KeyType::EncryptedKey { .. }));
    Ok(())
}

#[test]
fn noninteractive_encrypted_fallback_errors() {
    let backend = MemoryBackend::new(false, false, &[]);
    let error = KeyVault::new(backend)
        .provision_private_key("deployer", TEST_PRIVATE_KEY, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("interactive password prompt"), "{error}");
}

#[test]
fn plaintext_to_keyring_migration_is_hermetic() -> Result<()> {
    let source = Key::new(
        "deployer".into(),
        KeyType::PrivateKey {
            value: TEST_PRIVATE_KEY.into(),
        },
    );
    let vault = KeyVault::new(MemoryBackend::new(false, true, &[]));
    let migrated = vault
        .provision_migration(&source, Some(MigrationTargetArg::Keyring))?
        .key()
        .clone();
    assert!(matches!(migrated.kind, KeyType::Keyring { .. }));
    assert_eq!(vault.resolve(&migrated)?.as_str(), TEST_PRIVATE_KEY);
    Ok(())
}

#[test]
fn plaintext_to_encrypted_migration_round_trips() -> Result<()> {
    let source = Key::new(
        "deployer".into(),
        KeyType::PrivateKey {
            value: TEST_PRIVATE_KEY.into(),
        },
    );
    let vault = KeyVault::new(MemoryBackend::new(
        true,
        false,
        &["password", "password", "password"],
    ));
    let migrated = vault
        .provision_migration(&source, Some(MigrationTargetArg::Encrypted))?
        .key()
        .clone();
    assert!(matches!(migrated.kind, KeyType::EncryptedKey { .. }));
    assert_eq!(vault.resolve(&migrated)?.as_str(), TEST_PRIVATE_KEY);
    Ok(())
}

#[test]
fn debug_never_contains_key_material() {
    let key = Key::new(
        "test".into(),
        KeyType::PrivateKey {
            value: TEST_PRIVATE_KEY.into(),
        },
    );
    assert!(!format!("{key:?}").contains(TEST_PRIVATE_KEY));
}

#[test]
fn plaintext_cached_address_must_match_private_key() {
    let mut key = Key::new(
        "test".into(),
        KeyType::PrivateKey {
            value: TEST_PRIVATE_KEY.into(),
        },
    );
    key.address = Some("0x0000000000000000000000000000000000000001".into());
    assert!(
        key.validate_record()
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );
}

#[test]
fn display_includes_cached_address_and_storage_backend() {
    let key = Key {
        name: "deployer".into(),
        address: Some(TEST_ADDRESS.into()),
        kind: KeyType::Keyring {
            service: KEYRING_SERVICE.into(),
            username: "deployer".into(),
        },
    };
    let output = key.to_string();
    assert!(output.contains(TEST_ADDRESS));
    assert!(output.contains("keyring"));
    assert!(!output.contains(TEST_PRIVATE_KEY));
}

#[test]
fn kind_names_match_serialized_tags() {
    let keys = [
        Key::new(
            "plain".into(),
            KeyType::PrivateKey {
                value: TEST_PRIVATE_KEY.into(),
            },
        ),
        encrypt_with_password("encrypted".into(), TEST_PRIVATE_KEY, "pw").unwrap(),
        Key::new(
            "op".into(),
            KeyType::OnePassword {
                vault: "vault".into(),
                item: "item/field".into(),
            },
        ),
        Key::new(
            "keyring".into(),
            KeyType::Keyring {
                service: "chainz".into(),
                username: "keyring".into(),
            },
        ),
    ];
    for key in keys {
        let serialized = serde_json::to_value(&key.kind).unwrap();
        assert_eq!(serialized["type"], key.kind_name());
    }
}

#[test]
fn legacy_encrypted_record_gets_default_kdf_parameters() -> Result<()> {
    let encrypted = encrypt_with_password("test".into(), TEST_PRIVATE_KEY, "pw")?;
    let mut json = serde_json::to_value(&encrypted)?;
    let object = json.as_object_mut().unwrap();
    object.remove("version");
    object.remove("kdf_memory_kib");
    object.remove("kdf_iterations");
    object.remove("kdf_parallelism");
    let restored: Key = serde_json::from_value(json)?;
    let backend = MemoryBackend::new(true, false, &["pw"]);
    assert_eq!(
        KeyVault::new(backend).resolve(&restored)?.as_str(),
        TEST_PRIVATE_KEY
    );
    Ok(())
}
