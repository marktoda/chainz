//! `addr …` and `label`: the address book. Names for addresses, per chain
//! or for every chain, and the reverse-label filter over stdin.

use super::{note_default, open, parse_address, runtime, select_target};
use crate::cli::AddrCommand;
use crate::{ui, view};
use anyhow::{Context, Result, bail};
use chainz_core::addrbook::{self, Labeler};
use chainz_core::broadcast::{self, Action, Planned};
use chainz_core::labels::{ALL_CHAINS, Label, LabelIndex, LabelScope, LabelSource, builtin_tokens};
use chainz_core::model::Config;
use chainz_core::resolve::{self, ChainRef, Source};
use chainz_core::tx::{Overwrite, SetAddress};
use chainz_core::{Address, names};
use serde::Serialize;
use std::io;
use std::path::PathBuf;

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
        AddrCommand::Import {
            paths,
            dry_run,
            prefix,
            only,
            force,
            json,
        } => {
            let options = broadcast::Options {
                prefix: prefix.unwrap_or_default(),
                only,
                force,
            };
            import(&paths, &options, dry_run, json)
        }
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
    names::check_label_name(&name, chainz.config().signers.keys().map(String::as_str))?;
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
        chainz.transact(|tx| tx.set_address(&name, &scope, &address, Overwrite::Refuse))?;
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
    let removed = chainz.transact(|tx| tx.remove_address(&name, scope.as_deref()))?;
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

/// One `addr import` entry, as `--json` prints it.
#[derive(Serialize)]
struct ImportRow {
    /// The configured chain (else the chain's well-known name or ID).
    chain: String,
    chain_id: u64,
    name: Option<String>,
    address: Option<Address>,
    /// `added`, `unchanged`, `overwritten` or `skipped`.
    action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// The address an overwritten label held.
    #[serde(skip_serializing_if = "Option::is_none")]
    previous: Option<Address>,
    contract: Option<String>,
    file: PathBuf,
}

impl ImportRow {
    fn new(config: &Config, p: Planned) -> Self {
        let (reason, previous) = match &p.action {
            Action::Skip(why) => (Some(why.clone()), None),
            Action::Overwrite(old) => (None, Some(*old)),
            Action::Add | Action::Unchanged => (None, None),
        };
        ImportRow {
            chain: p
                .chain
                .clone()
                .unwrap_or_else(|| chain_label(config, p.chain_id)),
            chain_id: p.chain_id,
            name: p.name,
            address: p.address,
            action: p.action.as_str(),
            reason,
            previous,
            contract: p.contract,
            file: p.file,
        }
    }
}

/// `addr import`: label the contracts in Foundry broadcast output. Every
/// label is written in one transaction (planned again under the lock).
fn import(
    paths: &[PathBuf],
    options: &broadcast::Options,
    dry_run: bool,
    json: bool,
) -> Result<i32> {
    let mut chainz = open()?;
    let files = broadcast::find_runs(paths)?;
    let mut runs = Vec::new();
    for file in &files {
        runs.extend(broadcast::read(file)?);
    }
    let mut planned = broadcast::plan(chainz.config(), &runs, options);
    if !dry_run && planned.iter().any(|p| p.action.writes()) {
        planned = chainz.transact(|tx| {
            let planned = broadcast::plan(&tx.config()?, &runs, options);
            for p in planned.iter().filter(|p| p.action.writes()) {
                let (Some(name), Some(chain), Some(address)) = (&p.name, &p.chain, &p.address)
                else {
                    unreachable!("a planned write has a name, chain and address");
                };
                tx.set_address(name, chain, address, Overwrite::Replace)?;
            }
            Ok(planned)
        })?;
    }
    let config = chainz.config();
    if json {
        let rows: Vec<ImportRow> = planned
            .into_iter()
            .map(|p| ImportRow::new(config, p))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    if planned.is_empty() {
        let runs = match files.len() {
            1 => "1 run file".to_string(),
            n => format!("{n} run files"),
        };
        println!("No contracts to import in {runs}.");
        return Ok(0);
    }
    let count = |action: &str| {
        planned
            .iter()
            .filter(|p| p.action.as_str() == action)
            .count()
    };
    let (added, unchanged, overwritten, skipped) = (
        count("added"),
        count("unchanged"),
        count("overwritten"),
        count("skipped"),
    );
    let rows: Vec<[String; 4]> = planned
        .iter()
        .map(|p| {
            let result = match &p.action {
                Action::Add if dry_run => "add".to_string(),
                Action::Add => "added".to_string(),
                Action::Unchanged => ui::dim("unchanged"),
                Action::Overwrite(old) if dry_run => format!("overwrite (was {old})"),
                Action::Overwrite(old) => format!("overwritten (was {old})"),
                Action::Skip(why) => ui::emph(&format!("skipped: {why}")),
            };
            [
                p.chain
                    .clone()
                    .unwrap_or_else(|| chain_label(config, p.chain_id)),
                p.name
                    .as_ref()
                    .map_or_else(|| "-".to_string(), |n| format!("@{n}")),
                p.address.map_or_else(|| "-".to_string(), |a| a.to_string()),
                result,
            ]
        })
        .collect();
    print!(
        "{}",
        view::table(&["CHAIN", "NAME", "ADDRESS", "RESULT"], &rows)
    );
    let mut rest = Vec::new();
    if unchanged > 0 {
        rest.push(format!("{unchanged} unchanged"));
    }
    if skipped > 0 {
        rest.push(format!("{skipped} skipped"));
    }
    let rest = if rest.is_empty() {
        String::new()
    } else {
        format!("; {}", rest.join(", "))
    };
    let overwrote = |verb: &str| {
        if overwritten > 0 {
            format!(", {verb} {overwritten}")
        } else {
            String::new()
        }
    };
    if dry_run {
        println!(
            "\nDry run: would add {added}{}{rest}. Nothing written.",
            overwrote("overwrite")
        );
    } else {
        let labels = if added == 1 { "label" } else { "labels" };
        println!("\nAdded {added} {labels}{}{rest}.", overwrote("overwrote"));
    }
    let shadowed: Vec<String> = planned
        .iter()
        .filter(|p| p.action.writes())
        .filter_map(|p| {
            let name = p.name.as_deref()?;
            builtin_tokens()
                .iter()
                .any(|t| t.name == name && t.chain_id == p.chain_id && Some(t.address) != p.address)
                .then(|| format!("@{name} on {}", chain_label(config, p.chain_id)))
        })
        .collect();
    if !shadowed.is_empty() {
        eprintln!(
            "{}",
            ui::warn(&format!(
                "{} the built-in token: {}",
                if dry_run {
                    "would shadow"
                } else {
                    "now shadows"
                },
                shadowed.join(", ")
            ))
        );
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
