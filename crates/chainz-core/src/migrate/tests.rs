use super::*;
use crate::model::Config;
use crate::secrets::Secrets;
use crate::store::{Chainz, LoadMode, StorePaths};

const FIXTURE: &str = include_str!("../../tests/fixtures/v1-config.json");

fn fixture_secrets() -> Vec<String> {
    let v1: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let mut secrets: Vec<String> = v1["variables"]
        .as_object()
        .unwrap()
        .values()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    secrets.push(
        v1["chains"][0]["verification_api_key"]
            .as_str()
            .unwrap()
            .to_string(),
    );
    for chain in v1["chains"].as_array().unwrap() {
        for url in chain["rpc_urls"].as_array().unwrap() {
            if let Some(url) = url.as_str()
                && let Some(i) = url.find("alchemy.invalid/v2/")
            {
                let key = &url[i + "alchemy.invalid/v2/".len()..];
                if !key.starts_with("${") {
                    secrets.push(key.to_string());
                }
            }
        }
    }
    secrets
}

fn parsed(plan: &Plan) -> (Config, Secrets) {
    (
        crate::model::Config::parse(&plan.config_toml).unwrap(),
        Secrets::parse(&plan.secrets_toml).unwrap(),
    )
}

#[test]
fn fixture_migrates_with_the_expected_shape() {
    let plan = plan(FIXTURE).unwrap();
    let s = &plan.summary;
    assert_eq!(
        (s.chains, s.endpoints, s.signers, s.variables),
        (17, 149, 3, 4)
    );
    let (config, secrets) = parsed(&plan);
    assert_eq!(
        config.chains.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "eth",
            "base",
            "uni",
            "bsc",
            "polygon",
            "celo",
            "op",
            "xlayer",
            "world",
            "arb",
            "zora",
            "soneium",
            "megaeth",
            "avax",
            "robinhood",
            "zksync",
            "arc"
        ]
    );
    let endpoints: Vec<_> = config.chains.values().flat_map(|c| c.rpcs.iter()).collect();
    assert_eq!(endpoints.len(), 149);
    assert_eq!(
        endpoints
            .iter()
            .filter(|(_, e)| !e.headers.is_empty())
            .count(),
        16
    );
    let ws: Vec<_> = endpoints.iter().filter(|(_, e)| e.is_websocket()).collect();
    assert_eq!(ws.len(), 24);
    assert!(ws.iter().all(|(name, _)| name.contains("-ws")), "{ws:?}");
    assert!(config.chains.values().all(|c| c.failover.is_empty()));

    // Names and aliases (step 2).
    let eth = &config.chains["eth"];
    assert_eq!(eth.aliases, ["Ethereum Mainnet"]);
    assert_eq!(eth.rpc, "infura");
    assert_eq!(
        config.chains["zksync"].aliases,
        ["zkSync Mainnet", "zksync era"]
    );
    assert_eq!(config.chains["arb"].rpc, "uniswap");
    assert_eq!(
        config.chains["polygon"].rpcs[&config.chains["polygon"].rpc].url,
        "https://polygon-mainnet.infura.invalid/v3/${INFURA_API_KEY}"
    );

    // Credentials (step 4): the Infura key reuses the variable that already
    // held it; the two Alchemy keys become new variables.
    assert_eq!(
        eth.rpcs["infura"].url,
        "https://ethereum-mainnet.infura.invalid/v3/${INFURA_API_KEY}"
    );
    let mut extracted = s.extracted.clone();
    extracted.sort();
    assert_eq!(
        extracted,
        ["ALCHEMY_KEY", "ALCHEMY_KEY_2", "ETHERSCAN_API_KEY"]
    );
    assert_eq!(secrets.vars.len(), 7);
    let quiknode_notes = s
        .attention
        .iter()
        .filter(|a| a.contains("quiknode"))
        .count();
    assert_eq!(quiknode_notes, 5, "{:?}", s.attention);

    // Explorers (step 5): one shared explorer on exactly 8 chains.
    assert_eq!(config.explorers.len(), 1);
    let etherscan = &config.explorers["etherscan"];
    assert_eq!(
        etherscan.api,
        "https://api.etherscan.invalid/v2/api?chainid=${CHAIN_ID}"
    );
    assert_eq!(etherscan.key.as_deref(), Some("${ETHERSCAN_API_KEY}"));
    assert_eq!(
        config
            .chains
            .values()
            .filter(|c| c.explorer.is_some())
            .count(),
        8
    );

    // Signers (step 6).
    assert_eq!(config.signers.len(), 3);
    for (name, signer) in &config.signers {
        assert_eq!(signer.backend, Backend::Keyring);
        assert!(signer.address.is_some());
        assert_eq!(signer.keyring_entry(name), ("chainz", name.as_str()));
    }
    assert_eq!(config.chains["xlayer"].signer.as_deref(), Some("funder"));

    assert!(
        s.to_string()
            .starts_with("migrated 17 chains, 149 endpoints, 3 signers, 4 variables")
    );
}

