use super::*;
use crate::model::Config;
use crate::secrets::Secrets;
use crate::store::{Chainz, LoadMode};

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
        toml_edit::de::from_str(&plan.config_toml).unwrap(),
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
    std::fs::create_dir_all(&paths.dir).unwrap();
    std::fs::write(&paths.v1_config, FIXTURE).unwrap();

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let paths = paths.clone();
            std::thread::spawn(move || {
                let _lock = WriteLock::acquire(&paths).unwrap();
                migrate_locked(&paths).unwrap().is_some()
            })
        })
        .collect();
    let migrated = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|m| *m)
        .count();
    assert_eq!(migrated, 1);
    assert!(!paths.v1_config.exists());
    assert!(paths.dir.join("config.v1.json.bak").exists());
    let chainz = Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
    assert_eq!(chainz.config().chains.len(), 17);
    assert_eq!(chainz.secrets().unwrap().vars.len(), 7);
}

#[test]
fn open_auto_migrates() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path().to_path_buf());
    std::fs::write(&paths.v1_config, FIXTURE).unwrap();
    let chainz = Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
    assert!(chainz.exists());
    assert_eq!(chainz.config().chains.len(), 17);
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

#[test]
fn credential_heuristic() {
    assert_eq!(
        inline_credential("https://u:p@x.example/"),
        Some("user info")
    );
    assert_eq!(
        inline_credential("https://x.example/?apikey=abc"),
        Some("query parameter")
    );
    assert_eq!(inline_credential("https://x.example/?apikey=${K}"), None);
    assert_eq!(
        inline_credential("https://x.example/abc123def456ghi789/"),
        Some("path segment")
    );
    assert_eq!(inline_credential("https://x.example/rpc/1"), None);
    assert_eq!(inline_credential("https://x.example/v3/${KEY}"), None);
}

#[test]
#[ignore = "prints the migrated fixture for inspection"]
fn print_fixture_migration() {
    let plan = plan(FIXTURE).unwrap();
    println!("{}\n---\n{}", plan.config_toml, plan.summary);
}
