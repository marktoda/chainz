use crate::{
    chain::{ChainDefinition, ChainInstance},
    key::Key,
    opt::AddArgs,
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
    pub fn new(config: Config) -> Self {
        Self {
            config,
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
        let (rpc_url, provider) = def.get_rpc().await?;
        let key = self.config.get_key(&def.key_name.clone())?;

        Ok(ChainInstance {
            definition: def.clone(),
            provider,
            rpc_url,
            key,
        })
    }

    // Key management methods
    pub async fn add_key(&mut self, name: &str, key: Key) -> Result<()> {
        self.config.add_key(name, key).await?;
        Ok(())
    }

    pub fn list_keys(&self) -> Result<Vec<(String, Key)>> {
        self.config.list_keys()
    }

    pub fn remove_key(&mut self, name: &str) -> Result<()> {
        self.config.remove_key(name)
    }

    pub async fn add_chain(&mut self, args: &AddArgs) -> Result<ChainDefinition> {
        self.config.add_chain(args).await
    }

    pub fn list_chains(&self) -> &[ChainDefinition] {
        self.config.list_chains()
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

    pub async fn add_key(&mut self, name: &str, key: Key) -> Result<()> {
        if self.keys.contains_key(name) {
            anyhow::bail!("Key '{}' already exists", name);
        }
        self.keys.insert(name.to_string(), key);
        Ok(())
    }

    pub fn list_keys(&self) -> Result<Vec<(String, Key)>> {
        Ok(self
            .keys
            .iter()
            .map(|(n, k)| (n.clone(), k.clone()))
            .collect())
    }

    pub fn remove_key(&mut self, name: &str) -> Result<()> {
        if !self.keys.contains_key(name) {
            anyhow::bail!("Key '{}' not found", name);
        }
        self.keys.remove(name);
        Ok(())
    }

    pub fn get_key(&self, key_name: &str) -> Result<Key> {
        self.keys
            .get(key_name)
            .cloned()
            .ok_or(anyhow!("Key '{}' not found", key_name))
    }

    pub fn list_chains(&self) -> &[ChainDefinition] {
        &self.chains
    }

    pub fn get_chain(&self, name_or_id: &str) -> Result<ChainDefinition> {
        // Try to parse as chain ID first
        if let Ok(chain_id) = name_or_id.parse::<u64>() {
            if let Some(chain) = self.chains.iter().find(|c| c.chain_id == chain_id) {
                return Ok(chain.clone());
            }
        }

        // Try as name
        if let Some(chain) = self.chains.iter().find(|c| c.name == name_or_id) {
            return Ok(chain.clone());
        }

        Err(anyhow!("Chain '{}' not found", name_or_id))
    }

    pub fn get_chain_config_by_name(&self, name: &str) -> Result<ChainDefinition> {
        Ok(self
            .list_chains()
            .iter()
            .find(|chain| chain.name == name)
            .ok_or(anyhow!("Chain not found"))?
            .clone())
    }

    pub fn get_chain_config_by_id(&self, chain_id: u64) -> Result<ChainDefinition> {
        Ok(self
            .list_chains()
            .iter()
            .find(|chain| chain.chain_id == chain_id)
            .ok_or(anyhow!("Chain not found"))?
            .clone())
    }

    pub async fn add_chain(&mut self, args: &AddArgs) -> Result<ChainDefinition> {
        let chain = ChainDefinition::new(args).await?;

        // Replace if exists, otherwise add
        if let Some(pos) = self.chains.iter().position(|c| c.name == chain.name) {
            self.chains[pos] = chain.clone();
        } else {
            self.chains.push(chain.clone());
        }

        Ok(chain)
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