#[test]
fn config_toml_holds_no_extracted_secret() {
    let plan = plan(FIXTURE).unwrap();
    for secret in fixture_secrets() {
        assert!(
            !plan.config_toml.contains(&secret),
            "config.toml leaks a secret"
        );
        assert!(
            plan.secrets_toml.contains(&secret),
            "secrets.toml lost a value"
        );
        for note in &plan.summary.attention {
            assert!(!note.contains(&secret));
        }
    }
}

#[test]
fn migration_writes_once_in_order_and_backs_up_v1() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path().join("chainz"));
    std::fs::create_dir_all(paths.dir()).unwrap();
    assert_eq!(status(&paths), Status::NoV1);
    std::fs::write(paths.v1_config(), FIXTURE).unwrap();
    assert_eq!(
        status(&paths),
        Status::Pending(paths.v1_config().to_path_buf())
    );
    // A dry run plans the same migration and writes nothing.
    let planned = preview(&paths).unwrap().unwrap();
    assert!(!paths.config().exists());

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let paths = paths.clone();
            std::thread::spawn(move || {
                let lock = WriteLock::acquire(&paths).unwrap();
                migrate_locked(&lock).unwrap().is_some()
            })
        })
        .collect();
    let migrated = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|m| *m)
        .count();
    assert_eq!(migrated, 1);
    assert!(!paths.v1_config().exists());
    assert_eq!(status(&paths), Status::Done);
    assert!(preview(&paths).unwrap().is_none());
    assert_eq!(
        std::fs::read_to_string(paths.config()).unwrap(),
        planned.config_toml
    );
    assert!(paths.dir().join("config.v1.json.bak").exists());
    let summary = Summary {
        backup: Some(paths.dir().join("config.v1.json.bak")),
        ..Summary::default()
    };
    let report = summary.report(Some(paths.config()));
    assert!(report[1].contains(" (v1 config kept as "), "{report:?}");
    let chainz = Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
    assert_eq!(chainz.config().chains.len(), 17);
    assert_eq!(chainz.secrets().unwrap().vars.len(), 7);
}

#[test]
fn open_auto_migrates() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path().to_path_buf());
    std::fs::write(paths.v1_config(), FIXTURE).unwrap();
    let chainz = Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
    assert!(chainz.exists());
    assert_eq!(chainz.config().chains.len(), 17);
    // Reported, never printed.
    let notices = chainz.take_notices();
    assert!(
        matches!(&notices[..], [crate::store::Notice::Migrated { summary, .. }] if summary.chains == 17),
        "{notices:?}"
    );
}

