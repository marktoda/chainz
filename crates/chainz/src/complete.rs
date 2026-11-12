//! Dynamic shell completion: `CHAINZ_COMPLETE=<shell> chainz -- <words…>` prints
//! candidates, and `chainz completions <shell>` prints the loader that
//! wires a shell to that protocol (clap_complete's `CompleteEnv`).
//!
//! Hot-path rules:
//! - `main` checks [`requested`] (one env lookup) before anything else;
//!   nothing here runs unless the shell asked for completions.
//! - Completers are attached to a fresh `Opt::command()` only in completion
//!   mode ([`command`]), so the normal parse builds nothing extra.
//! - Completers read `config.toml` (and, for variable names,
//!   `secrets.toml`) directly: no v1 migration, no validation, no chmod,
//!   no key unlocking, no network. A missing config completes nothing.
//! - Every failure is an empty list; nothing is ever printed to stderr.

use crate::cli::Opt;
use chainz_core::Chainz;
use chainz_core::labels::{LabelIndex, LabelSource};
use chainz_core::model::Config;
use chainz_core::names::CONTEXT_TOKENS;
use chainz_core::resolve::{self, ChainRef};
use chainz_core::runtime::{ForkRecord, Runtime};
use clap::{Command, CommandFactory};
use clap_complete::engine::{
    ArgValueCompleter, CompletionCandidate, PathCompleter, ValueCompleter,
};
use clap_complete::env::{CompleteEnv, Shells};
use std::ffi::OsStr;
use std::io::Write;

/// The variable that switches the binary into completion mode.
pub const VAR: &str = "CHAINZ_COMPLETE";

/// Whether the shell is asking for completions (the only hot-path cost).
pub fn requested() -> bool {
    std::env::var_os(VAR).is_some_and(|v| !v.is_empty() && v != "0")
}

/// Answer the completion request on stdout and exit: 0 on success, 1
/// (silently) on any error or panic.
pub fn run() -> ! {
    std::panic::set_hook(Box::new(|_| {}));
    let ok = std::panic::catch_unwind(|| {
        let cwd = std::env::current_dir().ok();
        CompleteEnv::with_factory(command)
            .var(VAR)
            .bin("chainz")
            .try_complete(std::env::args_os(), cwd.as_deref())
            .is_ok()
    })
    .unwrap_or(false);
    let _ = std::io::stdout().flush();
    std::process::exit(if ok { 0 } else { 1 })
}

/// `chainz completions <shell>`: the loader script for `shell`.
pub fn write_loader(shell: clap_complete::Shell, out: &mut dyn Write) -> std::io::Result<()> {
    let name = shell.to_string();
    let shells = Shells::builtins();
    let completer = shells.completer(&name).ok_or_else(|| {
        std::io::Error::other(format!("dynamic completion does not support {name}"))
    })?;
    completer.write_registration(VAR, "chainz", "chainz", &completer_path(), out)
}

/// How the loader calls back into chainz: as invoked (`chainz` through
/// `$PATH`, or an absolute path when run by path), like `CompleteEnv`.
fn completer_path() -> String {
    let argv0 = std::env::args_os()
        .next()
        .unwrap_or_else(|| "chainz".into());
    let mut path = std::path::PathBuf::from(argv0);
    if path.components().count() > 1
        && let Ok(cwd) = std::env::current_dir()
    {
        path = cwd.join(path);
    }
    path.to_string_lossy().into_owned()
}

/// The CLI with value completers attached (completion mode only).
fn command() -> Command {
    annotate(Opt::command(), &mut Vec::new())
}

fn annotate(cmd: Command, path: &mut Vec<String>) -> Command {
    let here = path.join(" ");
    let mut cmd = cmd.mut_args(|arg| match completer_for(&here, arg.get_id().as_str()) {
        Some(completer) => arg.add(completer),
        None => arg,
    });
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_string())
        .collect();
    for name in names {
        path.push(name.clone());
        cmd = cmd.mut_subcommand(&name, |sub| annotate(sub, path));
        path.pop();
    }
    cmd
}

