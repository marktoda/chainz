use anyhow::Result;
use clap::Parser;
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
        opt::Command::List => {
            let chains = chainz.list_chains();
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
            let mut chain = chainz.get_chain(&name_or_id).await?;
            if let Some(key_name) = key {
                chain = chain.with_key(chainz.get_key(&key_name)?);
            }
            let variables = ChainVariables::new(&chain)?;
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
