//! The whole CLI surface (clap definitions only). Handlers live in
//! `commands/`, one module per command.

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "chainz",
    version,
    about = "A local multichain workbench: chains, endpoints, signers, and exec with context",
    after_help = "Run `chainz <command> --help` for details. `chainz exec base -- cast block-number` is the hot path."
)]
pub struct Opt {
    #[command(subcommand)]
    pub cmd: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a command with a chain's context: environment and @tokens
    ///
    /// Exported: ETH_RPC_URL, ETH_RPC_HEADERS (direct only), CHAIN_ID,
    /// CHAINZ_CHAIN, ETHERSCAN_API_KEY, VERIFIER_URL/VERIFIER_API_KEY
    /// (non-Etherscan explorers), ETH_FROM, WALLET_ADDRESS, ETH_KEYSTORE,
    /// RAW_PRIVATE_KEY (--expose-key only), CHAIN_NAME (deprecated).
    ///
    /// Tokens: @rpc @chainid @chain @wallet @explorer @explorer_key,
    /// any signer name (@funder), labels (@usdc, @myrouter), and
    /// `@name:chain` for another chain. `@@` is a literal @.
    ///
    /// Example: chainz exec base -- cast balance @wallet
    #[command(visible_alias = "x", verbatim_doc_comment)]
    Exec(ExecArgs),

    /// Open a subshell with a chain's context exported
    Shell {
        /// Chain name, alias, prefix, ID, or fork name
        chain: Option<String>,
        /// Use this signer instead of the chain's
        #[arg(long)]
        signer: Option<String>,
        /// Bypass a running gateway
        #[arg(long)]
        direct: bool,
    },

    /// Print a chain's context as shell exports: eval "$(chainz env base)"
    Env {
        /// Chain name, alias, prefix, ID, or fork name
        chain: Option<String>,
        /// Output syntax (default: from $SHELL)
        #[arg(long, value_enum)]
        shell: Option<ShellKind>,
        /// Use this signer instead of the chain's
        #[arg(long)]
        signer: Option<String>,
        /// Use this endpoint instead of the active one
        #[arg(long)]
        rpc: Option<String>,
        /// Bypass a running gateway
        #[arg(long)]
        direct: bool,
    },

    /// Set the default chain (exec notes on stderr whenever it applies)
    Use {
        /// Chain name, alias, prefix or ID (picker if omitted on a TTY)
        chain: Option<String>,
        /// Clear the default instead
        #[arg(long, conflicts_with = "chain")]
        clear: bool,
    },

    /// List chains (alias of `chain ls`)
    #[command(visible_alias = "list")]
    Ls(ListArgs),

    /// Live dashboard of chains, endpoints, balances and gateway traffic
    Top {
        /// Print one snapshot and exit
        #[arg(long)]
        once: bool,
        /// JSON output (with --once)
        #[arg(long)]
        json: bool,
        /// Show secret values
        #[arg(long)]
        reveal: bool,
    },

    /// Local RPC gateway with failover, rate-limit smoothing and an inspector
    #[command(args_conflicts_with_subcommands = true)]
    Serve {
        #[command(subcommand)]
        cmd: Option<ServeCommand>,
        #[command(flatten)]
        args: ServeArgs,
    },

    /// Start an anvil fork of a chain, usable as a chain name
    #[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
    Fork {
        #[command(subcommand)]
        cmd: Option<ForkCommand>,
        #[command(flatten)]
        args: ForkArgs,
    },

    /// Manage chains
    Chain {
        #[command(subcommand)]
        cmd: ChainCommand,
    },

    /// Manage a chain's RPC endpoints
    Rpc {
        #[command(subcommand)]
        cmd: RpcCommand,
    },

    /// Manage signers (key references with cached addresses)
    #[command(alias = "key")]
    Signer {
        #[command(subcommand)]
        cmd: SignerCommand,
    },

    /// Manage the address book
    Addr {
        #[command(subcommand)]
        cmd: AddrCommand,
    },

    /// Rewrite addresses on stdin as name(0x…)
    Label {
        /// Chain whose labels apply
        #[arg(long)]
        chain: Option<String>,
    },

    /// Manage variables (values live in secrets.toml)
    Var {
        #[command(subcommand)]
        cmd: VarCommand,
    },

    /// Check config, secrets, references and RPC health
    Doctor {
        /// Repair what can be repaired (dead active RPCs, literal credentials)
        #[arg(long)]
        fix: bool,
        /// Verify the built-in token registry on chain
        #[arg(long)]
        tokens: bool,
    },

