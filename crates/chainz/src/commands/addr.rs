//! `addr …` and `label`: the address book. Names for addresses, per chain
//! or for every chain, and the reverse-label filter over stdin.

use super::{open, select_target};
use crate::cli::AddrCommand;
use anyhow::{Context, Result, anyhow, bail};
use chainz_core::addrbook::{self, Labeler};
use chainz_core::edit::{self, ALL_CHAINS, SetAddress};
use chainz_core::labels::{Label, LabelIndex, LabelScope, LabelSource, builtin_tokens};
use chainz_core::model::Config;
use chainz_core::resolve::{self, ChainRef, Source};
use chainz_core::runtime::Runtime;
use chainz_core::{Address, names};
use std::io;

pub fn run(cmd: AddrCommand) -> Result<i32> {
    match cmd {
        AddrCommand::Add {
            name,
            address,
            chain,
            all_chains,
        } => add(&name, &address, chain.as_deref(), all_chains),
        AddrCommand::Ls { chain, builtin } => ls(chain.as_deref(), builtin),
        AddrCommand::Rm { name, chain } => rm(&name, chain.as_deref()),
        AddrCommand::Find { address } => find(&address),
    }
}

fn runtime() -> Result<Runtime> {
    Runtime::resolve().context("unable to find the runtime directory")
}

/// `usdc` from `usdc`, `@usdc` or `USDC`.
fn label_name(input: &str) -> String {
    input
        .strip_prefix('@')
        .unwrap_or(input)
        .to_ascii_lowercase()
}

fn parse_address(input: &str) -> Result<Address> {
    input
        .parse()
        .map_err(|e| anyhow!("'{input}' is not an address: {e}"))
}

/// What to call chain `id`: its configured name, else its well-known
/// short name, else the bare ID.
fn chain_label(config: &Config, id: u64) -> String {
    match config.chain_by_id(id) {
        Some((name, _)) => name.to_string(),
        None => names::short_name(id).map_or_else(|| format!("chain {id}"), str::to_string),
    }
}

fn scope_label(config: &Config, scope: LabelScope) -> String {
    match scope {
        LabelScope::AllChains => ALL_CHAINS.to_string(),
        LabelScope::Chain(id) => chain_label(config, id),
    }
}

fn source_label(source: LabelSource) -> &'static str {
    match source {
        LabelSource::User => "user",
        LabelSource::Signer => "signer",
        LabelSource::Token => "built-in",
    }
}

/// The chain ID labels resolve on for a chain reference.
fn chain_id(config: &Config, target: &ChainRef) -> u64 {
    match target {
        ChainRef::Configured(name) => config.chains[name].id,
        ChainRef::Fork(fork) => fork.chain_id,
    }
}

fn add(name: &str, address: &str, chain: Option<&str>, all_chains: bool) -> Result<i32> {
    let mut chainz = open()?;
    let name = label_name(name);
    let address = parse_address(address)?;
    // Fail on a bad name before asking which chain.
    edit::check_label_name(&name, chainz.config().signers.keys().map(String::as_str))?;
    let scope = if all_chains {
        ALL_CHAINS.to_string()
    } else {
        match select_target(&chainz, &runtime()?, chain)? {
            ChainRef::Configured(name) => name,
            // A fork has no config entry; its labels are its parent's.
            ChainRef::Fork(fork) => {
                if !chainz.config().chains.contains_key(&fork.parent) {
                    bail!(
                        "fork '{}' has no configured parent chain to label",
                        fork.name
                    );
                }
                eprintln!(
                    "chainz: labelling '{}', the parent of fork '{}'",
                    fork.parent, fork.name
                );
                fork.parent
            }
        }
    };
    let on = if scope == ALL_CHAINS {
        "on every chain".to_string()
    } else {
        format!("on {scope}")
    };
    let existing = chainz
        .config()
        .addresses
        .get(&name)
        .and_then(|entries| entries.get(&scope));
    if let Some(existing) = existing
        && existing.parse::<Address>().ok() != Some(address)
    {
        bail!(
            "@{name} is already {existing} {on}; to change it, first run `chainz addr rm {name} --chain '{scope}'`"
        );
    }
    let outcome =
        chainz.transact(|tx| edit::set_address(&mut tx.doc, &name, &scope, &address, false))?;
    match outcome {
        SetAddress::Unchanged => println!("@{name} is already {address} {on}"),
        SetAddress::Added | SetAddress::Replaced(_) => println!("Added @{name} {on}: {address}"),
    }
    let shadowed: Vec<String> = builtin_tokens()
        .iter()
        .filter(|t| t.name == name && t.address != address)
        .filter(|t| match chainz.config().chains.get(&scope) {
            Some(chain) => chain.id == t.chain_id,
            None => chainz.config().chain_by_id(t.chain_id).is_some(),
        })
        .map(|t| chain_label(chainz.config(), t.chain_id))
        .collect();
    if !shadowed.is_empty() {
        eprintln!(
            "chainz: note: @{name} now shadows the built-in token on {}",
            shadowed.join(", ")
        );
    }
    Ok(0)
}

