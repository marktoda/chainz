use crate::chain::ChainInstance;
use anyhow::Result;
use std::collections::HashMap;
use std::fs::File;
use std::io::prelude::*;

pub const DOT_ENV: &str = ".env";

pub struct ChainVariables {
    env: HashMap<String, String>,
    expansions: HashMap<String, String>,
}

impl ChainVariables {
    pub fn new(chain: &ChainInstance) -> Result<Self> {
        let env_vars = [
            (
                "WALLET_ADDRESS",
                "@wallet",
                chain.key.address().unwrap_or_default().to_string(),
            ),
            ("ETH_RPC_URL", "@rpc", chain.rpc_url.clone()),
            (
                "CHAIN_ID",
                "@chainid",
                chain.definition.chain_id.to_string(),
            ),
            ("CHAIN_NAME", "@chainname", chain.definition.name.clone()),
            ("RAW_PRIVATE_KEY", "@key", chain.key.private_key()?),
        ];

        let mut env = HashMap::new();
        let mut expansions = HashMap::new();

        for (env_var, expansion, val) in &env_vars {
            env.insert(env_var.to_string(), val.clone());
            expansions.insert(expansion.to_string(), val.clone());
        }

        Ok(Self { env, expansions })
    }

    pub fn as_map(&self) -> &HashMap<String, String> {
        &self.env
    }

    // make .env file text string with VAR=VAL
    pub fn as_env_file(&self) -> String {
        let mut res = String::new();
        for (var, val) in &self.env {
            res.push_str(&format!("{}={}\n", var, val));
        }
        res
    }

    // make evaluable exports
    pub fn as_exports(&self) -> String {
        let mut res = String::new();
        for (var, val) in &self.env {
            res.push_str(&format!("export {}={}\n", var, val));
        }
        res
    }

    pub fn write_env(&self) -> Result<()> {
        let mut file = File::create(DOT_ENV)?;
        file.write_all(self.as_env_file().as_bytes())?;
        Ok(())
    }

    pub fn expand(&self, input: Vec<String>) -> Vec<String> {
        input
            .into_iter()
            .map(|arg| {
                let mut result = arg;
                for (key, value) in &self.expansions {
                    result = result.replace(key, value);
                }
                result
            })
            .collect()
    }
}
