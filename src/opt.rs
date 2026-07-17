use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "chainz",
    version,
    about = "CLI tool for managing EVM chain configurations"
)]
pub struct Opt {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand)]
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
    /// Example: `chainz add --name ethereum --chain-id 1 --rpc-url https://eth.llamarpc.com`
    Add {
        #[command(flatten)]
        args: AddArgs,
    },

    /// Update an existing chain's configuration
    Update {
        #[command(flatten)]
        args: UpdateArgs,
    },

    /// Remove a chain configuration
    ///
    /// Example: chainz remove ethereum
    #[command(alias = "rm")]
    Remove {
        /// Chain name or ID to remove
        name_or_id: String,
    },

    /// List all configured chains
    ///
    /// Displays all chains with their details:
    /// - Name
    /// - Chain ID
    /// - RPC URL
    /// - Associated private key name
    List {
        /// Output as JSON (for scripting)
        #[arg(long)]
        json: bool,
        /// Include credential-bearing URLs and verification keys
        #[arg(long)]
        show_secrets: bool,
    },

    /// Set the default chain used by exec when no chain is given
    ///
    /// Example: chainz use base
    Use {
        /// Chain name or ID (interactive picker if omitted)
        name_or_id: Option<String>,
    },

    /// Check config health: key storage, key references, RPC connectivity
    ///
    /// Exits nonzero if failures are found. With --fix, each dead selected
    /// RPC is switched to a healthy alternative from the chain's RPC list.
    Doctor {
        /// Switch dead selected RPCs to a healthy configured alternative
        #[arg(long)]
        fix: bool,
    },

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
        /// Expose the selected private key as RAW_PRIVATE_KEY without adding it to argv
        #[arg(long)]
        expose_key: bool,
    },

    /// Open a subshell with the chain's environment loaded
    ///
    /// Sets ETH_RPC_URL, CHAIN_ID, CHAIN_NAME, VERIFIER_* and CHAINZ_CHAIN,
    /// and prefixes PS1 with the chain name for bash-like shells.
    /// Key material is NOT loaded into the environment.
    ///
    /// Example: chainz shell base
    Shell {
        /// Chain name or ID (default chain or interactive picker if omitted)
        name_or_id: Option<String>,
    },

    /// Manage private keys
    ///
    /// Subcommands for adding, listing, and removing private keys.
    /// Keys use the OS keyring or encrypted storage by default.
    ///
    /// Example: chainz key add mykey
    Key {
        #[command(subcommand)]
        cmd: KeyCommand,
    },

    /// Generate shell completions
    ///
    /// Example: chainz completions zsh > ~/.zfunc/_chainz
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
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

#[derive(Subcommand)]
pub enum KeyCommand {
    /// Add a new private key
    ///
    /// Use --stdin instead of --key in scripts to keep secrets out of argv.
    Add {
        /// Name for the private key
        name: String,
        /// The private key (will prompt if not provided)
        #[arg(long)]
        key: Option<String>,
        /// Read the private key from stdin (safer for scripts than --key)
        #[arg(long, conflicts_with = "key")]
        stdin: bool,
        /// How to store the key (safe OS-keyring/encrypted default if omitted)
        #[arg(long = "type", value_enum)]
        key_type: Option<KeyTypeArg>,
    },
    /// List all stored private keys
    List {
        /// Output as JSON (for scripting; never includes key material)
        #[arg(long)]
        json: bool,
    },
    /// Remove a private key
    Remove {
        /// Name of the private key to remove
        name: String,
    },
    /// Move keys from plaintext or another backend into safe storage
    Migrate {
        /// Key name to migrate (omit when using --all)
        name: Option<String>,
        /// Migrate every plaintext key; individual failures are reported and skipped
        #[arg(long, conflicts_with = "name")]
        all: bool,
        /// Destination backend (safe default when omitted)
        #[arg(long, value_enum)]
        to: Option<MigrationTargetArg>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MigrationTargetArg {
    Keyring,
    Encrypted,
}

/// Storage backend for a private key
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KeyTypeArg {
    /// Store the raw private key in the config file
    PrivateKey,
    /// Encrypt the key with a password (AES-256-GCM + Argon2)
    Encrypted,
    /// Reference a 1Password item (requires `op` CLI)
    OnePassword,
    /// Store the key in the OS keyring
    Keyring,
}

#[derive(Subcommand)]
pub enum VarCommand {
    /// Set or update a variable
    Set {
        /// Variable name
        name: String,
        /// Variable value
        value: Option<String>,
        /// Read the value from stdin instead of argv
        #[arg(long)]
        stdin: bool,
    },
    /// Get a variable's value
    Get {
        /// Variable name
        name: String,
        /// Print the stored value instead of a redacted marker
        #[arg(long)]
        show: bool,
    },
    /// List all variables
    List {
        /// Print stored values instead of redacted markers
        #[arg(long)]
        show: bool,
    },
    /// Remove a variable
    Rm {
        /// Variable name
        name: String,
    },
}

#[derive(Args)]
pub struct UpdateArgs {
    /// Re-download the chainlist instead of using the local cache
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Args)]
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

    /// Read the block explorer API key from stdin instead of argv
    #[arg(long, conflicts_with = "verification_api_key")]
    pub verification_api_key_stdin: bool,

    /// Overwrite existing chain without prompting
    #[arg(long, default_value_t = false)]
    pub force: bool,

    /// Re-download the chainlist instead of using the local cache
    #[arg(long)]
    pub refresh: bool,
}
