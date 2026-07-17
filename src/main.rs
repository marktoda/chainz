use anyhow::Result;
use chainz::{
    chain::ChainDefinition, config::Chainz, doctor, init, listing, opt, opt::Opt, ui,
    variables::ChainVariables,
};
use clap::{CommandFactory, Parser};
use dialoguer::FuzzySelect;
use std::process::Command as ProcessCommand;

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opt::parse();

    // These commands run before the config is loaded: completions needs no
    // config, and init must be able to recover from a corrupt config
    // (which Chainz::load rejects) by recreating it.
    match opts.cmd {
        opt::Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Opt::command(), "chainz", &mut std::io::stdout());
            return Ok(());
        }
        opt::Command::Init {} => return init::handle_init().await,
        _ => {}
    }

    let mut chainz = Chainz::load().await?;

    match opts.cmd {
        opt::Command::Init {} | opt::Command::Completions { .. } => {
            unreachable!("handled above")
        }
        opt::Command::Key { cmd } => {
            cmd.handle(&mut chainz).await?;
        }
        opt::Command::Var { cmd } => {
            cmd.handle(&mut chainz).await?;
        }
        opt::Command::Add { args } => {
            let chain = args.handle(&mut chainz).await?;
            println!("Added chain {}", chain.name);
        }
        opt::Command::Update { args } => {
            let chain = args.handle(&mut chainz).await?;
            println!("\nFinal configuration:");
            println!("{}", chain);
        }
        opt::Command::Remove { name_or_id } => {
            let removed = chainz.remove_chain(&name_or_id)?;
            chainz.save().await?;
            println!("Removed chain '{}'", removed.name);
        }
        opt::Command::Use { name_or_id } => {
            let target = match name_or_id {
                Some(id) => id,
                None => select_chain(&chainz)?,
            };
            let definition = chainz.config.get_chain(&target)?;
            chainz.config.default_chain = Some(definition.name.clone());
            chainz.save().await?;
            println!("Default chain set to '{}'", definition.name);
        }
        opt::Command::List { json, verbose } => {
            let chains = chainz.list_chains();
            if json {
                let entries: Vec<_> = chains
                    .iter()
                    .map(|chain| ChainListing::new(chain, &chainz.config.default_chain))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if verbose {
                print!(
                    "{}",
                    listing::verbose(chains, chainz.config.default_chain.as_deref())
                );
            } else {
                print!(
                    "{}",
                    listing::compact(chains, chainz.config.default_chain.as_deref())
                );
            }
        }
        opt::Command::Show { name_or_id, json } => {
            let chain = chainz.config.get_chain(&name_or_id)?;
            let is_default = chainz.config.default_chain.as_deref() == Some(chain.name.as_str());
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&ChainListing::new(
                        &chain,
                        &chainz.config.default_chain
                    ))?
                );
            } else {
                println!("{}", chain);
                println!("Default: {}", if is_default { "Yes" } else { "No" });
            }
        }
        opt::Command::Doctor { fix } => {
            let report = doctor::run(&mut chainz, fix).await?;
            if report.failures > 0 {
                std::process::exit(1);
            }
        }
        opt::Command::Shell { name_or_id } => {
            let name_or_id = match name_or_id.or_else(|| chainz.config.default_chain.clone()) {
                Some(id) => id,
                None => select_chain(&chainz)?,
            };
            let chain = chainz.get_chain(&name_or_id)?;
            // Empty command args → lazy rule: key backends are never touched
            let variables = ChainVariables::new(&chain, &[])?;
            let chain_name = chain.definition.name.clone();
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());

            eprintln!(
                "{}",
                ui::item(&format!("entering {} shell — ctrl-d to exit", chain_name))
            );
            let ps1 = format!(
                "(⛓ {}) {}",
                chain_name,
                std::env::var("PS1").unwrap_or_default()
            );
            let status = ProcessCommand::new(&shell)
                .envs(variables.as_map())
                .env("CHAINZ_CHAIN", &chain_name)
                .env("PS1", ps1)
                .status()?;
            eprintln!("{}", ui::dim(&format!("left {} shell", chain_name)));
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
        opt::Command::Exec {
            name_or_id,
            command,
            key,
        } => {
            if command.is_empty() {
                anyhow::bail!("No command specified");
            }
            // Explicit chain > configured default > interactive picker
            let name_or_id = match name_or_id.or_else(|| chainz.config.default_chain.clone()) {
                Some(id) => id,
                None => select_chain(&chainz)?,
            };
            let mut chain = chainz.get_chain(&name_or_id)?;
            if let Some(key_name) = key {
                chain = chain.with_key(chainz.get_key(&key_name)?);
            }
            let variables = ChainVariables::new(&chain, &command)?;
            let expanded_command = variables.expand(command);

            let status = ProcessCommand::new(&expanded_command[0])
                .args(&expanded_command[1..])
                .envs(variables.as_map())
                .status()?;

            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
    }
    Ok(())
}

/// The `list --json` scripting contract: a stable shape decoupled from the
/// storage schema, which deliberately never includes credentials
/// (`verification_api_key` stays out).
#[derive(serde::Serialize)]
struct ChainListing<'a> {
    name: &'a str,
    aliases: &'a [String],
    chain_id: u64,
    selected_rpc: &'a str,
    rpc_urls: &'a [String],
    key_name: Option<&'a str>,
    verification_url: Option<&'a str>,
    is_default: bool,
}

impl<'a> ChainListing<'a> {
    fn new(chain: &'a ChainDefinition, default: &Option<String>) -> Self {
        Self {
            name: &chain.name,
            aliases: &chain.aliases,
            chain_id: chain.chain_id,
            selected_rpc: &chain.selected_rpc,
            rpc_urls: &chain.rpc_urls,
            key_name: chain.key_name.as_deref(),
            verification_url: chain.verification_url.as_deref(),
            is_default: default.as_deref() == Some(chain.name.as_str()),
        }
    }
}

fn select_chain(chainz: &Chainz) -> Result<String> {
    let chains = chainz.list_chains();
    if chains.is_empty() {
        anyhow::bail!("No chains configured. Use 'chainz add' to add a chain first.");
    }
    let items: Vec<String> = chains
        .iter()
        .map(|c| format!("{} ({})", c.name, c.chain_id))
        .collect();
    let selection = FuzzySelect::new()
        .with_prompt("Select a chain")
        .items(&items)
        .default(0)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("No chain selected"))?;
    Ok(chains[selection].name.clone())
}
