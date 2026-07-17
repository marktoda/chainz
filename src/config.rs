use crate::{
    chain::{ChainDefinition, ChainInstance},
    key::Key,
    variables::GlobalVariables,
};
use anyhow::{Context, Result, anyhow};
use dirs::home_dir;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

/// Pre-0.3 config location, relative to $HOME. Migrated on first load.
pub const LEGACY_CONFIG_FILE: &str = ".chainz.json";
/// Config location relative to $HOME (when XDG_CONFIG_HOME is unset).
pub const DEFAULT_CONFIG_RELATIVE: &str = ".config/chainz/config.json";

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
}

impl Chainz {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn load() -> Result<Self> {
        // Start fresh when no config exists yet; Config::load propagates
        // errors for a config that exists but can't be read/parsed —
        // silently defaulting would wipe the real config on the next save.
        let config = Config::load().await?.unwrap_or_default();
        Ok(Self { config })
    }

    /// Load deserializable config without enforcing semantic invariants so
    /// `doctor` can report and help repair legacy-invalid states.
    pub async fn load_for_doctor() -> Result<Self> {
        let config = Config::load_unvalidated().await?.unwrap_or_default();
        Ok(Self { config })
    }

    pub fn get_chain(&self, name_or_id: &str) -> Result<ChainInstance> {
        let definition = self.config.get_chain(name_or_id)?;
        let rpc_url = self.config.globals.expand_rpc_url(&definition.selected_rpc);
        let key = definition
            .key_name
            .as_deref()
            .and_then(|name| self.config.keys.get(name))
            .cloned();
        Ok(ChainInstance::new(definition, rpc_url, key))
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

        let previous_default = self.config.default_chain.clone();
        let previous_chain = replacement.map(|pos| self.config.chains[pos].clone());
        if let Some(pos) = replacement {
            let previous_name = self.config.chains[pos].name.clone();
            if self.config.default_chain.as_deref() == Some(previous_name.as_str()) {
                self.config.default_chain = Some(chain.name.clone());
            }
            self.config.chains[pos] = chain;
        } else {
            self.config.chains.push(chain);
        }
        if let Err(error) = self.config.validate() {
            self.config.default_chain = previous_default;
            match (replacement, previous_chain) {
                (Some(pos), Some(previous)) => self.config.chains[pos] = previous,
                (None, None) => {
                    self.config.chains.pop();
                }
                _ => unreachable!("replacement state is paired"),
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn replace_chain(&mut self, name_or_id: &str, chain: ChainDefinition) -> Result<()> {
        let index = self.config.find_chain_index(name_or_id)?;
        self.config.chains[index] = chain;
        Ok(())
    }

    /// Whether a chain would collide with `name` (by name or alias).
    pub fn chain_exists(&self, name: &str) -> bool {
        self.config.chains.iter().any(|c| c.matches_exact(name))
    }

    pub fn list_chains(&self) -> &[ChainDefinition] {
        &self.config.chains
    }

    pub fn remove_chain(&mut self, name_or_id: &str) -> Result<ChainDefinition> {
        let pos = self.config.find_chain_index(name_or_id)?;
        self.remove_chain_at(pos)
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
        self.config.write().await
    }
}

impl Config {
    /// Load the config, returning `Ok(None)` when no config file exists yet.
    /// Any other failure (unreadable file, parse error) is propagated so a
    /// broken config is never mistaken for a missing one.
    pub async fn load() -> Result<Option<Self>> {
        let config = Self::load_unvalidated().await?;
        if let Some(config) = &config {
            let path = get_config_path().ok_or(anyhow!("Unable to find config path"))?;
            config
                .validate()
                .with_context(|| format!("Invalid config at {}", path.display()))?;
        }
        Ok(config)
    }

    async fn load_unvalidated() -> Result<Option<Self>> {
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
        Ok(Some(config))
    }

    pub fn get_chain(&self, name_or_id: &str) -> Result<ChainDefinition> {
        self.find_chain_index(name_or_id)
            .map(|i| self.chains[i].clone())
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

    pub async fn write(&self) -> Result<()> {
        self.validate()
            .context("Refusing to write invalid config")?;
        let path = get_config_path().ok_or(anyhow!("Unable to find config path"))?;
        if let Some(dir) = path.parent() {
            ensure_private_dir(dir).await?;
        }
        let tmp_path = path.with_extension("json.tmp");

        let json = serde_json::to_string_pretty(self)?;

        // Write to temp file, sync to disk, then atomically rename over the real file.
        // This ensures the config is never left in a partial/corrupt state.
        // The config may hold private keys, so it must only be readable by the owner;
        // remove any stale temp file so the mode applies at creation.
        let _ = tokio::fs::remove_file(&tmp_path).await;
        let mut open_options = tokio::fs::OpenOptions::new();
        open_options.write(true).create_new(true);
        #[cfg(unix)]
        open_options.mode(0o600);
        let mut file = open_options.open(&tmp_path).await?;
        file.write_all(json.as_bytes()).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp_path, &path).await?;

        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
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

/// Create `dir` (if needed) with owner-only permissions; it will hold a
/// config that can contain private keys.
async fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    #[cfg(unix)]
    tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
    Ok(())
}

/// Tighten permissions on an existing config to owner-only, since it may
/// contain private keys. A missing file is fine; other failures are fatal.
async fn restrict_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    match tokio::fs::metadata(path).await {
        Ok(meta) => {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to restrict config permissions at {}",
                            path.display()
                        )
                    })?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to inspect {}", path.display()));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn get_config_path() -> Option<PathBuf> {
    // Honor XDG_CONFIG_HOME when set; otherwise use ~/.config even on macOS,
    // matching common CLI-tool convention.
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir).join("chainz").join("config.json")),
        _ => Some(home_dir()?.join(DEFAULT_CONFIG_RELATIVE)),
    }
}

fn legacy_config_path() -> Option<PathBuf> {
    Some(home_dir()?.join(LEGACY_CONFIG_FILE))
}

/// Move a pre-0.3 config from ~/.chainz.json to the current location.
/// No-op when there is nothing to migrate or a config already exists at the
/// new path (the legacy file is then left untouched).
async fn migrate_legacy_config(new_path: &Path) -> Result<()> {
    match tokio::fs::metadata(new_path).await {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to inspect {}", new_path.display()));
        }
    }
    let Some(legacy) = legacy_config_path() else {
        return Ok(());
    };
    match tokio::fs::metadata(&legacy).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to inspect {}", legacy.display()));
        }
    }
    if let Some(dir) = new_path.parent() {
        ensure_private_dir(dir)
            .await
            .with_context(|| format!("Failed to create config directory at {}", dir.display()))?;
    }
    tokio::fs::rename(&legacy, new_path)
        .await
        .with_context(|| {
            format!(
                "Failed to migrate config from {} to {}",
                legacy.display(),
                new_path.display()
            )
        })?;
    eprintln!(
        "Migrated config from {} to {}",
        legacy.display(),
        new_path.display()
    );
    Ok(())
}

pub fn config_exists() -> bool {
    get_config_path().map(|p| p.exists()).unwrap_or(false)
        || legacy_config_path().map(|p| p.exists()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
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
                value: "0000000000000000000000000000000000000000000000000000000000000001"
                    .to_string(),
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
}
