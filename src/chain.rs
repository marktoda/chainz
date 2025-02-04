use crate::{
    chainlist::{fetch_all_chains, fetch_chain_data, ChainlistEntry},
    config::Chainz,
    key::{Key, KeyType},
    opt::{AddArgs, UpdateArgs},
    variables::GlobalVariables,
};
use alloy::{
    providers::{Provider, ProviderBuilder},
    transports::BoxTransport,
};
use anyhow::Result;
use colored::*;
use dialoguer::{FuzzySelect, Input};
use serde::{Deserialize, Serialize};
use std::fmt::{Debug, Display};
use std::sync::Arc;

pub const DEFAULT_KEY_NAME: &str = "default";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainDefinition {
    pub name: String,
    pub chain_id: u64,
    pub rpc_urls: Vec<String>,
    pub selected_rpc: String,
    pub verification_api_key: Option<String>,
    pub verification_url: Option<String>,
    pub key_name: String,
}

#[derive(Clone)]
pub struct ChainInstance {
    pub definition: ChainDefinition,
    pub provider: Arc<dyn Provider<BoxTransport>>,
    pub rpc_url: String,
    pub key: Key,
}

impl ChainInstance {
    pub fn new(definition: ChainDefinition, provider: Box<dyn Provider<BoxTransport>>, rpc_url: String, key: Key) -> Self {
        Self {
            definition,
            provider: Arc::from(provider),
            rpc_url,
            key,
        }
    }

    pub fn with_key(mut self, key: Key) -> Self {
        self.key = key;
        self
    }
}

pub struct Rpc {
    pub rpc_url: String,
    pub provider: Box<dyn Provider<BoxTransport>>,
}

impl ChainDefinition {
    pub async fn get_rpc(&self, globals: &GlobalVariables) -> Result<Rpc> {
        resolve_rpc(&self.selected_rpc, globals).await
    }
}

impl Display for Rpc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.rpc_url)
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
            self.selected_rpc.bright_green()
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            "├".bright_black(),
            "Verification URL".bright_blue(),
            self.verification_url
                .as_deref()
                .map(|k| k.bright_green().to_string())
                .unwrap_or_else(|| "None".bright_red().to_string())
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

/// Helper function to manually enter chain details
// Helper function to handle text input with ESC cancellation
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
    println!("\n{}", "Manual Chain Entry".bright_yellow().bold());
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

/// Helper function to select or enter RPC URL
pub async fn select_rpc(
    chain_name: &str,
    chain_id: u64,
    available_rpcs: Vec<Rpc>,
) -> Result<String> {
    println!("\nTesting RPCs...");

    // Initialize displays with "testing" status
    let mut rpc_displays: Vec<String> = available_rpcs
        .iter()
        .map(|rpc| format!("{} {}", "⋯".bright_yellow(), rpc))
        .collect();

    // Create a vector of futures for testing RPCs
    let mut test_futures = Vec::new();
    for (idx, rpc) in available_rpcs.iter().enumerate() {
        // Clone the necessary data for the spawned task
        let rpc_to_test = Rpc {
            rpc_url: rpc.rpc_url.clone(),
            provider: create_provider(&rpc.rpc_url).await?,
        };

        let test_future = async move {
            let result = test_rpc(&rpc_to_test, chain_id).await;
            (idx, result)
        };
        test_futures.push(tokio::spawn(test_future));
    }

    // Process results as they complete
    for (idx, result) in (futures::future::join_all(test_futures).await)
        .into_iter()
        .flatten()
    {
        if result.is_ok() {
            rpc_displays[idx] = format!("{} {}", "✓".bright_green(), available_rpcs[idx]);
        } else {
            rpc_displays[idx] = format!("{} {}", "✗".bright_red(), available_rpcs[idx]);
        }
    }

    // Add manual entry option
    rpc_displays.push("Enter RPC URL manually...".to_string());

    let rpc_selection = fuzzy_select(
        &format!("Select an RPC URL for {}", chain_name.yellow()),
        &rpc_displays,
        0,
    )?;

    if rpc_selection == rpc_displays.len() - 1 {
        Ok(select_manual_rpc(chain_id).await?.rpc_url)
    } else if let Some(rpc) = available_rpcs.get(rpc_selection) {
        Ok(rpc.rpc_url.clone())
    } else {
        anyhow::bail!("Selected RPC is not working")
    }
}

async fn select_manual_rpc(chain_id: u64) -> Result<Rpc> {
    loop {
        let rpc_url: String = text_input("Enter RPC URL", None)?;
        println!("Testing RPC...");
        let rpc = resolve_rpc(&rpc_url, &GlobalVariables::default()).await?;

        if test_rpc(&rpc, chain_id).await.is_ok() {
            println!("{} RPC working", "✓".bright_green());
            return Ok(rpc);
        }

        println!("{} RPC failed. Try again? (ESC to exit)", "✗".bright_red());
    }
}

/// Helper function to select or create a key
pub async fn select_key(chainz: &mut Chainz) -> Result<String> {
    let keys = chainz.list_keys();

    // Create display strings with addresses
    let mut key_displays: Vec<(String, String)> = keys
        .iter()
        .map(|(name, key)| (name.clone(), key.to_string()))
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
        let private_key: String = Input::new()
            .with_prompt("Enter private key")
            .interact_text()?;
        chainz
            .add_key(
                &kname,
                Key {
                    name: kname.clone(),
                    kind: KeyType::PrivateKey { value: private_key },
                },
            )
            .await?;
        Ok(kname)
    } else {
        Ok(key_displays[key_selection].0.clone())
    }
}

