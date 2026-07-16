use super::{
    ChainDefinition, DEFAULT_KEY_NAME,
    rpc::{check_url, check_urls},
};
use crate::ui;
use crate::{
    chainlist::{ChainlistEntry, fetch_all_chains, fetch_chain_by_id},
    config::Chainz,
    key::{Key, KeyType},
    opt::{AddArgs, UpdateArgs},
    variables::GlobalVariables,
};
use anyhow::Result;
use console::style;
use dialoguer::{FuzzySelect, Input};

/// Helper function to handle text input with ESC cancellation
fn text_input<T: std::str::FromStr>(prompt: &str, default: Option<String>) -> Result<T>
where
    <T as std::str::FromStr>::Err: std::fmt::Debug,
{
    let mut input = Input::new()
        .with_prompt(format!("{} (Ctrl+C to exit)", prompt))
        .allow_empty(true);

    if let Some(def) = default {
        input = input.default(def);
    }

    match input.interact() {
        Ok(value) if !value.is_empty() => value
            .parse()
            .map_err(|_| anyhow::anyhow!("Failed to parse input")),
        Ok(_) => anyhow::bail!("Operation cancelled by user"),
        Err(_) => anyhow::bail!("Operation cancelled by user"),
    }
}

pub async fn manual_chain_entry(
    name: Option<String>,
    chain_id: Option<u64>,
) -> Result<ChainlistEntry> {
    println!("\n{}", style("Manual Chain Entry").yellow().bold());
    let name = if let Some(n) = name {
        n
    } else {
        text_input("Chain name", None)?
    };
    let chain_id = if let Some(id) = chain_id {
        id
    } else {
        text_input("Chain ID", None)?
    };

    Ok(ChainlistEntry {
        name,
        chain_id,
        rpc: vec![],
    })
}

/// Helper function to select or enter RPC URL.
/// `available_rpcs` are raw (unexpanded) URLs; they are only expanded for
/// the health probe itself and stored/displayed raw.
pub async fn select_rpc(
    chain_name: &str,
    chain_id: u64,
    available_rpcs: Vec<String>,
    globals: &GlobalVariables,
) -> Result<String> {
    println!("\nTesting RPCs...");

    // Initialize displays with "testing" status
    let mut rpc_displays: Vec<String> = available_rpcs
        .iter()
        .map(|url| ui::dim(&format!("⋯ {}", url)))
        .collect();

    // Test all RPCs concurrently against the expected chain id
    let expanded: Vec<String> = available_rpcs
        .iter()
        .map(|url| globals.expand_rpc_url(url))
        .collect();
    for (idx, healthy) in check_urls(&expanded, chain_id)
        .await
        .into_iter()
        .enumerate()
    {
        rpc_displays[idx] = if healthy {
            ui::success(&available_rpcs[idx])
        } else {
            ui::fail(&available_rpcs[idx])
        };
    }

    // Add manual entry option
    rpc_displays.push("Enter RPC URL manually...".to_string());

    let rpc_selection = fuzzy_select(
        &format!("Select an RPC URL for {}", ui::emph(chain_name)),
        &rpc_displays,
        0,
    )?;

    if rpc_selection == rpc_displays.len() - 1 {
        select_manual_rpc(chain_id, globals).await
    } else if let Some(rpc) = available_rpcs.get(rpc_selection) {
        Ok(rpc.clone())
    } else {
        anyhow::bail!("Selected RPC is not working")
    }
}

async fn select_manual_rpc(chain_id: u64, globals: &GlobalVariables) -> Result<String> {
    loop {
        let rpc_url: String = text_input("Enter RPC URL", None)?;
        println!("Testing RPC...");

        if check_url(&globals.expand_rpc_url(&rpc_url), chain_id)
            .await
            .is_ok()
        {
            println!("{}", ui::success("RPC working"));
            return Ok(rpc_url);
        }

        println!("{}", ui::fail("RPC failed. Try again? (ESC to exit)"));
    }
}

/// Helper function to select or create a key
pub fn select_key(chainz: &mut Chainz) -> Result<String> {
    let keys = chainz.list_keys();

    // Create display strings with addresses
    let mut key_displays: Vec<(String, String)> = keys
        .iter()
        .map(|(name, key)| (name.to_string(), key.to_string()))
        .collect();

    // Add the "Add new key" option
    key_displays.push(("Add new key".to_string(), "Add new key".to_string()));

    let key_selection = fuzzy_select(
        "Select a key",
        &key_displays
            .iter()
            .map(|(_, display)| display)
            .collect::<Vec<_>>(),
        0,
    )?;

    if key_selection == key_displays.len() - 1 {
        let kname: String = Input::new().with_prompt("Enter key name").interact_text()?;
        let private_key = rpassword::prompt_password("Enter private key: ")?;
        chainz.add_key(
            &kname,
            Key {
                name: kname.clone(),
                kind: KeyType::PrivateKey { value: private_key },
            },
        )?;
        Ok(kname)
    } else {
        Ok(key_displays[key_selection].0.clone())
    }
}