#[test]
fn a_migration_inside_a_write_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path().to_path_buf());
    let mut chainz = Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
    // v1 appears after the load (an older chainz ran meanwhile).
    std::fs::write(paths.v1_config(), FIXTURE).unwrap();
    chainz.transact(|tx| tx.set_var("K", "v")).unwrap();
    assert_eq!(chainz.config().chains.len(), 17);
    let notices = chainz.take_notices();
    assert!(
        matches!(&notices[..], [crate::store::Notice::Migrated { .. }]),
        "{notices:?}"
    );
}

#[test]
fn peek_never_migrates() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path().to_path_buf());
    std::fs::write(paths.v1_config(), FIXTURE).unwrap();
    let chainz = Chainz::open_at(paths.clone(), LoadMode::Peek).unwrap();
    assert!(!chainz.exists() && paths.v1_config().exists());
    assert!(chainz.take_notices().is_empty());
}

#[test]
fn plaintext_and_encrypted_keys_move_to_secrets() {
    let v1 = r#"{
      "chains": [{"name": "Local Dev", "chain_id": 31337, "rpc_urls": ["http://127.0.0.1:8545"],
                  "selected_rpc": "http://127.0.0.1:8545", "verification_api_key": null,
                  "verification_url": null, "key_name": "My Key"}],
      "variables": {"bad-name": "v"},
      "keys": {
        "My Key": {"name": "My Key", "type": "PrivateKey",
                   "value": "0000000000000000000000000000000000000000000000000000000000000001"},
        "wallet": {"name": "wallet", "type": "OnePassword", "vault": "v", "item": "i"}
      },
      "default_chain": "local dev"
    }"#;
    let plan = plan(v1).unwrap();
    let (config, secrets) = parsed(&plan);
    assert_eq!(config.default.as_deref(), Some("local-dev"));
    assert_eq!(config.chains["local-dev"].signer.as_deref(), Some("my-key"));
    assert_eq!(config.signers["my-key"].backend, Backend::Plaintext);
    assert_eq!(
        config.signers["my-key"].address.unwrap().to_string(),
        "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"
    );
    assert!(secrets.signers["my-key"].private_key.is_some());
    assert!(
        !plan
            .config_toml
            .contains("0000000000000000000000000000000000000000000000000000000000000001")
    );
    assert!(config.signers.contains_key("wallet-signer"));
    assert!(secrets.vars.contains_key("bad_name"));
    assert!(
        plan.summary
            .attention
            .iter()
            .any(|a| a.contains("plaintext"))
    );
}

/// A v1 config with one chain per `(name, id)` and the given extra JSON
/// members (`"keys": …` etc.).
fn v1_with(chains: &[(&str, u64)], extra: &str) -> String {
    let chains: Vec<String> = chains
        .iter()
        .map(|(name, id)| {
            format!(
                r#"{{"name": "{name}", "chain_id": {id}, "rpc_urls": ["https://rpc{id}.example"], "selected_rpc": "https://rpc{id}.example"}}"#
            )
        })
        .collect();
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!(", {extra}")
    };
    format!(r#"{{"chains": [{}]{extra}}}"#, chains.join(", "))
}

fn attention(plan: &Plan) -> String {
    plan.summary.attention.join("\n")
}

#[test]
fn legacy_home_source_is_backed_up_under_its_own_name() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join(".chainz.json");
    let paths = StorePaths::in_dir(dir.path().join("config/chainz")).with_legacy(legacy.clone());
    std::fs::write(&legacy, v1_with(&[("Base", 8453)], "")).unwrap();
    let lock = WriteLock::acquire(&paths).unwrap();
    let summary = migrate_locked(&lock).unwrap().unwrap();
    let backup = dir.path().join(".chainz.v1.json.bak");
    assert_eq!(summary.backup.as_deref(), Some(backup.as_path()));
    assert!(backup.exists() && !legacy.exists());
    assert!(!paths.dir().join("config.v1.json.bak").exists());
    assert!(paths.config().exists());
}

