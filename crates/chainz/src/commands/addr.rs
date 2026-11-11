//! `addr …` and `label`: the address book. Names for addresses, per chain
//! or for every chain, and the reverse-label filter over stdin.

use super::{note_default, open, parse_address, runtime, select_target};
use crate::cli::AddrCommand;
use crate::ui;
use anyhow::{Context, Result, bail};
use chainz_core::addrbook::{self, Labeler};
use chainz_core::edit::{self, ALL_CHAINS, SetAddress};
use chainz_core::labels::{Label, LabelIndex, LabelScope, LabelSource, builtin_tokens};
use chainz_core::model::Config;
use chainz_core::resolve::{self, ChainRef, Source};
use chainz_core::{Address, names};
use serde::Serialize;
use std::io;

pub fn run(cmd: AddrCommand) -> Result<i32> {
    match cmd {
        AddrCommand::Add {
            name,
            address,
            chain,
            all_chains,
        } => add(&name, &address, chain.as_deref(), all_chains),
        AddrCommand::Ls {
            chain,
            builtin,
            json,
        } => ls(chain.as_deref(), builtin, json),
        AddrCommand::Rm { name, chain } => rm(&name, chain.as_deref()),
        AddrCommand::Find { address, json } => find(&address, json),
    }
}

/// `usdc` from `usdc`, `@usdc` or `USDC`.
fn label_name(input: &str) -> String {
    input
        .strip_prefix('@')
        .unwrap_or(input)
        .to_ascii_lowercase()
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
    let address = parse_address(address, "address")?;
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
            "{}",
            ui::warn(&format!(
                "@{name} now shadows the built-in token on {}",
                shadowed.join(", ")
            ))
        );
    }
    Ok(0)
}

#[derive(Serialize)]
struct Row {
    name: String,
    /// A chain name, or `*` for every chain.
    scope: String,
    chain_id: Option<u64>,
    address: Address,
    source: &'static str,
    /// A built-in token hidden by your own label of the same name.
    shadowed: bool,
}

impl Row {
    fn new(config: &Config, label: Label, shadowed: bool) -> Self {
        Row {
            scope: scope_label(config, label.scope),
            chain_id: match label.scope {
                LabelScope::Chain(id) => Some(id),
                LabelScope::AllChains => None,
            },
            source: source_label(label.source),
            name: label.name,
            address: label.address,
            shadowed,
        }
    }
}

fn ls(chain: Option<&str>, builtin: bool, json: bool) -> Result<i32> {
    let chainz = open()?;
    let config = chainz.config();
    let index = LabelIndex::new(config);
    let rows: Vec<Row> = match chain {
        // What `@name` means on one chain, after shadowing.
        Some(query) => {
            let target = resolve::resolve(config, Some(&runtime()?), query)?;
            index
                .labels_on(chain_id(config, &target))
                .into_iter()
                .filter(|l| builtin || l.source != LabelSource::Token)
                .map(|l| Row::new(config, l, false))
                .collect()
        }
        // Everything, in config order; built-in tokens by chain ID.
        None => index
            .labels()
            .filter(|l| builtin || l.source != LabelSource::Token)
            .map(|l| {
                let shadowed = match l.scope {
                    LabelScope::Chain(id) if l.source == LabelSource::Token => index
                        .resolve(&l.name, id)
                        .is_ok_and(|r| r.source != LabelSource::Token),
                    _ => false,
                };
                Row::new(config, l, shadowed)
            })
            .collect(),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
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
        let note = if r.shadowed { " (shadowed)" } else { "" };
        println!(
            "{:<name_width$}  {:<scope_width$}  {}  {}",
            format!("@{}", r.name),
            r.scope,
            r.address,
            ui::dim(&format!("{}{note}", r.source))
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
    // Removal is destructive: an exact name, alias or ID, never a prefix
    // (and never a fork: its labels are its parent's).
    let scope = match chain {
        None => None,
        Some(ALL_CHAINS) => Some(ALL_CHAINS.to_string()),
        Some(query) => Some(exact_chain(config, query)?),
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

fn find(address: &str, json: bool) -> Result<i32> {
    let chainz = open()?;
    let config = chainz.config();
    let address = parse_address(address, "address")?;
    let index = LabelIndex::new(config);
    let rows: Vec<Row> = addrbook::find(config, &address)
        .into_iter()
        .map(|label| {
            let shadowed = match label.scope {
                LabelScope::AllChains => false,
                LabelScope::Chain(id) => index
                    .resolve(&label.name, id)
                    .is_ok_and(|r| r.address != address),
            };
            Row::new(config, label, shadowed)
        })
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(if rows.is_empty() { 1 } else { 0 });
    }
    if rows.is_empty() {
        eprintln!("no label for {address}");
        return Ok(1);
    }
    for row in rows {
        let on = match row.chain_id {
            None => "every chain",
            Some(_) => row.scope.as_str(),
        };
        let note = if row.shadowed {
            format!(", shadowed by your @{}", row.name)
        } else {
            String::new()
        };
        println!("@{} on {on} ({}{note})", row.name, row.source);
    }
    Ok(0)
}

/// A configured chain by exact name, alias (any case) or ID.
fn exact_chain(config: &Config, query: &str) -> Result<String> {
    resolve::resolve_exact(config, query).or_else(|error| {
        config
            .chains
            .iter()
            .find(|(_, chain)| chain.aliases.iter().any(|a| a.eq_ignore_ascii_case(query)))
            .map(|(name, _)| name.clone())
            .ok_or(error)
    })
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
                note_default(target.name());
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
