use alloy_primitives::utils::format_ether;
use anyhow::Result;
use std::fs::File;
use std::io::prelude::*;
use structopt::StructOpt;

pub mod chainlist;
pub mod config;
pub mod init;
pub mod key;
pub mod opt;
use config::{Chain, ChainzConfig};
use opt::Opt;

pub const DOT_ENV: &str = ".env";

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opt::from_args();
    // load or default to new
    let mut chainz = ChainzConfig::load()
        .await
        .unwrap_or_else(|_| ChainzConfig::default());
    match opts.cmd {
        opt::Command::Init {} => init::handle_init().await?,
        opt::Command::Key { cmd } => key::handle_key_command(chainz, cmd).await?,
        opt::Command::Add { args } => {
            let config = chainz.add_chain(&args).await?;
            println!("Added chain {}", config.name);
            print_chain(&chainz.get_chain(&config).await?).await?;
            chainz.write().await?;
        }
        opt::Command::List => {
            for chain in &chainz.get_chains().await? {
                print_chain(chain).await?;
            }
        }
        opt::Command::Use { name_or_id, print } => {
            // try parse as a u64 id, else use as name
            let chain = match name_or_id.parse::<u64>() {
                Ok(chain_id) => chainz.get_chain_by_id(chain_id).await?,
                Err(_) => chainz.get_chain_by_name(&name_or_id).await?,
            };
            println!("Using chain {}", chain.config.name);
            print_chain(&chain).await?;
            if print {
                println!("{}", get_env_file_str(&chainz, &chain));
            }
            write_env(&chainz, &chain)?;
        }
    }
    Ok(())
}

async fn print_chain(chain: &Chain) -> Result<()> {
    let balance = chain.provider.get_balance(chain.wallet.address()).await?;
    println!(
        "{} (ChainId: {})\nWallet: {} (Balance: {})\n",
        chain.config.name,
        chain.config.chain_id,
        chain.wallet.address(),
        format_ether(balance)
    );
    Ok(())
}

fn get_env_file_str(chainz: &ChainzConfig, chain: &Chain) -> String {
    let mut res = String::new();
    res.push_str(&format!(
        "{}_RPC_URL={}\n",
        chainz.env_prefix, chain.rpc_url
    ));
    res.push_str(&format!(
        "{}_VERIFICATION_API_KEY={:?}\n",
        chainz.env_prefix, chain.config.verification_api_key
    ));
    res.push_str(&format!(
        "{}_PRIVATE_KEY={}\n",
        chainz.env_prefix, chain.private_key
    ));
    res
}

fn write_env(chainz: &ChainzConfig, chain: &Chain) -> Result<()> {
    // write to .env
    let mut file = File::create(DOT_ENV)?;
    file.write_all(get_env_file_str(chainz, chain).as_bytes())?;
    Ok(())
}