/// Helper function to select or create a verifier
pub fn select_verifier() -> Result<(Option<String>, Option<String>)> {
    // TODO: try to autogenerate best guess etherscan
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
        println!("\n{}", "Chain Update".bright_blue().bold());
        println!("{}", "═".bright_black().repeat(50));

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

        println!("\n{}", "Update Options".bright_blue().bold());
        println!("{}", "═".bright_black().repeat(50));
        println!("Current configuration:");
        println!("{}", chain);

        let selection = fuzzy_select("What would you like to update?", &options, 0)?;

        match selection {
            0 => {
                // Update RPC URL
                println!("\n{}", "RPC Configuration".bright_blue().bold());
                println!("{}", "═".bright_black().repeat(50));

                // Try to get fresh RPC list from chainlist
                let chainlist_entry = fetch_chain_data(Some(chain.chain_id), None).await;
                let available_rpcs = chainlist_entry
                    .map(|c| c.rpc)
                    .unwrap_or_else(|_| chain.rpc_urls.clone());

                let new_rpc = select_rpc(
                    &chain.name,
                    chain.chain_id,
                    resolve_rpcs(available_rpcs, &chainz.config.globals).await?,
                )
                .await?;
                chain.selected_rpc = new_rpc;
            }
            1 => {
                // Update key
                println!("\n{}", "Key Configuration".bright_blue().bold());
                println!("{}", "═".bright_black().repeat(50));

                let new_key = select_key(chainz).await?;
                chain.key_name = new_key;
            }
            2 => {
                // Update verification API key
                println!("\n{}", "Verification Configuration".bright_blue().bold());
                println!("{}", "═".bright_black().repeat(50));

                let (verification_url, verification_key) = select_verifier()?;
                chain.verification_url = verification_url;
                chain.verification_api_key = verification_key;
            }
            _ => unreachable!(),
        }

        // Save changes
        chainz.add_chain(chain.clone()).await?;
        chainz.save().await?;
        println!("\n{}", "Chain updated successfully".bright_green());

        Ok(chain)
    }
}

impl AddArgs {
    pub async fn handle(&self, chainz: &mut Chainz) -> Result<ChainDefinition> {
        println!("\n{}", "Chain Selection".bright_blue().bold());
        println!("{}", "═".bright_black().repeat(50));

        // Interactive flow with chainlist
        let chains = fetch_all_chains().await?;
        let items: Vec<String> = chains
            .iter()
            .map(|c| format!("{} ({})", c.name, c.chain_id))
            .collect();

        let selected_chain = match fuzzy_select("Type to search and select a chain", &items, 0) {
            Ok(selection) => chains[selection].clone(),
            Err(_) => manual_chain_entry(None, None).await?,
        };

        println!("\n{}", "RPC Configuration".bright_blue().bold());
        println!("{}", "═".bright_black().repeat(50));

        let selected_rpc = select_rpc(
            &selected_chain.name,
            selected_chain.chain_id,
            resolve_rpcs(selected_chain.rpc.clone(), &chainz.config.globals).await?,
        )
        .await?;

        println!("\n{}", "Key Configuration".bright_blue().bold());
        println!("{}", "═".bright_black().repeat(50));

        let key_name = select_key(chainz).await?;

        let (verification_url, verification_api_key) = select_verifier()?;

        // Create and add the chain
        let chain_def = ChainDefinition {
            name: selected_chain.name.clone(),
            chain_id: selected_chain.chain_id,
            rpc_urls: selected_chain.rpc,
            selected_rpc,
            verification_api_key,
            verification_url,
            key_name,
        };
        chainz.add_chain(chain_def.clone()).await?;
        chainz.save().await?;
        Ok(chain_def)
    }
}

async fn test_rpc(rpc: &Rpc, expected_chain_id: u64) -> Result<()> {
    // Try the resolved RPC URL
    if let Ok(chain_id) = rpc.provider.get_chain_id().await {
        if chain_id == expected_chain_id {
            return Ok(()); // Return original URL with variables
        }
    }
    anyhow::bail!("Invalid chain ID");
}

pub async fn resolve_rpcs(rpc_urls: Vec<String>, globals: &GlobalVariables) -> Result<Vec<Rpc>> {
    let mut result = Vec::new();
    for rpc in rpc_urls {
        if let Ok(r) = resolve_rpc(&rpc, globals).await {
            result.push(r);
        }
    }
    Ok(result)
}

pub async fn resolve_rpc(rpc_url: &str, globals: &GlobalVariables) -> Result<Rpc> {
    let rpc_url = globals.expand_rpc_url(rpc_url);
    Ok(Rpc {
        rpc_url: rpc_url.clone(),
        provider: create_provider(&rpc_url).await?,
    })
}

async fn create_provider(rpc_url: &str) -> Result<Box<dyn Provider<BoxTransport>>> {
    Ok(Box::new(
        ProviderBuilder::new()
            .with_recommended_fillers()
            .on_builtin(rpc_url)
            .await?,
    ))
}

// Helper function to handle fuzzy select with ESC cancellation
fn fuzzy_select<T: ToString>(prompt: &str, items: &[T], default: usize) -> Result<usize> {
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
