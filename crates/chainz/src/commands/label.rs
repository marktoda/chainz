//! `label …`: the address book. Names for addresses, per chain or for
//! every chain; `label 0x…`, the reverse lookup; and `label` alone, the
//! reverse-label filter over stdin.

use crate::args::parse_address;
use crate::cli::{
    LabelAddArgs, LabelArgs, LabelCommand, LabelImportArgs, LabelLsArgs, LabelRmArgs, usage_error,
};
use crate::load::open;
use crate::select::{Missing, NAME_A_CHAIN, note_default, select_target};
use crate::{ui, view};
use anyhow::{Context, Result, anyhow, bail};
use chainz_core::addrbook::{self, Labeler};
use chainz_core::broadcast::{self, Action, Planned};
use chainz_core::labels::{
    self, ALL_CHAINS, Label, LabelIndex, LabelScope, LabelSource, scope_phrase,
};
use chainz_core::model::Config;
use chainz_core::resolve::{self, ChainRef, Policy, Source};
use chainz_core::runtime::Runtime;
use chainz_core::tx::{Overwrite, SetAddress};
use chainz_core::{Address, names};
use serde::Serialize;
use std::io;
use std::path::PathBuf;

pub fn run(cmd: Option<LabelCommand>, args: LabelArgs) -> Result<i32> {
    match cmd {
        Some(LabelCommand::Add(args)) => add(&args),
        Some(LabelCommand::Ls(args)) => ls(&args),
        Some(LabelCommand::Rm(args)) => rm(&args),
        Some(LabelCommand::Import(args)) => import(&args),
        None => match &args.address {
            Some(address) => find(address, args.json),
            None => filter(&args),
        },
    }
}

/// `usdc` from `usdc`, `@usdc` or `USDC`.
fn label_name(input: &str) -> String {
    input
        .strip_prefix('@')
        .unwrap_or(input)
        .to_ascii_lowercase()
}

fn scope_label(config: &Config, scope: LabelScope) -> String {
    match scope {
        LabelScope::AllChains => ALL_CHAINS.to_string(),
        LabelScope::Chain(id) => config.chain_display(id),
    }
}

/// The chain ID labels resolve on for a chain reference.
fn chain_id(config: &Config, target: &ChainRef) -> Result<u64> {
    target
        .chain_id(config)
        .ok_or_else(|| anyhow!("chain '{}' is not configured", target.name()))
}

fn add(args: &LabelAddArgs) -> Result<i32> {
    let mut chainz = open()?;
    let name = label_name(&args.name);
    let address = parse_address(&args.address, "address")?;
    // Fail on a bad name before asking which chain.
    names::check_label_name(&name, chainz.config().signers.keys().map(String::as_str))?;
    let scope = if args.all_chains {
        ALL_CHAINS.to_string()
    } else {
        let off_tty = format!("pass --chain (or --all-chains), {NAME_A_CHAIN}");
        let missing = Missing {
            command: &["label", "add"],
            off_tty: &off_tty,
        };
        match select_target(
            &chainz,
            &Runtime::resolve()?,
            args.chain.as_deref(),
            &missing,
        )? {
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
    let on = scope_phrase(&scope);
    // Overwrite::Refuse: core refuses a label that holds another address
    // here, under the lock, and says how to change it.
    let outcome =
        chainz.transact(|tx| tx.set_address(&name, &scope, &address, Overwrite::Refuse))?;
    match outcome {
        SetAddress::Unchanged => println!("@{name} is already {address} {on}"),
        SetAddress::Added | SetAddress::Replaced(_) => println!("Added @{name} {on}: {address}"),
    }
    let config = chainz.config();
    let shadowed: Vec<String> = LabelIndex::new(config)
        .shadowed_tokens()
        .filter(|(t, winner)| t.name == name && winner.address == address)
        .filter(|(t, _)| match config.chains.get(&scope) {
            Some(chain) => chain.id == t.chain_id,
            None => config.chain_by_id(t.chain_id).is_some(),
        })
        .map(|(t, _)| config.chain_display(t.chain_id))
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
    source: String,
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
            source: label.source.to_string(),
            name: label.name,
            address: label.address,
            shadowed,
        }
    }
}

