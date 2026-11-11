//! `chain …`, `ls` and `use`.

use super::{chain_arg, open, pick_chain};
use crate::cli::{ChainCommand, ListArgs};
use crate::prompt::{Prompt, SystemPrompt, is_tty};
use crate::{net, ui, view};
use anyhow::{Context, Result, anyhow, bail};
use chainz_core::edit::{self, NewChain};
use chainz_core::labels::LabelIndex;
use chainz_core::model::Endpoint;
use chainz_core::{names, resolve};
use chainz_rpc::chainlist::{self, ChainlistEntry};
use chainz_rpc::{RpcClient, probe_all, rank};

pub fn run(cmd: ChainCommand) -> Result<i32> {
    match cmd {
        ChainCommand::Ls(args) => ls(&args),
        ChainCommand::Add {
            chain,
            name,
            rpcs,
            signer,
            no_check,
            refresh,
        } => add(chain, name, rpcs, signer, no_check, refresh),
        ChainCommand::Show {
            chain,
            json,
            reveal,
        } => show(&chain, json, reveal),
        ChainCommand::Edit { chain } => edit_chain(&chain),
        ChainCommand::Rename { chain, new_name } => rename(&chain, &new_name),
        ChainCommand::Alias {
            chain,
            aliases,
            remove,
        } => alias(&chain, aliases, remove),
        ChainCommand::Rm { chain } => rm(&chain),
    }
}

/// `ls` / `chain ls`: config only (hot path, no network).
pub fn ls(args: &ListArgs) -> Result<i32> {
    let chainz = open()?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&view::chains(chainz.config(), args.reveal))?
        );
    } else {
        print!("{}", view::compact(chainz.config(), args.reveal));
    }
    Ok(0)
}

/// `use [CHAIN]`.
pub fn use_default(chain: Option<String>, clear: bool) -> Result<i32> {
    let mut chainz = open()?;
    if clear {
        chainz.transact(|tx| {
            edit::set_default(&mut tx.doc, None);
            Ok(())
        })?;
        println!("Default chain cleared");
        return Ok(0);
    }
    let name = match chain {
        Some(query) => chain_arg(&chainz, &query)?,
        None => pick_chain(&chainz, "Default chain")?,
    };
    chainz.transact(|tx| {
        edit::set_default(&mut tx.doc, Some(&name));
        Ok(())
    })?;
    println!("Default chain set to '{name}'");
    Ok(0)
}

