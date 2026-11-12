//! The whole CLI surface (clap definitions only). Handlers live in
//! `commands/`, one module per command; each takes its `…Args` struct.
//! Conversions to core options happen here, or in the handler when they
//! need I/O (reading a key from stdin, say). Dynamic completion is
//! declared on the arguments (`add = complete::…`, see `complete`).
//! [`usage_error`] words a handler's own usage errors the way clap does.

use crate::complete;
use chainz_core::context::ExecOptions;
use chainz_core::model::Backend;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::engine::ArgValueCompleter;

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

/// A usage error for the subcommand at `path` (`["chain", "set"]`), worded
/// like clap's own: the message, the subcommand's usage line, and a
/// pointer to `--help`. It exits 2, as clap's do (see `ui::exit_code`).
pub fn usage_error(path: &[&str], message: &str) -> anyhow::Error {
    let mut command = Opt::command();
    command.build();
    let mut sub = &mut command;
    for name in path {
        sub = sub
            .find_subcommand_mut(name)
            .unwrap_or_else(|| panic!("no subcommand '{name}'"));
    }
    anyhow::Error::new(sub.error(clap::error::ErrorKind::MissingRequiredArgument, message))
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
    Shell(ContextArgs),

    /// Print a chain's context as shell exports: eval "$(chainz env base)"
    Env(EnvArgs),

    /// Set the default chain (exec notes on stderr whenever it applies)
    Use(UseArgs),

    /// List chains (alias of `chain ls`)
    #[command(visible_alias = "list")]
    Ls(ListArgs),

    /// Live dashboard of chains, endpoints, balances and gateway traffic
    Top(TopArgs),

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

    /// Guided first-run setup: chains, a signer, an Etherscan key, a default
    Init,

    /// Change a chain's settings (same as `chain set`)
    Update(ChainSetArgs),

    /// Edit config.toml, or one chain's table, in $EDITOR (validated on save)
    Edit(EditArgs),

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
    Label(LabelArgs),

    /// Manage variables (values live in secrets.toml)
    Var {
        #[command(subcommand)]
        cmd: VarCommand,
    },

    /// Check config, secrets, references and RPC health
    Doctor(DoctorArgs),

    /// Config file paths and the v1 migration
    Config {
        #[command(subcommand)]
        cmd: ConfigCommand,
    },

    /// Print the shell loader for dynamic completions
    ///
    /// The loader calls back into chainz as you type, so chain names,
    /// endpoints, signers, labels, variables and exec's @tokens come from
    /// your current config. Load it at shell startup:
    ///
    ///   zsh:        source <(chainz completions zsh)      (after compinit)
    ///   bash:       source <(chainz completions bash)
    ///   fish:       chainz completions fish | source
    ///   elvish:     eval (chainz completions elvish | slurp)
    ///   powershell: chainz completions powershell | Out-String | Invoke-Expression
    #[command(verbatim_doc_comment)]
    Completions(CompletionsArgs),
}

/// The context selectors shared by `exec`, `env` and `shell`.
#[derive(Debug, Clone, Default, Args)]
pub struct ContextArgs {
    /// Chain name, alias, prefix, ID, or fork name
    #[arg(add = ArgValueCompleter::new(complete::context_chains))]
    pub chain: Option<String>,
    /// Use this signer instead of the chain's
    #[arg(long, add = ArgValueCompleter::new(complete::signers))]
    pub signer: Option<String>,
    /// Use this endpoint instead of the active one
    #[arg(long, add = ArgValueCompleter::new(complete::endpoints))]
    pub rpc: Option<String>,
    /// Bypass a running gateway
    #[arg(long)]
    pub direct: bool,
}

impl From<&ContextArgs> for ExecOptions {
    fn from(args: &ContextArgs) -> Self {
        ExecOptions {
            signer: args.signer.clone(),
            rpc: args.rpc.clone(),
            direct: args.direct,
            expose_key: false,
        }
    }
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    #[command(flatten)]
    pub context: ContextArgs,
    /// Export the signer's private key as RAW_PRIVATE_KEY
    #[arg(long)]
    pub expose_key: bool,
    /// Print the context, exported names and rewritten argv, then exit
    #[arg(long)]
    pub explain: bool,
    /// The command to run (after --)
    #[arg(last = true, required = true, value_name = "CMD", add = ArgValueCompleter::new(complete::exec_words))]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct EnvArgs {
    #[command(flatten)]
    pub context: ContextArgs,
    /// Output syntax (default: from $SHELL)
    #[arg(long, value_enum)]
    pub shell: Option<ShellKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
}

