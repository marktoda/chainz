use crate::{
    chain::DEFAULT_KEY_NAME,
    config::{Chainz, config_exists},
    key::{Key, KeyType, create_default_key},
    opt, ui,
};
use anyhow::Result;
use dialoguer::{Confirm, Input};

const INFURA_API_KEY_ENV_VAR: &str = "INFURA_API_KEY";

pub async fn handle_init() -> Result<()> {
    if config_exists() {
        let overwrite = Confirm::new()
            .with_prompt("Configuration already exists. Overwrite?")
            .interact()?;
        if !overwrite {
            println!("Aborting initialization");
            return Ok(());
        }
    }

    let (mut chainz, private_key) = initialize_with_wizard().await?;
    if let Some(private_key) = private_key {
        let secure_key = create_default_key(DEFAULT_KEY_NAME, private_key)?;
        chainz
            .config
            .keys
            .insert(DEFAULT_KEY_NAME.to_string(), secure_key);
    }

    chainz.save().await?;
    println!("Configuration initialized successfully!");
    Ok(())
}

async fn initialize_with_wizard() -> Result<(Chainz, Option<String>)> {
    println!("{}", ui::header("Chainz Initialization"));
    let mut chainz = Chainz::new();

    let input = rpassword::prompt_password(
        "Default private key (optional; leave empty for RPC-only setup): ",
    )?;
    let private_key = (!input.is_empty()).then_some(input);
    // Keep the key only in process memory while the wizard runs. It is
    // converted to the selected safe backend immediately before the single,
    // atomic config save in `handle_init`.
    if let Some(private_key) = &private_key {
        chainz.add_key(
            DEFAULT_KEY_NAME,
            Key {
                name: DEFAULT_KEY_NAME.to_string(),
                kind: KeyType::PrivateKey {
                    value: private_key.clone(),
                },
            },
        )?;
    }

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
        println!("{}", ui::header("Chain Management"));
        let should_add = Confirm::new()
            .with_prompt(if chainz.config.chains.is_empty() {
                "Would you like to add a chain?"
            } else {
                "Would you like to add another chain?"
            })
            .default(true)
            .interact()?;

        if !should_add {
            break;
        }

        let args = opt::AddArgs {
            name: None,
            chain_id: None,
            rpc_url: None,
            key: None,
            verification_url: None,
            verification_api_key: None,
            force: false,
            refresh: false,
        };

        match args.handle_in_memory(&mut chainz).await {
            Ok(chain) => println!("Added chain: {}", chain.name),
            Err(e) => println!("Failed to add chain: {}", e),
        }
    }

    Ok((chainz, private_key))
}
