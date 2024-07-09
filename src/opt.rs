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
    #[structopt(about = "Set a global config parameter")]
    Set {
        #[structopt(short, long)]
        default_private_key: Option<String>,
        #[structopt(short, long)]
        env_prefix: Option<String>,
    },
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
}

#[derive(Debug, StructOpt)]
pub struct AddArgs {
    #[structopt(short, long)]
    pub name: String,
    #[structopt(short, long)]
    pub rpc_url: String,
    #[structopt(short, long)]
    pub verification_api_key: String,
    #[structopt(short, long)]
    pub private_key: Option<String>,
}