#[derive(Debug, Args)]
pub struct UseArgs {
    /// Chain name, alias, prefix or ID (picker if omitted on a TTY)
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: Option<String>,
    /// Clear the default instead
    #[arg(long, conflicts_with = "chain")]
    pub clear: bool,
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

/// `--json` alone.
#[derive(Debug, Args)]
pub struct JsonArgs {
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct TopArgs {
    /// Print one snapshot and exit
    #[arg(long)]
    pub once: bool,
    /// JSON output (with --once)
    #[arg(long, requires = "once")]
    pub json: bool,
    /// Show secret values
    #[arg(long)]
    pub reveal: bool,
}

#[derive(Debug, Args)]
pub struct EditArgs {
    /// Chain name, alias, prefix or ID: edit only its table (default: the
    /// whole config.toml)
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: Option<String>,
}

#[derive(Debug, Args)]
pub struct LabelArgs {
    /// Chain whose labels apply
    #[arg(long, add = ArgValueCompleter::new(complete::chains_and_forks))]
    pub chain: Option<String>,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Repair what can be repaired (dead active RPCs, literal credentials)
    #[arg(long)]
    pub fix: bool,
    /// Verify the built-in token registry on chain (checks only)
    #[arg(long, conflicts_with = "fix")]
    pub tokens: bool,
    /// List each finding of a group (one cause on many chains)
    #[arg(short, long)]
    pub verbose: bool,
}

#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// The shell to print the loader for
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Port on 127.0.0.1
    #[arg(long, default_value_t = chainz_core::runtime::DEFAULT_PORT)]
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
    Status(JsonArgs),
    /// Stop the gateway
    Stop,
    /// Show request records
    Log(ServeLogArgs),
}

#[derive(Debug, Args)]
pub struct ServeLogArgs {
    /// Follow
    #[arg(short, long)]
    pub follow: bool,
    /// Only this chain
    #[arg(long, add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: Option<String>,
    /// Only this method
    #[arg(long)]
    pub method: Option<String>,
}

#[derive(Debug, Args)]
pub struct ForkArgs {
    /// Chain to fork (`ls` and `stop` are subcommands)
    #[arg(required = true, add = ArgValueCompleter::new(complete::configured_chains))]
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
    #[command(visible_alias = "list")]
    Ls(JsonArgs),
    /// Stop a fork
    Stop(ForkStopArgs),
}

#[derive(Debug, Args)]
pub struct ForkStopArgs {
    /// Fork name
    #[arg(add = ArgValueCompleter::new(complete::fork_names))]
    pub name: Option<String>,
    /// Stop every fork
    #[arg(long, conflicts_with = "name")]
    pub all: bool,
}

#[derive(Debug, Subcommand)]
pub enum ChainCommand {
    /// List chains
    #[command(visible_alias = "list")]
    Ls(ListArgs),
    /// Add a chain from chainlist (by ID or name) or by hand
    Add(ChainAddArgs),
    /// Show one chain in full
    Show(ChainShowArgs),
    /// Change a chain's settings (also `chainz update`); a menu with no flags
    Set(ChainSetArgs),
    /// Remove a chain (exact name or ID) and its labels; asks first unless --yes
    #[command(visible_alias = "remove")]
    Rm(ChainRmArgs),
}

#[derive(Debug, Default, Args)]
pub struct ChainAddArgs {
    /// Chain ID or chainlist name (picker if omitted on a TTY); with
    /// --url, a chain ID or built-in short name (no chainlist lookup)
    pub chain: Option<String>,
    /// Short name (default: built-in table, else a slug)
    #[arg(long)]
    pub name: Option<String>,
    /// RPC URL; repeatable, the first becomes active
    #[arg(long = "url", value_name = "URL")]
    pub urls: Vec<String>,
    /// Signer to attach (default: the only signer, if there is one)
    #[arg(long, conflicts_with = "no_signer", add = ArgValueCompleter::new(complete::signers))]
    pub signer: Option<String>,
    /// Attach no signer
    #[arg(long)]
    pub no_signer: bool,
    /// Skip the chain-ID check of the active endpoint
    #[arg(long)]
    pub no_check: bool,
    /// Re-download the chainlist
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Debug, Args)]
pub struct ChainShowArgs {
    /// Chain name, alias, prefix or ID
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: String,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Show credential-bearing URLs and header values
    #[arg(long)]
    pub reveal: bool,
}

#[derive(Debug, Args)]
pub struct ChainRmArgs {
    /// Exact chain name or ID
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: String,
    /// Remove without asking (required off a terminal)
    #[arg(long, short)]
    pub yes: bool,
}