/// Helper function to select or create a verifier
pub fn select_verifier() -> Result<(Option<String>, Option<String>)> {
    let new_url: String = Input::new()
        .with_prompt("Enter verifier URL (empty to remove)")
        .allow_empty(true)
        .interact_text()?;

    let new_key: String = Input::new()
        .with_prompt("Enter verification API key (empty to remove)")
        .allow_empty(true)
        .interact_text()?;

    match (new_url.is_empty(), new_key.is_empty()) {
        (true, true) => Ok((None, None)),
        (true, false) => Ok((None, Some(new_key))),
        (false, true) => Ok((Some(new_url), None)),
        (false, false) => Ok((Some(new_url), Some(new_key))),
    }
}

impl UpdateArgs {
    pub async fn handle(&self, chainz: &mut Chainz) -> Result<ChainDefinition> {
        println!("{}", ui::header("Chain Update"));

        // Select chain to update
        let chains: Vec<String> = chainz
            .list_chains()
            .iter()
            .map(|c| format!("{} ({})", c.name, c.chain_id))
            .collect();

        if chains.is_empty() {
            anyhow::bail!("No chains configured. Use 'chainz add' to add a chain first.");
        }

        let chain_selection = fuzzy_select("Select chain to update", &chains, 0)?;

        let mut chain = chainz.list_chains()[chain_selection].clone();

        // Select what to update
        let options = vec!["RPC URL", "Key", "Verification"];

        println!("{}", ui::header("Update Options"));
        println!("Current configuration:");
        println!("{}", chain);

        let selection = fuzzy_select("What would you like to update?", &options, 0)?;

        match selection {
            0 => {
                // Update RPC URL
                println!("{}", ui::header("RPC Configuration"));

                // Try to get fresh RPC list from chainlist
                let chainlist_entry = fetch_chain_by_id(chain.chain_id, self.refresh).await;
                let available_rpcs = chainlist_entry
                    .map(|c| c.rpc)
                    .unwrap_or_else(|_| chain.rpc_urls.clone());

                let new_rpc = select_rpc(
                    &chain.name,
                    chain.chain_id,
                    available_rpcs,
                    &chainz.config.globals,
                )
                .await?;
                chain.selected_rpc = new_rpc;
            }
            1 => {
                // Update key
                println!("{}", ui::header("Key Configuration"));

                let new_key = select_key(chainz)?;
                chain.key_name = new_key;
            }
            2 => {
                // Update verification API key
                println!("{}", ui::header("Verification Configuration"));

                let (verification_url, verification_key) = select_verifier()?;
                chain.verification_url = verification_url;
                chain.verification_api_key = verification_key;
            }
            _ => unreachable!(),
        }

        // Save changes
        chainz.add_chain(chain.clone())?;
        chainz.save().await?;
        println!("\n{}", style("Chain updated successfully").green());

        Ok(chain)
    }
}

impl AddArgs {
    pub async fn handle(&self, chainz: &mut Chainz) -> Result<ChainDefinition> {
        if self.name.is_some() && self.chain_id.is_some() && self.rpc_url.is_some() {
            self.handle_non_interactive(chainz).await
        } else {
            self.handle_interactive(chainz).await
        }
    }

    async fn handle_non_interactive(&self, chainz: &mut Chainz) -> Result<ChainDefinition> {
        let name = self.name.clone().unwrap();
        let chain_id = self.chain_id.unwrap();
        let rpc_url = self.rpc_url.clone().unwrap();
        let key_name = self
            .key
            .clone()
            .unwrap_or_else(|| DEFAULT_KEY_NAME.to_string());

        // Validate that the key exists
        chainz.get_key(&key_name).map_err(|_| {
            anyhow::anyhow!(
                "Key '{}' not found. Add it first with 'chainz key add'.",
                key_name
            )
        })?;

        // Test the RPC
        check_url(&chainz.config.globals.expand_rpc_url(&rpc_url), chain_id).await?;

        let chain_def = ChainDefinition {
            name: name.clone(),
            aliases: vec![],
            chain_id,
            rpc_urls: vec![rpc_url.clone()],
            selected_rpc: rpc_url,
            verification_api_key: self.verification_api_key.clone(),
            verification_url: self.verification_url.clone(),
            key_name,
        };

        // Check for existing chain (by name or alias)
        if chainz.chain_exists(&chain_def.name) && !self.force {
            anyhow::bail!(
                "Chain '{}' already exists. Use --force to overwrite.",
                chain_def.name
            );
        }

        chainz.add_chain(chain_def.clone())?;
        chainz.save().await?;
        Ok(chain_def)
    }

