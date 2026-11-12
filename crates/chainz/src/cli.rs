//! The whole CLI surface: clap definitions, and the help built from them.
//! Handlers live in `commands/`, one module per command; each takes its
//! `…Args` struct. Conversions to core options that need nothing else
//! happen here (`ExecOptions::from`); the rest are the handler's (an
//! `update` patch, a key read from stdin). Dynamic completion is declared
//! on the arguments (`add = complete::…`, see `complete`).
//! [`usage_error`] words a handler's own usage errors the way clap does.

use crate::complete;
use chainz_core::context::ExecOptions;
use chainz_core::model::Backend;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use clap_complete::engine::ArgValueCompleter;

#[derive(Debug, Parser)]
#[command(
    name = "chainz",
    version,
    about = "A local multichain workbench: chains, endpoints, signers, and exec with context",
    after_help = "`chainz x base -- cast block-number` is the hot path. `chainz COMMAND --help` for details."
)]
pub struct Opt {
    #[command(subcommand)]
    pub cmd: Option<Command>,
}

/// The top-level help's command groups, in order. Every visible command
/// is in exactly one (a test checks); each line is built from the
/// command's own name, aliases and about.
const HELP_GROUPS: &[(&str, &[&str])] = &[
    ("Run", &["exec", "env"]),
    (
        "Chains",
        &["ls", "use", "add", "update", "rm", "edit", "init"],
    ),
    ("Signers, labels, variables", &["signer", "label", "var"]),
    ("Health and runtime", &["doctor", "top", "serve", "fork"]),
    ("Setup", &["completions"]),
];

/// The command line definition: [`Opt`]'s, with the top-level help
/// grouped under [`HELP_GROUPS`]. Parsing, completion and help all start
/// here.
pub fn command() -> clap::Command {
    let command = Opt::command();
    let template = grouped_help(&command);
    command.help_template(template)
}

/// Parse the process's arguments (exits on a usage error, as clap does).
pub fn parse() -> Opt {
    Opt::from_arg_matches(&command().get_matches()).unwrap_or_else(|e| e.exit())
}

/// The top-level help template: about, usage, then each group's commands
/// (`exec, x  Run a command…`), aligned across groups, then the options.
fn grouped_help(command: &clap::Command) -> clap::builder::StyledStr {
    use std::fmt::Write;
    let header = command.get_styles().get_header();
    let rows: Vec<Vec<(String, String)>> = HELP_GROUPS
        .iter()
        .map(|(_, names)| {
            names
                .iter()
                .map(|name| {
                    let sub = command
                        .find_subcommand(name)
                        .unwrap_or_else(|| panic!("no command '{name}' to list in help"));
                    let label = std::iter::once(sub.get_name())
                        .chain(sub.get_visible_aliases())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let about = sub.get_about().map(ToString::to_string).unwrap_or_default();
                    (label, about)
                })
                .collect()
        })
        .collect();
    let width = rows
        .iter()
        .flatten()
        .map(|(l, _)| l.len())
        .max()
        .unwrap_or(0);
    let mut out = String::from("{about-with-newline}\n{usage-heading} {usage}\n");
    for ((title, _), rows) in HELP_GROUPS.iter().zip(&rows) {
        let _ = write!(out, "\n{header}{title}:{header:#}\n");
        for (label, about) in rows {
            let _ = writeln!(out, "  {header}{label:<width$}{header:#}  {about}");
        }
    }
    let _ = write!(
        out,
        "\n{header}Options:{header:#}\n{{options}}{{after-help}}"
    );
    out.into()
}

