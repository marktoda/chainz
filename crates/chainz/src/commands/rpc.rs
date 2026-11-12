//! `rpc …`: endpoints, probing, ranking and discovery (via chainz-rpc).

use super::{chain_arg, open};
use crate::cli::RpcCommand;
use crate::probe::{self, Depth, Repair, check, probe_urls};
use crate::prompt::{self, is_tty};
use crate::{net, ui};
use anyhow::{Result, anyhow, bail};
use chainz_core::extract::Moved;
use chainz_core::model::Endpoint;
use chainz_core::redact::{self, redact};
use chainz_core::tx::EndpointRole;
use chainz_core::{Chainz, names};
use chainz_rpc::chainlist;
use indexmap::IndexMap;
use serde::Serialize;

pub fn run(cmd: RpcCommand) -> Result<i32> {
    match cmd {
        RpcCommand::Ls {
            chain,
            json,
            reveal,
        } => ls(chain, json, reveal),
        RpcCommand::Add {
            chain,
            url,
            name,
            headers,
            failover,
            activate,
            no_check,
        } => add(&chain, &url, name, &headers, failover, activate, no_check),
        RpcCommand::Use { chain, name } => use_endpoint(&chain, &name),
        RpcCommand::Rm { chain, name } => rm(&chain, &name),
        RpcCommand::Bench { chain, fix, yes } => bench(chain, fix, yes),
        RpcCommand::Discover {
            chain,
            refresh,
            yes,
        } => discover(&chain, refresh, yes),
    }
}

fn chains_for(chainz: &Chainz, chain: Option<String>) -> Result<Vec<String>> {
    match chain {
        Some(q) => Ok(vec![chain_arg(chainz, &q)?]),
        None => Ok(chainz.config().chains.keys().cloned().collect()),
    }
}

#[derive(Serialize)]
struct EndpointRow {
    chain: String,
    name: String,
    url: String,
    /// Header names and values (values redacted unless `--reveal`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    headers: Vec<(String, String)>,
    active: bool,
    failover: bool,
    latency_ms: Option<u32>,
    head: Option<u64>,
    error: Option<String>,
    tags: Vec<String>,
}

