//! Command handlers. One module per noun; `stubs` holds the stream-owned
//! commands until their streams replace them.
//!
//! To add or fill in a handler: write `pub fn run(...) -> Result<i32>` in
//! the noun's module and route it from [`dispatch`]. Return the process
//! exit code; errors print as `Error: …` and exit 1.

mod chain;
mod config;
mod doctor;
mod exec;
mod fork;
mod rpc;
mod signer;
mod stubs;
mod var;

use crate::cli::{Command, Opt};
use crate::prompt::{Prompt, SystemPrompt, is_tty};
use anyhow::{Result, bail};
use chainz_core::resolve::{self, ChainRef, Source};
use chainz_core::runtime::Runtime;
use chainz_core::{Chainz, LoadMode, StorePaths};
use clap::CommandFactory;

/// Route a parsed command line to its handler.
pub fn dispatch(opt: Opt) -> Result<i32> {
    let Some(cmd) = opt.cmd else {
        return no_command();
    };
    match cmd {
        Command::Exec(args) => exec::exec(args),
        Command::Shell {
            chain,
            signer,
            direct,
        } => exec::shell(chain, signer, direct),
        Command::Env {
            chain,
            shell,
            signer,
            rpc,
            direct,
        } => exec::env(chain, shell, signer, rpc, direct),
        Command::Use { chain, clear } => chain::use_default(chain, clear),
        Command::Ls(args) => chain::ls(&args),
        Command::Top { once, json, reveal } => stubs::top(once, json, reveal),
        Command::Serve { cmd, args } => stubs::serve(cmd, args),
        Command::Fork { cmd, args } => fork::run(cmd, args),
        Command::Chain { cmd } => chain::run(cmd),
        Command::Rpc { cmd } => rpc::run(cmd),
        Command::Signer { cmd } => signer::run(cmd),
        Command::Addr { cmd } => stubs::addr(cmd),
        Command::Label { chain } => stubs::label(chain),
        Command::Var { cmd } => var::run(cmd),
        Command::Doctor { fix, tokens } => {
            if tokens {
                stubs::doctor_tokens()
            } else {
                doctor::run(fix)
            }
        }
        Command::Config { cmd } => config::run(cmd),
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Opt::command(), "chainz", &mut std::io::stdout());
            Ok(0)
        }
    }
}

fn no_command() -> Result<i32> {
    let chainz = open()?;
    if chainz.config().chains.is_empty() {
        println!("chainz: no chains configured yet. Quick start:");
        println!("  chainz chain add base                      # add a chain from chainlist");
        println!(
            "  chainz signer add default                  # store a key (OS keyring by default)"
        );
        println!(
            "  chainz exec base -- cast block-number      # run a tool with the chain's context"
        );
    } else {
        Opt::command().print_help()?;
    }
    Ok(0)
}

/// Load the config (strict), migrating v1 on first run.
pub fn open() -> Result<Chainz> {
    Chainz::open()
}

/// Load leniently (doctor).
pub fn open_lenient() -> Result<Chainz> {
    Chainz::open_at(StorePaths::resolve()?, LoadMode::Lenient)
}

/// Interactive picker over configured chains; errors off a TTY.
pub fn pick_chain(chainz: &Chainz, message: &str) -> Result<String> {
    let chains = &chainz.config().chains;
    if chains.is_empty() {
        bail!("no chains configured; add one with `chainz chain add`");
    }
    if !is_tty() {
        bail!(
            "no chain given: pass one, set $CHAINZ_CHAIN, or set a default with `chainz use <chain>`"
        );
    }
    let items: Vec<String> = chains
        .iter()
        .map(|(n, c)| format!("{n} ({})", c.id))
        .collect();
    let index = SystemPrompt.select(message, &items, 0)?;
    Ok(chains
        .get_index(index)
        .expect("picked from the list")
        .0
        .clone())
}

/// Resolve the chain for a context command (exec, shell, env): explicit,
/// `$CHAINZ_CHAIN`, default (with a stderr notice), then a TTY picker.
pub fn select_target(
    chainz: &Chainz,
    runtime: &Runtime,
    explicit: Option<&str>,
) -> Result<ChainRef> {
    match resolve::select(chainz.config(), Some(runtime), explicit)? {
        Some((target, Source::Default)) => {
            eprintln!("chainz: {} (default)", target.name());
            Ok(target)
        }
        Some((target, _)) => Ok(target),
        None => pick_chain(chainz, "Select a chain").map(ChainRef::Configured),
    }
}

/// Resolve a configured chain (no forks) for management commands.
pub fn chain_arg(chainz: &Chainz, query: &str) -> Result<String> {
    resolve::resolve_configured(chainz.config(), query)
}