    /// Config file utilities
    Config {
        #[command(subcommand)]
        cmd: ConfigCommand,
    },

    /// Generate shell completions
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    /// Chain name, alias, prefix, ID, or fork name
    pub chain: Option<String>,
    /// Use this signer instead of the chain's
    #[arg(long)]
    pub signer: Option<String>,
    /// Use this endpoint instead of the active one
    #[arg(long)]
    pub rpc: Option<String>,
    /// Bypass a running gateway
    #[arg(long)]
    pub direct: bool,
    /// Export the signer's private key as RAW_PRIVATE_KEY
    #[arg(long)]
    pub expose_key: bool,
    /// Print the context, exported names and rewritten argv, then exit
    #[arg(long)]
    pub explain: bool,
    /// The command to run (after --)
    #[arg(last = true, required = true, value_name = "CMD")]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Show credential-bearing URLs and header values
    #[arg(long)]
    pub reveal: bool,
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Port on 127.0.0.1
    #[arg(long, default_value_t = 18545)]
    pub port: u16,
    /// Run in the background
    #[arg(long)]
    pub detach: bool,
    /// Append request records as JSONL
    #[arg(long, value_name = "FILE")]
    pub log: Option<std::path::PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum ServeCommand {
    /// Show whether the gateway runs, and its chains
    Status,
    /// Stop the gateway
    Stop,
    /// Show request records
    Log {
        /// Follow
        #[arg(short, long)]
        follow: bool,
        /// Only this chain
        #[arg(long)]
        chain: Option<String>,
        /// Only this method
        #[arg(long)]
        method: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct ForkArgs {
    /// Chain to fork (`ls` and `stop` are subcommands)
    #[arg(required = true)]
    pub chain: Option<String>,
    /// Fork at this block (needs an archive endpoint)
    #[arg(long)]
    pub block: Option<u64>,
    /// Anvil port (default: first free from 8601)
    #[arg(long)]
    pub port: Option<u16>,
    /// Fork name (default: CHAIN-fork)
    #[arg(long)]
    pub name: Option<String>,
    /// Give the fork its own chain ID
    #[arg(long)]
    pub chain_id: Option<u64>,
    /// Fund the signer: optional SPEC like usdc=10000,weth=5
    #[arg(long, num_args = 0..=1, default_missing_value = "", conflicts_with = "no_fund")]
    pub fund: Option<String>,
    /// Do not fund the signer
    #[arg(long)]
    pub no_fund: bool,
    /// Run in the background
    #[arg(long)]
    pub detach: bool,
}

#[derive(Debug, Subcommand)]
pub enum ForkCommand {
    /// List live forks
    Ls,
    /// Stop a fork
    Stop {
        /// Fork name
        name: Option<String>,
        /// Stop every fork
        #[arg(long, conflicts_with = "name")]
        all: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ChainCommand {
    /// List chains
    Ls(ListArgs),
    /// Add a chain from chainlist (by ID or name) or by hand
    Add {
        /// Chain ID or chainlist name (picker if omitted on a TTY)
        chain: Option<String>,
        /// Short name (default: built-in table, else a slug)
        #[arg(long)]
        name: Option<String>,
        /// RPC URL; repeatable, the first becomes active
        #[arg(long = "rpc", value_name = "URL")]
        rpcs: Vec<String>,
        /// Signer to attach
        #[arg(long)]
        signer: Option<String>,
        /// Skip the chain-ID check of the active endpoint
        #[arg(long)]
        no_check: bool,
        /// Re-download the chainlist
        #[arg(long)]
        refresh: bool,
    },
    /// Show one chain in full
    Show {
        chain: String,
        #[arg(long)]
        json: bool,
        /// Show credential-bearing URLs and header values
        #[arg(long)]
        reveal: bool,
    },
    /// Edit one chain's TOML in $EDITOR (validated on save)
    Edit { chain: String },
    /// Rename a chain
    Rename { chain: String, new_name: String },
    /// Add (or with --remove, drop) aliases
    Alias {
        chain: String,
        #[arg(required = true)]
        aliases: Vec<String>,
        #[arg(long)]
        remove: bool,
    },
    /// Remove a chain (exact name or ID)
    Rm { chain: String },
}

#[derive(Debug, Subcommand)]
pub enum RpcCommand {
    /// List endpoints with their latest health
    Ls {
        /// Only this chain
        chain: Option<String>,
        #[arg(long)]
        json: bool,
        /// Show credential-bearing URLs and header values
        #[arg(long)]
        reveal: bool,
    },
    /// Add an endpoint
    Add {
        chain: String,
        url: String,
        /// Endpoint name (default: from the host)
        #[arg(long)]
        name: Option<String>,
        /// Header for this endpoint only, as 'name: value' (repeatable)
        #[arg(long = "header", value_name = "NAME: VALUE")]
        headers: Vec<String>,
        /// Append to the chain's failover list
        #[arg(long)]
        failover: bool,
        /// Make it the active endpoint
        #[arg(long = "use")]
        activate: bool,
        /// Skip the chain-ID check
        #[arg(long)]
        no_check: bool,
    },
    /// Make an endpoint active
    Use { chain: String, name: String },
    /// Remove an endpoint
    Rm { chain: String, name: String },
    /// Probe, rank and tag endpoints
    Bench {
        /// Only this chain
        chain: Option<String>,
        /// Activate the best endpoint and propose a failover list
        #[arg(long)]
        fix: bool,
    },
    /// Find public endpoints on chainlist, probe them, offer to add
    Discover {
        chain: String,
        /// Re-download the chainlist
        #[arg(long)]
        refresh: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BackendArg {
    Keyring,
    Encrypted,
    #[value(name = "1password")]
    OnePassword,
    Keystore,
    Plaintext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MigrateTarget {
    Keyring,
    Encrypted,
}

#[derive(Debug, Subcommand)]
pub enum SignerCommand {
    /// Add a signer
    Add {
        name: String,
        /// Read the private key from stdin
        #[arg(long)]
        stdin: bool,
        /// Private key as an argument (visible in history; prefer --stdin)
        #[arg(long, hide = true, conflicts_with = "stdin")]
        key: Option<String>,
        /// Where the key lives (default: keyring, else encrypted)
        #[arg(long, value_enum, alias = "type")]
        backend: Option<BackendArg>,
        /// Keystore file (keystore backend)
        #[arg(long)]
        path: Option<String>,
        /// 1Password vault (1password backend)
        #[arg(long)]
        vault: Option<String>,
        /// 1Password item (1password backend)
        #[arg(long)]
        item: Option<String>,
        /// Cached address, when it cannot be derived
        #[arg(long)]
        address: Option<String>,
    },
    /// List signers (never shows key material)
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Remove a signer
    Rm {
        name: String,
        /// Detach it from chains that use it
        #[arg(long)]
        force: bool,
    },
    /// Move keys into safe storage
    Migrate {
        name: Option<String>,
        /// Every plaintext signer
        #[arg(long, conflicts_with = "name")]
        all: bool,
        /// Destination (default: keyring, else encrypted)
        #[arg(long, value_enum)]
        to: Option<MigrateTarget>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AddrCommand {
    /// Name an address
    Add {
        name: String,
        address: String,
        #[arg(long)]
        chain: Option<String>,
        #[arg(long, conflicts_with = "chain")]
        all_chains: bool,
    },
    /// List labels
    Ls {
        #[arg(long)]
        chain: Option<String>,
        #[arg(long)]
        builtin: bool,
    },
    /// Remove a label
    Rm {
        name: String,
        #[arg(long)]
        chain: Option<String>,
    },
    /// Reverse-look-up an address
    Find { address: String },
}

#[derive(Debug, Subcommand)]
pub enum VarCommand {
    /// Set a variable (prefer --stdin over an argument)
    Set {
        name: String,
        value: Option<String>,
        #[arg(long, conflicts_with = "value")]
        stdin: bool,
    },
    /// Show a variable (redacted unless --reveal)
    Get {
        name: String,
        #[arg(long, alias = "show")]
        reveal: bool,
    },
    /// List variables
    #[command(visible_alias = "list")]
    Ls {
        #[arg(long, alias = "show")]
        reveal: bool,
        #[arg(long)]
        json: bool,
    },
    /// Remove a variable
    #[command(visible_alias = "remove")]
    Rm { name: String },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the config file path
    Path {
        /// Print every chainz path (config, secrets, state, cache, runtime)
        #[arg(long)]
        all: bool,
    },
    /// Edit config.toml in $EDITOR (validated on save)
    Edit,
    /// Migrate the v1 config.json
    Migrate {
        /// Print the resulting config.toml without writing anything
        #[arg(long)]
        dry_run: bool,
    },
}