#[test]
fn keyring_service_and_username_are_kept() {
    let v1 = v1_with(
        &[("Base", 8453)],
        r#""keys": {
          "Work Key": {"name": "Work Key", "type": "Keyring", "service": "my-service", "username": "alice",
                       "address": "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"},
          "plain": {"name": "plain", "type": "Keyring", "service": "chainz", "username": "plain"}
        }"#,
    );
    let plan = plan(&v1).unwrap();
    let (config, _) = parsed(&plan);
    let work = &config.signers["work-key"];
    assert_eq!(work.backend, Backend::Keyring);
    assert_eq!(work.keyring_entry("work-key"), ("my-service", "alice"));
    let plain = &config.signers["plain"];
    assert!(plain.service.is_none() && plain.username.is_none());
    assert_eq!(plain.keyring_entry("plain"), ("chainz", "plain"));
}

#[test]
fn variable_renames_never_collide_or_chain() {
    let v1 = r#"{
      "chains": [{"name": "Base", "chain_id": 8453,
                  "rpc_urls": [{"url": "https://a.example/${A-B}/${A_B}", "headers": {"x-k": "${CHAIN}-${CHAIN_VAR}"}}],
                  "selected_rpc": "https://a.example/${A-B}/${A_B}",
                  "verification_url": "https://v.example/api", "verification_api_key": "${A-B}"}],
      "variables": {"A-B": "dash", "A_B": "under", "CHAIN": "c", "CHAIN_VAR": "cv", "9x": "nine"}
    }"#;
    let plan = plan(v1).unwrap();
    let (config, secrets) = parsed(&plan);
    // Valid names keep their name; the cleaned-up ones take the next free.
    assert_eq!(secrets.vars["A_B"], "under");
    assert_eq!(secrets.vars["A_B_2"], "dash");
    assert_eq!(secrets.vars["CHAIN_VAR"], "cv");
    assert_eq!(secrets.vars["CHAIN_VAR_2"], "c");
    assert_eq!(secrets.vars["_9x"], "nine");
    // Every reference is rewritten once, to the variable it meant.
    let base = &config.chains["base"];
    let endpoint = &base.rpcs[&base.rpc];
    assert_eq!(endpoint.url, "https://a.example/${A_B_2}/${A_B}");
    assert_eq!(endpoint.headers["x-k"], "${CHAIN_VAR_2}-${CHAIN_VAR}");
    let explorer = &config.explorers[base.explorer.as_deref().unwrap()];
    assert_eq!(explorer.key.as_deref(), Some("${A_B_2}"));
    let notes = attention(&plan);
    assert!(notes.contains("'A-B' was renamed to 'A_B_2'"), "{notes}");
    assert!(
        notes.contains("'CHAIN' was renamed to 'CHAIN_VAR_2'"),
        "{notes}"
    );
}

#[test]
fn clashing_aliases_are_dropped_and_duplicate_ids_merged() {
    let v1 = r#"{
      "chains": [
        {"name": "Arbitrum One", "aliases": ["arbitrum"], "chain_id": 42161,
         "rpc_urls": ["https://arb1.example"], "selected_rpc": "https://arb1.example"},
        {"name": "arb", "aliases": ["arbitrum", "Arb Sepolia"], "chain_id": 421614,
         "rpc_urls": ["https://arbsep.example"], "selected_rpc": "https://arbsep.example"},
        {"name": "Ethereum", "chain_id": 1, "rpc_urls": ["https://one.example"], "selected_rpc": "https://one.example"},
        {"name": "Mainnet Again", "chain_id": 1, "rpc_urls": ["https://one.example", "https://two.example"],
         "selected_rpc": "https://two.example"},
        {"name": "Empty", "chain_id": 777, "rpc_urls": [], "selected_rpc": ""}
      ],
      "default_chain": "arb"
    }"#;
    let plan = plan(v1).unwrap();
    let (config, _) = parsed(&plan);
    assert_eq!(config.chains["arb"].aliases, ["Arbitrum One", "arbitrum"]);
    assert_eq!(config.chains["arb-sepolia"].aliases, ["Arb Sepolia"]);
    // The v1 default named the 421614 chain by its v1 name.
    assert_eq!(config.default.as_deref(), Some("arb-sepolia"));
    let eth = &config.chains["eth"];
    assert_eq!(eth.aliases, ["Ethereum", "Mainnet Again"]);
    // The repeated endpoint is kept once; the first entry's selection wins.
    assert_eq!(eth.rpcs.keys().collect::<Vec<_>>(), ["one", "two"]);
    assert_eq!(eth.rpc, "one");
    assert!(!config.chains.contains_key("empty"));
    assert_eq!(plan.summary.chains, 3);
    let notes = attention(&plan);
    assert!(
        notes.contains("alias 'arb' of chain 'arb-sepolia' was dropped: it is already the name or an alias of chain 'arb'"),
        "{notes}"
    );
    assert!(
        notes.contains("alias 'arbitrum' of chain 'arb-sepolia' was dropped"),
        "{notes}"
    );
    assert!(
        notes.contains("share chain ID 1; they were merged into chain 'eth'"),
        "{notes}"
    );
    assert!(
        notes.contains("chain 'empty' (chain ID 777) has no usable RPC URL"),
        "{notes}"
    );
}

