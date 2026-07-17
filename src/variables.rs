use crate::{chain::ChainInstance, config::Chainz, opt::VarCommand};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::io::{IsTerminal, Read};
use zeroize::Zeroize;

#[derive(Default, Serialize, Deserialize)]
pub struct GlobalVariables {
    /// INFURA_API_KEY etc
    #[serde(flatten)]
    rpc_expansions: HashMap<String, String>,
}

impl fmt::Debug for GlobalVariables {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<_> = self.rpc_expansions.keys().collect();
        names.sort();
        f.debug_struct("GlobalVariables")
            .field("names", &names)
            .finish()
    }
}

pub struct ChainVariables {
    env: HashMap<String, String>,
    expansions: HashMap<String, String>,
}

impl ChainVariables {
    pub fn new(chain: &ChainInstance, command: &[String], expose_key: bool) -> Result<Self> {
        let needs_key_arg = command.iter().any(|arg| arg.contains("@key"));
        let needs_wallet = command.iter().any(|arg| arg.contains("@wallet"));

        let mut env = HashMap::new();
        let mut expansions = HashMap::new();

        // Always set non-key variables
        let basic_vars = [
            ("ETH_RPC_URL", "@rpc", chain.rpc_url.clone()),
            (
                "CHAIN_ID",
                "@chainid",
                chain.definition.chain_id.to_string(),
            ),
            ("CHAIN_NAME", "@chainname", chain.definition.name.clone()),
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

        for (env_var, expansion, val) in &basic_vars {
            env.insert(env_var.to_string(), val.clone());
            expansions.insert(expansion.to_string(), val.clone());
        }

        // Only resolve the private key when the command explicitly needs it.
        // New safe-storage records cache the public address, so @wallet does
        // not need to unlock the backing credential store. Legacy records
        // without that metadata derive it once at execution time.
        // Note: private_key() returns Zeroizing<String>, but we convert to String here
        // because std::process::Command::envs() requires String values and doesn't zeroize
        // internally. The child process also holds the key in its environment unzeroed.
        // The real security boundary is the lazy check above — encrypted keys are never
        // decrypted unless the command explicitly needs them.
        let key = if needs_key_arg || needs_wallet || expose_key {
            Some(chain.key.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Chain '{}' has no key attached; use `chainz update {} --key <name>`",
                    chain.definition.name,
                    chain.definition.name
                )
            })?)
        } else {
            None
        };
        let cached_address = needs_wallet
            .then(|| {
                key.expect("wallet expansion requires a key")
                    .address_noninteractive()
            })
            .flatten();
        let must_resolve_key =
            needs_key_arg || expose_key || (needs_wallet && cached_address.is_none());

        if must_resolve_key || cached_address.is_some() {
            let private_key = must_resolve_key
                .then(|| key.expect("private-key use requires a key").private_key())
                .transpose()?;
            if needs_wallet {
                let address = match cached_address {
                    Some(address) => address,
                    None => crate::key::Key::address_from_private_key(
                        private_key
                            .as_deref()
                            .expect("private key is resolved for legacy wallet metadata"),
                    )?
                    .to_string(),
                };
                env.insert("WALLET_ADDRESS".to_string(), address.clone());
                expansions.insert("@wallet".to_string(), address);
            }
            if needs_key_arg || expose_key {
                env.insert(
                    "RAW_PRIVATE_KEY".to_string(),
                    private_key
                        .as_deref()
                        .expect("private key is resolved when explicitly requested")
                        .to_string(),
                );
            }
            if needs_key_arg {
                eprintln!(
                    "Warning: @key expands the private key into process arguments; prefer --expose-key with $RAW_PRIVATE_KEY"
                );
                expansions.insert(
                    "@key".to_string(),
                    private_key
                        .as_deref()
                        .expect("private key is resolved for @key")
                        .to_string(),
                );
            }
        }

        Ok(Self { env, expansions })
    }

    pub fn as_map(&self) -> &HashMap<String, String> {
        &self.env
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

    pub(crate) fn validate(&self) -> Result<()> {
        for key in self.rpc_expansions.keys() {
            if key.is_empty() || key.contains(['{', '}']) {
                anyhow::bail!("Invalid variable name '{}'", key);
            }
        }
        Ok(())
    }

    pub fn remove_rpc_expansion(&mut self, key: &str) -> Option<String> {
        self.rpc_expansions.remove(key)
    }

    pub fn get_rpc_expansion(&self, key: &str) -> Option<String> {
        self.rpc_expansions.get(key).cloned()
    }

    pub fn list_rpc_expansions(&self) -> &HashMap<String, String> {
        &self.rpc_expansions
    }
}

