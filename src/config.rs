use crate::{
    chain::{ChainDefinition, ChainInstance},
    key::Key,
    variables::GlobalVariables,
};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

mod store;
pub(crate) use store::config_exists;
use store::{
    ConfigLock, ensure_private_dir, get_config_path, migrate_legacy_config, restrict_permissions,
    write_atomically,
};

/// Pre-0.3 config location, relative to $HOME. Migrated on first load.
pub const LEGACY_CONFIG_FILE: &str = ".chainz.json";
/// Config location relative to $HOME (when XDG_CONFIG_HOME is unset).
const DEFAULT_CONFIG_RELATIVE: &str = ".config/chainz/config.json";

#[derive(Default, Debug, Serialize, Deserialize)]
pub struct Config {
    pub chains: Vec<ChainDefinition>,
    #[serde(rename = "variables")]
    pub globals: GlobalVariables,
    pub keys: HashMap<String, Key>,
    /// Chain used by `exec` when none is specified; set via `chainz use`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_chain: Option<String>,
}

#[derive(Default)]
pub struct Chainz {
    pub config: Config,
    // Commands hold this lock from load through save, making the complete
    // read-modify-write operation serial across chainz processes.
    _config_lock: Option<ConfigLock>,
}

impl Chainz {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn load() -> Result<Self> {
        // Start fresh only when no config exists. Read and parse failures are
        // propagated so the next save can never wipe a broken real config.
        let config_lock = ConfigLock::acquire().await?;
        let config = Config::load_locked(true).await?.unwrap_or_default();
        Ok(Self {
            config,
            _config_lock: Some(config_lock),
        })
    }

    /// Load deserializable config without enforcing semantic invariants so
    /// `doctor` can report and help repair legacy-invalid states.
    pub async fn load_for_doctor() -> Result<Self> {
        let config_lock = ConfigLock::acquire().await?;
        let config = Config::load_locked(false).await?.unwrap_or_default();
        Ok(Self {
            config,
            _config_lock: Some(config_lock),
        })
    }

    pub fn get_chain(&self, name_or_id: &str) -> Result<ChainInstance> {
        let definition = self.config.get_chain(name_or_id)?.clone();
        let rpc_url = self.config.globals.expand_rpc_url(&definition.selected_rpc);
        let key = definition
            .key_name
            .as_deref()
            .and_then(|name| self.config.keys.get(name))
            .cloned();
        Ok(ChainInstance {
            definition,
            rpc_url,
            key,
        })
    }

    // Key management methods
    pub fn add_key(&mut self, name: &str, key: Key) -> Result<()> {
        if self.config.keys.contains_key(name) {
            anyhow::bail!("Key '{}' already exists", name);
        }
        if name != key.name {
            anyhow::bail!(
                "Key map name '{}' does not match record name '{}'",
                name,
                key.name
            );
        }
        key.validate_record()?;
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
        let referenced_by: Vec<&str> = self
            .config
            .chains
            .iter()
            .filter(|chain| chain.key_name.as_deref() == Some(name))
            .map(|chain| chain.name.as_str())
            .collect();
        if !referenced_by.is_empty() {
            anyhow::bail!(
                "Key '{}' is still used by chain(s): {}",
                name,
                referenced_by.join(", ")
            );
        }
        self.config.keys.remove(name);
        Ok(())
    }

    pub fn chains_using_key(&self, name: &str) -> Vec<String> {
        self.config
            .chains
            .iter()
            .filter(|chain| chain.key_name.as_deref() == Some(name))
            .map(|chain| chain.name.clone())
            .collect()
    }

    pub fn detach_key(&mut self, name: &str) -> usize {
        let mut detached = 0;
        for chain in &mut self.config.chains {
            if chain.key_name.as_deref() == Some(name) {
                chain.key_name = None;
                detached += 1;
            }
        }
        detached
    }

    pub fn get_key(&self, key_name: &str) -> Result<Key> {
        self.config
            .keys
            .get(key_name)
            .cloned()
            .ok_or(anyhow!("Key '{}' not found", key_name))
    }

    pub fn add_chain(&mut self, chain: ChainDefinition) -> Result<()> {
        // Replace if exists, otherwise add. Identity is the same
        // case-insensitive name/alias matching used by lookups, so a new
        // chain can never be shadowed by an existing chain's alias.
        let replacement = self
            .config
            .chains
            .iter()
            .position(|existing| existing.matches_exact(&chain.name));

        for (index, existing) in self.config.chains.iter().enumerate() {
            if Some(index) == replacement {
                continue;
            }
            if existing.chain_id == chain.chain_id {
                anyhow::bail!(
                    "Chain ID {} is already configured as '{}'",
                    chain.chain_id,
                    existing.name
                );
            }
            if let Some(collision) = chain.names().find(|name| existing.matches_exact(name)) {
                anyhow::bail!(
                    "Chain name or alias '{}' collides with '{}'",
                    collision,
                    existing.name
                );
            }
        }

        self.commit_chain(replacement, chain)
    }

