//! `rpc …`: endpoints, probing, ranking and discovery (via chainz-rpc).

use crate::args::parse_headers;
use crate::cli::{BenchArgs, DiscoverArgs, RpcAddArgs, RpcCommand, RpcLsArgs, RpcRmArgs};
use crate::load::open;
use crate::probe::{self, Depth, Repair, check, probe_urls};
use crate::prompt::{self, Ask, Consent, Tty};
use crate::view::EndpointView;
use crate::{net, ui};
use anyhow::{Result, bail};
use chainz_core::Chainz;
use chainz_core::model::Endpoint;
use chainz_core::resolve::{self, Policy};
use chainz_core::tx::EndpointRole;
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
    let ep = c.new_endpoint(
        &chain,
        &args.url,
        args.name.as_deref(),
        &chainz.secrets()?.vars,
    )?;
    if !args.no_check && !endpoint.is_websocket() {
        check(&chainz, &chain, c.id, &ep, &endpoint)?;
    }
    let role = if args.activate {
        EndpointRole::Active
    } else if args.failover {
        EndpointRole::Failover
    } else {
        EndpointRole::Spare
    };
    let (ep, moved) =
        chainz.transact(|tx| tx.add_endpoint_url(&chain, Some(&ep), &endpoint, role))?;
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

/// `rpc bench`: probe, rank and print each chain's endpoints. Exit 1
/// when some chain has no endpoint that answered.
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
    let mut all_down = false;
    for p in &probed {
        let c = &chainz.config().chains[&p.chain];
        if p.probes.is_empty() {
            println!("{}", ui::section(&p.chain));
            println!("  no HTTP endpoints to probe");
            continue;
        }
        let (ok, total) = probe::answered(p);
        println!(
            "{}  {}",
            ui::section(&p.chain),
            ui::dim(&format!("({ok} of {total} answered)"))
        );
        all_down |= ok == 0;
        let ranking = p.ranking(&store);
        probe::print_ranked(c, p, &ranking, &store);
        let Some(repair) = Repair::propose(&p.chain, c, p, &ranking) else {
            let fix = if args.fix { ", nothing to fix" } else { "" };
            println!(
                "  {}",
                ui::warn(&format!(
                    "no healthy endpoint on {}{fix}; check your network, or `chainz rpc discover {}`",
                    p.chain, p.chain
                ))
            );
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
    if args.fix && !repairs.is_empty() {
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
                &mut Tty,
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
    Ok(if all_down { 1 } else { 0 })
}

fn discover(args: &DiscoverArgs) -> Result<i32> {
    let mut chainz = open()?;
    let (chain, c) = resolve::configured(chainz.config(), &args.chain, Policy::Prefix)?;
    let (chain, c) = (chain.to_string(), c.clone());
    let registry = net::chainlist(&chainz_core::paths::cache_dir()?, args.refresh)?;
    let entry = registry.find(c.id)?;
    let vars = &chainz.secrets()?.vars;
    let fresh: Vec<String> = chainlist::public_rpcs(entry)
        .into_iter()
        .filter(|u| Endpoint::new(u.as_str()).is_http() && c.endpoint_at(u, vars).is_none())
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
    } else if Tty.is_interactive() {
        let defaults: Vec<bool> = (0..order.len()).map(|p| answered.contains(&p)).collect();
        Tty.multi_select("Endpoints to add", &items, &defaults)?
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
    let (added, moved) = chainz.transact(|tx| {
        let (mut added, mut moved) = (Vec::new(), Vec::new());
        for p in &picked {
            let url = &probed[order[*p]].0;
            let endpoint = Endpoint::new(url.as_str());
            let (ep, m) = tx.add_endpoint_url(&chain, None, &endpoint, EndpointRole::Spare)?;
            added.push(ep);
            moved.extend(m);
        }
        Ok((added, moved))
    })?;
    println!(
        "Added {} endpoint(s) to '{chain}': {}",
        added.len(),
        added.join(", ")
    );
    ui::report_moved(&moved);
    Ok(0)
}
