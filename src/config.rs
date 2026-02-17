use crate::{
    chain::{ChainDefinition, ChainInstance},
    key::Key,
    variables::GlobalVariables,
};
use anyhow::{anyhow, Context, Result};
use dirs::home_dir;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

pub const CONFIG_FILE_LOCATION: &str = ".chainz.json";

#[derive(Default, Debug, Serialize, Deserialize)]
pub struct Config {
    pub chains: Vec<ChainDefinition>,
    #[serde(rename = "variables")]
    pub globals: GlobalVariables,
    pub keys: HashMap<String, Key>,
}

#[derive(Default)]
pub struct Chainz {
    pub config: Config,
    active_chains: HashMap<String, ChainInstance>, // keyed by name
}

impl Chainz {
    pub fn new() -> Self {
        Self {
            config: Config::default(),
            active_chains: HashMap::new(),
        }
    }

    pub async fn load() -> Result<Self> {
        // use default config if none
        let config = Config::load().await.unwrap_or_default();
        Ok(Self {
            config,
            active_chains: HashMap::new(),
        })
    }

    pub async fn get_chain(&mut self, name_or_id: &str) -> Result<ChainInstance> {
        let definition = self.config.get_chain(name_or_id)?;
        let name = definition.name.clone();

        if !self.active_chains.contains_key(&name) {
            let instance = self.instantiate_chain(&definition).await?;
            self.active_chains.insert(name.clone(), instance);
        }
        Ok(self
            .active_chains
            .get(&name)
            .expect("Chain was just inserted")
            .clone())
    }

    async fn instantiate_chain(&self, def: &ChainDefinition) -> Result<ChainInstance> {
        let rpc = def.get_rpc(&self.config.globals).await?;
        let key = self.get_key(&def.key_name)?;

        Ok(ChainInstance::new(
            def.clone(),
            rpc.provider,
            rpc.rpc_url,
            key,
        ))
    }

    // Key management methods
    pub async fn add_key(&mut self, name: &str, key: Key) -> Result<()> {
        if self.config.keys.contains_key(name) {
            anyhow::bail!("Key '{}' already exists", name);
        }
        self.config.keys.insert(name.to_string(), key);
        Ok(())
    }

    pub fn list_keys(&self) -> Vec<(&str, &Key)> {
        let mut keys: Vec<_> = self
            .config
            .keys
            .iter()
            .map(|(n, k)| (n.as_str(), k))
            .collect();

        // If "default" exists, move it to the front
        if let Some(default_pos) = keys.iter().position(|(name, _)| *name == "default") {
            keys.swap(0, default_pos);
        }

        keys
    }

    pub fn remove_key(&mut self, name: &str) -> Result<()> {
        if !self.config.keys.contains_key(name) {
            anyhow::bail!("Key '{}' not found", name);
        }
        self.config.keys.remove(name);
        Ok(())
    }

    pub fn get_key(&self, key_name: &str) -> Result<Key> {
        self.config
            .keys
            .get(key_name)
            .cloned()
            .ok_or(anyhow!("Key '{}' not found", key_name))
    }

    pub async fn add_chain(&mut self, chain: ChainDefinition) -> Result<()> {
        // Replace if exists, otherwise add
        if let Some(pos) = self.config.chains.iter().position(|c| c.name == chain.name) {
            self.config.chains[pos] = chain;
        } else {
            self.config.chains.push(chain);
        }

        Ok(())
    }

    pub fn list_chains(&self) -> &[ChainDefinition] {
        &self.config.chains
    }

    pub async fn save(&self) -> Result<()> {
        self.config.write().await
    }

    pub async fn delete() -> Result<()> {
        Config::delete().await
    }
}

