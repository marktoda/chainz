use super::*;
use crate::chain::ChainDefinition;
use crate::key::{Key, KeyType};

fn test_chain(name: &str, chain_id: u64) -> ChainDefinition {
    ChainDefinition {
        name: name.to_string(),
        aliases: vec![],
        chain_id,
        rpc_urls: vec!["https://rpc.example.com".to_string()],
        selected_rpc: "https://rpc.example.com".to_string(),
        verification_api_key: None,
        verification_url: None,
        key_name: Some("default".to_string()),
    }
}

fn test_key(name: &str) -> Key {
    Key::new(
        name.to_string(),
        KeyType::PrivateKey {
            value: "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
        },
    )
}

fn chainz_for_chains() -> Result<Chainz> {
    let mut chainz = Chainz::new();
    chainz.add_key("default", test_key("default"))?;
    Ok(chainz)
}

#[test]
fn config_json_round_trip() -> Result<()> {
    let mut config = Config::default();
    config.chains.push(test_chain("ethereum", 1));
    config.chains.push(test_chain("polygon", 137));
    config.globals.add_rpc_expansion("INFURA_KEY", "abc123");
    config
        .keys
        .insert("default".to_string(), test_key("default"));
    config
        .keys
        .insert("deployer".to_string(), test_key("deployer"));

    let json = serde_json::to_string_pretty(&config)?;
    let restored: Config = serde_json::from_str(&json)?;

    assert_eq!(restored.chains.len(), 2);
    assert_eq!(restored.chains[0].name, "ethereum");
    assert_eq!(restored.chains[0].chain_id, 1);
    assert_eq!(restored.chains[1].name, "polygon");
    assert_eq!(restored.chains[1].chain_id, 137);
    assert_eq!(
        restored.globals.get_rpc_expansion("INFURA_KEY"),
        Some("abc123".to_string())
    );
    assert_eq!(restored.keys.len(), 2);
    assert!(restored.keys.contains_key("default"));
    assert!(restored.keys.contains_key("deployer"));
    Ok(())
}

#[test]
fn get_chain_by_name_case_insensitive() -> Result<()> {
    let mut config = Config::default();
    config.chains.push(test_chain("Ethereum", 1));

    let found = config.get_chain("ethereum")?;
    assert_eq!(found.name, "Ethereum");

    let found = config.get_chain("ETHEREUM")?;
    assert_eq!(found.name, "Ethereum");
    Ok(())
}

#[test]
fn get_chain_by_id_string() -> Result<()> {
    let mut config = Config::default();
    config.chains.push(test_chain("ethereum", 1));
    config.chains.push(test_chain("polygon", 137));

    let found = config.get_chain("137")?;
    assert_eq!(found.name, "polygon");

    let found = config.get_chain("1")?;
    assert_eq!(found.name, "ethereum");
    Ok(())
}

#[test]
fn get_chain_by_alias_and_prefix() -> Result<()> {
    let mut config = Config::default();
    let mut eth = test_chain("ethereum", 1);
    eth.aliases = vec!["Ethereum Mainnet".to_string()];
    config.chains.push(eth);
    config.chains.push(test_chain("optimism", 10));

    // exact alias, case-insensitive
    assert_eq!(config.get_chain("ethereum mainnet")?.name, "ethereum");
    // unambiguous prefix on name
    assert_eq!(config.get_chain("opti")?.name, "optimism");
    // exact match wins over prefix ambiguity
    config.chains.push(test_chain("op", 130));
    assert_eq!(config.get_chain("op")?.name, "op");
    Ok(())
}

#[test]
fn get_chain_ambiguous_prefix_errors() {
    let mut config = Config::default();
    config.chains.push(test_chain("base", 8453));
    config.chains.push(test_chain("basecamp", 123));

    let err = config.get_chain("bas").unwrap_err().to_string();
    assert!(err.contains("ambiguous"), "got: {}", err);
    assert!(err.contains("base") && err.contains("basecamp"));
}

#[test]
fn get_chain_not_found() {
    let config = Config::default();
    assert!(config.get_chain("nonexistent").is_err());
    assert!(config.get_chain("999").is_err());
}

#[test]
fn add_chain_new() -> Result<()> {
    let mut chainz = chainz_for_chains()?;
    chainz.add_chain(test_chain("ethereum", 1))?;

    let chains = chainz.list_chains();
    assert_eq!(chains.len(), 1);
    assert_eq!(chains[0].name, "ethereum");
    Ok(())
}

#[test]
fn add_chain_replaces_by_name() -> Result<()> {
    let mut chainz = chainz_for_chains()?;
    chainz.add_chain(test_chain("foo", 1))?;
    chainz.add_chain(test_chain("foo", 42))?;

    let chains = chainz.list_chains();
    assert_eq!(chains.len(), 1);
    assert_eq!(chains[0].chain_id, 42);
    Ok(())
}

#[test]
fn add_chain_replaces_by_alias_and_case() -> Result<()> {
    let mut chainz = chainz_for_chains()?;
    let mut eth = test_chain("ethereum", 1);
    eth.aliases = vec!["Ethereum Mainnet".to_string()];
    chainz.add_chain(eth)?;

    // A new chain whose name collides with an existing ALIAS replaces
    // that chain instead of being appended (and shadowed forever)
    chainz.add_chain(test_chain("Ethereum Mainnet", 11))?;
    assert_eq!(chainz.list_chains().len(), 1);
    assert_eq!(chainz.list_chains()[0].chain_id, 11);

    // Same for a case-variant of the primary name
    chainz.add_chain(test_chain("ETHEREUM MAINNET", 12))?;
    assert_eq!(chainz.list_chains().len(), 1);
    assert_eq!(chainz.list_chains()[0].chain_id, 12);
    Ok(())
}

