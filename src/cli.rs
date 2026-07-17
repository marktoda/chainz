//! Top-level command dispatch for the `chainz` binary.
//!
//! Keeping dispatch inside the library leaves the binary as a tiny runtime
//! adapter and lets the command implementation modules remain private.

use crate::{
    config::Chainz,
    doctor, init, listing,
    listing::SecretVisibility,
    opt,
    opt::Opt,
    prompt::{Prompt, SystemPrompt},
    ui,
    variables::ChainVariables,
};
use anyhow::Result;
use clap::{CommandFactory, Parser};
use std::process::Command as ProcessCommand;

pub async fn run_cli() {
    if let Err(error) = dispatch().await {
        if ui::is_cancelled(&error) {
            eprintln!("Cancelled");
            return;
        }
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

async fn dispatch() -> Result<()> {
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
        opt::Command::Doctor { fix } => {
            let mut chainz = Chainz::load_for_doctor().await?;
            if !fix {
                chainz.release_config_lock();
            }
            let report = doctor::run(&mut chainz, fix).await?;
            if report.failures > 0 {
                std::process::exit(1);
            }
            return Ok(());
        }
        _ => {}
    }

    let mut chainz = Chainz::load().await?;

    match opts.cmd {
        opt::Command::Init {} | opt::Command::Completions { .. } | opt::Command::Doctor { .. } => {
            unreachable!("handled above")
        }
        opt::Command::Key { cmd } => cmd.handle(&mut chainz).await?,
        opt::Command::Var { cmd } => cmd.handle(&mut chainz).await?,
        opt::Command::Add { args } => {
            let chain = args.handle(&mut chainz).await?;
            println!("Added chain {}", chain.name);
        }
        opt::Command::Update { args } => {
            args.handle(&mut chainz).await?;
        }
        opt::Command::Remove { name_or_id } => {
            let removed = chainz.remove_chain_exact(&name_or_id)?;
            chainz.save().await?;
            println!("Removed chain '{}'", removed.name);
        }
        opt::Command::Use { name_or_id } => {
            let target = match name_or_id {
                Some(id) => id,
                None => select_chain(&chainz)?,
            };
            let name = chainz.set_default_chain(&target)?;
            chainz.save().await?;
            println!("Default chain set to '{}'", name);
        }
        opt::Command::List {
            json,
            show_secrets,
            verbose,
        } => {
            let chains = chainz.list_chains();
            let visibility = SecretVisibility::from(show_secrets);
            if json {
                println!(
                    "{}",
                    listing::json(chains, chainz.config.default_chain.as_deref(), visibility)?
                );
            } else if show_secrets || verbose {
                print!(
                    "{}",
                    listing::verbose(chains, chainz.config.default_chain.as_deref(), visibility)
                );
            } else {
                print!(
                    "{}",
                    listing::compact(chains, chainz.config.default_chain.as_deref())
                );
            }
        }
        opt::Command::Show {
            name_or_id,
            json,
            show_secrets,
        } => {
            let chain = chainz.config.get_chain(&name_or_id)?;
            let visibility = SecretVisibility::from(show_secrets);
            if json {
                println!(
                    "{}",
                    listing::show_json(chain, chainz.config.default_chain.as_deref(), visibility)?
                );
            } else {
                print!(
                    "{}",
                    listing::show(chain, chainz.config.default_chain.as_deref(), visibility)
                );
            }
        }
        opt::Command::Shell { name_or_id } => {
            let name_or_id = match name_or_id.or_else(|| chainz.config.default_chain.clone()) {
                Some(id) => id,
                None => select_chain(&chainz)?,
            };
            let chain = chainz.get_chain(&name_or_id)?;
            // Empty command args → lazy rule: key backends are never touched.
            let variables = ChainVariables::new(&chain, &[], false)?;
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
            chainz.release_config_lock();
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
            expose_key,
        } => {
            // Explicit chain > configured default > interactive picker.
            let name_or_id = match name_or_id.or_else(|| chainz.config.default_chain.clone()) {
                Some(id) => id,
                None => select_chain(&chainz)?,
            };
            let mut chain = chainz.get_chain(&name_or_id)?;
            if let Some(key_name) = key {
                chain = chain.with_key(chainz.get_key(&key_name)?);
            }
            let variables = ChainVariables::new(&chain, &command, expose_key)?;
            let expanded_command = variables.expand(command);

            chainz.release_config_lock();
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

fn select_chain(chainz: &Chainz) -> Result<String> {
    let chains = chainz.list_chains();
    if chains.is_empty() {
        anyhow::bail!("No chains configured. Use 'chainz add' to add a chain first.");
    }
    let items: Vec<String> = chains
        .iter()
        .map(|chain| format!("{} ({})", chain.name, chain.chain_id))
        .collect();
    let selection = SystemPrompt.select("Select a chain", &items, 0)?;
    Ok(chains[selection].name.clone())
}