fn ls(chain: Option<String>, json: bool, reveal: bool) -> Result<i32> {
    let chainz = open()?;
    let store = net::health_store();
    let mut rows = Vec::new();
    for name in chains_for(&chainz, chain)? {
        let c = &chainz.config().chains[&name];
        for (ep, e) in &c.rpcs {
            let latest = store.latest(c.id, ep);
            rows.push(EndpointRow {
                chain: name.clone(),
                name: ep.clone(),
                url: if reveal {
                    e.url.clone()
                } else {
                    redact(&e.url)
                },
                headers: e
                    .headers
                    .iter()
                    .map(|(k, v)| {
                        let v = if reveal {
                            v.clone()
                        } else {
                            "[REDACTED]".into()
                        };
                        (k.clone(), v)
                    })
                    .collect(),
                active: c.rpc == *ep,
                failover: c.failover.contains(ep),
                latency_ms: latest.filter(|s| s.error.is_none()).map(|s| s.latency_ms),
                head: latest.and_then(|s| s.head),
                error: latest.and_then(|s| s.error).map(|c| c.to_string()),
                tags: store
                    .capabilities(c.id, ep)
                    .map(|c| c.tags())
                    .unwrap_or_default(),
            });
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    let mut current = String::new();
    for r in rows {
        if r.chain != current {
            println!("{}", ui::section(&r.chain));
            current = r.chain.clone();
        }
        let marker = if r.active {
            "●"
        } else if r.failover {
            "◐"
        } else {
            " "
        };
        let health = match (&r.latency_ms, &r.error) {
            (Some(ms), _) => format!("{ms}ms"),
            (None, Some(e)) => e.clone(),
            _ => "-".into(),
        };
        println!(
            "  {marker} {:<18} {:<8} {} {}",
            r.name,
            health,
            r.url,
            ui::dim(&r.tags.join(" "))
        );
        for (k, v) in &r.headers {
            println!("      {}", ui::dim(&format!("{k}: {v}")));
        }
    }
    Ok(0)
}

/// Parse `--header 'name: value'`.
fn parse_headers(raw: &[String]) -> Result<IndexMap<String, String>> {
    let mut out = IndexMap::new();
    for h in raw {
        let (name, value) = h
            .split_once(':')
            .ok_or_else(|| anyhow!("header must be 'name: value' (got a value without ':')"))?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() {
            bail!("header name cannot be empty");
        }
        out.insert(name, value.trim().to_string());
    }
    Ok(out)
}

/// Say which secrets moved out of `config.toml`.
pub(super) fn report_moved(moved: &[Moved]) {
    for m in moved {
        println!("  moved the secret in {m} (secrets.toml)");
    }
}

fn add(
    chain: &str,
    url: &str,
    name: Option<String>,
    headers: &[String],
    failover: bool,
    activate: bool,
    no_check: bool,
) -> Result<i32> {
    let mut chainz = open()?;
    let chain = chain_arg(&chainz, chain)?;
    let endpoint = Endpoint {
        url: url.to_string(),
        headers: parse_headers(headers)?,
    };
    let c = &chainz.config().chains[&chain];
    let ep = match name {
        Some(n) => {
            if c.rpcs.contains_key(&n) {
                bail!("chain '{chain}' already has an endpoint named '{n}'");
            }
            n
        }
        None => names::unique_name(&names::endpoint_name_for_url(url), |n| {
            c.rpcs.contains_key(n)
        }),
    };
    if !no_check && !endpoint.is_websocket() {
        check(&chainz, &chain, c.id, &ep, &endpoint)?;
    }
    let moved = chainz.transact(|tx| {
        let mut endpoints = [(ep.clone(), endpoint)];
        let moved = tx.extract_secrets(&chain, &mut endpoints)?;
        let [(_, endpoint)] = &endpoints;
        let role = if activate {
            EndpointRole::Active
        } else if failover {
            EndpointRole::Failover
        } else {
            EndpointRole::Spare
        };
        tx.add_endpoint(&chain, &ep, endpoint, role)?;
        Ok(moved)
    })?;
    println!(
        "Added endpoint '{ep}' to '{chain}'{}",
        if activate { " (active)" } else { "" }
    );
    report_moved(&moved);
    Ok(0)
}

fn use_endpoint(chain: &str, name: &str) -> Result<i32> {
    let mut chainz = open()?;
    let chain = chain_arg(&chainz, chain)?;
    if !chainz.config().chains[&chain].rpcs.contains_key(name) {
        bail!("chain '{chain}' has no endpoint '{name}'");
    }
    chainz.transact(|tx| tx.set_active(&chain, name))?;
    println!("'{chain}' now uses '{name}'");
    Ok(0)
}

fn rm(chain: &str, name: &str) -> Result<i32> {
    let mut chainz = open()?;
    let chain = chain_arg(&chainz, chain)?;
    if chainz.config().chains[&chain].rpc == name {
        bail!(
            "'{name}' is the active endpoint of '{chain}'; switch first with `chainz rpc use {chain} <name>`"
        );
    }
    chainz.transact(|tx| tx.remove_endpoint(&chain, name))?;
    println!("Removed endpoint '{name}' from '{chain}'");
    Ok(0)
}

fn bench(chain: Option<String>, fix: bool, yes: bool) -> Result<i32> {
    let mut chainz = open()?;
    let chains = chains_for(&chainz, chain)?;
    let mut store = net::health_store();
    let probed = net::block_on(probe::probe_chains(
        &chainz,
        &chains,
        Depth::Capabilities,
        &mut store,
    ))?;
    let mut repairs: Vec<Repair> = Vec::new();
    for p in &probed {
        let c = &chainz.config().chains[&p.chain];
        println!("{}", ui::section(&p.chain));
        if p.probes.is_empty() {
            println!("  no HTTP endpoints to probe");
            continue;
        }
        let ranking = p.ranking(&store);
        probe::print_ranked(c, p, &ranking, &store);
        let Some(repair) = Repair::propose(&p.chain, c, p, &ranking) else {
            continue;
        };
        if fix {
            repairs.push(repair);
        } else if repair.active != c.rpc {
            println!(
                "  {}",
                ui::dim(&format!(
                    "best: {} (run with --fix to activate)",
                    repair.active
                ))
            );
        }
    }
    if fix {
        let mut write_failover = false;
        for r in &repairs {
            println!(
                "{}: active → {}; proposed failover: {}",
                r.chain,
                r.active,
                if r.failover.is_empty() {
                    "-".into()
                } else {
                    r.failover.join(", ")
                }
            );
        }
        if repairs.iter().any(|r| !r.failover.is_empty()) {
            if yes {
                write_failover = true;
            } else if is_tty() {
                write_failover = prompt::confirm("Write the proposed failover lists?", false)?;
            } else {
                println!(
                    "{}",
                    ui::dim("failover lists not written (pass --yes, or confirm on a TTY)")
                );
            }
        }
        chainz.transact(|tx| {
            for r in &repairs {
                r.apply(tx, write_failover)?;
            }
            Ok(())
        })?;
    }
    Ok(0)
}

fn discover(chain: &str, refresh: bool, yes: bool) -> Result<i32> {
    let mut chainz = open()?;
    let chain = chain_arg(&chainz, chain)?;
    let c = chainz.config().chains[&chain].clone();
    let registry = net::block_on(chainlist::fetch_all(&net::cache_dir()?, refresh))?;
    if let Some(warning) = &registry.cache_warning {
        eprintln!("{}", ui::warn(warning));
    }
    let entry = registry.find(c.id)?;
    let known: Vec<&str> = c
        .rpcs
        .values()
        .map(|e| e.url.trim_end_matches('/'))
        .collect();
    let fresh: Vec<String> = chainlist::public_rpcs(entry)
        .into_iter()
        .filter(|u| {
            Endpoint::new(u.as_str()).is_http() && !known.contains(&u.trim_end_matches('/'))
        })
        .collect();
    if fresh.is_empty() {
        println!("Every public chainlist RPC for '{chain}' is already configured");
        return Ok(0);
    }
    let probed = probe_urls(c.id, &fresh);
    let (ranking, items) = probe::ranked_urls(&probed);
    let order = ranking.order;
    let answered: Vec<usize> = (0..order.len())
        .filter(|&p| probed[order[p]].1.is_ok())
        .collect();
    let picked = if yes {
        answered
    } else if is_tty() {
        let defaults: Vec<bool> = (0..order.len()).map(|p| answered.contains(&p)).collect();
        prompt::multi_select("Endpoints to add", &items, &defaults)?
    } else {
        for item in &items {
            println!("{item}");
        }
        println!(
            "{}",
            ui::dim(&format!(
                "add them with `chainz rpc discover {chain} --yes`, or one with `chainz rpc add {chain} <url>`"
            ))
        );
        return Ok(0);
    };
    if picked.is_empty() {
        println!("Nothing added");
        return Ok(0);
    }
    let added = chainz.transact(|tx| {
        let mut taken: Vec<String> = c.rpcs.keys().cloned().collect();
        let mut added = Vec::new();
        for p in &picked {
            let url = &probed[order[*p]].0;
            let ep = names::unique_name(&names::endpoint_name_for_url(url), |n| {
                taken.iter().any(|t| t == n)
            });
            tx.add_endpoint(
                &chain,
                &ep,
                &Endpoint::new(url.as_str()),
                EndpointRole::Spare,
            )?;
            taken.push(ep.clone());
            added.push(ep);
        }
        Ok(added)
    })?;
    println!(
        "Added {} endpoint(s) to '{chain}': {}",
        added.len(),
        added.join(", ")
    );
    Ok(0)
}