/// Which completer an argument gets, by subcommand path and argument ID.
/// New `chain`/`signer` arguments pick theirs up automatically.
fn completer_for(path: &str, id: &str) -> Option<ArgValueCompleter> {
    let completer = match (path, id) {
        // A chainlist ID or name, not a configured chain.
        ("chain add", "chain") => return None,
        ("exec" | "shell" | "env", "chain") => ArgValueCompleter::new(context_chains),
        (_, "chain") => ArgValueCompleter::new(configured_chains),
        ("exec" | "shell" | "env", "rpc") | ("rpc use" | "rpc rm", "name") => {
            ArgValueCompleter::new(endpoints)
        }
        (_, "signer") | ("signer rm" | "signer migrate", "name") => ArgValueCompleter::new(signers),
        ("addr rm", "name") => ArgValueCompleter::new(user_labels),
        ("var get" | "var rm" | "var set", "name") => ArgValueCompleter::new(vars),
        ("fork stop", "name") => ArgValueCompleter::new(fork_names),
        ("exec", "command") => ArgValueCompleter::new(exec_words),
        _ => return None,
    };
    Some(completer)
}

// ---------------------------------------------------------------------------
// Completers. Each filters by the current word's prefix itself (clap does
// not filter `ArgValueCompleter` output).

fn context_chains(current: &OsStr) -> Vec<CompletionCandidate> {
    // `exec -- @w`: with no chain typed, clap offers the word to CHAIN.
    if escaped(&preceding()) {
        return exec_words(current);
    }
    chains(current, true)
}

fn configured_chains(current: &OsStr) -> Vec<CompletionCandidate> {
    chains(current, false)
}

fn chains(current: &OsStr, with_forks: bool) -> Vec<CompletionCandidate> {
    let (Some(prefix), Some(config)) = (current.to_str(), load_config()) else {
        return Vec::new();
    };
    let mut out = Candidates::new(prefix);
    for (name, chain) in &config.chains {
        out.push(name, format!("chain {}", chain.id));
    }
    for (name, chain) in &config.chains {
        for alias in &chain.aliases {
            out.push(alias, format!("alias of {name}"));
        }
    }
    if with_forks {
        for fork in live_forks() {
            out.push(&fork.name, format!("fork of {}", fork.parent));
        }
    }
    out.done()
}

/// Endpoints of the chain typed earlier on the line, else of the
/// `$CHAINZ_CHAIN` / default chain.
fn endpoints(current: &OsStr) -> Vec<CompletionCandidate> {
    let (Some(prefix), Some(config)) = (current.to_str(), load_config()) else {
        return Vec::new();
    };
    let typed = typed_chain(&preceding());
    let runtime = Runtime::resolve().ok();
    let Ok(Some((ChainRef::Configured(name), _))) =
        resolve::select(&config, runtime.as_ref(), typed.as_deref())
    else {
        return Vec::new();
    };
    let Some(chain) = config.chains.get(&name) else {
        return Vec::new();
    };
    let mut out = Candidates::new(prefix);
    for endpoint in chain.rpcs.keys() {
        let help = if *endpoint == chain.rpc {
            format!("{name} (active)")
        } else {
            name.clone()
        };
        out.push(endpoint, help);
    }
    out.done()
}

fn signers(current: &OsStr) -> Vec<CompletionCandidate> {
    let (Some(prefix), Some(config)) = (current.to_str(), load_config()) else {
        return Vec::new();
    };
    let mut out = Candidates::new(prefix);
    for (name, signer) in &config.signers {
        out.push(name, signer.backend.to_string());
    }
    out.done()
}

fn user_labels(current: &OsStr) -> Vec<CompletionCandidate> {
    let (Some(prefix), Some(config)) = (current.to_str(), load_config()) else {
        return Vec::new();
    };
    let mut out = Candidates::new(prefix);
    for (name, scopes) in &config.addresses {
        let chains: Vec<&str> = scopes.keys().map(String::as_str).collect();
        out.push(name, chains.join(", "));
    }
    out.done()
}

fn vars(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    // Names only: values never leave this function.
    let Ok(chainz) = Chainz::peek() else {
        return Vec::new();
    };
    let Ok(secrets) = chainz.secrets() else {
        return Vec::new();
    };
    let mut out = Candidates::new(prefix);
    for name in secrets.vars.keys() {
        out.push(name, String::new());
    }
    out.done()
}

