// module for storing configurations of encrypted private keys

use crate::{config::Chainz, opt::KeyCommand};
use alloy::{
    primitives::Address,
    signers::{local::PrivateKeySigner, Signer},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fmt::Display;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", content = "value")]
pub enum Key {
    #[serde(rename = "PrivateKey")]
    PrivateKey(String),
}

impl Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Key::PrivateKey(key) => write!(f, "{}", key),
        }
    }
}

impl Key {
    pub fn private_key(&self) -> String {
        match self {
            Key::PrivateKey(key) => key.clone(),
        }
    }

    pub fn signer(&self) -> Result<Box<dyn Signer>> {
        Ok(Box::new(self.private_key().parse::<PrivateKeySigner>()?))
    }

    pub fn address(&self) -> Result<Address> {
        Ok(self.signer()?.address())
    }
}

// TODO: encrypt keys
impl KeyCommand {
    pub async fn handle(self, config: &mut Chainz) -> Result<()> {
        match self {
            KeyCommand::Add { name, key } => {
                let key = if let Some(k) = key {
                    k
                } else {
                    rpassword::prompt_password("Enter private key: ")?
                };
                config.add_key(&name, Key::PrivateKey(key)).await?;
                println!("Added key '{}'", name);
                config.save().await?;
            }
            KeyCommand::List => {
                let keys = config.list_keys()?;
                if keys.is_empty() {
                    println!("No stored keys");
                } else {
                    println!("Stored keys:");
                    for (name, key) in keys {
                        println!("- {}: {}", name, key.address().unwrap_or_default());
                    }
                }
            }
            KeyCommand::Remove { name } => {
                config.remove_key(&name)?;
                println!("Removed key '{}'", name);
                config.save().await?;
            }
        }
        Ok(())
    }
}