    async fn handle_interactive(&self, chainz: &mut Chainz) -> Result<ChainDefinition> {
        println!("{}", ui::header("Chain Selection"));

        let selected_chain = if self.name.is_some() || self.chain_id.is_some() {
            // Pre-fill from CLI args when partially provided
            manual_chain_entry(self.name.clone(), self.chain_id).await?
        } else {
            // Full interactive flow with chainlist
            let chains = fetch_all_chains(self.refresh).await?;
            let items: Vec<String> = chains
                .iter()
                .map(|c| format!("{} ({})", c.name, c.chain_id))
                .collect();

            match fuzzy_select("Type to search and select a chain", &items, 0) {
                Ok(selection) => chains[selection].clone(),
                Err(_) => manual_chain_entry(None, None).await?,
            }
        };

        // Chainlist names are long ("Ethereum Mainnet"); offer a short name
        // for everyday use and keep the original as an alias.
        let (name, aliases) = if self.name.is_none() {
            let chosen: String = Input::new()
                .with_prompt("Chain name")
                .default(suggest_short_name(&selected_chain.name))
                .interact_text()?;
            let aliases = if chosen.eq_ignore_ascii_case(&selected_chain.name) {
                vec![]
            } else {
                vec![selected_chain.name.clone()]
            };
            (chosen, aliases)
        } else {
            (selected_chain.name.clone(), vec![])
        };

        let selected_rpc = if let Some(rpc_url) = &self.rpc_url {
            // Use provided RPC URL directly
            println!("Testing RPC...");
            check_url(
                &chainz.config.globals.expand_rpc_url(rpc_url),
                selected_chain.chain_id,
            )
            .await?;
            println!("{}", ui::success("RPC working"));
            rpc_url.clone()
        } else {
            println!("{}", ui::header("RPC Configuration"));

            select_rpc(
                &selected_chain.name,
                selected_chain.chain_id,
                selected_chain.rpc.clone(),
                &chainz.config.globals,
            )
            .await?
        };

        let key_name = if let Some(key) = &self.key {
            chainz.get_key(key).map_err(|_| {
                anyhow::anyhow!(
                    "Key '{}' not found. Add it first with 'chainz key add'.",
                    key
                )
            })?;
            key.clone()
        } else {
            println!("{}", ui::header("Key Configuration"));
            select_key(chainz)?
        };

        let (verification_url, verification_api_key) =
            if self.verification_url.is_some() || self.verification_api_key.is_some() {
                (
                    self.verification_url.clone(),
                    self.verification_api_key.clone(),
                )
            } else {
                select_verifier()?
            };

        // Create and add the chain
        let chain_def = ChainDefinition {
            name,
            aliases,
            chain_id: selected_chain.chain_id,
            rpc_urls: selected_chain.rpc,
            selected_rpc,
            verification_api_key,
            verification_url,
            key_name,
        };

        // Confirm before replacing an existing chain (matched by name or alias)
        if chainz.chain_exists(&chain_def.name) {
            if self.force {
                // Skip prompt with --force
            } else {
                let confirm = dialoguer::Confirm::new()
                    .with_prompt(format!(
                        "Chain '{}' already exists. Replace it?",
                        chain_def.name
                    ))
                    .default(false)
                    .interact()?;
                if !confirm {
                    anyhow::bail!("Cancelled");
                }
            }
        }

        chainz.add_chain(chain_def.clone())?;
        chainz.save().await?;
        Ok(chain_def)
    }
}

/// Suggest an everyday short name for a chainlist entry:
/// "Ethereum Mainnet" -> "ethereum", "OP Mainnet" -> "op".
fn suggest_short_name(name: &str) -> String {
    name.split_whitespace()
        .next()
        .unwrap_or(name)
        .to_lowercase()
}

// Helper function to handle fuzzy select with ESC cancellation
fn fuzzy_select<T: ToString + std::fmt::Display>(
    prompt: &str,
    items: &[T],
    default: usize,
) -> Result<usize> {
    match FuzzySelect::new()
        .with_prompt(format!("{} (ESC to exit)", prompt))
        .items(items)
        .default(default)
        .interact_opt()?
    {
        Some(selection) => Ok(selection),
        None => anyhow::bail!("Operation cancelled by user"),
    }
}

#[cfg(test)]
mod tests {
    use super::suggest_short_name;

    #[test]
    fn short_name_suggestions() {
        assert_eq!(suggest_short_name("Ethereum Mainnet"), "ethereum");
        assert_eq!(suggest_short_name("OP Mainnet"), "op");
        assert_eq!(suggest_short_name("Avalanche C-Chain"), "avalanche");
        assert_eq!(suggest_short_name("zora"), "zora");
    }
}