fn show(query: &str, json: bool, reveal: bool) -> Result<i32> {
    let chainz = open()?;
    let name = chain_arg(&chainz, query)?;
    let config = chainz.config();
    let chain = &config.chains[&name];
    let v = view::ChainView::new(config, &name, chain, reveal);
    if json {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    let mut extra = Vec::new();
    if let Some(signer) = chain.signer.as_ref().and_then(|s| config.signers.get(s))
        && let Some(address) = signer.address
    {
        extra.push(("wallet".to_string(), address.to_string()));
    }
    let index = LabelIndex::new(config);
    let labels: Vec<String> = index
        .labels_on(chain.id)
        .into_iter()
        .filter(|l| !matches!(l.source, chainz_core::labels::LabelSource::Signer))
        .map(|l| format!("@{}", l.name))
        .collect();
    if !labels.is_empty() {
        extra.push(("labels".to_string(), labels.join(" ")));
    }
    print!("{}", view::show(&v, &extra));
    Ok(0)
}

fn find_in_chainlist<'a>(entries: &'a [ChainlistEntry], query: &str) -> Result<&'a ChainlistEntry> {
    if let Ok(id) = query.parse::<u64>() {
        return entries.iter().find(|e| e.chain_id == id).ok_or_else(|| {
            anyhow!("chain ID {id} is not on chainlist; pass --name and --rpc to add it by hand")
        });
    }
    let q = query.to_ascii_lowercase();
    if let Some(id) = names::SHORT_NAMES
        .iter()
        .find(|(_, n)| *n == q)
        .map(|(id, _)| *id)
        && let Some(e) = entries.iter().find(|e| e.chain_id == id)
    {
        return Ok(e);
    }
    if let Some(e) = entries
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case(query) || e.short_name.eq_ignore_ascii_case(query))
    {
        return Ok(e);
    }
    let matches: Vec<&ChainlistEntry> = entries
        .iter()
        .filter(|e| e.name.to_ascii_lowercase().contains(&q))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => bail!("'{query}' is not on chainlist; pass a chain ID"),
        many => bail!(
            "'{query}' matches {} chainlist entries ({}…); pass a chain ID",
            many.len(),
            many.iter()
                .take(5)
                .map(|e| format!("{} {}", e.chain_id, e.name))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn add(
    query: Option<String>,
    name: Option<String>,
    rpcs: Vec<String>,
    signer: Option<String>,
    no_check: bool,
    refresh: bool,
) -> Result<i32> {
    let mut chainz = open()?;
    let cache = net::cache_dir()?;
    // Hand-added chains with an explicit ID and RPC need no chainlist.
    let offline =
        matches!(query.as_deref().map(str::parse::<u64>), Some(Ok(_))) && !rpcs.is_empty();
    let entry: Option<ChainlistEntry> = if offline {
        let id: u64 = query.as_deref().unwrap_or_default().parse()?;
        chainlist::read_cache(&cache).and_then(|all| all.into_iter().find(|e| e.chain_id == id))
    } else {
        let all = net::block_on(chainlist::fetch_all(&cache, refresh))
            .context("chainlist is unavailable")?;
        let entry = match &query {
            Some(q) => find_in_chainlist(&all, q)?.clone(),
            None => {
                if !is_tty() {
                    bail!("usage: chainz chain add <CHAIN_ID|NAME> [--rpc URL]...");
                }
                let items: Vec<String> = all
                    .iter()
                    .map(|e| format!("{} ({})", e.name, e.chain_id))
                    .collect();
                all[SystemPrompt.select("Chain to add", &items, 0)?].clone()
            }
        };
        Some(entry)
    };
    let id = match (&entry, &query) {
        (Some(e), _) => e.chain_id,
        (None, Some(q)) => q.parse()?,
        (None, None) => unreachable!("offline requires an ID"),
    };
    let short = name
        .clone()
        .or_else(|| names::short_name(id).map(str::to_string))
        .or_else(|| entry.as_ref().map(|e| names::slugify(&e.name)))
        .ok_or_else(|| anyhow!("chain {id} has no built-in short name; pass --name"))?;
    if !names::is_valid_chain_name(&short) {
        bail!("chain name '{short}' must match [a-z0-9][a-z0-9_-]* and not be all digits");
    }
    if let Some((existing, _)) = chainz.config().chain_by_id(id) {
        bail!("chain ID {id} is already configured as '{existing}'");
    }
    if let Some(signer) = &signer
        && !chainz.config().signers.contains_key(signer)
    {
        bail!("signer '{signer}' not found; add it with `chainz signer add {signer}`");
    }

    let mut endpoints: Vec<(String, Endpoint)> = Vec::new();
    let push = |endpoints: &mut Vec<(String, Endpoint)>, url: &str| {
        let ep = names::unique_name(&names::endpoint_name_for_url(url), |n| {
            endpoints.iter().any(|(e, _)| e == n)
        });
        endpoints.push((ep, Endpoint::new(url)));
    };
    let active;
    if rpcs.is_empty() {
        let entry = entry.as_ref().expect("online path");
        let public = chainlist::public_rpcs(entry);
        let http: Vec<String> = public
            .iter()
            .filter(|u| u.starts_with("http"))
            .cloned()
            .collect();
        if http.is_empty() {
            bail!(
                "chainlist lists no public HTTP RPC for {}; pass --rpc URL",
                entry.name
            );
        }
        eprintln!(
            "{}",
            ui::dim(&format!("probing {} public RPCs…", http.len()))
        );
        let targets: Vec<_> = http
            .iter()
            .enumerate()
            .filter_map(|(i, u)| chainz_rpc::Target::new(id, format!("rpc{i}"), u, &[]).ok())
            .collect();
        let results = net::block_on(async {
            let mut rx = probe_all(&RpcClient::new(), targets, id);
            let mut out = vec![None; http.len()];
            while let Some((i, r)) = rx.recv().await {
                out[i] = Some(r);
            }
            out.into_iter()
                .map(|r| r.expect("one result per target"))
                .collect::<Vec<_>>()
        });
        let order = rank(&results, &vec![None; results.len()]);
        if !results[order[0]].is_ok() {
            bail!("no public RPC for {} answered; pass --rpc URL", entry.name);
        }
        let chosen = if is_tty() {
            let items: Vec<String> = order
                .iter()
                .map(|&i| match &results[i] {
                    r if r.is_ok() => {
                        format!("{}  {}ms", view_url(&http[i]), r.latency.as_millis())
                    }
                    _ => format!("{}  unreachable", view_url(&http[i])),
                })
                .collect();
            order[SystemPrompt.select("Active RPC", &items, 0)?]
        } else {
            order[0]
        };
        push(&mut endpoints, &http[chosen]);
        for url in public.iter().filter(|u| **u != http[chosen]) {
            push(&mut endpoints, url);
        }
        active = endpoints[0].0.clone();
    } else {
        for url in &rpcs {
            push(&mut endpoints, url);
        }
        active = endpoints[0].0.clone();
        if !no_check && !endpoints[0].1.is_websocket() {
            let (ep_name, endpoint) = &endpoints[0];
            let chain_stub = chainz_core::model::Chain {
                id,
                aliases: vec![],
                signer: None,
                explorer: None,
                rpc: ep_name.clone(),
                failover: vec![],
                rpcs: Default::default(),
            };
            let target = net::target(&chainz, &short, &chain_stub, ep_name, endpoint)?;
            net::block_on(RpcClient::new().check_chain_id(&target, id))
                .map_err(|e| anyhow!("RPC check failed for {}: {e}", view_url(&endpoint.url)))?;
        }
    }

    let aliases: Vec<String> = entry
        .as_ref()
        .map(|e| e.name.clone())
        .filter(|n| !n.eq_ignore_ascii_case(&short))
        .into_iter()
        .filter(|alias| resolve::resolve_configured(chainz.config(), alias).is_err())
        .collect();
    let count = endpoints.len();
    let new = NewChain {
        id,
        aliases,
        signer,
        explorer: None,
        rpc: active.clone(),
        failover: vec![],
        rpcs: endpoints,
    };
    chainz.transact(|tx| edit::insert_chain(&mut tx.doc, &short, &new))?;
    println!("Added chain '{short}' ({id}) with {count} endpoint(s); active: {active}");
    Ok(0)
}

fn view_url(url: &str) -> String {
    chainz_core::redact::redact(url)
}

fn editor() -> String {
    std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string())
}

