use super::{
    ChainDefinition, DEFAULT_KEY_NAME,
    rpc::{check_url, probe_urls, rank_by_health},
};
use crate::ui;
use crate::{
    chainlist::{ChainlistEntry, fetch_all_chains, fetch_chain_by_id},
    config::Chainz,
    opt::{AddArgs, UpdateArgs},
    variables::GlobalVariables,
};
use anyhow::{Context, Result};
use console::style;
use dialoguer::{FuzzySelect, Input};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

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

/// Pick an RPC for a chain. `urls` are raw (may contain ${VAR}); they are
/// expanded only for probing. Displays and returns raw URLs so secrets are
/// never shown on screen or written to config.
pub async fn select_rpc(
    chain_name: &str,
    chain_id: u64,
    urls: Vec<String>,
    globals: &GlobalVariables,
) -> Result<String> {
    let expanded: Vec<String> = urls.iter().map(|u| globals.expand_rpc_url(u)).collect();

    // Live per-RPC status lines; hidden automatically when not a TTY
    let multi = MultiProgress::new();
    let bars: Vec<ProgressBar> = urls
        .iter()
        .map(|url| {
            let bar = multi.add(ProgressBar::new_spinner());
            bar.set_style(
                ProgressStyle::with_template("{spinner} {msg}").expect("static template"),
            );
            bar.enable_steady_tick(std::time::Duration::from_millis(120));
            bar.set_message(url.clone());
            bar
        })
        .collect();

    let mut results = Vec::with_capacity(urls.len());
    let mut rx = probe_urls(&expanded, chain_id);
    while let Some(result) = rx.recv().await {
        let bar = &bars[result.index];
        if result.healthy {
            bar.finish_with_message(ui::success(&format!(
                "{}  {}ms",
                urls[result.index],
                result.latency.as_millis()
            )));
        } else {
            bar.finish_with_message(ui::fail(&format!(
                "{}  {}",
                urls[result.index],
                ui::dim("unreachable")
            )));
        }
        results.push(result);
    }

    // Healthy-first, fastest-first picker over RAW urls
    let order = rank_by_health(&results);
    // Index results by url position once, rather than a linear scan per item.
    let mut by_index: Vec<Option<&_>> = vec![None; urls.len()];
    for r in &results {
        by_index[r.index] = Some(r);
    }
    let mut items: Vec<String> = order
        .iter()
        .map(|&i| {
            let r = by_index[i].expect("every url index has a probe result");
            if r.healthy {
                format!("{} ({}ms)", urls[i], r.latency.as_millis())
            } else {
                format!("{} (unreachable)", urls[i])
            }
        })
        .collect();
    items.push("Enter RPC URL manually...".to_string());

    let selection = fuzzy_select(
        &format!("Select an RPC URL for {}", ui::emph(chain_name)),
        &items,
        0,
    )?;

    if selection == items.len() - 1 {
        select_manual_rpc(chain_id, globals).await
    } else {
        Ok(urls[order[selection]].clone())
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
        let key = crate::key::prompt_for_new_key(&kname)?;
        chainz.add_key(&kname, key)?;
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
        let interactive =
            !(self.name.is_some() && self.chain_id.is_some() && self.rpc_url.is_some());
        let chain = if interactive {
            self.build_interactive(chainz).await?
        } else {
            self.build_non_interactive(chainz).await?
        };
        self.confirm_replacement(chainz, &chain, interactive)?;
        chainz.add_chain(chain.clone())?;
        chainz.save().await?;
        Ok(chain)
    }

    pub(crate) async fn handle_in_memory(&self, chainz: &mut Chainz) -> Result<ChainDefinition> {
        let chain = self.build_interactive(chainz).await?;
        self.confirm_replacement(chainz, &chain, true)?;
        chainz.add_chain(chain.clone())?;
        Ok(chain)
    }

    async fn build_non_interactive(&self, chainz: &mut Chainz) -> Result<ChainDefinition> {
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
        check_url(&chainz.config.globals.expand_rpc_url(&rpc_url), chain_id)
            .await
            .with_context(|| format!("RPC check failed for {}", rpc_url))?;

        Ok(ChainDefinition {
            name: name.clone(),
            aliases: vec![],
            chain_id,
            rpc_urls: vec![rpc_url.clone()],
            selected_rpc: rpc_url,
            verification_api_key: self.verification_api_key.clone(),
            verification_url: self.verification_url.clone(),
            key_name,
        })
    }

    async fn build_interactive(&self, chainz: &mut Chainz) -> Result<ChainDefinition> {
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
            .await
            .with_context(|| format!("RPC check failed for {}", rpc_url))?;
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

        Ok(ChainDefinition {
            name,
            aliases,
            chain_id: selected_chain.chain_id,
            rpc_urls: selected_chain.rpc,
            selected_rpc,
            verification_api_key,
            verification_url,
            key_name,
        })
    }

    fn confirm_replacement(
        &self,
        chainz: &Chainz,
        chain: &ChainDefinition,
        interactive: bool,
    ) -> Result<()> {
        if chainz.chain_exists(&chain.name) && !self.force {
            if interactive {
                let confirm = dialoguer::Confirm::new()
                    .with_prompt(format!(
                        "Chain '{}' already exists. Replace it?",
                        chain.name
                    ))
                    .default(false)
                    .interact()?;
                if !confirm {
                    anyhow::bail!("Cancelled");
                }
            } else {
                anyhow::bail!(
                    "Chain '{}' already exists. Use --force to overwrite.",
                    chain.name
                );
            }
        }
        Ok(())
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
fn fuzzy_select<T: std::fmt::Display>(prompt: &str, items: &[T], default: usize) -> Result<usize> {
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
