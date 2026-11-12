//! `rpc …`: endpoints, probing, ranking and discovery (via chainz-rpc).

use crate::args::parse_headers;
use crate::cli::{BenchArgs, DiscoverArgs, RpcAddArgs, RpcCommand, RpcLsArgs, RpcRmArgs};
use crate::load::open;
use crate::probe::{self, Depth, Repair, check, probe_urls};
use crate::prompt::{self, Consent, is_tty};
use crate::view::EndpointView;
use crate::{net, ui};
use anyhow::{Result, bail};
use chainz_core::model::Endpoint;
use chainz_core::resolve::{self, Policy};
use chainz_core::tx::EndpointRole;
use chainz_core::{Chainz, names};
use chainz_rpc::chainlist;
use serde::Serialize;

pub fn run(cmd: RpcCommand) -> Result<i32> {
    match cmd {
        RpcCommand::Ls(args) => ls(&args),
        RpcCommand::Add(args) => add(&args),
        RpcCommand::Rm(args) => rm(&args),
        RpcCommand::Bench(args) => bench(&args),
        RpcCommand::Discover(args) => discover(&args),
    }
}

/// The chains a command covers: the one named, else every chain.
fn chains_for(chainz: &Chainz, chain: Option<&str>) -> Result<Vec<String>> {
    match chain {
        Some(q) => Ok(vec![
            resolve::configured(chainz.config(), q, Policy::Prefix)?
                .0
                .to_string(),
        ]),
        None => Ok(chainz.config().chains.keys().cloned().collect()),
    }
}

/// One `rpc ls` row: the endpoint as every listing shows it, plus its
/// latest health.
#[derive(Serialize)]
struct EndpointRow {
    chain: String,
    #[serde(flatten)]
    endpoint: EndpointView,
    latency_ms: Option<u32>,
    head: Option<u64>,
    error: Option<String>,
    tags: Vec<String>,
}

fn ls(args: &RpcLsArgs) -> Result<i32> {
    let chainz = open()?;
    let store = net::health_store();
    let mut rows = Vec::new();
    for name in chains_for(&chainz, args.chain.as_deref())? {
        let c = &chainz.config().chains[&name];
        for (ep, e) in &c.rpcs {
            let latest = store.latest(c.id, ep);
            rows.push(EndpointRow {
                chain: name.clone(),
                endpoint: EndpointView::new(ep, e, c, args.reveal),
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
    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    let mut current = String::new();
    for r in rows {
        if r.chain != current {
            println!("{}", ui::section(&r.chain));
            current = r.chain.clone();
        }
        let health = match (&r.latency_ms, &r.error) {
            (Some(ms), _) => format!("{ms}ms"),
            (None, Some(e)) => e.clone(),
            _ => "-".into(),
        };
        let e = &r.endpoint;
        println!(
            "  {} {:<18} {:<8} {} {}",
            e.marker(),
            e.name,
            health,
            e.url,
            ui::dim(&r.tags.join(" "))
        );
        for (k, v) in &e.headers {
            println!("      {}", ui::dim(&format!("{k}: {v}")));
        }
    }
    Ok(0)
}

fn add(args: &RpcAddArgs) -> Result<i32> {
    let mut chainz = open()?;
    let (chain, c) = resolve::configured(chainz.config(), &args.chain, Policy::Prefix)?;
    let (chain, c) = (chain.to_string(), c.clone());
    let endpoint = Endpoint {
        url: args.url.clone(),
        headers: parse_headers(&args.headers)?.into_iter().collect(),
    };
    let ep = match &args.name {
        Some(n) => {
            if c.rpcs.contains_key(n) {
                bail!("chain '{chain}' already has an endpoint named '{n}'");
            }
            n.clone()
        }
        None => names::unique_name(&names::endpoint_name_for_url(&args.url), |n| {
            c.rpcs.contains_key(n)
        }),
    };
    if !args.no_check && !endpoint.is_websocket() {
        check(&chainz, &chain, c.id, &ep, &endpoint)?;
    }
    let moved = chainz.transact(|tx| {
        let mut endpoints = [(ep.clone(), endpoint)];
        let moved = tx.extract_secrets(&chain, &mut endpoints)?;
        let [(_, endpoint)] = &endpoints;
        let role = if args.activate {
            EndpointRole::Active
        } else if args.failover {
            EndpointRole::Failover
        } else {
            EndpointRole::Spare
        };
        tx.add_endpoint(&chain, &ep, endpoint, role)?;
        Ok(moved)
    })?;
    println!(
        "Added endpoint '{ep}' to '{chain}'{}",
        if args.activate { " (active)" } else { "" }
    );
    ui::report_moved(&moved);
    Ok(0)
}

fn rm(args: &RpcRmArgs) -> Result<i32> {
    let mut chainz = open()?;
    let (chain, c) = resolve::configured(chainz.config(), &args.chain, Policy::Prefix)?;
    let name = &args.name;
    if c.rpc == *name {
        bail!(
            "'{name}' is the active endpoint of '{chain}'; switch first with `chainz chain set {chain} --rpc <name>`"
        );
    }
    let chain = chain.to_string();
    chainz.transact(|tx| tx.remove_endpoint(&chain, name))?;
    println!("Removed endpoint '{name}' from '{chain}'");
    Ok(0)
}

fn bench(args: &BenchArgs) -> Result<i32> {
    let mut chainz = open()?;
    let chains = chains_for(&chainz, args.chain.as_deref())?;
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
        if args.fix {
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
    if args.fix {
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
            write_failover = prompt::confirm_or_yes(
                args.yes,
                "Write the proposed failover lists?",
                Consent::Proposal {
                    note: "failover lists not written (pass --yes, or confirm on a TTY)",
                },
            )?;
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

fn discover(args: &DiscoverArgs) -> Result<i32> {
    let mut chainz = open()?;
    let (chain, c) = resolve::configured(chainz.config(), &args.chain, Policy::Prefix)?;
    let (chain, c) = (chain.to_string(), c.clone());
    let registry = net::block_on(chainlist::fetch_all(
        &chainz_core::paths::cache_dir()?,
        args.refresh,
    ))?;
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
    let picked = if args.yes {
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
