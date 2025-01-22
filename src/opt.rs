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
        #[structopt(flatten)]
        args: AddArgs,
    },

    /// Update an existing chain's configuration
    Update {
        #[structopt(flatten)]
        args: UpdateArgs,
    },

    /// Use a specific chain configuration
    ///
    /// Sets up environment for working with a specific chain.
    /// Can identify chain by name or chain ID.
    ///
    /// Flags:
    ///     -p, --print  : Print variables to stdout instead of writing to .env
    ///     -e, --export : Include 'export' prefix in output
    ///
    /// Example: chainz use ethereum --print
    Use {
        /// Chain name or ID to select
        name_or_id: String,
        /// Print to stdout instead of writing to .env
        #[structopt(short, long)]
        print: bool,
        /// Include 'export' prefix in output
        #[structopt(short, long)]
        export: bool,
    },

    /// List all configured chains
    ///
    /// Displays all chains with their details:
    /// - Name
    /// - Chain ID
    /// - RPC URL
    /// - Associated private key name
    List,

    #[structopt(
        about = "Execute a command",
        long_about = "Execute a command with chain-specific variables expanded.\n\n\
                  Available expansions:\n\
                      @wallet : The wallet address\n\
                      @rpc    : RPC URL\n\
                      @chainid  : Chain ID\n\
                      @chainname  : Chain name\n\
                      @key    : Private key\n\
                  \n\
                  Example: chainz exec ethereum -- cast balance @wallet"
    )]
    Exec {
        /// Chain name or ID to use
        name_or_id: String,
        /// Command to execute (after --)
        #[structopt(last = true)]
        command: Vec<String>,
    },

    /// Manage private keys
    ///
    /// Subcommands for adding, listing, and removing private keys.
    /// Keys are stored encrypted in the configuration.
    ///
    /// Example: chainz key add mykey
    Key {
        #[structopt(subcommand)]
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
        #[structopt(subcommand)]
        cmd: VarCommand,
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

#[derive(Debug, StructOpt)]
pub struct UpdateArgs {}

#[derive(Debug, StructOpt)]
pub struct AddArgs {}
