use alloy_primitives::utils::format_ether;
use anyhow::Result;
use std::fs::File;
use std::io::prelude::*;
use structopt::StructOpt;

pub mod config;
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
        .unwrap_or_else(|_| ChainzConfig::new());
    match opts.cmd {
        opt::Command::Add { args } => {
            let chain = chainz.add_chain(&args).await?;
            print_chain(&chain).await?;
            chainz.write().await?;
        }
        opt::Command::List => {
            for chain in &chainz.get_chains().await? {
                print_chain(chain).await?;
            }
        }
        opt::Command::Set {
            default_private_key,
            env_prefix,
        } => {
            if let Some(env_prefix) = env_prefix {
                chainz.set_default_env_prefix(env_prefix);
            }
            if let Some(default_private_key) = default_private_key {
                chainz.set_default_private_key(default_private_key);
            }
            chainz.write().await?;
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
        chainz.env_prefix, chain.config.rpc_url
    ));
    res.push_str(&format!(
        "{}_VERIFICATION_API_KEY={}\n",
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