#[test]
fn remove_chain_clears_default() -> Result<()> {
    let mut chainz = chainz_for_chains()?;
    chainz.add_chain(test_chain("ethereum", 1))?;
    chainz.config.default_chain = Some("ethereum".to_string());

    chainz.remove_chain("1")?;
    assert_eq!(chainz.config.default_chain, None);
    Ok(())
}

#[test]
fn add_key_and_get_key() -> Result<()> {
    let mut chainz = Chainz::new();
    chainz.add_key("mykey", test_key("mykey"))?;

    let retrieved = chainz.get_key("mykey")?;
    assert_eq!(retrieved.name, "mykey");
    Ok(())
}

#[test]
fn add_key_duplicate_errors() -> Result<()> {
    let mut chainz = Chainz::new();
    chainz.add_key("dup", test_key("dup"))?;

    let result = chainz.add_key("dup", test_key("dup"));
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("already exists"));
    Ok(())
}

#[test]
fn get_key_not_found() {
    let chainz = Chainz::new();
    let result = chainz.get_key("nonexistent");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not found"));
}

#[test]
fn remove_key() -> Result<()> {
    let mut chainz = Chainz::new();
    chainz.add_key("temp", test_key("temp"))?;
    assert!(chainz.get_key("temp").is_ok());

    chainz.remove_key("temp")?;
    assert!(chainz.get_key("temp").is_err());
    Ok(())
}

#[test]
fn remove_key_not_found() {
    let mut chainz = Chainz::new();
    let result = chainz.remove_key("ghost");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not found"));
}

#[test]
fn list_keys_default_first() -> Result<()> {
    let mut chainz = Chainz::new();
    chainz.add_key("alpha", test_key("alpha"))?;
    chainz.add_key("zebra", test_key("zebra"))?;
    chainz.add_key("default", test_key("default"))?;

    let keys = chainz.list_keys();
    assert_eq!(keys.len(), 3);
    assert_eq!(keys[0].0, "default");
    Ok(())
}

#[test]
fn duplicate_chain_id_is_rejected_without_mutating_config() -> Result<()> {
    let mut chainz = chainz_for_chains()?;
    chainz.add_chain(test_chain("ethereum", 1))?;
    let error = chainz.add_chain(test_chain("mainnet", 1)).unwrap_err();
    assert!(error.to_string().contains("already configured"));
    assert_eq!(chainz.list_chains().len(), 1);
    Ok(())
}

#[test]
fn alias_collision_is_rejected() -> Result<()> {
    let mut chainz = chainz_for_chains()?;
    chainz.add_chain(test_chain("ethereum", 1))?;
    let mut optimism = test_chain("optimism", 10);
    optimism.aliases.push("ethereum".to_string());
    let error = chainz.add_chain(optimism).unwrap_err();
    assert!(error.to_string().contains("collides"));
    assert_eq!(chainz.list_chains().len(), 1);
    Ok(())
}

#[test]
fn replacing_default_chain_tracks_new_canonical_name() -> Result<()> {
    let mut chainz = chainz_for_chains()?;
    let mut ethereum = test_chain("ethereum", 1);
    ethereum.aliases.push("Ethereum Mainnet".to_string());
    chainz.add_chain(ethereum)?;
    chainz.config.default_chain = Some("ethereum".to_string());
    chainz.add_chain(test_chain("Ethereum Mainnet", 1))?;
    assert_eq!(
        chainz.config.default_chain.as_deref(),
        Some("Ethereum Mainnet")
    );
    Ok(())
}

#[test]
fn referenced_key_cannot_be_removed() -> Result<()> {
    let mut chainz = chainz_for_chains()?;
    chainz.add_chain(test_chain("ethereum", 1))?;
    let error = chainz.remove_key("default").unwrap_err();
    assert!(error.to_string().contains("still used"));
    assert!(chainz.get_key("default").is_ok());
    Ok(())
}

#[test]
fn config_validation_rejects_selected_rpc_outside_list() {
    let mut config = Config::default();
    config.keys.insert("default".into(), test_key("default"));
    let mut chain = test_chain("ethereum", 1);
    chain.selected_rpc = "https://other.example.com".into();
    config.chains.push(chain);
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("selected RPC")
    );
}

#[test]
fn legacy_normalization_restores_selected_rpc_and_canonical_default() {
    let mut config = Config::default();
    config.keys.insert("default".into(), test_key("default"));
    let mut chain = test_chain("ethereum", 1);
    chain.aliases.push("Ethereum Mainnet".into());
    chain.rpc_urls.clear();
    config.chains.push(chain);
    config.default_chain = Some("ethereum mainnet".into());

    config.normalize_legacy();
    assert_eq!(config.chains[0].rpc_urls, vec!["https://rpc.example.com"]);
    assert_eq!(config.default_chain.as_deref(), Some("ethereum"));
    assert!(config.validate().is_ok());
}

#[test]
fn selecting_a_new_rpc_adds_it_to_the_configured_list() -> Result<()> {
    let mut chainz = chainz_for_chains()?;
    chainz.add_chain(test_chain("ethereum", 1))?;
    chainz.set_selected_rpc("ethereum", "https://backup.example.com".into())?;

    let chain = &chainz.list_chains()[0];
    assert_eq!(chain.selected_rpc, "https://backup.example.com");
    assert!(chain.rpc_urls.contains(&chain.selected_rpc));
    assert!(chainz.config.validate().is_ok());
    Ok(())
}