/// `chain set` / `update`: one patch to one chain, applied at once.
#[derive(Debug, Default, Args)]
pub struct ChainSetArgs {
    /// Chain name, alias, prefix or ID (picker if omitted on a TTY)
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: Option<String>,
    /// Attach this signer
    #[arg(long, value_name = "NAME", conflicts_with = "no_signer", add = ArgValueCompleter::new(complete::signers))]
    pub signer: Option<String>,
    /// Detach the signer
    #[arg(long)]
    pub no_signer: bool,
    /// Activate an endpoint by name or URL; a new URL is added as an endpoint
    #[arg(long, value_name = "NAME|URL", add = ArgValueCompleter::new(complete::endpoints))]
    pub rpc: Option<String>,
    /// Header for the --rpc endpoint (else the active one), as 'name: value' (repeatable)
    #[arg(long = "header", value_name = "NAME: VALUE")]
    pub headers: Vec<String>,
    /// Drop that endpoint's headers first
    #[arg(long)]
    pub clear_headers: bool,
    /// Failover endpoints, in order
    #[arg(
        long,
        value_name = "NAME,...",
        value_delimiter = ',',
        conflicts_with = "no_failover",
        add = ArgValueCompleter::new(complete::endpoints)
    )]
    pub failover: Vec<String>,
    /// Clear the failover list
    #[arg(long)]
    pub no_failover: bool,
    /// Attach this explorer (`etherscan` is built in)
    #[arg(long, value_name = "NAME", conflicts_with = "no_explorer", add = ArgValueCompleter::new(complete::explorers))]
    pub explorer: Option<String>,
    /// Detach the explorer
    #[arg(long)]
    pub no_explorer: bool,
    /// Read the explorer's API key from stdin (one key per explorer, shared by its chains)
    #[arg(long, conflicts_with = "api_key")]
    pub api_key_stdin: bool,
    /// The explorer's API key as an argument (visible in history and `ps`; prefer --api-key-stdin)
    #[arg(long, hide = true, value_name = "KEY")]
    pub api_key: Option<String>,
    /// Rename the chain
    #[arg(long, value_name = "NEW")]
    pub name: Option<String>,
    /// Add an alias (repeatable)
    #[arg(long = "alias", value_name = "ALIAS")]
    pub aliases: Vec<String>,
    /// Remove an alias (repeatable)
    #[arg(long = "rm-alias", value_name = "ALIAS")]
    pub rm_aliases: Vec<String>,
    /// Skip the chain-ID check of a new --rpc URL
    #[arg(long)]
    pub no_check: bool,
}

#[derive(Debug, Subcommand)]
pub enum RpcCommand {
    /// List endpoints with their latest health
    #[command(visible_alias = "list")]
    Ls(RpcLsArgs),
    /// Add an endpoint
    Add(RpcAddArgs),
    /// Remove an endpoint; asks first unless --yes
    #[command(visible_alias = "remove")]
    Rm(RpcRmArgs),
    /// Probe, rank and tag endpoints
    Bench(BenchArgs),
    /// Find public endpoints on chainlist, probe them, offer to add
    Discover(DiscoverArgs),
}