/// Open `text` in `$EDITOR` and return the edited text.
pub fn edit_text(text: &str, suffix: &str) -> Result<String> {
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

fn edit_chain(query: &str) -> Result<i32> {
    let mut chainz = open()?;
    let name = chain_arg(&chainz, query)?;
    let text = std::fs::read_to_string(&chainz.paths().config)?;
    let doc: toml_edit::DocumentMut = text.parse()?;
    let mut snippet = edit::chain_snippet(&doc, &name)?;
    loop {
        let edited = edit_text(&snippet, ".toml")?;
        if edited == snippet {
            println!("No changes");
            return Ok(0);
        }
        let result = chainz.transact(|tx| {
            if tx.doc.to_string() != text {
                bail!("config.toml changed while you were editing; run the command again");
            }
            edit::replace_chain_from_snippet(&mut tx.doc, &name, &edited)
        });
        match result {
            Ok(()) => {
                println!("Updated chain '{name}'");
                return Ok(0);
            }
            Err(e) if is_tty() => {
                eprintln!("{}", ui::fail(&format!("{e:#}")));
                if !SystemPrompt.confirm("Edit again?", true)? {
                    return Err(ui::cancelled());
                }
                snippet = edited;
            }
            Err(e) => return Err(e),
        }
    }
}

fn rename(query: &str, new: &str) -> Result<i32> {
    let mut chainz = open()?;
    let old = chain_arg(&chainz, query)?;
    if !names::is_valid_chain_name(new) {
        bail!("chain name '{new}' must match [a-z0-9][a-z0-9_-]* and not be all digits");
    }
    if chainz.config().chains.contains_key(new) {
        bail!("chain '{new}' already exists");
    }
    chainz.transact(|tx| {
        edit::rename_chain(&mut tx.doc, &old, new)?;
        // The new name makes a matching alias redundant.
        let cfg = tx.config()?;
        let aliases: Vec<String> = cfg.chains[new]
            .aliases
            .iter()
            .filter(|a| !a.eq_ignore_ascii_case(new))
            .cloned()
            .collect();
        edit::set_aliases(&mut tx.doc, new, &aliases)
    })?;
    println!("Renamed chain '{old}' to '{new}'");
    Ok(0)
}

fn alias(query: &str, aliases: Vec<String>, remove: bool) -> Result<i32> {
    let mut chainz = open()?;
    let name = chain_arg(&chainz, query)?;
    let mut current = chainz.config().chains[&name].aliases.clone();
    if remove {
        let before = current.len();
        current.retain(|a| !aliases.iter().any(|r| r.eq_ignore_ascii_case(a)));
        if current.len() == before {
            bail!("chain '{name}' has none of those aliases");
        }
    } else {
        for alias in aliases {
            if !current.iter().any(|a| a.eq_ignore_ascii_case(&alias)) {
                current.push(alias);
            }
        }
    }
    chainz.transact(|tx| edit::set_aliases(&mut tx.doc, &name, &current))?;
    println!(
        "Aliases for '{name}': {}",
        if current.is_empty() {
            "-".into()
        } else {
            current.join(", ")
        }
    );
    Ok(0)
}

fn rm(query: &str) -> Result<i32> {
    let mut chainz = open()?;
    let name = resolve::resolve_exact(chainz.config(), query)?;
    chainz.transact(|tx| edit::remove_chain(&mut tx.doc, &name))?;
    println!("Removed chain '{name}'");
    Ok(0)
}