struct Row {
    name: String,
    scope: String,
    address: Address,
    source: String,
}

fn ls(chain: Option<&str>, builtin: bool) -> Result<i32> {
    let chainz = open()?;
    let config = chainz.config();
    let index = LabelIndex::new(config);
    let row = |label: Label, note: &str| Row {
        scope: scope_label(config, label.scope),
        source: format!("{}{note}", source_label(label.source)),
        name: label.name,
        address: label.address,
    };
    let rows: Vec<Row> = match chain {
        // What `@name` means on one chain, after shadowing.
        Some(query) => {
            let target = resolve::resolve(config, Some(&runtime()?), query)?;
            index
                .labels_on(chain_id(config, &target))
                .into_iter()
                .filter(|l| builtin || l.source != LabelSource::Token)
                .map(|l| row(l, ""))
                .collect()
        }
        // Everything, in config order; built-in tokens by chain ID.
        None => addrbook::all_labels(config)
            .into_iter()
            .filter(|l| builtin || l.source != LabelSource::Token)
            .map(|l| {
                let shadowed = match l.scope {
                    LabelScope::Chain(id) if l.source == LabelSource::Token => index
                        .resolve(&l.name, id)
                        .is_ok_and(|r| r.source != LabelSource::Token),
                    _ => false,
                };
                row(l, if shadowed { " (shadowed)" } else { "" })
            })
            .collect(),
    };
    if rows.is_empty() {
        let hint = if builtin {
            ""
        } else {
            " (--builtin lists the built-in tokens)"
        };
        println!("No labels. Add one with `chainz addr add <name> <address>`{hint}.");
        return Ok(0);
    }
    let name_width = rows.iter().map(|r| r.name.len() + 1).max().unwrap_or(0);
    let scope_width = rows.iter().map(|r| r.scope.len()).max().unwrap_or(0);
    for r in rows {
        println!(
            "{:<name_width$}  {:<scope_width$}  {}  {}",
            format!("@{}", r.name),
            r.scope,
            r.address,
            crate::ui::dim(&r.source)
        );
    }
    Ok(0)
}

fn rm(name: &str, chain: Option<&str>) -> Result<i32> {
    let mut chainz = open()?;
    let config = chainz.config();
    let name = label_name(name);
    if !config.addresses.contains_key(&name) {
        if config.signers.contains_key(&name) {
            bail!("@{name} is a signer, not a label; remove it with `chainz signer rm {name}`");
        }
        if builtin_tokens().iter().any(|t| t.name == name) {
            bail!("@{name} is a built-in token; only your own labels can be removed");
        }
        let hint = names::suggest(&name, config.addresses.keys().map(String::as_str))
            .map(|s| format!(" (did you mean {s}?)"))
            .unwrap_or_default();
        bail!("label '{name}' not found{hint}");
    }
    let scope = match chain {
        None => None,
        Some(ALL_CHAINS) => Some(ALL_CHAINS.to_string()),
        Some(query) => Some(resolve::resolve_exact(config, query)?),
    };
    let removed =
        chainz.transact(|tx| edit::remove_address(&mut tx.doc, &name, scope.as_deref()))?;
    for (scope, address) in removed {
        let on = if scope == ALL_CHAINS {
            "on every chain".to_string()
        } else {
            format!("on {scope}")
        };
        println!("Removed @{name} {on} ({address})");
    }
    Ok(0)
}

fn find(address: &str) -> Result<i32> {
    let chainz = open()?;
    let config = chainz.config();
    let address = parse_address(address)?;
    let found = addrbook::find(config, &address);
    if found.is_empty() {
        eprintln!("no label for {address}");
        return Ok(1);
    }
    let index = LabelIndex::new(config);
    for label in found {
        let (on, shadowed) = match label.scope {
            LabelScope::AllChains => ("every chain".to_string(), false),
            LabelScope::Chain(id) => (
                chain_label(config, id),
                index
                    .resolve(&label.name, id)
                    .is_ok_and(|r| r.address != address),
            ),
        };
        let note = if shadowed {
            format!(", shadowed by your @{}", label.name)
        } else {
            String::new()
        };
        println!(
            "@{} on {on} ({}{note})",
            label.name,
            source_label(label.source)
        );
    }
    Ok(0)
}

/// `chainz label [--chain C]`: stdin to stdout, addresses rewritten as
/// `name(0x…)`. With no chain context, only names that hold on every
/// chain are used.
pub fn label(chain: Option<String>) -> Result<i32> {
    let chainz = open()?;
    let config = chainz.config();
    let labeler = match resolve::select(config, Some(&runtime()?), chain.as_deref())? {
        Some((target, source)) => {
            if source == Source::Default {
                eprintln!("chainz: {} (default)", target.name());
            }
            Labeler::for_chain(config, chain_id(config, &target))
        }
        None => Labeler::any_chain(config),
    };
    match labeler.filter(io::stdin().lock(), io::stdout().lock()) {
        // The reader went away (`… | chainz label | head`): not an error.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(0),
        other => other.map(|()| 0).context("label: I/O error"),
    }
}