impl VarCommand {
    pub async fn handle(self, chainz: &mut Chainz) -> Result<()> {
        match self {
            VarCommand::Set { name, value, stdin } => {
                if stdin && value.is_some() {
                    anyhow::bail!("Provide a value or --stdin, not both");
                }
                let value = if stdin {
                    read_value_from_stdin()?
                } else if let Some(value) = value {
                    eprintln!(
                        "Warning: variable values in argv may be visible in shell history; prefer --stdin"
                    );
                    value
                } else if std::io::stdin().is_terminal() {
                    rpassword::prompt_password(format!("Value for {}: ", name))?
                } else {
                    anyhow::bail!("No value provided; use --stdin for scripts")
                };
                chainz.config.globals.add_rpc_expansion(&name, &value);
                chainz.save().await?;
                println!("Set variable {}", name);
            }
            VarCommand::Get { name, show } => {
                match chainz.config.globals.get_rpc_expansion(&name) {
                    Some(value) if show => println!("{} = {}", name, value),
                    Some(_) => println!("{} = [REDACTED]", name),
                    None => anyhow::bail!("Variable '{}' not found", name),
                }
            }
            VarCommand::List { show, json } => {
                let vars = chainz.config.globals.list_rpc_expansions();
                if json {
                    println!("{}", serde_json::to_string_pretty(vars)?);
                } else if vars.is_empty() {
                    println!("No variables set");
                } else {
                    println!("Variables:");
                    let mut entries: Vec<_> = vars.iter().collect();
                    entries.sort_by_key(|(name, _)| *name);
                    for (name, value) in entries {
                        println!(
                            "  {} = {}",
                            name,
                            if show { value.as_str() } else { "[REDACTED]" }
                        );
                    }
                }
            }
            VarCommand::Remove { name } => {
                if chainz.config.globals.remove_rpc_expansion(&name).is_none() {
                    anyhow::bail!("Variable '{}' not found", name);
                }
                chainz.save().await?;
                println!("Removed variable '{}'", name);
            }
        }
        Ok(())
    }
}

fn read_value_from_stdin() -> Result<String> {
    let mut value = String::new();
    std::io::stdin().read_to_string(&mut value)?;
    let normalized = value.trim_end_matches(['\r', '\n']).to_string();
    value.zeroize();
    if normalized.is_empty() {
        anyhow::bail!("Value from stdin was empty");
    }
    Ok(normalized)
}

/// Redact credential-bearing URL shapes for display and JSON output.
pub fn redact_url(input: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(input) else {
        return "[REDACTED URL]".to_string();
    };
    if !url.username().is_empty() {
        let _ = url.set_username("REDACTED");
    }
    if url.password().is_some() {
        let _ = url.set_password(Some("REDACTED"));
    }
    if url.query().is_some() {
        let names: Vec<String> = url
            .query_pairs()
            .map(|(name, _)| name.into_owned())
            .collect();
        url.set_query(None);
        for name in names {
            url.query_pairs_mut().append_pair(&name, "REDACTED");
        }
    }
    if url.path() != "/" && !url.path().is_empty() {
        let readable_path = url.path().replace("%7B", "{").replace("%7D", "}");
        let templates = variable_templates(&readable_path);
        let redacted_path = if templates.is_empty() {
            "/REDACTED".to_string()
        } else {
            format!("/REDACTED/{}", templates.join("/"))
        };
        url.set_path(&redacted_path);
    }
    if url.fragment().is_some() {
        url.set_fragment(Some("REDACTED"));
    }
    // url::Url percent-encodes braces, but variable names are safe public
    // metadata and keeping ${NAME} intact makes redacted output actionable.
    url.to_string().replace("%7B", "{").replace("%7D", "}")
}

fn variable_templates(input: &str) -> Vec<String> {
    let mut templates = Vec::new();
    let mut remainder = input;
    while let Some((start, end)) = find_next_var(remainder) {
        templates.push(remainder[start..end].to_string());
        remainder = &remainder[end..];
    }
    templates
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
mod tests;
