use crate::{
    chain::DEFAULT_KEY_NAME,
    config::{Chainz, config_exists},
    key::{
        Key, KeyType, provision_safe_key, provision_safe_replacement_key, rollback_key_provision,
    },
    opt,
    prompt::{Prompt, SystemPrompt},
    ui,
};
use anyhow::Result;

const INFURA_API_KEY_ENV_VAR: &str = "INFURA_API_KEY";

pub async fn handle_init() -> Result<()> {
    handle_init_with(&mut SystemPrompt).await
}

async fn handle_init_with(prompt: &mut impl Prompt) -> Result<()> {
    let replacing = config_exists();
    if replacing {
        let overwrite = prompt.confirm("Configuration already exists. Overwrite?", false)?;
        if !overwrite {
            println!("Aborting initialization");
            return Ok(());
        }
    }

    // Build and validate the replacement before atomically writing it. The
    // existing config remains untouched if the wizard is cancelled or fails.
    let mut chainz = initialize_with_wizard(prompt).await?;

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
            provision_safe_replacement_key(&name, &private_key)
        } else {
            provision_safe_key(&name, &private_key)
        };
        let provision = match result {
            Ok(provision) => provision,
            Err(error) => {
                for provision in &provisioned {
                    let _ = rollback_key_provision(provision);
                }
                return Err(error);
            }
        };
        chainz.config.keys.insert(name, provision.key().clone());
        provisioned.push(provision);
    }

    if let Err(error) = chainz.save().await {
        for provision in &provisioned {
            let _ = rollback_key_provision(provision);
        }
        return Err(error);
    }
    println!("Configuration initialized successfully!");
    Ok(())
}

async fn initialize_with_wizard(prompt: &mut impl Prompt) -> Result<Chainz> {
    println!("{}", ui::header("Chainz Initialization"));
    let mut chainz = Chainz::new();

    let private_key =
        prompt.secret("Enter default private key (optional; leave empty for RPC-only setup): ")?;
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
    let infura_api_key = prompt.text("Infura API Key (optional)", None, true)?;
    if !infura_api_key.is_empty() {
        chainz
            .config
            .globals
            .add_rpc_expansion(INFURA_API_KEY_ENV_VAR, &infura_api_key);
    }

    // Add chains in a loop until user chooses to exit
    loop {
        println!("{}", ui::header("Chain Management"));
        let should_add = prompt.confirm("Would you like to add another chain?", true)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::testing::{Answer, ScriptedPrompt};

    const TEST_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";

    #[tokio::test]
    async fn wizard_supports_rpc_only_initialization() -> Result<()> {
        let mut prompt = ScriptedPrompt::new([
            Answer::Secret(String::new()),
            Answer::Text("infura-token".into()),
            Answer::Confirm(false),
        ]);
        let chainz = initialize_with_wizard(&mut prompt).await?;
        assert!(chainz.config.keys.is_empty());
        assert_eq!(
            chainz
                .config
                .globals
                .get_rpc_expansion(INFURA_API_KEY_ENV_VAR),
            Some("infura-token".to_string())
        );
        Ok(())
    }

    #[tokio::test]
    async fn wizard_stages_a_valid_default_key_without_external_io() -> Result<()> {
        let mut prompt = ScriptedPrompt::new([
            Answer::Secret(TEST_KEY.into()),
            Answer::Text(String::new()),
            Answer::Confirm(false),
        ]);
        let chainz = initialize_with_wizard(&mut prompt).await?;
        assert!(matches!(
            chainz.config.keys[DEFAULT_KEY_NAME].kind,
            KeyType::PrivateKey { .. }
        ));
        Ok(())
    }
}