#[test]
fn header_values_with_commas_move_into_variables() {
    let v1 = r#"{
      "chains": [{"name": "Base", "chain_id": 8453,
        "rpc_urls": [{"url": "https://gw.example/rpc", "headers": {
           "x-list": "a,b", "x-token": "0123456789abcdef-secret", "accept": "json", "bad name": "kept-value"}}],
        "selected_rpc": "https://gw.example/rpc"}]
    }"#;
    let plan = plan(v1).unwrap();
    let (config, secrets) = parsed(&plan);
    let endpoint = &config.chains["base"].rpcs["gw"];
    assert_eq!(endpoint.headers["x-list"], "${GW_X_LIST}");
    assert_eq!(endpoint.headers["x-token"], "${GW_X_TOKEN}");
    assert_eq!(endpoint.headers["accept"], "json");
    assert!(!endpoint.headers.contains_key("bad name"));
    assert_eq!(secrets.vars["GW_X_LIST"], "a,b");
    assert_eq!(secrets.vars["GW_X_TOKEN"], "0123456789abcdef-secret");
    assert_eq!(secrets.vars["GW_HEADER"], "kept-value");
    for secret in ["0123456789abcdef-secret", "kept-value"] {
        assert!(!plan.config_toml.contains(secret));
    }
    let notes = attention(&plan);
    assert!(
        notes.contains("header 'x-list' contains a comma"),
        "{notes}"
    );
    assert!(
        notes.contains("header 'accept' holds a literal value"),
        "{notes}"
    );
    assert!(
        notes.contains("header 'bad name' has an invalid name and was dropped; its value is kept as ${GW_HEADER}"),
        "{notes}"
    );
}

#[test]
fn unmigratable_keys_are_skipped_with_a_note() {
    let v1 = r#"{
      "chains": [{"name": "Base", "chain_id": 8453, "rpc_urls": ["https://base.example"],
                  "selected_rpc": "https://base.example", "key_name": "broken"}],
      "keys": {
        "broken": {"name": "broken", "type": "PrivateKey", "value": "not-a-key"},
        "ledger": {"name": "ledger", "type": "Ledger", "path": "m/44'"},
        "bad-envelope": {"name": "bad-envelope", "type": "EncryptedKey", "value": "", "nonce": "", "salt": ""},
        "wallet": {"name": "wallet", "type": "Keyring", "service": "chainz", "username": "wallet"},
        "chainname": {"name": "chainname", "type": "Keyring", "service": "chainz", "username": "chainname"},
        "mismatch": {"name": "mismatch", "type": "PrivateKey",
                     "value": "0000000000000000000000000000000000000000000000000000000000000001",
                     "address": "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF"}
      }
    }"#;
    let plan = plan(v1).unwrap();
    let (config, secrets) = parsed(&plan);
    assert_eq!(
        config
            .signers
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["chainname-signer", "mismatch", "wallet-signer"]
    );
    assert_eq!(
        config.signers["mismatch"].address.unwrap().to_string(),
        "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"
    );
    assert!(secrets.signers.contains_key("mismatch"));
    assert!(config.chains["base"].signer.is_none());
    let notes = attention(&plan);
    for expected in [
        "key 'broken' was not migrated: it holds an invalid private key; it stays in the v1 backup",
        "key 'ledger' could not be read",
        "key 'bad-envelope' was not migrated: its encrypted key cannot be read",
        "chain 'base' referenced key 'broken', which was not migrated",
        "key 'wallet' became signer 'wallet-signer'",
        "key 'chainname' became signer 'chainname-signer'",
        "signer 'mismatch' had a cached address that does not match",
    ] {
        assert!(notes.contains(expected), "{expected}\n{notes}");
    }
    assert!(!notes.contains("not-a-key"));
}

