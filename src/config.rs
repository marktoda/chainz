use crate::{chainlist::fetch_chain_data, opt::AddArgs};
use alloy::{
    providers::{Provider, ProviderBuilder},
    signers::{local::PrivateKeySigner, Signer},
    transports::BoxTransport,
};
use anyhow::{anyhow, Result};
use dirs::home_dir;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{Debug, Display};
use std::path::PathBuf;

pub const CONFIG_FILE_LOCATION: &str = ".chainz.json";
pub const DEFAULT_ENV_PREFIX: &str = "FOUNDRY";

#[derive(Serialize, Deserialize)]
pub struct ChainzConfig {
    pub default_private_key: String,
    pub env_prefix: String,
    pub chains: Vec<ChainConfig>,
    pub variables: HashMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainConfig {
    pub name: String,
    pub chain_id: u64,
    // sorted by order to attempt
    pub rpc_urls: Vec<String>,
    pub verification_api_key: Option<String>,
    pub private_key: Option<String>,
}

pub struct Chain {
    pub config: ChainConfig,
    pub provider: Box<dyn Provider<BoxTransport>>,
    pub rpc_url: String,
    pub private_key: String,
    pub wallet: Box<dyn Signer>,
}

impl Default for ChainzConfig {
    fn default() -> Self {
        // generate a random private key as default
        let signer = PrivateKeySigner::random();
        let private_key = signer.to_bytes().to_string();
        Self {
            default_private_key: private_key,
            env_prefix: DEFAULT_ENV_PREFIX.to_string(),
            chains: vec![],
            variables: HashMap::new(),
        }
    }
}

impl ChainzConfig {
    pub fn set_default_private_key(&mut self, default_private_key: String) {
        self.default_private_key = default_private_key
    }

    pub fn set_default_env_prefix(&mut self, env_prefix: String) {
        self.env_prefix = env_prefix
    }

    pub async fn get_chain_by_name(&self, name: &str) -> Result<Chain> {
        let config = self
            .chains
            .iter()
            .find(|chain| chain.name == name)
            .ok_or(anyhow!("Chain not found"))?;
        self.get_chain(config).await
    }

    pub async fn get_chain_by_id(&self, chain_id: u64) -> Result<Chain> {
        let config = self
            .chains
            .iter()
            .find(|chain| chain.chain_id == chain_id)
            .ok_or(anyhow!("Chain not found"))?;
        self.get_chain(config).await
    }

    // get a chain from a chain config
    async fn get_chain(&self, config: &ChainConfig) -> Result<Chain> {
        let rpc_url = self.get_rpc(config).await?;
        let provider = create_provider(&rpc_url).await?;
        let private_key = config
            .private_key
            .clone()
            .unwrap_or(self.default_private_key.clone());
        let signer = private_key.parse::<PrivateKeySigner>()?;
        Ok(Chain {
            config: config.clone(),
            provider,
            private_key,
            rpc_url,
            wallet: Box::new(signer),
        })
    }

    // get the first rpc url that returns the correct chain id
    async fn get_rpc(&self, config: &ChainConfig) -> Result<String> {
        // try RPC urls one by one
        // injecting environment variables if needed
        // returning the first one that successfully returns chainid
        for rpc_url in &config.rpc_urls {
            // Interpolate environment variables in the RPC URL
            let interpolated_url = interpolate_variables(rpc_url, &self.variables);
            if let Ok(provider) = create_provider(&interpolated_url).await {
                // ensure it equals config.chainId
                if let Ok(chain_id) = provider.get_chain_id().await {
                    if chain_id == config.chain_id {
                        return Ok(interpolated_url);
                    }
                }
            }
        }
        Err(anyhow!("No valid RPC urls found"))
    }

    // get all chains
    pub async fn get_chains(&self) -> Result<Vec<Chain>> {
        let mut chains = vec![];
        for chain in &self.chains {
            chains.push(self.get_chain(chain).await?);
        }
        Ok(chains)
    }