fn fork_names(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    let mut out = Candidates::new(prefix);
    for fork in all_forks() {
        out.push(&fork.name, format!("fork of {}", fork.parent));
    }
    out.done()
}

/// A word of `exec`'s command: `@tokens` when it starts with `@`, else
/// file names.
fn exec_words(current: &OsStr) -> Vec<CompletionCandidate> {
    let words = preceding();
    if let Some(candidates) = bash_colon_split(current, &words) {
        return candidates;
    }
    match current.to_str() {
        Some(word) if word.starts_with('@') => tokens(word, &words),
        _ => PathCompleter::any().complete(current),
    }
}

/// `@…` candidates: context tokens, signers, and labels on the typed
/// chain; after `@name:`, the chains `name` resolves on.
fn tokens(word: &str, words: &[String]) -> Vec<CompletionCandidate> {
    let Some(config) = load_config() else {
        return Vec::new();
    };
    let index = LabelIndex::new(&config);
    if let Some((name, _)) = word[1..].split_once(':') {
        let mut out = Candidates::new(word);
        for (chain_name, address) in label_chains(&config, &index, name) {
            out.push(&format!("@{name}:{chain_name}"), address);
        }
        return out.done();
    }
    let mut out = Candidates::new(word);
    for token in CONTEXT_TOKENS {
        out.push(&format!("@{token}"), format!("context: {token}"));
    }
    out.push("@@", "a literal @".to_string());
    for (name, signer) in &config.signers {
        out.push(&format!("@{name}"), format!("signer ({})", signer.backend));
    }
    match typed_chain_id(&config, words) {
        Some(chain_id) => {
            for label in index.labels_on(chain_id) {
                out.push(&format!("@{}", label.name), describe(&label));
            }
        }
        None => {
            for name in index.names() {
                out.push(&format!("@{name}"), "label".to_string());
            }
        }
    }
    out.done()
}

/// Bash splits words at `:` (`COMP_WORDBREAKS`), so `@usdc:ba` arrives as
/// `@usdc`, `:`, `ba` and only the last part is replaced. Answer with what
/// goes in that part.
fn bash_colon_split(current: &OsStr, words: &[String]) -> Option<Vec<CompletionCandidate>> {
    std::env::var_os("_CLAP_COMPLETE_COMP_TYPE")?;
    let current = current.to_str()?;
    let (label, prefix, chain_prefix) = match words {
        [.., label, colon] if colon == ":" && label.starts_with('@') => (label, "", current),
        [.., label] if label.starts_with('@') && current.starts_with(':') => {
            (label, ":", &current[1..])
        }
        _ => return None,
    };
    let name = &label[1..];
    let config = load_config()?;
    let index = LabelIndex::new(&config);
    let mut out = Candidates::new(chain_prefix);
    for (chain_name, address) in label_chains(&config, &index, name) {
        out.push(&chain_name, address);
    }
    Some(
        out.done()
            .into_iter()
            .map(|c| c.add_prefix(prefix))
            .collect(),
    )
}

/// Configured chains `@name` resolves on, with the address there.
fn label_chains(config: &Config, index: &LabelIndex, name: &str) -> Vec<(String, String)> {
    config
        .chains
        .iter()
        .filter_map(|(chain_name, chain)| {
            let label = index.resolve(name, chain.id).ok()?;
            Some((chain_name.clone(), label.address.to_string()))
        })
        .collect()
}

fn describe(label: &chainz_core::labels::Label) -> String {
    let kind = match label.source {
        LabelSource::User => "label",
        LabelSource::Signer => "signer",
        LabelSource::Token => "token",
    };
    format!("{kind} {}", label.address)
}

// ---------------------------------------------------------------------------
// Context: the command line and the config, read without side effects.

