use crate::{chainlist::fetch_chain_data, key::Key, opt::AddArgs};
use alloy::{
    providers::{Provider, ProviderBuilder},
    transports::BoxTransport,
};
use anyhow::{anyhow, Result};
use colored::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{Debug, Display};

pub const DEFAULT_KEY_NAME: &str = "default";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainDefinition {
    pub name: String,
    pub chain_id: u64,
    pub rpc_urls: Vec<String>,
    pub selected_rpc: Option<String>,
    pub verification_api_key: Option<String>,
    pub key_name: String,
}

pub struct ChainInstance {
    pub definition: ChainDefinition,
    pub provider: Box<dyn Provider<BoxTransport>>,
    pub rpc_url: String,
    pub key: Key,
}

impl ChainDefinition {
    pub async fn new(args: &AddArgs) -> Result<Self> {
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
            selected_rpc: None,
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
            key_name: args
                .key_name
                .clone()
                .unwrap_or(DEFAULT_KEY_NAME.to_string()),
        })
    }

    pub fn resolve_variables(&self, variables: &HashMap<String, String>) -> Self {
        let new_rpc_urls = self
            .rpc_urls
            .iter()
            .map(|url| interpolate_variables(url, variables))
            .collect();
        let mut new_config = self.clone();
        new_config.rpc_urls = new_rpc_urls;
        new_config
    }

    pub async fn get_rpc(&self) -> Result<(String, Box<dyn Provider<BoxTransport>>)> {
        // First try the last working RPC if available
        if let Some(selected) = &self.selected_rpc {
            if let Some(rpc_url) = test_rpc(selected, self.chain_id).await {
                return Ok((rpc_url.clone(), create_provider(&rpc_url).await?));
            }
        }

        // If last working RPC failed or doesn't exist, try others
        for rpc_url in &self.rpc_urls {
            if let Some(rpc_url) = test_rpc(rpc_url, self.chain_id).await {
                return Ok((rpc_url.clone(), create_provider(&rpc_url).await?));
            }
        }

        Err(anyhow!("No valid RPC urls found"))
    }
}

impl Display for ChainDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "{}: {}",
            "Chain".bright_blue().bold(),
            self.name.yellow()
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            "├".bright_black(),
            "ID".bright_blue(),
            self.chain_id.to_string().yellow()
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            "├".bright_black(),
            "Active RPC".bright_blue(),
            self.selected_rpc
                .as_deref()
                .unwrap_or("None")
                .bright_green()
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            "├".bright_black(),
            "Verification Key".bright_blue(),
            self.verification_api_key
                .as_deref()
                .map(|k| k.bright_green().to_string())
                .unwrap_or_else(|| "None".bright_red().to_string())
        )?;
        write!(
            f,
            "{}─ {}: {}",
            "└".bright_black(),
            "Key Name".bright_blue(),
            self.key_name.bright_green(),
        )
    }
}

impl Display for ChainInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "{}: {}",
            "Chain".bright_blue().bold(),
            self.definition.name.yellow()
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            "├".bright_black(),
            "ID".bright_blue(),
            self.definition.chain_id.to_string().yellow()
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            "├".bright_black(),
            "RPC".bright_blue(),
            self.rpc_url.bright_green()
        )?;
        write!(
            f,
            "{}─ {}: {}",
            "└".bright_black(),
            "Wallet".bright_blue(),
            self.key
                .address()
                .map(|addr| addr.to_string().bright_green())
                .unwrap_or("None".bright_red())
        )
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
async fn test_rpc(rpc_url: &str, expected_chain_id: u64) -> Option<String> {
    // First try the last working RPC if available
    if let Ok(provider) = create_provider(rpc_url).await {
        if let Ok(chain_id) = provider.get_chain_id().await {
            if chain_id == expected_chain_id {
                return Some(rpc_url.to_string());
            }
        }
    }
    None
}
async fn create_provider(rpc_url: &str) -> Result<Box<dyn Provider<BoxTransport>>> {
    Ok(Box::new(
        ProviderBuilder::new()
            .with_recommended_fillers()
            .on_builtin(rpc_url)
            .await?,
    ))
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