/// A usage error for the subcommand at `path` (`["signer", "add"]`), worded
/// like clap's own: the message, the subcommand's usage line, and a
/// pointer to `--help`. It exits 2, as clap's do (see `ui::exit_code`).
pub fn usage_error(path: &[&str], message: &str) -> anyhow::Error {
    let mut command = command();
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
    /// Run a command with a chain's context (no command: open a subshell)
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
    /// With no command, $SHELL opens with the context exported (ctrl-d
    /// leaves it).
    ///
    /// Example: chainz exec base -- cast balance @wallet
    #[command(visible_alias = "x", verbatim_doc_comment)]
    Exec(ExecArgs),

    /// Print a chain's context as shell exports: eval "$(chainz env base)"
    Env(EnvArgs),

    /// Set the default chain, or show it; exec says on stderr when it falls back to it
    Use(UseArgs),

    /// List chains, or show one in full with its endpoints' health
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

    /// Add a chain from chainlist (by ID or name) or by hand
    Add(AddArgs),

    /// Change a chain: endpoints, signer, explorer, name, aliases (a menu
    /// with no flags)
    Update(UpdateArgs),

    /// Remove a chain (exact name or ID) and its labels; asks first unless
    /// --yes
    Rm(RmArgs),

    /// Edit config.toml, or one chain's table, in $EDITOR (validated on save)
    Edit(EditArgs),

    /// Key references with cached addresses
    Signer {
        #[command(subcommand)]
        cmd: SignerCommand,
    },

    /// Name addresses; `label 0x…` looks one up, `… | chainz label` labels text
    #[command(args_conflicts_with_subcommands = true)]
    Label {
        #[command(subcommand)]
        cmd: Option<LabelCommand>,
        #[command(flatten)]
        args: LabelArgs,
    },

    /// Values for ${NAME} in config (kept in secrets.toml)
    Var {
        #[command(subcommand)]
        cmd: VarCommand,
    },

    /// Check config, credentials, signers and endpoints; --fix repairs
    Doctor(DoctorArgs),

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

impl Command {
    /// Whether this command keeps running (the foreground gateway, a fork
    /// supervisor, the dashboard) and owns what only `Drop` releases:
    /// serve.json, a fork's claim and port, anvil, the terminal mode.
    pub fn runs_long(&self) -> bool {
        match self {
            Command::Serve { cmd, .. } => cmd.is_none(),
            Command::Fork { cmd, .. } => cmd.is_none(),
            Command::Top(args) => !args.once,
            _ => false,
        }
    }
}

/// The context selectors shared by `exec` and `env`.
#[derive(Debug, Clone, Default, Args)]
pub struct ContextArgs {
    /// Chain name, alias, prefix, ID, or fork name
    #[arg(add = ArgValueCompleter::new(complete::context_chains))]
    pub chain: Option<String>,
    /// Use this signer instead of the chain's
    #[arg(long, add = ArgValueCompleter::new(complete::signers))]
    pub signer: Option<String>,
    /// Use this endpoint instead of the active one, or a URL for this run
    /// only (not saved)
    #[arg(long, value_name = "NAME|URL", add = ArgValueCompleter::new(complete::endpoints))]
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
    /// The command to run (after --); none opens a subshell on a terminal
    #[arg(last = true, value_name = "CMD", add = ArgValueCompleter::new(complete::exec_words))]
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
    /// Chain name, alias, prefix or ID (omitted: show the default)
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: Option<String>,
    /// Clear the default instead
    #[arg(long, conflicts_with = "chain")]
    pub clear: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Show this chain in full (name, alias, prefix or ID), with its
    /// endpoints' latest health
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: Option<String>,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Show secret values (credential-bearing URLs, header values)
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
    /// Show secret values (credential-bearing URLs, header values)
    #[arg(long)]
    pub reveal: bool,
}

#[derive(Debug, Args)]
pub struct EditArgs {
    /// Chain name, alias, prefix or ID: edit only its table (default: the
    /// whole config.toml)
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: Option<String>,
    /// Print config.toml's path instead (`doctor` lists every chainz path)
    #[arg(long, conflicts_with = "chain")]
    pub path: bool,
}

/// `label [0x…]`: a reverse lookup, or (no address) the stdin filter.
#[derive(Debug, Args)]
pub struct LabelArgs {
    /// The address to look up on every chain (omitted: rewrite addresses
    /// on stdin as name(0x…))
    #[arg(value_name = "0x…")]
    pub address: Option<String>,
    /// For stdin: the chain whose labels apply (default: the chain context,
    /// else names that hold on every chain)
    #[arg(long, conflicts_with = "address", add = ArgValueCompleter::new(complete::chains_and_forks))]
    pub chain: Option<String>,
    /// JSON output (with an address)
    #[arg(long, requires = "address")]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Check this chain only: probe, rank and tag every endpoint (default:
    /// the active endpoint of every chain)
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: Option<String>,
    /// Repair what can be repaired: file modes, literal credentials, dead
    /// active endpoints; with CHAIN, activate the best endpoint and propose
    /// a failover list
    #[arg(long)]
    pub fix: bool,
    /// Don't ask: write the proposed failover list (--fix), add every
    /// public endpoint that answered (--discover)
    #[arg(long, short)]
    pub yes: bool,
    /// With CHAIN: find public endpoints on chainlist, probe them, offer
    /// to add them
    #[arg(long, requires = "chain")]
    pub discover: bool,
    /// With --discover: re-download the chainlist
    #[arg(long, requires = "discover")]
    pub refresh: bool,
    /// Verify the built-in token registry on chain (checks only; for
    /// maintainers)
    #[arg(long, hide = true, conflicts_with_all = ["fix", "chain"])]
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

#[derive(Debug, Default, Args)]
pub struct ForkArgs {
    /// Chain to fork (`ls` and `stop` are subcommands)
    #[arg(required = true, add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: Option<String>,
    /// Fork at this block (needs an archive endpoint)
    #[arg(long)]
    pub block: Option<u64>,
    /// Pin the fork to this endpoint, or a URL (not saved), instead of the
    /// active one (with --block, instead of the first archive endpoint)
    #[arg(long, value_name = "NAME|URL", add = ArgValueCompleter::new(complete::endpoints))]
    pub rpc: Option<String>,
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

#[derive(Debug, Default, Args)]
pub struct AddArgs {
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
pub struct RmArgs {
    /// Exact chain name or ID
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: String,
    /// Remove without asking (required off a terminal)
    #[arg(long, short)]
    pub yes: bool,
}

/// `update`: one patch to one chain, applied at once.
#[derive(Debug, Default, Args)]
pub struct UpdateArgs {
    /// Chain name, alias, prefix or ID (picker if omitted on a TTY)
    #[arg(add = ArgValueCompleter::new(complete::configured_chains))]
    pub chain: Option<String>,
    /// Activate an endpoint by name or URL; a new URL is added, checked and
    /// activated
    #[arg(long, value_name = "NAME|URL", help_heading = "Endpoints", add = ArgValueCompleter::new(complete::endpoints))]
    pub rpc: Option<String>,
    /// Add an endpoint without activating it (repeatable; the name
    /// defaults to the host)
    #[arg(
        long = "add-rpc",
        value_name = "[NAME=]URL",
        help_heading = "Endpoints"
    )]
    pub add_rpcs: Vec<String>,
    /// Remove an endpoint, and its failover entry (repeatable)
    #[arg(long = "rm-rpc", value_name = "NAME", help_heading = "Endpoints", add = ArgValueCompleter::new(complete::removable_endpoints))]
    pub rm_rpcs: Vec<String>,
    /// Failover endpoints, in order
    #[arg(
        long,
        value_name = "NAME,...",
        value_delimiter = ',',
        conflicts_with = "no_failover",
        help_heading = "Endpoints",
        add = ArgValueCompleter::new(complete::endpoints)
    )]
    pub failover: Vec<String>,
    /// Clear the failover list
    #[arg(long, help_heading = "Endpoints")]
    pub no_failover: bool,
    /// Header for the --rpc endpoint, else the one --add-rpc endpoint, else
    /// the active one, as 'name: value' (repeatable)
    #[arg(
        long = "header",
        value_name = "NAME: VALUE",
        help_heading = "Endpoints"
    )]
    pub headers: Vec<String>,
    /// Drop that endpoint's headers first
    #[arg(long, help_heading = "Endpoints")]
    pub clear_headers: bool,
    /// Skip the chain-ID check of new URLs (--rpc, --add-rpc)
    #[arg(long, help_heading = "Endpoints")]
    pub no_check: bool,
    /// Attach this signer
    #[arg(long, value_name = "NAME", conflicts_with = "no_signer", help_heading = "Signer and explorer", add = ArgValueCompleter::new(complete::signers))]
    pub signer: Option<String>,
    /// Detach the signer
    #[arg(long, help_heading = "Signer and explorer")]
    pub no_signer: bool,
    /// Attach this explorer (`etherscan` is built in)
    #[arg(long, value_name = "NAME", conflicts_with = "no_explorer", help_heading = "Signer and explorer", add = ArgValueCompleter::new(complete::explorers))]
    pub explorer: Option<String>,
    /// Detach the explorer
    #[arg(long, help_heading = "Signer and explorer")]
    pub no_explorer: bool,
    /// Read the explorer's API key from stdin (one key per explorer, shared by its chains)
    #[arg(long, conflicts_with = "api_key", help_heading = "Signer and explorer")]
    pub api_key_stdin: bool,
    /// The explorer's API key as an argument (visible in history and `ps`; prefer --api-key-stdin)
    #[arg(long, hide = true, value_name = "KEY")]
    pub api_key: Option<String>,
    /// Rename the chain
    #[arg(long, value_name = "NEW", help_heading = "Naming")]
    pub name: Option<String>,
    /// Add an alias (repeatable)
    #[arg(long = "alias", value_name = "ALIAS", help_heading = "Naming")]
    pub aliases: Vec<String>,
    /// Remove an alias (repeatable)
    #[arg(long = "rm-alias", value_name = "ALIAS", help_heading = "Naming")]
    pub rm_aliases: Vec<String>,
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
    Ls(JsonArgs),
    /// Remove a signer and delete its key material (keyring entry or
    /// secrets.toml entry); asks first unless --yes
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
    #[arg(long, value_enum)]
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
pub enum LabelCommand {
    /// Name an address
    Add(LabelAddArgs),
    /// List labels
    Ls(LabelLsArgs),
    /// Remove a label; asks first unless --yes
    Rm(LabelRmArgs),
    /// Label contracts from Foundry broadcast output
    Import(LabelImportArgs),
}

