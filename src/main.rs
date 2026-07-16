use anyhow::Result;
use clap::Parser;
use dialoguer::FuzzySelect;
use std::process::Command as ProcessCommand;

pub mod chain;
pub mod chainlist;
pub mod config;
pub mod init;
pub mod key;
pub mod opt;
pub mod variables;

use config::Chainz;
use opt::Opt;
use variables::ChainVariables;

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opt::parse();
    let mut chainz = Chainz::load().await?;

    match opts.cmd {
        opt::Command::Init {} => {
            init::handle_init().await?;
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
        opt::Command::List => {
            let chains = chainz.list_chains();
            if chains.is_empty() {
                println!("No chains configured. Run 'chainz init' or 'chainz add' to get started.");
            }
            for chain_def in chains {
                println!("{}", chain_def);
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
            let name_or_id = match name_or_id {
                Some(id) => id,
                None => select_chain(&chainz)?,
            };
            let mut chain = chainz.get_chain(&name_or_id).await?;
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
