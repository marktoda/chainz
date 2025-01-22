// module for storing configurations of encrypted private keys
// #![allow(dead_code)]

use crate::{
    chain::DEFAULT_KEY_NAME,
    config::{config_exists, Chainz},
    key::{Key, KeyType},
    opt,
};
use alloy::signers::local::PrivateKeySigner;
use anyhow::Result;
use colored::Colorize;
use dialoguer::{Confirm, Input};

const INFURA_API_KEY_ENV_VAR: &str = "INFURA_API_KEY";

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
    println!("\n{}", "Chainz Initialization".bright_blue().bold());
    println!("{}", "═".bright_black().repeat(50));
    println!("Chainz Init");
    let mut chainz = Chainz::new();

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
    chainz
        .add_key(
            DEFAULT_KEY_NAME,
            Key {
                name: DEFAULT_KEY_NAME.to_string(),
                kind: KeyType::PrivateKey { value: private_key },
            },
        )
        .await?;

    // get infura_api_key, optionally
    let infura_api_key: String = Input::new()
        .with_prompt("Infura API Key (optional)")
        .allow_empty(true)
        .interact_text()?;
    if !infura_api_key.is_empty() {
        chainz
            .config
            .globals
            .add_rpc_expansion(INFURA_API_KEY_ENV_VAR, &infura_api_key);
    }

    // Add chains in a loop until user chooses to exit
    loop {
        println!("\n{}", "Chain Management".bright_blue().bold());
        println!("{}", "═".bright_black().repeat(50));
        let should_add = Confirm::new()
            .with_prompt("Would you like to add another chain?")
            .default(true)
            .interact()?;

        if !should_add {
            break;
        }

        let args = opt::AddArgs {};

        match args.handle(&mut chainz).await {
            Ok(chain) => println!("Added chain: {}", chain.name),
            Err(e) => println!("Failed to add chain: {}", e),
        }
    }

    Ok(chainz)
}