impl Config {
    pub async fn load() -> Result<Self> {
        let path = get_config_path().ok_or(anyhow!("Unable to find config path"))?;
        let json = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("Failed to read config at {}", path.display()))?;
        let config = serde_json::from_str(&json)
            .with_context(|| "Failed to parse config (file may be corrupted)")?;
        Ok(config)
    }

    pub fn get_chain(&self, name_or_id: &str) -> Result<ChainDefinition> {
        // Try to parse as chain ID first
        if let Ok(chain_id) = name_or_id.parse::<u64>() {
            return self.get_chain_config_by_id(chain_id);
        }

        // Try as name
        self.get_chain_config_by_name(name_or_id)
    }

    pub fn get_chain_config_by_name(&self, name: &str) -> Result<ChainDefinition> {
        Ok(self
            .chains
            .iter()
            .find(|chain| chain.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| anyhow!("Chain '{}' not found", name))?
            .clone())
    }

    pub fn get_chain_config_by_id(&self, chain_id: u64) -> Result<ChainDefinition> {
        Ok(self
            .chains
            .iter()
            .find(|chain| chain.chain_id == chain_id)
            .ok_or_else(|| anyhow!("Chain with ID {} not found", chain_id))?
            .clone())
    }

    pub async fn write(&self) -> Result<()> {
        let path = get_config_path().ok_or(anyhow!("Unable to find config path"))?;
        let tmp_path = path.with_extension("json.tmp");

        let json = serde_json::to_string_pretty(self)?;

        // Write to temp file, sync to disk, then atomically rename over the real file.
        // This ensures the config is never left in a partial/corrupt state.
        tokio::fs::write(&tmp_path, &json).await?;
        let file = tokio::fs::File::open(&tmp_path).await?;
        file.sync_all().await?;
        tokio::fs::rename(&tmp_path, &path).await?;

        Ok(())
    }

    pub async fn delete() -> Result<()> {
        tokio::fs::remove_file(get_config_path().ok_or(anyhow!("Unable to find config path"))?)
            .await?;
        Ok(())
    }
}

fn get_config_path() -> Option<PathBuf> {
    let mut path = home_dir()?;
    path.push(CONFIG_FILE_LOCATION);
    Some(path)
}

pub async fn config_exists() -> Result<bool> {
    Ok(get_config_path().map(|p| p.exists()).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::ChainDefinition;
    use crate::key::{Key, KeyType};

    fn test_chain(name: &str, chain_id: u64) -> ChainDefinition {
        ChainDefinition {
            name: name.to_string(),
            chain_id,
            rpc_urls: vec!["https://rpc.example.com".to_string()],
            selected_rpc: "https://rpc.example.com".to_string(),
            verification_api_key: None,
            verification_url: None,
            key_name: "default".to_string(),
        }
    }

    fn test_key(name: &str) -> Key {
        Key::new(
            name.to_string(),
            KeyType::PrivateKey {
                value: "0000000000000000000000000000000000000000000000000000000000000001"
                    .to_string(),
            },
        )
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
    fn get_chain_not_found() {
        let config = Config::default();
        assert!(config.get_chain("nonexistent").is_err());
        assert!(config.get_chain("999").is_err());
    }

    #[tokio::test]
    async fn add_chain_new() -> Result<()> {
        let mut chainz = Chainz::new();
        chainz.add_chain(test_chain("ethereum", 1)).await?;

        let chains = chainz.list_chains();
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].name, "ethereum");
        Ok(())
    }

    #[tokio::test]
    async fn add_chain_replaces_by_name() -> Result<()> {
        let mut chainz = Chainz::new();
        chainz.add_chain(test_chain("foo", 1)).await?;
        chainz.add_chain(test_chain("foo", 42)).await?;

        let chains = chainz.list_chains();
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].chain_id, 42);
        Ok(())
    }

    #[tokio::test]
    async fn add_key_and_get_key() -> Result<()> {
        let mut chainz = Chainz::new();
        chainz.add_key("mykey", test_key("mykey")).await?;

        let retrieved = chainz.get_key("mykey")?;
        assert_eq!(retrieved.name, "mykey");
        Ok(())
    }

    #[tokio::test]
    async fn add_key_duplicate_errors() -> Result<()> {
        let mut chainz = Chainz::new();
        chainz.add_key("dup", test_key("dup")).await?;

        let result = chainz.add_key("dup", test_key("dup")).await;
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

    #[tokio::test]
    async fn remove_key() -> Result<()> {
        let mut chainz = Chainz::new();
        chainz.add_key("temp", test_key("temp")).await?;
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

    #[tokio::test]
    async fn list_keys_default_first() -> Result<()> {
        let mut chainz = Chainz::new();
        chainz.add_key("alpha", test_key("alpha")).await?;
        chainz.add_key("zebra", test_key("zebra")).await?;
        chainz.add_key("default", test_key("default")).await?;

        let keys = chainz.list_keys();
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0].0, "default");
        Ok(())
    }
}