    pub fn replace_chain(&mut self, name_or_id: &str, chain: ChainDefinition) -> Result<()> {
        let index = self.config.find_chain_index(name_or_id)?;
        self.commit_chain(Some(index), chain)
    }

    /// Apply a chain mutation transactionally: canonical-default changes and
    /// semantic validation either commit together or are both rolled back.
    fn commit_chain(&mut self, replacement: Option<usize>, chain: ChainDefinition) -> Result<()> {
        let previous_default = self.config.default_chain.clone();
        let previous_chain = replacement.map(|index| self.config.chains[index].clone());
        if let Some(index) = replacement {
            if self.config.default_chain.as_deref() == Some(self.config.chains[index].name.as_str())
            {
                self.config.default_chain = Some(chain.name.clone());
            }
            self.config.chains[index] = chain;
        } else {
            self.config.chains.push(chain);
        }
        if let Err(error) = self.config.validate() {
            self.config.default_chain = previous_default;
            match (replacement, previous_chain) {
                (Some(index), Some(previous)) => self.config.chains[index] = previous,
                (None, None) => {
                    self.config.chains.pop();
                }
                _ => unreachable!("replacement state is paired"),
            }
            return Err(error);
        }
        Ok(())
    }

    /// Resolve and select a default chain while preserving the canonical
    /// primary name in the persisted config.
    pub fn set_default_chain(&mut self, name_or_id: &str) -> Result<String> {
        let name = self.config.get_chain(name_or_id)?.name.clone();
        self.config.default_chain = Some(name.clone());
        Ok(name)
    }

    /// Whether a chain would collide with `name` (by name or alias).
    pub fn chain_exists(&self, name: &str) -> bool {
        self.config.chains.iter().any(|c| c.matches_exact(name))
    }

    pub fn list_chains(&self) -> &[ChainDefinition] {
        &self.config.chains
    }

    /// Destructive commands deliberately require an exact primary name or ID.
    pub fn remove_chain_exact(&mut self, name_or_id: &str) -> Result<ChainDefinition> {
        let pos = self.config.find_chain_exact_index(name_or_id)?;
        self.remove_chain_at(pos)
    }

    fn remove_chain_at(&mut self, pos: usize) -> Result<ChainDefinition> {
        let removed = self.config.chains.remove(pos);
        // Keep the default-chain invariant here so every caller gets it
        if self.config.default_chain.as_deref() == Some(removed.name.as_str()) {
            self.config.default_chain = None;
        }
        Ok(removed)
    }

    pub fn set_selected_rpc(&mut self, name_or_id: &str, rpc_url: String) -> Result<()> {
        let pos = self.config.find_chain_index(name_or_id)?;
        self.config.chains[pos].select_rpc(rpc_url);
        Ok(())
    }

    pub async fn save(&self) -> Result<()> {
        if self._config_lock.is_some() {
            self.config.write_locked().await
        } else {
            let _config_lock = ConfigLock::acquire().await?;
            self.config.write_locked().await
        }
    }

    /// Release the process-wide config transaction before starting work that
    /// cannot mutate config (for example, a long-running child process).
    pub fn release_config_lock(&mut self) {
        self._config_lock.take();
    }
}

