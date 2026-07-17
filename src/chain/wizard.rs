use super::{
    ChainDefinition,
    rpc::{check_url, probe_urls, rank_by_health},
};
use crate::ui;
use crate::{
    chainlist::{ChainlistEntry, fetch_all_chains, fetch_chain_by_id},
    config::Chainz,
    key::{Key, KeyType, create_safe_key_from_prompt},
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
        Ok(_) | Err(_) => Err(ui::cancelled()),
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
            bar.set_message(crate::variables::redact_url(url));
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
                crate::variables::redact_url(&urls[result.index]),
                result.latency.as_millis()
            )));
        } else {
            bar.finish_with_message(ui::fail(&format!(
                "{}  {}",
                crate::variables::redact_url(&urls[result.index]),
                ui::dim("unreachable")
            )));
        }
        results.push(result);
    }
    let show_summary = !multi.is_hidden();
    multi.clear()?;
    if show_summary {
        println!("{}", ui::success(&probe_summary(&results)));
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
                format!(
                    "RPC {} · {} ({}ms)",
                    i + 1,
                    crate::variables::redact_url(&urls[i]),
                    r.latency.as_millis()
                )
            } else {
                format!(
                    "RPC {} · {} (unreachable)",
                    i + 1,
                    crate::variables::redact_url(&urls[i])
                )
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

fn probe_summary(results: &[super::rpc::ProbeResult]) -> String {
    let healthy = results.iter().filter(|result| result.healthy).count();
    format!("{} of {} RPCs healthy", healthy, results.len())
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
fn select_key(chainz: &mut Chainz) -> Result<Option<String>> {
    select_key_inner(chainz, true)
}

fn select_key_staged(chainz: &mut Chainz) -> Result<Option<String>> {
    select_key_inner(chainz, false)
}

fn select_key_inner(chainz: &mut Chainz, store_safely_now: bool) -> Result<Option<String>> {
    let keys = chainz.list_keys();

    // Create display strings with addresses
    let mut key_displays: Vec<(String, String)> = keys
        .iter()
        .map(|(name, key)| (name.to_string(), key.to_string()))
        .collect();

    key_displays.push(("No key".to_string(), "No key (RPC only)".to_string()));
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
        if chainz.get_key(&kname).is_ok() {
            anyhow::bail!("Key '{}' already exists", kname);
        }
        let key = if store_safely_now {
            create_safe_key_from_prompt(&kname)?
        } else {
            let private_key = rpassword::prompt_password("Enter private key: ")?;
            Key::validate_private_key(&private_key)?;
            Key::new(kname.clone(), KeyType::PrivateKey { value: private_key })
        };
        chainz.add_key(&kname, key)?;
        Ok(Some(kname))
    } else if key_selection == key_displays.len() - 2 {
        Ok(None)
    } else {
        Ok(Some(key_displays[key_selection].0.clone()))
    }
}

/// Helper function to select or create a verifier
pub fn select_verifier() -> Result<(Option<String>, Option<String>)> {
    let new_url: String = Input::new()
        .with_prompt("Enter verifier URL (empty to remove)")
        .allow_empty(true)
        .interact_text()?;

    let new_key = rpassword::prompt_password("Enter verification API key (empty to remove): ")?;

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
        let direct = self.has_direct_changes();
        if direct && self.name_or_id.is_none() {
            anyhow::bail!("Direct update flags require a chain argument");
        }

        let original = match &self.name_or_id {
            Some(name_or_id) => chainz.config.get_chain(name_or_id)?,
            None => {
                let chains: Vec<String> = chainz
                    .list_chains()
                    .iter()
                    .map(|chain| format!("{} ({})", chain.name, chain.chain_id))
                    .collect();
                if chains.is_empty() {
                    anyhow::bail!("No chains configured. Use 'chainz add' to add a chain first.");
                }
                let selection = fuzzy_select("Select chain to update", &chains, 0)?;
                chainz.list_chains()[selection].clone()
            }
        };
        let original_name = original.name.clone();
        let mut chain = original;

        if direct {
            self.apply_direct(chainz, &mut chain).await?;
        } else {
            self.edit_interactively(chainz, &mut chain).await?;
        }

        chainz.replace_chain(&original_name, chain.clone())?;
        if chainz.config.default_chain.as_deref() == Some(original_name.as_str()) {
            chainz.config.default_chain = Some(chain.name.clone());
        }
        chainz.save().await?;
        println!("\n{}", style("Chain updated successfully").green());
        Ok(chain)
    }

    fn has_direct_changes(&self) -> bool {
        self.name.is_some()
            || self.rpc_url.is_some()
            || self.key.is_some()
            || self.no_key
            || self.verification_url.is_some()
            || self.verification_api_key.is_some()
            || self.verification_api_key_stdin
            || self.clear_verification
    }

    async fn apply_direct(&self, chainz: &Chainz, chain: &mut ChainDefinition) -> Result<()> {
        if let Some(name) = &self.name {
            let name = name.trim();
            if name.is_empty() {
                anyhow::bail!("Chain name cannot be empty");
            }
            if !chain.matches_exact(name) && chainz.chain_exists(name) {
                anyhow::bail!("Chain '{}' already exists", name);
            }
            chain
                .aliases
                .retain(|alias| !alias.eq_ignore_ascii_case(name));
            chain.name = name.to_string();
        }
        if let Some(rpc_url) = &self.rpc_url {
            check_url(
                &chainz.config.globals.expand_rpc_url(rpc_url),
                chain.chain_id,
            )
            .await
            .with_context(|| {
                format!(
                    "RPC check failed for {}",
                    crate::variables::redact_url(rpc_url)
                )
            })?;
            chain.select_rpc(rpc_url.clone());
        }
        if let Some(key) = &self.key {
            chainz.get_key(key)?;
            chain.key_name = Some(key.clone());
        } else if self.no_key {
            chain.key_name = None;
        }
        if self.clear_verification {
            chain.verification_url = None;
            chain.verification_api_key = None;
        } else {
            if let Some(url) = &self.verification_url {
                chain.verification_url = Some(url.clone());
            }
            if self.verification_api_key.is_some() || self.verification_api_key_stdin {
                chain.verification_api_key = self.read_verification_api_key()?;
            }
        }
        Ok(())
    }

    async fn edit_interactively(
        &self,
        chainz: &mut Chainz,
        chain: &mut ChainDefinition,
    ) -> Result<()> {
        loop {
            println!("{}", ui::header("Update Options"));
            println!("Current configuration:");
            println!("{}", chain);
            let options = [
                "RPC URL",
                "Key",
                "Verification",
                "Rename",
                "Save and finish",
            ];
            match fuzzy_select("What would you like to update?", &options, 0)? {
                0 => {
                    println!("{}", ui::header("RPC Configuration"));
                    let available_rpcs = fetch_chain_by_id(chain.chain_id, self.refresh)
                        .await
                        .map(|entry| entry.rpc)
                        .unwrap_or_else(|_| chain.rpc_urls.clone());
                    let new_rpc = select_rpc(
                        &chain.name,
                        chain.chain_id,
                        available_rpcs.clone(),
                        &chainz.config.globals,
                    )
                    .await?;
                    chain.rpc_urls = available_rpcs;
                    chain.select_rpc(new_rpc);
                }
                1 => {
                    println!("{}", ui::header("Key Configuration"));
                    chain.key_name = select_key(chainz)?;
                }
                2 => {
                    println!("{}", ui::header("Verification Configuration"));
                    let (url, key) = select_verifier()?;
                    chain.verification_url = url;
                    chain.verification_api_key = key;
                }
                3 => {
                    let name: String = Input::new()
                        .with_prompt("Chain name")
                        .default(chain.name.clone())
                        .interact_text()?;
                    if !chain.matches_exact(&name) && chainz.chain_exists(&name) {
                        anyhow::bail!("Chain '{}' already exists", name);
                    }
                    chain
                        .aliases
                        .retain(|alias| !alias.eq_ignore_ascii_case(&name));
                    chain.name = name;
                }
                4 => break,
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    fn read_verification_api_key(&self) -> Result<Option<String>> {
        read_verification_api_key(
            self.verification_api_key_stdin,
            self.verification_api_key.clone(),
        )
    }
}

impl AddArgs {
    pub async fn handle(&self, chainz: &mut Chainz) -> Result<ChainDefinition> {
        self.handle_with_persistence(chainz, true).await
    }

    /// Build an addition in memory for a larger transaction such as `init`.
    pub(crate) async fn handle_staged(&self, chainz: &mut Chainz) -> Result<ChainDefinition> {
        self.handle_with_persistence(chainz, false).await
    }

    async fn handle_with_persistence(
        &self,
        chainz: &mut Chainz,
        persist: bool,
    ) -> Result<ChainDefinition> {
        if self.name.is_some() && self.chain_id.is_some() && self.rpc_url.is_some() {
            self.handle_non_interactive(chainz, persist).await
        } else {
            self.handle_interactive(chainz, persist).await
        }
    }

    async fn handle_non_interactive(
        &self,
        chainz: &mut Chainz,
        persist: bool,
    ) -> Result<ChainDefinition> {
        let name = self.name.clone().unwrap();
        let chain_id = self.chain_id.unwrap();
        let rpc_url = self.rpc_url.clone().unwrap();
        let key_name = match &self.key {
            Some(name) => {
                chainz.get_key(name).map_err(|_| {
                    anyhow::anyhow!(
                        "Key '{}' not found. Add it first with 'chainz key add'.",
                        name
                    )
                })?;
                Some(name.clone())
            }
            None => None,
        };

        // Test the RPC
        check_url(&chainz.config.globals.expand_rpc_url(&rpc_url), chain_id)
            .await
            .with_context(|| {
                format!(
                    "RPC check failed for {}",
                    crate::variables::redact_url(&rpc_url)
                )
            })?;

        let chain_def = ChainDefinition {
            name: name.clone(),
            aliases: vec![],
            chain_id,
            rpc_urls: vec![rpc_url.clone()],
            selected_rpc: rpc_url,
            verification_api_key: self.read_verification_api_key()?,
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
        if persist {
            chainz.save().await?;
        }
        Ok(chain_def)
    }

    async fn handle_interactive(
        &self,
        chainz: &mut Chainz,
        persist: bool,
    ) -> Result<ChainDefinition> {
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

            let selection = fuzzy_select("Type to search and select a chain", &items, 0)?;
            chains[selection].clone()
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
            .with_context(|| {
                format!(
                    "RPC check failed for {}",
                    crate::variables::redact_url(rpc_url)
                )
            })?;
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
            Some(key.clone())
        } else {
            println!("{}", ui::header("Key Configuration"));
            if persist {
                select_key(chainz)?
            } else {
                select_key_staged(chainz)?
            }
        };

        let (verification_url, verification_api_key) = if self.verification_url.is_some()
            || self.verification_api_key.is_some()
            || self.verification_api_key_stdin
        {
            (
                self.verification_url.clone(),
                self.read_verification_api_key()?,
            )
        } else {
            select_verifier()?
        };

        // Create and add the chain
        let mut chain_def = ChainDefinition {
            name,
            aliases,
            chain_id: selected_chain.chain_id,
            rpc_urls: selected_chain.rpc,
            selected_rpc: String::new(),
            verification_api_key,
            verification_url,
            key_name,
        };
        chain_def.select_rpc(selected_rpc);

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
                    return Err(ui::cancelled());
                }
            }
        }

        chainz.add_chain(chain_def.clone())?;
        if persist {
            chainz.save().await?;
        }
        Ok(chain_def)
    }

    fn read_verification_api_key(&self) -> Result<Option<String>> {
        read_verification_api_key(
            self.verification_api_key_stdin,
            self.verification_api_key.clone(),
        )
    }
}