/// A v1 `EncryptedKey` record exactly as v1's `encrypt_with_password`
/// wrote it (AES-256-GCM, Argon2id), with fixed salt and nonce.
fn v1_encrypted_record(private_key: &str, password: &str, with_params: bool) -> String {
    use aes_gcm::aead::{Aead, KeyInit};
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    let salt = [7u8; 16];
    let nonce = [9u8; 12];
    let (m, t, p) = if with_params {
        (1024, 1, 1)
    } else {
        (19_456, 2, 1)
    };
    let mut key = [0u8; 32];
    argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2::Params::new(m, t, p, Some(32)).unwrap(),
    )
    .hash_password_into(password.as_bytes(), &salt, &mut key)
    .unwrap();
    let ciphertext = aes_gcm::Aes256Gcm::new_from_slice(&key)
        .unwrap()
        .encrypt(aes_gcm::Nonce::from_slice(&nonce), private_key.as_bytes())
        .unwrap();
    let params = if with_params {
        format!(
            r#", "version": 1, "kdf_memory_kib": {m}, "kdf_iterations": {t}, "kdf_parallelism": {p}"#
        )
    } else {
        String::new()
    };
    format!(
        r#"{{"type": "EncryptedKey", "value": "{}", "nonce": "{}", "salt": "{}"{params},
            "address": "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"}}"#,
        BASE64.encode(ciphertext),
        BASE64.encode(nonce),
        BASE64.encode(salt)
    )
}

#[test]
fn v1_encrypted_keys_still_decrypt_after_migration() {
    let key = "0x0000000000000000000000000000000000000000000000000000000000000001";
    let v1 = v1_with(
        &[("Base", 8453)],
        &format!(
            r#""keys": {{"cold": {}, "legacy": {}}}"#,
            v1_encrypted_record(key, "hunter2", true),
            v1_encrypted_record(key, "hunter2", false)
        ),
    );
    let plan = plan(&v1).unwrap();
    let (config, secrets) = parsed(&plan);
    for name in ["cold", "legacy"] {
        assert_eq!(config.signers[name].backend, Backend::Encrypted);
        let secret = &secrets.signers[name];
        assert_eq!(secret.version, Some(1));
        let decrypted = crate::signer::decrypt(secret, "hunter2").unwrap();
        assert_eq!(decrypted.as_str(), key);
        assert!(crate::signer::decrypt(secret, "wrong").is_err());
    }
    assert_eq!(secrets.signers["cold"].kdf_memory_kib, Some(1024));
    // A record from before v1 stored its KDF parameters uses v1's defaults.
    assert_eq!(secrets.signers["legacy"].kdf_memory_kib, None);
}

#[test]
#[ignore = "prints the migrated fixture for inspection"]
fn print_fixture_migration() {
    let plan = plan(FIXTURE).unwrap();
    println!("{}\n---\n{}", plan.config_toml, plan.summary);
}
