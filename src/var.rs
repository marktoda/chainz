// module for storing configurations of encrypted private keys

use crate::{config::Chainz, opt::VarCommand};
use anyhow::Result;

impl VarCommand {
    pub async fn handle(self, chainz: &mut Chainz) -> Result<()> {
        match self {
            VarCommand::Set { name, value } => {
                chainz.set_variable(&name, &value);
                chainz.save().await?;
                println!("Set variable {} = {}", name, value);
            }
            VarCommand::Get { name } => match chainz.get_variable(&name) {
                Some(value) => println!("{} = {}", name, value),
                None => println!("Variable '{}' not found", name),
            },
            VarCommand::List => {
                let vars = chainz.list_variables();
                if vars.is_empty() {
                    println!("No variables set");
                } else {
                    println!("Variables:");
                    for (name, value) in vars {
                        println!("  {} = {}", name, value);
                    }
                }
            }
            VarCommand::Rm { name } => {
                chainz.remove_variable(&name)?;
                chainz.save().await?;
                println!("Removed variable '{}'", name);
            }
        }
        Ok(())
    }
}
