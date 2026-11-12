//! `chain …`, `ls` and `use`.

use super::rpc::{check, probe_urls, report_moved};
use super::{chain_arg, edit_until_saved, open, pick_chain};
use crate::cli::{ChainCommand, ListArgs};
use crate::prompt::{self, is_tty};
use crate::{net, ui, view};
use anyhow::{Context, Result, anyhow, bail};
use chainz_core::labels::LabelIndex;
use chainz_core::model::{Chain, Endpoint};
use chainz_core::{names, resolve};
use chainz_rpc::chainlist::{self, ChainlistEntry};
use chainz_rpc::{ProbeResult, rank};

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
            tx.set_default(None);
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
        tx.set_default(Some(&name));
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
                all[prompt::select("Chain to add", &items, 0)?].clone()
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
    chainz
        .config()
        .check_new_chain(&short, id, signer.as_deref())?;

    let mut endpoints: Vec<(String, Endpoint)> = Vec::new();
    let push = |endpoints: &mut Vec<(String, Endpoint)>, url: &str| {
        let ep = names::unique_name(&names::endpoint_name_for_url(url), |n| {
            endpoints.iter().any(|(e, _)| e == n)
        });
        endpoints.push((ep, Endpoint::new(url)));
    };
    if rpcs.is_empty() {
        let entry = entry.as_ref().expect("online path");
        // Only URLs that make valid endpoints; websockets are kept but not
        // probed.
        let public: Vec<String> = chainlist::public_rpcs(entry)
            .into_iter()
            .filter(|u| chainz_rpc::Target::new(id, "x", u, &[]).is_ok())
            .collect();
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
        let probed = probe_urls(id, &http);
        let chosen = choose_active(&probed, &entry.name)?;
        push(&mut endpoints, chosen);
        for url in public.iter().filter(|u| *u != chosen) {
            push(&mut endpoints, url);
        }
    } else {
        for url in &rpcs {
            push(&mut endpoints, url);
        }
        let (ep_name, endpoint) = &endpoints[0];
        if !no_check && !endpoint.is_websocket() {
            check(&chainz, &short, id, ep_name, endpoint)?;
        }
    }
    let active = endpoints[0].0.clone();

    let aliases: Vec<String> = entry
        .as_ref()
        .map(|e| e.name.clone())
        .filter(|n| !n.eq_ignore_ascii_case(&short))
        .into_iter()
        .filter(|alias| resolve::resolve_configured(chainz.config(), alias).is_err())
        .collect();
    let count = endpoints.len();
    let moved = chainz.transact(|tx| {
        let mut rpcs = endpoints;
        let moved = tx.extract_secrets(&short, &mut rpcs)?;
        let new = Chain {
            id,
            aliases,
            signer,
            explorer: None,
            rpc: active.clone(),
            failover: vec![],
            rpcs: rpcs.into_iter().collect(),
        };
        tx.insert_chain(&short, &new)?;
        Ok(moved)
    })?;
    println!("Added chain '{short}' ({id}) with {count} endpoint(s); active: {active}");
    report_moved(&moved);
    Ok(0)
}

/// The probed URL to make active: the fastest healthy one, or the user's
/// pick from the ranked list on a TTY.
fn choose_active<'a>(probed: &'a [(String, ProbeResult)], chain: &str) -> Result<&'a str> {
    let results: Vec<ProbeResult> = probed.iter().map(|(_, r)| r.clone()).collect();
    let order = rank(&results, &vec![None; results.len()]);
    let Some(&best) = order.first().filter(|&&i| results[i].is_ok()) else {
        bail!("no public RPC for {chain} answered; pass --rpc URL");
    };
    if !is_tty() {
        return Ok(&probed[best].0);
    }
    let items: Vec<String> = order
        .iter()
        .map(|&i| match &probed[i] {
            (url, r) if r.is_ok() => format!("{}  {}ms", view_url(url), r.latency.as_millis()),
            (url, _) => format!("{}  unreachable", view_url(url)),
        })
        .collect();
    Ok(&probed[order[prompt::select("Active RPC", &items, 0)?]].0)
}

fn view_url(url: &str) -> String {
    chainz_core::redact::redact(url)
}

fn edit_chain(query: &str) -> Result<i32> {
    let mut chainz = open()?;
    let name = chain_arg(&chainz, query)?;
    let snippet = chainz.chain_snippet(&name)?;
    let saved = edit_until_saved(&snippet.text, |edited| {
        chainz.replace_chain_snippet(&snippet, edited)
    })?;
    if saved {
        println!("Updated chain '{name}'");
    } else {
        println!("No changes");
    }
    Ok(0)
}

fn rename(query: &str, new: &str) -> Result<i32> {
    let mut chainz = open()?;
    let old = chain_arg(&chainz, query)?;
    chainz.transact(|tx| tx.rename_chain(&old, new))?;
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
    chainz.transact(|tx| tx.set_aliases(&name, &current))?;
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
    chainz.transact(|tx| tx.remove_chain(&name))?;
    println!("Removed chain '{name}'");
    Ok(0)
}
