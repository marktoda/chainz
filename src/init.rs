// module for storing configurations of encrypted private keys
// #![allow(dead_code)]

use crate::{
    chain::DEFAULT_KEY_NAME,
    chainlist::fetch_all_chains,
    config::{config_exists, Chainz, Config},
    key::Key,
    opt,
};
use alloy::signers::local::PrivateKeySigner;
use anyhow::Result;
use dialoguer::{Confirm, Input, MultiSelect};

const INFURA_API_KEY_ENV_VAR: &str = "INFURA_API_KEY";
static DEFAULT_INIT_CHAINS: &[u64] = &[
    1, 56, 8453, 42161, 43114, 137, 130, 1301, 10, 81457, 59144, 100, 167000, 534352, 11155111,
];

pub async fn handle_init() -> Result<()> {
    if config_exists().await? {
        let overwrite = Confirm::new()
            .with_prompt("Configuration already exists. Overwrite?")
            .interact()?;
        if !overwrite {
            println!("Aborting initialization");
            return Ok(());
        }
        Chainz::delete().await?;
    }

    let chainz = initialize_with_wizard().await?;

    chainz.save().await?;
    println!("Configuration initialized successfully!");
    Ok(())
}

async fn initialize_with_wizard() -> Result<Chainz> {
    println!("Chainz Init");
    let mut config = Config::default();

    // Configure environment prefix

    let private_key = {
        let input = rpassword::prompt_password("Enter default private key (Optional): ")?;
        if input.is_empty() {
            let wallet = PrivateKeySigner::random();
            println!("Generated new wallet address: {}", wallet.address());
            format!("{:x}", wallet.credential().to_bytes())
        } else {
            input
        }
    };
    config
        .add_key(DEFAULT_KEY_NAME, Key::PrivateKey(private_key))
        .await?;

    // get infura_api_key, optionally
    let infura_api_key: String = Input::new()
        .with_prompt("Infura API Key (optional)")
        .allow_empty(true)
        .interact_text()?;
    if !infura_api_key.is_empty() {
        config
            .variables
            .insert(INFURA_API_KEY_ENV_VAR.to_string(), infura_api_key);
    }

    let mut chainz = Chainz::new(config);

    // Select chains to add
    // TODO: fzf?
    let available_chains = fetch_all_chains()
        .await?
        .into_iter()
        .map(|c| (c.name, c.chain_id))
        .filter(|(_, id)| DEFAULT_INIT_CHAINS.contains(id))
        .collect::<Vec<_>>();

    let selections = MultiSelect::new()
        .with_prompt("Select chains to configure")
        .items(
            &available_chains
                .iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
        )
        .interact()?;

    for &idx in selections.iter() {
        let (name, chain_id) = &available_chains[idx];
        let args = opt::AddArgs {
            name: Some(name.to_lowercase().replace(" ", "_")),
            chain_id: Some(*chain_id),
            rpc_url: None,
            verification_api_key: None,
            // TODO: allow key override
            key_name: None,
        };
        match chainz.add_chain(&args).await {
            Ok(_) => println!("Added {}", name),
            Err(e) => println!("Failed to add {}: {}", name, e),
        }
    }

    Ok(chainz)
}
