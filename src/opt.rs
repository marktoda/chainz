use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "chainz",
    about = "CLI tool for managing EVM chain configurations"
)]
pub struct Opt {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize a new configuration through an interactive wizard
    ///
    /// Guides you through setting up your first chain and private key.
    /// Creates a new configuration file if none exists.
    Init {},

    /// Add a new chain configuration
    ///
    /// Supports both interactive and command-line configuration.
    /// If options are omitted, will prompt for values.
    ///
    /// Example: chainz add --name ethereum --chain-id 1 --rpc-url https://eth.llamarpc.com
    Add {
        #[command(flatten)]
        args: AddArgs,
    },

    /// Update an existing chain's configuration
    Update {
        #[command(flatten)]
        args: UpdateArgs,
    },

    /// List all configured chains
    ///
    /// Displays all chains with their details:
    /// - Name
    /// - Chain ID
    /// - RPC URL
    /// - Associated private key name
    List,

    /// Execute a command with chain-specific variables expanded
    ///
    /// Available expansions:
    ///     @wallet : The wallet address
    ///     @rpc    : RPC URL
    ///     @chainid  : Chain ID
    ///     @chainname  : Chain name
    ///     @key    : Private key
    ///
    /// Example: chainz exec ethereum -- cast balance @wallet
    Exec {
        /// Chain name or ID to use (interactive picker if omitted)
        name_or_id: Option<String>,
        /// Command to execute (after --)
        #[arg(last = true)]
        command: Vec<String>,
        /// Override the key to use for this command
        #[arg(short, long)]
        key: Option<String>,
    },

    /// Manage private keys
    ///
    /// Subcommands for adding, listing, and removing private keys.
    /// Keys are stored encrypted in the configuration.
    ///
    /// Example: chainz key add mykey
    Key {
        #[command(subcommand)]
        cmd: KeyCommand,
    },

    /// Manage global variables
    ///
    /// Variables can be used for dynamically creating RPC urls, setting environment variables, or
    /// shell expansions
    ///
    /// Subcommands:
    ///     set   : Set or update a variable
    ///     get   : Get a variable's value
    ///     list  : List all variables
    ///     rm    : Remove a variable
    Var {
        #[command(subcommand)]
        cmd: VarCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum KeyCommand {
    /// Add a new private key
    Add {
        /// Name for the private key
        name: String,
        /// The private key (will prompt if not provided)
        #[arg(long)]
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

#[derive(Debug, Subcommand)]
pub enum VarCommand {
    /// Set or update a variable
    Set {
        /// Variable name
        name: String,
        /// Variable value
        value: String,
    },
    /// Get a variable's value
    Get {
        /// Variable name
        name: String,
    },
    /// List all variables
    List,
    /// Remove a variable
    Rm {
        /// Variable name
        name: String,
    },
}

#[derive(Debug, Args)]
pub struct UpdateArgs {}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Chain name
    #[arg(long)]
    pub name: Option<String>,

    /// Chain ID
    #[arg(long)]
    pub chain_id: Option<u64>,

    /// RPC URL
    #[arg(long)]
    pub rpc_url: Option<String>,

    /// Key name (defaults to "default")
    #[arg(long)]
    pub key: Option<String>,

    /// Block explorer API URL
    #[arg(long)]
    pub verification_url: Option<String>,

    /// Block explorer API key
    #[arg(long)]
    pub verification_api_key: Option<String>,

    /// Overwrite existing chain without prompting
    #[arg(long, default_value_t = false)]
    pub force: bool,
}
