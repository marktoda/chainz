use crate::{
    chain::DEFAULT_KEY_NAME,
    config::{Chainz, config_exists},
    key::{Key, KeyType, cleanup_external_key, create_safe_key, create_safe_replacement_key},
    opt, ui,
};
use anyhow::Result;
use dialoguer::{Confirm, Input};

const INFURA_API_KEY_ENV_VAR: &str = "INFURA_API_KEY";

pub async fn handle_init() -> Result<()> {
    let replacing = config_exists();
    if replacing {
        let overwrite = Confirm::new()
            .with_prompt("Configuration already exists. Overwrite?")
            .interact()?;
        if !overwrite {
            println!("Aborting initialization");
            return Ok(());
        }
    }

    // Build and validate the replacement before atomically writing it. The
    // existing config remains untouched if the wizard is cancelled or fails.
    let mut chainz = initialize_with_wizard().await?;

    // Key storage can have external side effects (OS keyring). Defer it until
    // every interactive step has succeeded, so cancellation leaves the old
    // config and its credentials untouched.
    let pending: Vec<(String, zeroize::Zeroizing<String>)> = chainz
        .list_keys()
        .into_iter()
        .filter(|(_, key)| matches!(key.kind, KeyType::PrivateKey { .. }))
        .map(|(name, key)| Ok((name.to_string(), key.private_key()?)))
        .collect::<Result<_>>()?;
    let mut provisioned = Vec::new();
    for (name, private_key) in pending {
        let result = if replacing {
            create_safe_replacement_key(&name, &private_key)
        } else {
            create_safe_key(&name, &private_key)
        };
        let key = match result {
            Ok(key) => key,
            Err(error) => {
                if replacing {
                    for key in &provisioned {
                        let _ = cleanup_external_key(key);
                    }
                }
                return Err(error);
            }
        };
        chainz.config.keys.insert(name, key.clone());
        provisioned.push(key);
    }

    if let Err(error) = chainz.save().await {
        if replacing {
            for key in &provisioned {
                let _ = cleanup_external_key(key);
            }
        }
        return Err(error);
    }
    println!("Configuration initialized successfully!");
    Ok(())
}

async fn initialize_with_wizard() -> Result<Chainz> {
    println!("{}", ui::header("Chainz Initialization"));
    let mut chainz = Chainz::new();

    let private_key = rpassword::prompt_password(
        "Enter default private key (optional; leave empty for RPC-only setup): ",
    )?;
    if !private_key.is_empty() {
        Key::validate_private_key(&private_key)?;
        // Keep the validated key only in this in-memory staging config. It is
        // converted to safe storage by handle_init immediately before commit.
        chainz.add_key(
            DEFAULT_KEY_NAME,
            Key::new(
                DEFAULT_KEY_NAME.to_string(),
                KeyType::PrivateKey { value: private_key },
            ),
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
            .with_prompt("Would you like to add another chain?")
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
            verification_api_key_stdin: false,
            force: false,
            refresh: false,
        };

        match args.handle_staged(&mut chainz).await {
            Ok(chain) => println!("Added chain: {}", chain.name),
            Err(e) => println!("Failed to add chain: {}", e),
        }
    }

    Ok(chainz)
}