fn read_verification_api_key(stdin: bool, value: Option<String>) -> Result<Option<String>> {
    if stdin {
        use std::io::Read;
        use zeroize::Zeroize;
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input)?;
        let normalized = input.trim().to_string();
        input.zeroize();
        if normalized.is_empty() {
            anyhow::bail!("Verification API key from stdin was empty");
        }
        Ok(Some(normalized))
    } else {
        if value.is_some() {
            eprintln!(
                "Warning: verification API keys in argv may be visible in shell history; prefer --verification-api-key-stdin"
            );
        }
        Ok(value)
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
        None => Err(ui::cancelled()),
    }
}

#[cfg(test)]
mod tests {
    use super::{probe_summary, suggest_short_name};
    use crate::chain::rpc::ProbeResult;
    use std::time::Duration;

    #[test]
    fn short_name_suggestions() {
        assert_eq!(suggest_short_name("Ethereum Mainnet"), "ethereum");
        assert_eq!(suggest_short_name("OP Mainnet"), "op");
        assert_eq!(suggest_short_name("Avalanche C-Chain"), "avalanche");
        assert_eq!(suggest_short_name("zora"), "zora");
    }

    #[test]
    fn probe_summary_collapses_endpoint_results() {
        let results = vec![
            ProbeResult {
                index: 0,
                healthy: true,
                latency: Duration::from_millis(20),
            },
            ProbeResult {
                index: 1,
                healthy: false,
                latency: Duration::from_millis(40),
            },
        ];
        assert_eq!(probe_summary(&results), "1 of 2 RPCs healthy");
    }
}