#[derive(Debug, Args)]
pub struct RpcLsArgs {
    /// Only this chain (name, alias, prefix or ID)
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: Option<String>,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Show credential-bearing URLs and header values (as configured,
    /// `${VAR}`s unexpanded)
    #[arg(long)]
    pub reveal: bool,
}

#[derive(Debug, Args)]
pub struct RpcAddArgs {
    /// Chain name, alias, prefix or ID
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: String,
    /// The endpoint's URL (http(s) or ws(s))
    pub url: String,
    /// Endpoint name (default: from the host)
    #[arg(long)]
    pub name: Option<String>,
    /// Header for this endpoint only, as 'name: value' (repeatable)
    #[arg(long = "header", value_name = "NAME: VALUE")]
    pub headers: Vec<String>,
    /// Append to the chain's failover list
    #[arg(long, conflicts_with = "activate")]
    pub failover: bool,
    /// Make it the active endpoint
    #[arg(long)]
    pub activate: bool,
    /// Skip the chain-ID check
    #[arg(long)]
    pub no_check: bool,
}

#[derive(Debug, Args)]
pub struct RpcRmArgs {
    /// Chain name, alias, prefix or ID
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: String,
    /// Endpoint name
    #[arg(add = ArgValueCompleter::new(complete::endpoints))]
    pub name: String,
    /// Remove without asking (required off a terminal)
    #[arg(long, short)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct BenchArgs {
    /// Only this chain (name, alias, prefix or ID)
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: Option<String>,
    /// Activate the best endpoint and propose a failover list
    #[arg(long)]
    pub fix: bool,
    /// With --fix: write the proposed failover lists without asking
    #[arg(long, short, requires = "fix")]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct DiscoverArgs {
    /// Chain name, alias, prefix or ID
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: String,
    /// Re-download the chainlist
    #[arg(long)]
    pub refresh: bool,
    /// Add every endpoint that answered without asking
    #[arg(long, short)]
    pub yes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BackendArg {
    Keyring,
    Encrypted,
    /// `one-password` is the v1 name
    #[value(name = "1password", alias = "one-password")]
    OnePassword,
    Keystore,
    /// `private-key` is the v1 name
    #[value(alias = "private-key")]
    Plaintext,
}

impl From<BackendArg> for Backend {
    fn from(arg: BackendArg) -> Self {
        match arg {
            BackendArg::Keyring => Backend::Keyring,
            BackendArg::Encrypted => Backend::Encrypted,
            BackendArg::OnePassword => Backend::OnePassword,
            BackendArg::Keystore => Backend::Keystore,
            BackendArg::Plaintext => Backend::Plaintext,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MigrateTarget {
    Keyring,
    Encrypted,
}

impl From<MigrateTarget> for Backend {
    fn from(arg: MigrateTarget) -> Self {
        match arg {
            MigrateTarget::Keyring => Backend::Keyring,
            MigrateTarget::Encrypted => Backend::Encrypted,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum SignerCommand {
    /// Add a signer
    Add(SignerAddArgs),
    /// List signers (never shows key material)
    #[command(visible_alias = "list")]
    Ls(JsonArgs),
    /// Remove a signer and delete its key material (keyring entry or
    /// secrets.toml entry); asks first unless --yes
    #[command(visible_alias = "remove")]
    Rm(SignerRmArgs),
    /// Move keys into safe storage
    Migrate(SignerMigrateArgs),
}

#[derive(Debug, Default, Args)]
pub struct SignerAddArgs {
    /// Name for the signer, also its @token (e.g. deployer)
    pub name: String,
    /// Read the private key from stdin
    #[arg(long)]
    pub stdin: bool,
    /// Private key as an argument (visible in history and `ps`; prefer --stdin)
    #[arg(long, hide = true, conflicts_with = "stdin")]
    pub key: Option<String>,
    /// Where the key lives (default: keyring, else encrypted)
    #[arg(long, value_enum, alias = "type")]
    pub backend: Option<BackendArg>,
    /// Keystore file (keystore backend)
    #[arg(long)]
    pub path: Option<String>,
    /// 1Password vault (1password backend)
    #[arg(long)]
    pub vault: Option<String>,
    /// 1Password item (1password backend)
    #[arg(long)]
    pub item: Option<String>,
    /// Cached address, when it cannot be derived
    #[arg(long)]
    pub address: Option<String>,
    /// Don't attach a first signer to the chains that have none
    #[arg(long)]
    pub no_attach: bool,
}

#[derive(Debug, Args)]
pub struct SignerRmArgs {
    /// Signer name
    #[arg(add = ArgValueCompleter::new(complete::signers))]
    pub name: String,
    /// Unassign it from the chains that use it
    #[arg(long)]
    pub unassign: bool,
    /// Remove without asking (required off a terminal)
    #[arg(long, short)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct SignerMigrateArgs {
    /// Signer name (default: the only plaintext signer)
    #[arg(add = ArgValueCompleter::new(complete::signers))]
    pub name: Option<String>,
    /// Every plaintext signer
    #[arg(long, conflicts_with = "name")]
    pub all: bool,
    /// Destination (default: keyring, else encrypted)
    #[arg(long, value_enum)]
    pub to: Option<MigrateTarget>,
}

#[derive(Debug, Subcommand)]
pub enum AddrCommand {
    /// Name an address
    Add(AddrAddArgs),
    /// List labels
    #[command(visible_alias = "list")]
    Ls(AddrLsArgs),
    /// Remove a label; asks first unless --yes
    #[command(visible_alias = "remove")]
    Rm(AddrRmArgs),
    /// Reverse-look-up an address
    Find(AddrFindArgs),
    /// Label contracts from Foundry broadcast output
    Import(AddrImportArgs),
}

#[derive(Debug, Args)]
pub struct AddrAddArgs {
    /// Label name, used as @name
    pub name: String,
    /// The 0x address
    pub address: String,
    /// Only on this chain (default: the chain context, else a picker)
    #[arg(long, add = ArgValueCompleter::new(complete::chains_and_forks))]
    pub chain: Option<String>,
    /// On every chain
    #[arg(long, conflicts_with = "chain")]
    pub all_chains: bool,
}

#[derive(Debug, Args)]
pub struct AddrLsArgs {
    /// What @name means on this chain
    #[arg(long, add = ArgValueCompleter::new(complete::chains_and_forks))]
    pub chain: Option<String>,
    /// Include the built-in tokens
    #[arg(long)]
    pub builtin: bool,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct AddrRmArgs {
    /// Label name
    #[arg(add = ArgValueCompleter::new(complete::user_labels))]
    pub name: String,
    /// Only on this configured chain (or '*' for the every-chain entry)
    #[arg(long, add = ArgValueCompleter::new(complete::label_scopes))]
    pub chain: Option<String>,
    /// Remove without asking (required off a terminal)
    #[arg(long, short)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct AddrFindArgs {
    /// The 0x address
    pub address: String,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct AddrImportArgs {
    /// run-latest.json files, or directories holding broadcast/
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<std::path::PathBuf>,
    /// Show the plan; write nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Prepend to every name (e.g. `v4-`)
    #[arg(long, value_name = "STR")]
    pub prefix: Option<String>,
    /// Only these contracts (contract or label names, comma-separated)
    #[arg(long, value_name = "NAME,...", value_delimiter = ',')]
    pub only: Vec<String>,
    /// Replace labels that hold a different address on the chain
    #[arg(long)]
    pub force: bool,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

impl From<&AddrImportArgs> for chainz_core::broadcast::Options {
    fn from(args: &AddrImportArgs) -> Self {
        chainz_core::broadcast::Options {
            prefix: args.prefix.clone().unwrap_or_default(),
            only: args.only.clone(),
            force: args.force,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum VarCommand {
    /// Set a variable (prefer --stdin over an argument)
    Set(VarSetArgs),
    /// Show a variable (redacted unless --reveal)
    Get(VarGetArgs),
    /// List variables
    #[command(visible_alias = "list")]
    Ls(VarLsArgs),
    /// Remove a variable and its value; asks first unless --yes
    #[command(visible_alias = "remove")]
    Rm(VarRmArgs),
}

#[derive(Debug, Args)]
pub struct VarSetArgs {
    /// Variable name, referenced as ${NAME}
    #[arg(add = ArgValueCompleter::new(complete::vars))]
    pub name: String,
    /// The value (visible in shell history and `ps`; prefer --stdin)
    pub value: Option<String>,
    /// Read the value from stdin
    #[arg(long, conflicts_with = "value")]
    pub stdin: bool,
}

#[derive(Debug, Args)]
pub struct VarGetArgs {
    /// Variable name
    #[arg(add = ArgValueCompleter::new(complete::vars))]
    pub name: String,
    /// Print the value
    #[arg(long, alias = "show")]
    pub reveal: bool,
}

#[derive(Debug, Args)]
pub struct VarLsArgs {
    /// Print the values
    #[arg(long, alias = "show")]
    pub reveal: bool,
    /// List every config record that uses each variable
    #[arg(short, long)]
    pub verbose: bool,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct VarRmArgs {
    /// Variable name
    #[arg(add = ArgValueCompleter::new(complete::vars))]
    pub name: String,
    /// Remove without asking (required off a terminal)
    #[arg(long, short)]
    pub yes: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the config file path
    Path(ConfigPathArgs),
    /// Migrate the v1 config.json
    Migrate(ConfigMigrateArgs),
}

#[derive(Debug, Args)]
pub struct ConfigPathArgs {
    /// Print every chainz path (config, secrets, state, cache, runtime)
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct ConfigMigrateArgs {
    /// Print the resulting config.toml without writing anything
    #[arg(long)]
    pub dry_run: bool,
}

#[cfg(test)]
mod tests {
    use super::Opt;
    use clap::CommandFactory;

    /// `exec --help` restates the contract by hand: it must name every
    /// exported variable and every context token.
    #[test]
    fn exec_help_names_the_whole_contract() {
        let command = Opt::command();
        let exec = command.find_subcommand("exec").unwrap();
        let help = exec.get_long_about().unwrap().to_string();
        for name in chainz_core::contract::CONTRACT_VARS {
            assert!(help.contains(name), "exec help misses {name}");
        }
        for token in chainz_core::names::context_tokens() {
            assert!(
                help.contains(&format!("@{token}")),
                "exec help misses @{token}"
            );
        }
    }
}
