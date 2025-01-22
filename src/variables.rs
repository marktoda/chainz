use crate::{chain::ChainInstance, config::Chainz, opt::VarCommand};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::prelude::*;

pub const DOT_ENV: &str = ".env";

#[derive(Default, Debug, Serialize, Deserialize)]
pub struct GlobalVariables {
    /// INFURA_API_KEY etc
    #[serde(flatten)]
    rpc_expansions: HashMap<String, String>,
}

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
            (
                "VERIFIER_URL",
                "@verification_url",
                chain
                    .definition
                    .verification_url
                    .clone()
                    .unwrap_or("UNDEFINED".to_string()),
            ),
            (
                "VERIFIER_API_KEY",
                "@verifier_api_key",
                chain
                    .definition
                    .verification_api_key
                    .clone()
                    .unwrap_or("UNDEFINED".to_string()),
            ),
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

impl GlobalVariables {
    pub fn expand_rpc_url(&self, rpc_url: &str) -> String {
        interpolate_variables(rpc_url, &self.rpc_expansions)
    }

    pub fn add_rpc_expansion(&mut self, key: &str, value: &str) {
        self.rpc_expansions
            .insert(key.to_string(), value.to_string());
    }

    pub fn remove_rpc_expansion(&mut self, key: &str) -> Option<String> {
        self.rpc_expansions.remove(key)
    }

    pub fn get_rpc_expansion(&self, key: &str) -> Option<String> {
        self.rpc_expansions.get(key).cloned()
    }

    // TODO: return iterator?
    pub fn list_rpc_expansions(&self) -> HashMap<String, String> {
        self.rpc_expansions.clone()
    }
}

impl VarCommand {
    // TODO: dynamically find all ${} fillins and list them
    pub async fn handle(self, chainz: &mut Chainz) -> Result<()> {
        match self {
            VarCommand::Set { name, value } => {
                chainz.config.globals.add_rpc_expansion(&name, &value);
                chainz.save().await?;
                println!("Set variable {} = {}", name, value);
            }
            VarCommand::Get { name } => match chainz.config.globals.get_rpc_expansion(&name) {
                Some(value) => println!("{} = {}", name, value),
                None => println!("Variable '{}' not found", name),
            },
            VarCommand::List => {
                let vars = chainz.config.globals.list_rpc_expansions();
                if vars.is_empty() {
                    println!("No variables set");
                } else {
                    println!("Variables:");
                    for (name, value) in vars {
                        println!("  {} = {}", name, value);
                    }
                }
            }
            VarCommand::Rm { name } => {
                chainz.config.globals.remove_rpc_expansion(&name);
                chainz.save().await?;
                println!("Removed variable '{}'", name);
            }
        }
        Ok(())
    }
}

fn interpolate_variables(input: &str, variables: &HashMap<String, String>) -> String {
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
        let mut globals = GlobalVariables::default();
        globals.add_rpc_expansion("API_KEY", "config_key");
        globals.add_rpc_expansion("EMPTY", "");

        assert_eq!(
            globals.expand_rpc_url("https://api.example.com/${API_KEY}/v1"),
            "https://api.example.com/config_key/v1"
        );

        assert_eq!(globals.expand_rpc_url("empty:${EMPTY}:end"), "empty::end");
    }

    #[test]
    fn test_environment_variables() {
        setup();
        let globals = GlobalVariables::default();

        assert_eq!(
            globals.expand_rpc_url("https://api.example.com/${TEST_ENV_KEY}/v1"),
            "https://api.example.com/env_key/v1"
        );
    }

    #[test]
    fn test_multiple_replacements() {
        setup();
        let mut globals = GlobalVariables::default();
        globals.add_rpc_expansion("API_KEY", "config_key");

        assert_eq!(
            globals.expand_rpc_url("${API_KEY} and ${TEST_ENV_KEY}"),
            "config_key and env_key"
        );
    }

    #[test]
    fn test_missing_variables() {
        let globals = GlobalVariables::default();

        assert_eq!(
            globals.expand_rpc_url("https://api.example.com/${MISSING_KEY}/v1"),
            "https://api.example.com/${MISSING_KEY}/v1"
        );
    }

    #[test]
    fn test_no_variables() {
        let globals = GlobalVariables::default();

        assert_eq!(
            globals.expand_rpc_url("https://api.example.com/v1"),
            "https://api.example.com/v1"
        );
    }
}