impl Config {
    /// Load the config, returning `Ok(None)` when no config file exists yet.
    /// Any other failure (unreadable file, parse error) is propagated so a
    /// broken config is never mistaken for a missing one.
    async fn load_locked(validate: bool) -> Result<Option<Self>> {
        let path = get_config_path().ok_or(anyhow!("Unable to find config path"))?;
        migrate_legacy_config(&path).await?;
        restrict_permissions(&path).await?;
        let json = match tokio::fs::read_to_string(&path).await {
            Ok(json) => json,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to read config at {}", path.display()));
            }
        };
        let mut config: Self = serde_json::from_str(&json).with_context(|| {
            format!(
                "Failed to parse config at {} (fix or remove the file, then retry)",
                path.display()
            )
        })?;
        config.normalize_legacy();
        if validate {
            config
                .validate()
                .with_context(|| format!("Invalid config at {}", path.display()))?;
        }
        Ok(Some(config))
    }

    pub(crate) fn get_chain(&self, name_or_id: &str) -> Result<&ChainDefinition> {
        self.find_chain_index(name_or_id)
            .map(|index| &self.chains[index])
    }

    /// Resolve a chain reference: exact chain ID, then exact name/alias
    /// (case-insensitive), then unambiguous name/alias prefix.
    pub(crate) fn find_chain_index(&self, name_or_id: &str) -> Result<usize> {
        if let Ok(chain_id) = name_or_id.parse::<u64>()
            && let Some(i) = self.chains.iter().position(|c| c.chain_id == chain_id)
        {
            return Ok(i);
        }

        if let Some(i) = self.chains.iter().position(|c| c.matches_exact(name_or_id)) {
            return Ok(i);
        }

        let matches: Vec<usize> = (0..self.chains.len())
            .filter(|&i| self.chains[i].matches_prefix(name_or_id))
            .collect();
        match matches.as_slice() {
            [i] => Ok(*i),
            [] => Err(anyhow!("Chain '{}' not found", name_or_id)),
            many => Err(anyhow!(
                "Chain '{}' is ambiguous: matches {}",
                name_or_id,
                many.iter()
                    .map(|&i| self.chains[i].name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    fn find_chain_exact_index(&self, name_or_id: &str) -> Result<usize> {
        if let Ok(chain_id) = name_or_id.parse::<u64>()
            && let Some(index) = self
                .chains
                .iter()
                .position(|chain| chain.chain_id == chain_id)
        {
            return Ok(index);
        }
        self.chains
            .iter()
            .position(|chain| chain.name.eq_ignore_ascii_case(name_or_id))
            .ok_or_else(|| {
                anyhow!(
                    "Chain '{}' not found; removal requires an exact chain name or ID",
                    name_or_id
                )
            })
    }

    async fn write_locked(&self) -> Result<()> {
        self.validate()
            .context("Refusing to write invalid config")?;
        let path = get_config_path().ok_or(anyhow!("Unable to find config path"))?;
        if let Some(dir) = path.parent() {
            ensure_private_dir(dir).await?;
        }
        let json = serde_json::to_string_pretty(self)?;
        tokio::task::spawn_blocking(move || write_atomically(&path, json.as_bytes()))
            .await
            .context("Config writer task failed")??;
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.globals.validate()?;

        for (map_name, key) in &self.keys {
            if map_name != &key.name {
                anyhow::bail!(
                    "Key map name '{}' does not match record name '{}'",
                    map_name,
                    key.name
                );
            }
            key.validate_record()
                .with_context(|| format!("Invalid key '{}'", map_name))?;
        }

        let mut names = HashMap::<String, String>::new();
        let mut ids = HashMap::<u64, String>::new();
        for chain in &self.chains {
            if chain.name.trim().is_empty() {
                anyhow::bail!("Chain names cannot be empty");
            }
            if chain.rpc_urls.is_empty() {
                anyhow::bail!("Chain '{}' has no RPC URLs", chain.name);
            }
            if !chain.rpc_urls.contains(&chain.selected_rpc) {
                anyhow::bail!(
                    "Chain '{}' selected RPC is not present in its RPC list",
                    chain.name
                );
            }
            if let Some(key_name) = chain.key_name.as_deref()
                && !self.keys.contains_key(key_name)
            {
                anyhow::bail!(
                    "Chain '{}' references missing key '{}'",
                    chain.name,
                    key_name
                );
            }
            if let Some(other) = ids.insert(chain.chain_id, chain.name.clone()) {
                anyhow::bail!(
                    "Chain ID {} is duplicated by '{}' and '{}'",
                    chain.chain_id,
                    other,
                    chain.name
                );
            }
            for name in chain.names() {
                if name.trim().is_empty() {
                    anyhow::bail!("Chain '{}' has an empty alias", chain.name);
                }
                let normalized = name.to_ascii_lowercase();
                if let Some(other) = names.insert(normalized, chain.name.clone()) {
                    anyhow::bail!(
                        "Chain name or alias '{}' is shared by '{}' and '{}'",
                        name,
                        other,
                        chain.name
                    );
                }
            }
        }

        if let Some(default) = &self.default_chain
            && !self.chains.iter().any(|chain| chain.name == *default)
        {
            anyhow::bail!("Default chain '{}' is not configured", default);
        }
        Ok(())
    }

    fn normalize_legacy(&mut self) {
        // Older manual-chain flows could persist only `selected_rpc`. Keep
        // those configs usable while converging them to the current invariant.
        for chain in &mut self.chains {
            if !chain.selected_rpc.is_empty() && !chain.rpc_urls.contains(&chain.selected_rpc) {
                chain.rpc_urls.push(chain.selected_rpc.clone());
            }
        }
        if let Some(default) = self.default_chain.clone()
            && let Some(chain) = self
                .chains
                .iter()
                .find(|chain| chain.matches_exact(&default))
        {
            self.default_chain = Some(chain.name.clone());
        }
    }
}

#[cfg(test)]
mod tests;
