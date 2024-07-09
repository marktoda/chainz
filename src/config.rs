use crate::opt::AddArgs;
use alloy::{
    providers::{Provider, ProviderBuilder},
    signers::{local::PrivateKeySigner, Signer},
    transports::BoxTransport,
};
use anyhow::{anyhow, Result};
use dirs::home_dir;
use serde::{Deserialize, Serialize};
use std::fmt::{Debug, Display};
use std::path::PathBuf;

pub const CONFIG_FILE_LOCATION: &str = ".chainz.json";
pub const DEFAULT_ENV_PREFIX: &str = "FOUNDRY";

#[derive(Serialize, Deserialize)]
pub struct ChainzConfig {
    pub default_private_key: String,
    pub env_prefix: String,
    pub chains: Vec<ChainConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainConfig {
    pub name: String,
    pub chain_id: u64,
    pub rpc_url: String,
    pub verification_api_key: String,
    pub private_key: Option<String>,
}

pub struct Chain {
    pub config: ChainConfig,
    pub provider: Box<dyn Provider<BoxTransport>>,
    pub private_key: String,
    pub wallet: Box<dyn Signer>,
}

impl ChainzConfig {
    pub fn new() -> Self {
        // generate a random private key as default
        let signer = PrivateKeySigner::random();
        let private_key = signer.to_bytes().to_string();
        Self {
            default_private_key: private_key,
            env_prefix: DEFAULT_ENV_PREFIX.to_string(),
            chains: vec![],
        }
    }

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

    async fn get_chain(&self, config: &ChainConfig) -> Result<Chain> {
        let provider = create_provider(&config.rpc_url).await?;
        let private_key = config
            .private_key
            .clone()
            .unwrap_or(self.default_private_key.clone());
        let signer = private_key.parse::<PrivateKeySigner>()?;
        Ok(Chain {
            config: config.clone(),
            provider,
            private_key,
            wallet: Box::new(signer),
        })
    }

    pub async fn get_chains(&self) -> Result<Vec<Chain>> {
        let mut chains = vec![];
        for chain in &self.chains {
            chains.push(self.get_chain(chain).await?);
        }
        Ok(chains)
    }

    pub async fn add_chain(&mut self, args: &AddArgs) -> Result<Chain> {
        let chain = ChainConfig::from_add_args(args).await?;
        // update chain if it already exists
        if let Some(existing_chain) = self.chains.iter_mut().find(|c| c.name == chain.name) {
            *existing_chain = chain.clone();
        } else {
            self.chains.push(chain.clone());
        }
        Ok(self.get_chain_by_name(&args.name).await?)
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
        let rpc_url = args.rpc_url.clone();
        let provider = create_provider(&rpc_url).await?;
        let chain_id = provider.get_chain_id().await?;
        Ok(Self {
            name: args.name.clone(),
            chain_id,
            rpc_url,
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
            "ChainConfig {{ name: {}, chain_id: {}, rpc_url: {}, verification_api_key: {}, private_key: {:?} }}",
            self.name, self.chain_id, self.rpc_url, self.verification_api_key, self.private_key
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
