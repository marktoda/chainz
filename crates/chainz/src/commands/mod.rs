//! Command handlers, one module per noun, routed from [`dispatch`]. A
//! handler returns the process exit code; an error prints as `Error: …`
//! and exits 1 (or the code a typed error carries, see `ui::exit_code`).

mod addr;
mod chain;
mod config;
mod doctor;
mod exec;
mod fork;
mod rpc;
mod serve;
mod signer;
mod top;
mod var;

use crate::cli::{Command, Opt};
use crate::prompt::{self, is_tty};
use crate::ui;
use anyhow::{Context, Result, anyhow, bail};
use chainz_core::resolve::{self, ChainRef, Policy, Source};
use chainz_core::runtime::Runtime;
use chainz_core::{Address, Chainz, LoadMode, StorePaths};
use clap::CommandFactory;

/// Route a parsed command line to its handler.
pub fn dispatch(opt: Opt) -> Result<i32> {
    let Some(cmd) = opt.cmd else {
        return no_command();
    };
    match cmd {
        Command::Exec(args) => exec::exec(args),
        Command::Shell { context } => exec::shell(context),
        Command::Env { context, shell } => exec::env(context, shell),
        Command::Use { chain, clear } => chain::use_default(chain, clear),
        Command::Ls(args) => chain::ls(&args),
        Command::Top { once, json, reveal } => top::run(once, json, reveal),
        Command::Serve { cmd, args } => serve::run(cmd, args),
        Command::Fork { cmd, args } => fork::run(cmd, args),
        Command::Chain { cmd } => chain::run(cmd),
        Command::Rpc { cmd } => rpc::run(cmd),
        Command::Signer { cmd } => signer::run(cmd),
        Command::Addr { cmd } => addr::run(cmd),
        Command::Label { chain } => addr::label(chain),
        Command::Var { cmd } => var::run(cmd),
        Command::Doctor { fix, tokens } => doctor::run(fix, tokens),
        Command::Config { cmd } => config::run(cmd),
        Command::Completions { shell } => {
            crate::complete::write_loader(shell, &mut std::io::stdout().lock())?;
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
    with_notices(Chainz::open()?)
}

/// Load leniently (doctor).
pub fn open_lenient() -> Result<Chainz> {
    with_notices(Chainz::open_at(StorePaths::resolve()?, LoadMode::Lenient)?)
}

/// Print `chainz`'s notices on stderr as they happen.
fn with_notices(mut chainz: Chainz) -> Result<Chainz> {
    chainz.set_notice_sink(ui::notice);
    Ok(chainz)
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
    let index = prompt::select(message, &items, 0)?;
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
    match resolve::select(chainz.config(), Some(runtime), explicit, chainz.env())? {
        Some((target, Source::Default)) => {
            note_default(target.name());
            Ok(target)
        }
        Some((target, _)) => Ok(target),
        None => pick_chain(chainz, "Select a chain").map(ChainRef::Configured),
    }
}

/// Resolve a configured chain (no forks) for management commands.
pub fn chain_arg(chainz: &Chainz, query: &str) -> Result<String> {
    resolve::configured(chainz.config(), query, Policy::Prefix).map(|(name, _)| name.to_string())
}

/// Say on stderr that the default chain applies (the user didn't name one).
pub fn note_default(chain: &str) {
    eprintln!("chainz: {chain} (default)");
}

/// The runtime registry (`$XDG_RUNTIME_DIR/chainz`).
pub fn runtime() -> Result<Runtime> {
    Runtime::resolve().context("unable to find the runtime directory")
}

/// Parse an address argument; the error names `what` it was for.
pub fn parse_address(input: &str, what: &str) -> Result<Address> {
    input
        .parse()
        .map_err(|e| anyhow!("{what} '{input}' is not an address: {e}"))
}

fn editor() -> String {
    std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string())
}

/// Open `text` in `$EDITOR` and return the edited text.
fn edit_text(text: &str, suffix: &str) -> Result<String> {
    let mut file = tempfile::Builder::new()
        .prefix("chainz-")
        .suffix(suffix)
        .tempfile()?;
    std::io::Write::write_all(&mut file, text.as_bytes())?;
    let path = file.path().to_path_buf();
    let editor = editor();
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .status()
        .with_context(|| format!("failed to start {editor}"))?;
    if !status.success() {
        bail!("{editor} exited with {status}; nothing changed");
    }
    Ok(std::fs::read_to_string(&path)?)
}

/// Edit `text` as TOML in `$EDITOR` until `save` accepts it. On a TTY a
/// rejected edit is shown and the user may edit again (from their edit);
/// off a TTY the error is returned. `Ok(false)` when nothing changed.
pub fn edit_until_saved(text: &str, mut save: impl FnMut(&str) -> Result<()>) -> Result<bool> {
    let mut current = text.to_string();
    loop {
        let edited = edit_text(&current, ".toml")?;
        if edited == current {
            return Ok(false);
        }
        match save(&edited) {
            Ok(()) => return Ok(true),
            Err(e) if is_tty() => {
                eprintln!("{}", ui::fail(&format!("{e:#}")));
                if !prompt::confirm("Edit again?", true)? {
                    return Err(ui::cancelled());
                }
                current = edited;
            }
            Err(e) => return Err(e),
        }
    }
}
