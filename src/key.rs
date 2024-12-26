// module for storing configurations of encrypted private keys

use crate::{config::ChainzConfig, opt::KeyCommand};
use anyhow::Result;

// TODO: encrypt keys
pub async fn handle_key_command(mut chainz: ChainzConfig, cmd: KeyCommand) -> Result<()> {
    match cmd {
        KeyCommand::Add { name, key } => {
            let key = if let Some(k) = key {
                k
            } else {
                rpassword::prompt_password("Enter private key: ")?
            };
            chainz.add_key(&name, &key).await?;
            println!("Added key '{}'", name);
            chainz.write().await?;
        }
        KeyCommand::List => {
            let keys = chainz.list_keys().await?;
            if keys.is_empty() {
                println!("No stored keys");
            } else {
                println!("Stored keys:");
                for name in keys {
                    println!("- {}", name);
                }
            }
        }
        KeyCommand::Remove { name } => {
            chainz.remove_key(&name).await?;
            println!("Removed key '{}'", name);
            chainz.write().await?;
        }
    }
    Ok(())
}