    pub async fn add_chain(&mut self, args: &AddArgs) -> Result<Chain> {
        let chain = ChainConfig::from_add_args(args).await?;
        // print
        // update chain if it already exists
        if let Some(existing_chain) = self.chains.iter_mut().find(|c| c.name == chain.name) {
            *existing_chain = chain.clone();
        } else {
            self.chains.push(chain.clone());
        }
        self.get_chain_by_name(&chain.name).await
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

    pub async fn load() -> Result<Self> {
        let json = tokio::fs::read_to_string(
            get_config_path().ok_or(anyhow!("Unable to find config path"))?,
        )
        .await?;
        let config = serde_json::from_str(&json)?;
        Ok(config)
    }
}

impl ChainConfig {
    pub async fn from_add_args(args: &AddArgs) -> Result<Self> {
        // if no chain_id or name, then throw
        if args.chain_id.is_none() && args.name.is_none() {
            return Err(anyhow!("Either chain_id or name must be provided"));
        }

        let chain_data = fetch_chain_data(args.chain_id, args.name.clone()).await?;

        // Get name and chain_id from either args, chainlist, or generate from chain_id
        let name = args.name.clone().unwrap_or(chain_data.name);
        let chain_id = args.chain_id.unwrap_or(chain_data.chain_id);
        Ok(Self {
            name,
            chain_id,
            // given rpc url is first in list to try if given
            rpc_urls: match &args.rpc_url {
                Some(rpc_url) => {
                    let mut urls = vec![rpc_url.clone()];
                    urls.extend(chain_data.rpc);
                    urls
                }
                None => chain_data.rpc,
            },
            verification_api_key: args.verification_api_key.clone(),
            private_key: args.private_key.clone(),
        })
    }
}

impl Display for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Chain {{ name: {}, chain id: {}, wallet: {} }}",
            self.config.name,
            self.config.chain_id,
            self.wallet.address()
        )
    }
}

impl Display for ChainConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ChainConfig {{ name: {}, chain id: {}, rpc_urls: {:?} }}",
            self.name, self.chain_id, self.rpc_urls
        )
    }
}

async fn create_provider(rpc_url: &str) -> Result<Box<dyn Provider<BoxTransport>>> {
    Ok(Box::new(
        ProviderBuilder::new()
            .with_recommended_fillers()
            .on_builtin(rpc_url)
            .await?,
    ))
}

fn get_config_path() -> Option<PathBuf> {
    let mut path = home_dir()?;
    path.push(CONFIG_FILE_LOCATION);
    Some(path)
}

fn interpolate_variables(
    input: &str,
    variables: &std::collections::HashMap<String, String>,
) -> String {
    let mut result = input.to_string();

    // First replace from config variables
    for (key, value) in variables {
        let pattern = format!("${{{}}}", key);
        result = result.replace(&pattern, value);
    }

    // Then try to replace any remaining ${VAR} patterns with environment variables
    let mut final_result = String::new();
    let mut last_end = 0;

    while let Some((start, end)) = find_next_var(&result[last_end..]) {
        let absolute_start = last_end + start;
        let absolute_end = last_end + end;

        // Add the part before the variable
        final_result.push_str(&result[last_end..absolute_start]);

        // Get the variable name
        let var_name = &result[absolute_start + 2..absolute_end - 1]; // strip ${ and }

        // Try to get the environment variable
        if let Ok(value) = std::env::var(var_name) {
            final_result.push_str(&value);
        } else {
            // If not found, keep the original ${VAR} syntax
            final_result.push_str(&result[absolute_start..absolute_end]);
        }

        last_end = absolute_end;
    }

    // Add any remaining part of the string
    final_result.push_str(&result[last_end..]);

    if final_result.is_empty() {
        result
    } else {
        final_result
    }
}

fn find_next_var(input: &str) -> Option<(usize, usize)> {
    let start = input.find("${")?;
    let end = input[start..].find("}")?.checked_add(start + 1)?;
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::Once;

    static INIT: Once = Once::new();

    /// Setup function that is only run once, even if called multiple times.
    fn setup() {
        INIT.call_once(|| {
            env::set_var("TEST_ENV_KEY", "env_key");
            env::set_var("TEST_OTHER_KEY", "other_value");
        });
    }

    #[test]
    fn test_config_variables() {
        let mut variables = HashMap::new();
        variables.insert("API_KEY".to_string(), "config_key".to_string());
        variables.insert("EMPTY".to_string(), "".to_string());

        assert_eq!(
            interpolate_variables("https://api.example.com/${API_KEY}/v1", &variables),
            "https://api.example.com/config_key/v1"
        );

        assert_eq!(
            interpolate_variables("empty:${EMPTY}:end", &variables),
            "empty::end"
        );
    }

    #[test]
    fn test_environment_variables() {
        setup();
        let variables = HashMap::new();

        assert_eq!(
            interpolate_variables("https://api.example.com/${TEST_ENV_KEY}/v1", &variables),
            "https://api.example.com/env_key/v1"
        );
    }

    #[test]
    fn test_multiple_replacements() {
        setup();
        let mut variables = HashMap::new();
        variables.insert("API_KEY".to_string(), "config_key".to_string());

        assert_eq!(
            interpolate_variables("${API_KEY} and ${TEST_ENV_KEY}", &variables),
            "config_key and env_key"
        );
    }

    #[test]
    fn test_missing_variables() {
        let variables = HashMap::new();

        assert_eq!(
            interpolate_variables("https://api.example.com/${MISSING_KEY}/v1", &variables),
            "https://api.example.com/${MISSING_KEY}/v1"
        );
    }

    #[test]
    fn test_no_variables() {
        let variables = HashMap::new();

        assert_eq!(
            interpolate_variables("https://api.example.com/v1", &variables),
            "https://api.example.com/v1"
        );
    }
}