/// The words before the one being completed, starting with the binary.
/// The shell passes them after `--` (`CHAINZ_COMPLETE=zsh chainz -- chainz …`),
/// with the cursor word's index in `_CLAP_COMPLETE_INDEX` (bash, zsh,
/// elvish) or last (fish, powershell).
fn preceding() -> Vec<String> {
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let start = args
        .iter()
        .position(|a| a == "--")
        .map_or(args.len(), |i| i + 1);
    let words = &args[start..];
    let index = std::env::var("_CLAP_COMPLETE_INDEX")
        .ok()
        .and_then(|i| i.parse().ok())
        .unwrap_or(words.len().saturating_sub(1));
    words[..index.min(words.len())].to_vec()
}

/// Whether the words include the `--` before `exec`'s command.
fn escaped(words: &[String]) -> bool {
    words.iter().skip(1).any(|w| w == "--")
}

/// The chain argument typed so far (positional `CHAIN` or `--chain`), by
/// walking the words against the CLI definition.
fn typed_chain(words: &[String]) -> Option<String> {
    let mut root = Opt::command();
    root.build();
    let mut cmd = &root;
    let mut position = 0;
    let mut chain = None;
    let mut iter = words.iter().skip(1);
    while let Some(word) = iter.next() {
        if word == "--" {
            break;
        }
        if let Some(long) = word.strip_prefix("--") {
            let (flag, inline) = match long.split_once('=') {
                Some((flag, value)) => (flag, Some(value.to_string())),
                None => (long, None),
            };
            let Some(arg) = cmd.get_arguments().find(|a| {
                a.get_long_and_visible_aliases()
                    .is_some_and(|l| l.contains(&flag))
            }) else {
                continue;
            };
            let value = if arg.get_action().takes_values() {
                inline.or_else(|| iter.next().cloned())
            } else {
                None
            };
            if arg.get_id() == "chain" {
                chain = value;
            }
            continue;
        }
        if word.starts_with('-') && word.len() > 1 {
            continue;
        }
        if position == 0
            && let Some(sub) = cmd.find_subcommand(word)
        {
            cmd = sub;
            continue;
        }
        position += 1;
        if let Some(arg) = cmd
            .get_positionals()
            .find(|p| p.get_index() == Some(position))
            && arg.get_id() == "chain"
        {
            chain = Some(word.clone());
        }
    }
    chain
}

/// The chain ID behind the typed (or `$CHAINZ_CHAIN` / default) chain.
fn typed_chain_id(config: &Config, words: &[String]) -> Option<u64> {
    let typed = typed_chain(words);
    let runtime = Runtime::resolve().ok();
    match resolve::select(config, runtime.as_ref(), typed.as_deref()).ok()?? {
        (ChainRef::Configured(name), _) => config.chains.get(&name).map(|c| c.id),
        (ChainRef::Fork(fork), _) => Some(fork.chain_id),
    }
}

/// `config.toml`, parsed but not validated; `None` when absent or broken.
/// Never migrates v1 and never touches file modes.
fn load_config() -> Option<Config> {
    Chainz::peek()
        .ok()
        .filter(Chainz::exists)
        .map(|chainz| chainz.config().clone())
}

fn all_forks() -> Vec<ForkRecord> {
    Runtime::resolve()
        .ok()
        .and_then(|runtime| runtime.list_forks().ok())
        .unwrap_or_default()
}

fn live_forks() -> Vec<ForkRecord> {
    all_forks().into_iter().filter(|f| alive(f.pid)).collect()
}

fn alive(pid: u32) -> bool {
    i32::try_from(pid).is_ok_and(|pid| pid > 1 && crate::sys::signal(pid, 0))
}

/// Prefix-filtered, deduplicated candidates in insertion order.
struct Candidates<'a> {
    prefix: &'a str,
    seen: std::collections::HashSet<String>,
    out: Vec<CompletionCandidate>,
}

impl<'a> Candidates<'a> {
    fn new(prefix: &'a str) -> Self {
        Self {
            prefix,
            seen: Default::default(),
            out: Vec::new(),
        }
    }

    fn push(&mut self, value: &str, help: String) {
        if !value.starts_with(self.prefix) || !self.seen.insert(value.to_string()) {
            return;
        }
        let help = (!help.is_empty()).then(|| help.into());
        self.out.push(CompletionCandidate::new(value).help(help));
    }

    fn done(self) -> Vec<CompletionCandidate> {
        self.out
    }
}
