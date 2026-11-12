//! `rpc …`: endpoints, probing, ranking and discovery (via chainz-rpc).

use crate::args::parse_headers;
use crate::cli::{BenchArgs, DiscoverArgs, RpcAddArgs, RpcCommand, RpcLsArgs, RpcRmArgs};
use crate::load::open;
use crate::probe::{self, Depth, Repair, check, probe_urls};
use crate::prompt::{self, Ask, Consent, Tty};
use crate::view::{self, EndpointView};
use crate::{net, ui};
use anyhow::{Result, bail};
use chainz_core::model::Endpoint;
use chainz_core::resolve::{self, Policy};
use chainz_core::tx::EndpointRole;
use chainz_core::{Chainz, fmt};
use chainz_rpc::chainlist;
use serde::Serialize;
use std::fmt::Write;

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
    /// When the latest health sample was taken (unix ms).
    probed_at_ms: Option<u64>,
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
                probed_at_ms: latest.map(|s| s.at_ms),
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
    print!("{}", render(&rows, chainz_os::now_ms()));
    Ok(0)
}

/// The `rpc ls` text: per chain, a section title and an aligned table
/// (column widths shared by every chain), each endpoint's headers under
/// it; then the legend of the role markers.
fn render(rows: &[EndpointRow], now_ms: u64) -> String {
    let cells: Vec<[String; 5]> = rows
        .iter()
        .map(|r| {
            let e = &r.endpoint;
            let health = match (&r.latency_ms, &r.error) {
                (Some(ms), _) => format!("{ms}ms"),
                (None, Some(error)) => error.clone(),
                _ => "-".into(),
            };
            let checked = r.probed_at_ms.map_or_else(
                || "-".to_string(),
                |at| format!("{} ago", fmt::age(now_ms.saturating_sub(at) / 1_000)),
            );
            let url = match r.tags.is_empty() {
                true => e.url.clone(),
                false => format!("{} {}", e.url, ui::dim(&r.tags.join(" "))),
            };
            [e.marker().to_string(), e.name.clone(), health, checked, url]
        })
        .collect();
    let lines = view::table_lines(&["", "NAME", "HEALTH", "CHECKED", "URL"], &cells);
    let (header, lines) = lines.split_first().expect("a header line");
    let mut out = String::new();
    let mut current = "";
    for (r, line) in rows.iter().zip(lines) {
        if r.chain != current {
            let _ = writeln!(out, "{}\n  {header}", ui::section(&r.chain));
            current = &r.chain;
        }
        let _ = writeln!(out, "  {line}");
        for (k, v) in &r.endpoint.headers {
            let _ = writeln!(out, "      {}", ui::dim(&format!("{k}: {v}")));
        }
    }
    if !rows.is_empty() {
        let _ = writeln!(out, "\n{}", ui::dim(view::LEGEND));
    }
    out
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
    if !c.rpcs.contains_key(name) {
        bail!("chain '{chain}' has no endpoint '{name}'");
    }
    let chain = chain.to_string();
    prompt::confirm_removal(
        &mut Tty,
        args.yes,
        &format!("rpc rm {} {name}", args.chain),
        &format!("endpoint '{name}' from '{chain}'"),
    )?;
    let failover = c.failover.clone();
    chainz.transact(|tx| tx.remove_endpoint(&chain, name))?;
    println!("Removed endpoint '{name}' from '{chain}'");
    if failover.contains(name) {
        let rest: Vec<&str> = failover
            .iter()
            .filter(|f| *f != name)
            .map(String::as_str)
            .collect();
        println!(
            "  also removed from failover (now: {})",
            if rest.is_empty() {
                "none".to_string()
            } else {
                rest.join(", ")
            }
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use chainz_core::model::Chain;

    fn row(chain: &Chain, name: &str, probed_at_ms: Option<u64>) -> EndpointRow {
        EndpointRow {
            chain: "eth".into(),
            endpoint: EndpointView::new(name, &chain.rpcs[name], chain, false),
            latency_ms: probed_at_ms.map(|_| 42),
            head: None,
            probed_at_ms,
            error: None,
            tags: vec![],
        }
    }

    #[test]
    fn long_names_keep_the_columns_aligned() {
        console::set_colors_enabled(false);
        let mut rpcs = indexmap::IndexMap::new();
        for name in ["a", "gw-uniswap-internal-prod"] {
            rpcs.insert(
                name.to_string(),
                Endpoint::new(format!("https://{name}.example")),
            );
        }
        let chain = Chain {
            id: 1,
            aliases: vec![],
            signer: None,
            explorer: None,
            rpc: "a".into(),
            failover: vec![],
            rpcs,
        };
        let now = 1_000_000_000;
        let rows = [
            row(&chain, "a", Some(now - 120_000)),
            row(&chain, "gw-uniswap-internal-prod", None),
        ];
        assert_eq!(
            render(&rows, now),
            "\neth
     NAME                      HEALTH  CHECKED  URL
  ●  a                         42ms    2m ago   https://a.example/
     gw-uniswap-internal-prod  -       -        https://gw-uniswap-internal-prod.example/

● active  ◐ failover
"
        );
    }
}