fn ls(args: &LabelLsArgs) -> Result<i32> {
    let (builtin, json) = (args.builtin, args.json);
    let chainz = open()?;
    let config = chainz.config();
    let index = LabelIndex::new(config);
    let rows: Vec<Row> = match &args.chain {
        // What `@name` means on one chain, after shadowing.
        Some(query) => {
            let target = resolve::resolve(config, Some(&Runtime::resolve()?), query)?;
            index
                .labels_on(chain_id(config, &target)?)
                .into_iter()
                .filter(|l| builtin || l.source != LabelSource::Token)
                .map(|l| Row::new(config, l, false))
                .collect()
        }
        // Everything, in config order; built-in tokens by chain ID, on
        // configured chains unless --all.
        None => index
            .labels()
            .filter(|l| match (l.source, l.scope) {
                (LabelSource::Token, LabelScope::Chain(id)) => {
                    builtin && (args.all_chains || config.chain_by_id(id).is_some())
                }
                (LabelSource::Token, LabelScope::AllChains) => builtin,
                _ => true,
            })
            .map(|l| {
                let shadowed = index.shadowed(&l).is_some();
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
        println!("No labels. Add one with `chainz label add <name> <address>`{hint}.");
        return Ok(0);
    }
    let cells: Vec<[String; 4]> = rows
        .iter()
        .map(|r| {
            let note = if r.shadowed { " (shadowed)" } else { "" };
            [
                format!("@{}", r.name),
                r.scope.clone(),
                r.address.to_string(),
                ui::dim(&format!("{}{note}", r.source)),
            ]
        })
        .collect();
    print!(
        "{}",
        view::table(&["NAME", "CHAIN", "ADDRESS", "SOURCE"], &cells)
    );
    Ok(0)
}

fn rm(args: &LabelRmArgs) -> Result<i32> {
    let mut chainz = open()?;
    let config = chainz.config();
    let name = label_name(&args.name);
    if !config.addresses.contains_key(&name) {
        if config.signers.contains_key(&name) {
            bail!("@{name} is a signer, not a label; remove it with `chainz signer rm {name}`");
        }
        if labels::builtin_tokens().iter().any(|t| t.name == name) {
            bail!("@{name} is a built-in token; only your own labels can be removed");
        }
        let hint = names::suggest(&name, config.addresses.keys().map(String::as_str))
            .map(|s| format!(" (did you mean {s}?)"))
            .unwrap_or_default();
        bail!("label '{name}' not found{hint}");
    }
    // Removal is destructive: an exact name, alias or ID, never a prefix
    // (and never a fork: its labels are its parent's).
    let chain = match args.all_chains {
        true => Some(ALL_CHAINS),
        false => args.chain.as_deref(),
    };
    let scope = match chain {
        None => None,
        Some(ALL_CHAINS) => Some(ALL_CHAINS.to_string()),
        Some(query) => Some(
            resolve::configured(config, query, Policy::ExactOrAlias)?
                .0
                .to_string(),
        ),
    };
    let going = chainz.preview(|tx| tx.remove_address(&name, scope.as_deref()))?;
    let entries: Vec<String> = going
        .iter()
        .map(|(scope, address)| format!("{} ({address})", scope_phrase(scope)))
        .collect();
    let mut command = format!("label rm {}", args.name);
    if args.all_chains {
        command.push_str(" --all-chains");
    } else if let Some(chain) = &args.chain {
        command.push_str(&format!(" --chain {chain}"));
    }
    crate::prompt::confirm_removal(
        &mut crate::prompt::Tty,
        args.yes,
        &crate::prompt::Removal::of(&command, &format!("@{name} {}", entries.join(", "))),
    )?;
    let removed = chainz.transact(|tx| tx.remove_address(&name, scope.as_deref()))?;
    for (scope, address) in removed {
        println!("Removed @{name} {} ({address})", scope_phrase(&scope));
    }
    Ok(0)
}

/// `label 0x…`: every label that names the address, on any chain.
fn find(address: &str, json: bool) -> Result<i32> {
    let chainz = open()?;
    let config = chainz.config();
    let address = parse_address(address, "address")?;
    let index = LabelIndex::new(config);
    let rows: Vec<Row> = addrbook::find(config, &address)
        .into_iter()
        .map(|label| {
            let shadowed = index
                .shadowed(&label)
                .is_some_and(|winner| winner.address != label.address);
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

/// One `label import` entry, as `--json` prints it.
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
            Action::Overwrite(_, old) => (None, Some(*old)),
            Action::Add(_) | Action::Unchanged => (None, None),
        };
        ImportRow {
            chain: p
                .chain
                .clone()
                .unwrap_or_else(|| config.chain_display(p.chain_id)),
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

/// `label import`: label the contracts in Foundry broadcast output. Every
/// label is written in one transaction (planned again under the lock).
fn import(args: &LabelImportArgs) -> Result<i32> {
    let (dry_run, json) = (args.dry_run, args.json);
    let options = &broadcast::Options::from(args);
    let mut chainz = open()?;
    let files = broadcast::find_runs(&args.paths)?;
    let mut runs = Vec::new();
    for file in &files {
        runs.extend(broadcast::read(file)?);
    }
    // One path for both: a dry run previews the same transaction.
    let apply = |tx: &mut chainz_core::Tx| {
        let planned = broadcast::plan(&tx.config()?, &runs, options);
        for write in planned.iter().filter_map(|p| p.action.write()) {
            tx.write_address(write)?;
        }
        Ok(planned)
    };
    let planned = broadcast::plan(chainz.config(), &runs, options);
    let planned = if !planned.iter().any(|p| p.action.write().is_some()) {
        planned
    } else if dry_run {
        chainz.preview(apply)?
    } else {
        chainz.transact(apply)?
    };
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
    let count = |f: fn(&Action) -> bool| planned.iter().filter(|p| f(&p.action)).count();
    let (added, unchanged, overwritten, skipped) = (
        count(|a| matches!(a, Action::Add(_))),
        count(|a| matches!(a, Action::Unchanged)),
        count(|a| matches!(a, Action::Overwrite(..))),
        count(|a| matches!(a, Action::Skip(_))),
    );
    let rows: Vec<[String; 4]> = planned
        .iter()
        .map(|p| {
            let result = match &p.action {
                Action::Add(_) if dry_run => "add".to_string(),
                Action::Add(_) => "added".to_string(),
                Action::Unchanged => ui::dim("unchanged"),
                Action::Overwrite(_, old) if dry_run => format!("overwrite (was {old})"),
                Action::Overwrite(_, old) => format!("overwritten (was {old})"),
                Action::Skip(why) => ui::emph(&format!("skipped: {why}")),
            };
            [
                p.chain
                    .clone()
                    .unwrap_or_else(|| config.chain_display(p.chain_id)),
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
        .filter(|p| p.action.write().is_some())
        .filter_map(|p| {
            let name = p.name.as_deref()?;
            // A per-chain label always hides the built-in token of its name.
            labels::builtin_token(p.chain_id, name)
                .is_some_and(|t| Some(t.address) != p.address)
                .then(|| format!("@{name} on {}", config.chain_display(p.chain_id)))
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

/// `chainz label [--chain C]`: stdin to stdout, addresses rewritten as
/// `name(0x…)`. With no chain context, only names that hold on every
/// chain are used. Stdin on a terminal is a usage error, not a hang.
fn filter(args: &LabelArgs) -> Result<i32> {
    if io::IsTerminal::is_terminal(&io::stdin()) {
        return Err(usage_error(
            &["label"],
            "pass an address to look up, or pipe text to label (`cast logs … | chainz label`)",
        ));
    }
    let chainz = open()?;
    let config = chainz.config();
    let runtime = Runtime::resolve()?;
    let labeler =
        match resolve::select(config, Some(&runtime), args.chain.as_deref(), chainz.env())? {
            Some((target, source)) => {
                if source == Source::Default {
                    note_default(target.name());
                }
                Labeler::for_chain(config, chain_id(config, &target)?)
            }
            None => Labeler::any_chain(config),
        };
    // A reader that went away (`… | chainz label | head`) is not an
    // error: `main` exits 0 on a broken pipe.
    labeler
        .filter(io::stdin().lock(), io::stdout().lock())
        .context("label: I/O error")?;
    Ok(0)
}
