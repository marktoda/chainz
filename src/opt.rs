use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "chainz",
    about = "CLI tool for managing EVM chain configurations"
)]
pub struct Opt {
    #[structopt(subcommand)]
    pub cmd: Command,
}

#[derive(Debug, StructOpt)]
#[structopt(about = "Subcommands for chainz")]
pub enum Command {
    /// Initialize a new configuration with wizard
    Init {},
    #[structopt(about = "Add a new chain")]
    Add {
        #[structopt(flatten)]
        args: AddArgs,
    },
    #[structopt(
        about = "Use a chain by name or chainid. Writes to a local .env which can be sourced"
    )]
    Use {
        name_or_id: String,
        // default to false
        #[structopt(short, long)]
        print: bool,
    },
    #[structopt(about = "List all chains")]
    List,
    #[structopt(about = "Manage Private Keys")]
    Key {
        #[structopt(subcommand)]
        cmd: KeyCommand,
    },
}

#[derive(Debug, StructOpt)]
pub enum KeyCommand {
    /// Add a new private key
    Add {
        /// Name for the private key
        name: String,
        /// The private key (will prompt if not provided)
        #[structopt(long)]
        key: Option<String>,
    },
    /// List all stored private keys
    List,
    /// Remove a private key
    Remove {
        /// Name of the private key to remove
        name: String,
    },
}

#[derive(Debug, StructOpt)]
pub struct AddArgs {
    #[structopt(short, long)]
    pub name: Option<String>,
    #[structopt(short, long)]
    pub chain_id: Option<u64>,
    #[structopt(short, long)]
    pub rpc_url: Option<String>,
    #[structopt(short, long)]
    pub verification_api_key: Option<String>,
    #[structopt(short, long)]
    pub key_name: Option<String>,
}
