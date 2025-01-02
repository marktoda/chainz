use crate::{
    chain::{ChainDefinition, ChainInstance},
    key::Key,
};
use anyhow::{anyhow, Result};
use dirs::home_dir;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Debug;
use std::path::PathBuf;

pub const CONFIG_FILE_LOCATION: &str = ".chainz.json";

#[derive(Default, Debug, Serialize, Deserialize)]
pub struct Config {
    pub chains: Vec<ChainDefinition>,
    pub variables: HashMap<String, String>,
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

    pub async fn get_chain(&mut self, name_or_id: &str) -> Result<&ChainInstance> {
        let definition = self.config.get_chain(name_or_id)?;
        let name = definition.name.clone();

        if !self.active_chains.contains_key(&name) {
            let instance = self.instantiate_chain(&definition).await?;
            self.active_chains.insert(name.clone(), instance);
        }
        Ok(&self.active_chains[&name])
    }

    async fn instantiate_chain(&self, def: &ChainDefinition) -> Result<ChainInstance> {
        let rpc = def.get_rpc(&self.config.variables).await?;
        let key = self.get_key(&def.key_name.clone())?;

        Ok(ChainInstance {
            definition: def.clone(),
            provider: rpc.provider,
            rpc_url: rpc.rpc_url,
            key,
        })
    }

    // Key management methods
    pub async fn add_key(&mut self, name: &str, key: Key) -> Result<()> {
        if self.config.keys.contains_key(name) {
            anyhow::bail!("Key '{}' already exists", name);
        }
        self.config.keys.insert(name.to_string(), key);
        Ok(())
    }

    pub fn list_keys(&self) -> Vec<(String, Key)> {
        let mut keys: Vec<_> = self
            .config
            .keys
            .iter()
            .map(|(n, k)| (n.clone(), k.clone()))
            .collect();

        // If "default" exists, move it to the front
        if let Some(default_pos) = keys.iter().position(|(name, _)| name == "default") {
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
            self.config.chains[pos] = chain.clone();
        } else {
            self.config.chains.push(chain.clone());
        }

        Ok(())
    }

    pub fn list_chains(&self) -> &[ChainDefinition] {
        &self.config.chains
    }

    /// Add or update a custom variable
    pub fn set_variable(&mut self, name: &str, value: &str) {
        self.config
            .variables
            .insert(name.to_string(), value.to_string());
    }

    /// Get a custom variable's value
    pub fn get_variable(&self, name: &str) -> Option<&String> {
        self.config.variables.get(name)
    }

    /// Remove a custom variable
    pub fn remove_variable(&mut self, name: &str) -> Result<()> {
        if !self.config.variables.contains_key(name) {
            anyhow::bail!("Variable '{}' not found", name);
        }
        self.config.variables.remove(name);
        Ok(())
    }

    /// List all custom variables
    pub fn list_variables(&self) -> Vec<(String, String)> {
        self.config
            .variables
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
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
        let json = tokio::fs::read_to_string(
            get_config_path().ok_or(anyhow!("Unable to find config path"))?,
        )
        .await?;
        let config = serde_json::from_str(&json)?;
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
            .find(|chain| chain.name.to_ascii_lowercase() == name.to_ascii_lowercase())
            .ok_or(anyhow!("Chain not found"))?
            .clone())
    }

    pub fn get_chain_config_by_id(&self, chain_id: u64) -> Result<ChainDefinition> {
        Ok(self
            .chains
            .iter()
            .find(|chain| chain.chain_id == chain_id)
            .ok_or(anyhow!("Chain not found"))?
            .clone())
    }

    pub async fn write(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        tokio::fs::write(
            get_config_path().ok_or(anyhow!("Unable to find config path"))?,
            json,
        )
        .await?;
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