#[derive(Debug, Args)]
pub struct LabelAddArgs {
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
pub struct LabelLsArgs {
    /// What @name means on this chain
    #[arg(long, add = ArgValueCompleter::new(complete::chains_and_forks))]
    pub chain: Option<String>,
    /// Include the built-in tokens on your chains
    #[arg(long)]
    pub builtin: bool,
    /// With --builtin: the built-in tokens on every chain, configured or not
    #[arg(long, requires = "builtin", conflicts_with = "chain")]
    pub all_chains: bool,
    /// JSON output
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct LabelRmArgs {
    /// Label name
    #[arg(add = ArgValueCompleter::new(complete::user_labels))]
    pub name: String,
    /// Only on this configured chain (or '*' for the every-chain entry)
    #[arg(long, add = ArgValueCompleter::new(complete::label_scopes))]
    pub chain: Option<String>,
    /// Only the every-chain entry (the same as --chain '*')
    #[arg(long, conflicts_with = "chain")]
    pub all_chains: bool,
    /// Remove without asking (required off a terminal)
    #[arg(long, short)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct LabelImportArgs {
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

impl From<&LabelImportArgs> for chainz_core::broadcast::Options {
    fn from(args: &LabelImportArgs) -> Self {
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
    /// List variables, or only the ones named (values redacted unless
    /// --reveal)
    Ls(VarLsArgs),
    /// Remove a variable and its value; asks first unless --yes
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
pub struct VarLsArgs {
    /// Only these variables
    #[arg(value_name = "NAME", add = ArgValueCompleter::new(complete::vars))]
    pub names: Vec<String>,
    /// Show secret values (the variables' values)
    #[arg(long)]
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

#[cfg(test)]
mod tests {
    use super::{HELP_GROUPS, Opt};
    use clap::{CommandFactory, Parser};

    /// Hidden flags are for maintainers: the README (for users) never
    /// mentions one.
    #[test]
    fn the_readme_documents_no_hidden_flag() {
        fn walk(command: &clap::Command, path: &str, hidden: &mut Vec<String>) {
            for arg in command.get_arguments().filter(|a| a.is_hide_set()) {
                if let Some(long) = arg.get_long() {
                    hidden.push(format!("{path} --{long}"));
                }
            }
            for sub in command.get_subcommands() {
                let path = format!("{path} {}", sub.get_name());
                walk(sub, path.trim_start(), hidden);
            }
        }
        let readme = include_str!("../../../README.md");
        let mut hidden = Vec::new();
        walk(&Opt::command(), "", &mut hidden);
        for nested in ["doctor --tokens", "signer add --key"] {
            assert!(hidden.iter().any(|h| h == nested), "{hidden:?}");
        }
        for flag in &hidden {
            assert!(
                !readme.contains(flag.as_str()),
                "README.md documents {flag}"
            );
        }
    }

    /// Every `--reveal` says the same thing, then what it shows there.
    #[test]
    fn every_reveal_reads_the_same() {
        fn walk(command: &clap::Command, path: &str, seen: &mut Vec<String>) {
            for arg in command.get_arguments() {
                if arg.get_id() == "reveal" {
                    let help = arg.get_help().map(ToString::to_string).unwrap_or_default();
                    assert!(help.starts_with("Show secret values ("), "{path}: {help}");
                    seen.push(path.to_string());
                }
            }
            for sub in command.get_subcommands() {
                walk(sub, &format!("{path} {}", sub.get_name()), seen);
            }
        }
        let mut seen = Vec::new();
        walk(&Opt::command(), "chainz", &mut seen);
        assert_eq!(seen, ["chainz ls", "chainz top", "chainz var ls"]);
    }

    /// Only the long-running commands go without the broken-pipe exit
    /// hook (it skips `Drop`).
    #[test]
    fn long_running_commands_are_the_foreground_ones() {
        let runs_long = |args: &[&str]| {
            let opt = Opt::try_parse_from([&["chainz"], args].concat()).unwrap();
            opt.cmd.as_ref().is_some_and(super::Command::runs_long)
        };
        for args in [&["serve"][..], &["fork", "eth"], &["top"]] {
            assert!(runs_long(args), "{args:?}");
        }
        for args in [
            &["serve", "status"][..],
            &["fork", "ls"],
            &["top", "--once"],
            &["ls"],
            &["exec", "eth", "--", "true"],
        ] {
            assert!(!runs_long(args), "{args:?}");
        }
    }

    /// The grouped top-level help lists every visible command exactly
    /// once, and nothing else.
    #[test]
    fn help_groups_list_every_command_once() {
        let command = Opt::command();
        let mut visible: Vec<&str> = command
            .get_subcommands()
            .filter(|c| !c.is_hide_set())
            .map(|c| c.get_name())
            .collect();
        let mut grouped: Vec<&str> = HELP_GROUPS
            .iter()
            .flat_map(|(_, names)| names.iter().copied())
            .collect();
        visible.sort_unstable();
        grouped.sort_unstable();
        assert_eq!(grouped, visible);
        let help = super::command().render_help().to_string();
        for name in &grouped {
            let lines = help
                .lines()
                .filter(|l| {
                    l.trim_start().starts_with(&format!("{name} "))
                        || l.trim_start().starts_with(&format!("{name},"))
                })
                .count();
            assert_eq!(lines, 1, "{name} in:\n{help}");
        }
        assert!(help.contains("exec, x"), "{help}");
    }

    /// `exec --help` is written by hand (it notes when each variable is
    /// set); this keeps it naming every exported variable and token.
    #[test]
    fn exec_help_names_the_whole_contract() {
        let command = super::command();
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
