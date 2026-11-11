//! `rpc …`: endpoints, probing, ranking and discovery (via chainz-rpc).

use super::{chain_arg, open};
use crate::cli::RpcCommand;
use crate::prompt::{self, is_tty};
use crate::{net, ui};
use anyhow::{Result, anyhow, bail};
use chainz_core::extract::{self, Moved, VarTable};
use chainz_core::model::Endpoint;
use chainz_core::redact::redact;
use chainz_core::{Chainz, Tx, edit, names};
use chainz_rpc::chainlist;
use chainz_rpc::{Health, HealthSample, ProbeResult, RpcClient, Target, classify, probe, rank};
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

/// Check that `endpoint` answers with chain ID `chain_id`.
pub(super) fn check(
    chainz: &Chainz,
    chain: &str,
    chain_id: u64,
    ep: &str,
    endpoint: &Endpoint,
) -> Result<()> {
    let target = net::target(chainz, chain, chain_id, ep, endpoint)?;
    net::block_on(RpcClient::new().check_chain_id(&target, chain_id))
        .map_err(|e| anyhow!("RPC check failed for {}: {e}", redact(&endpoint.url)))
}

/// Inside a transaction: move the literal secrets of `endpoints` (a known
/// URL key, credential header values) into `secrets.toml` variables,
/// rewriting the endpoints to reference them. Returns what moved.
pub(super) fn extract_secrets(
    tx: &mut Tx,
    chain: &str,
    endpoints: &mut [(String, Endpoint)],
) -> Result<Vec<Moved>> {
    let mut vars = VarTable::with_vars(&tx.secrets_view()?.vars);
    let mut moved = Vec::new();
    for (name, endpoint) in endpoints.iter_mut() {
        moved.extend(extract::extract_endpoint(chain, name, endpoint, &mut vars));
    }
    for (name, value) in vars.created_vars() {
        edit::set_var(&mut tx.secrets, name, value)?;
    }
    Ok(moved)
}

/// Say which secrets moved out of `config.toml`.
pub(super) fn report_moved(moved: &[Moved]) {
    for m in moved {
        println!("  moved the secret in {m} (secrets.toml)");
    }
}

/// Probe `urls` for chain `chain_id`. A URL that is not a valid endpoint is
/// dropped (with a warning), so each result is paired with the URL it
/// probed.
pub(super) fn probe_urls(chain_id: u64, urls: &[String]) -> Vec<(String, ProbeResult)> {
    let mut probed: Vec<String> = Vec::with_capacity(urls.len());
    let mut targets = Vec::with_capacity(urls.len());
    for url in urls {
        match Target::new(chain_id, format!("rpc{}", targets.len()), url, &[]) {
            Ok(target) => {
                probed.push(url.clone());
                targets.push(target);
            }
            Err(e) => eprintln!("{}", ui::warn(&format!("skipping {}: {e}", redact(url)))),
        }
    }
    let mut results = net::block_on(async {
        let mut rx = chainz_rpc::probe_all(&RpcClient::new(), targets, chain_id);
        let mut out: Vec<Option<ProbeResult>> = vec![None; probed.len()];
        while let Some((i, r)) = rx.recv().await {
            out[i] = Some(r);
        }
        out
    });
    probed
        .into_iter()
        .zip(results.iter_mut())
        .filter_map(|(url, result)| Some((url, result.take()?)))
        .collect()
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
        let moved = extract_secrets(tx, &chain, &mut endpoints)?;
        let [(_, endpoint)] = &endpoints;
        edit::add_endpoint(&mut tx.doc, &chain, &ep, endpoint, failover)?;
        if activate {
            edit::set_active(&mut tx.doc, &chain, &ep)?;
        }
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
    chainz.transact(|tx| edit::set_active(&mut tx.doc, &chain, name))?;
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
    chainz.transact(|tx| edit::remove_endpoint(&mut tx.doc, &chain, name))?;
    println!("Removed endpoint '{name}' from '{chain}'");
    Ok(0)
}

pub(super) struct Probed {
    pub chain: String,
    pub names: Vec<String>,
    pub results: Vec<ProbeResult>,
}

/// Probe every HTTP endpoint of `chains` concurrently.
pub(super) async fn probe_chains(
    chainz: &Chainz,
    chains: &[String],
    caps: bool,
) -> Result<Vec<Probed>> {
    let client = RpcClient::new();
    let mut out = Vec::new();
    let mut store = net::health_store();
    for chain in chains {
        let c = &chainz.config().chains[chain];
        let mut names = Vec::new();
        let mut futures = Vec::new();
        for (ep, e) in &c.rpcs {
            if e.is_websocket() {
                continue;
            }
            let target = match net::target(chainz, chain, c.id, ep, e) {
                Ok(t) => t,
                Err(err) => {
                    eprintln!("{}", ui::warn(&format!("{err:#}")));
                    continue;
                }
            };
            names.push(ep.clone());
            futures.push(probe_one(client.clone(), target, c.id, caps));
        }
        let joined = join_all(futures).await;
        let mut results = Vec::new();
        for (ep, (result, capabilities)) in names.iter().zip(joined) {
            store.record(c.id, ep, HealthSample::from_probe(&result));
            if let Some(capabilities) = capabilities {
                store.set_capabilities(c.id, ep, capabilities);
            }
            results.push(result);
        }
        out.push(Probed {
            chain: chain.clone(),
            names,
            results,
        });
    }
    if let Err(e) = store.save() {
        eprintln!(
            "{}",
            ui::warn(&format!("could not save health history: {e}"))
        );
    }
    Ok(out)
}

async fn probe_one(
    client: RpcClient,
    target: Target,
    chain_id: u64,
    caps: bool,
) -> (ProbeResult, Option<chainz_rpc::Capabilities>) {
    let result = probe(&client, &target, chain_id).await;
    let capabilities = if caps && result.is_ok() {
        Some(chainz_rpc::probe_capabilities(&client, &target).await)
    } else {
        None
    };
    (result, capabilities)
}

/// Minimal join_all over boxed futures (keeps the futures crate out).
async fn join_all<F: std::future::Future + Send + 'static>(futures: Vec<F>) -> Vec<F::Output>
where
    F::Output: Send + 'static,
{
    let mut set = tokio::task::JoinSet::new();
    for (i, f) in futures.into_iter().enumerate() {
        set.spawn(async move { (i, f.await) });
    }
    let mut out: Vec<Option<F::Output>> = (0..set.len()).map(|_| None).collect();
    while let Some(res) = set.join_next().await {
        if let Ok((i, v)) = res {
            out[i] = Some(v);
        }
    }
    out.into_iter()
        .map(|v| v.expect("every task completes"))
        .collect()
}

pub(super) fn health_label(h: &Health) -> String {
    match h {
        Health::Healthy => "ok".into(),
        Health::Lagging { behind } => format!("lag {behind}"),
        Health::Failing(class) => class.to_string(),
    }
}

fn bench(chain: Option<String>, fix: bool, yes: bool) -> Result<i32> {
    let mut chainz = open()?;
    let chains = chains_for(&chainz, chain)?;
    let probed = net::block_on(probe_chains(&chainz, &chains, true))?;
    let store = net::health_store();
    let mut changes: Vec<(String, String, Vec<String>)> = Vec::new();
    for p in &probed {
        let c = &chainz.config().chains[&p.chain];
        println!("{}", ui::section(&p.chain));
        if p.results.is_empty() {
            println!("  no HTTP endpoints to probe");
            continue;
        }
        let health = classify(&p.results);
        let p50: Vec<_> = p.names.iter().map(|n| store.p50(c.id, n, 16)).collect();
        let order = rank(&p.results, &p50);
        for &i in &order {
            let r = &p.results[i];
            let marker = if c.rpc == p.names[i] {
                "●"
            } else if c.failover.contains(&p.names[i]) {
                "◐"
            } else {
                " "
            };
            let tags = store
                .capabilities(c.id, &p.names[i])
                .map(|c| c.tags().join(" "))
                .unwrap_or_default();
            println!(
                "  {marker} {:<18} {:>6}ms  {:<10} {}",
                p.names[i],
                r.latency.as_millis(),
                health_label(&health[i]),
                ui::dim(&tags)
            );
        }
        let best = &p.names[order[0]];
        if health[order[0]] == Health::Healthy {
            // Proposed failover: the next healthy endpoints without headers
            // (never route a private header-authenticated request elsewhere).
            let proposal: Vec<String> = order[1..]
                .iter()
                .filter(|&&i| health[i] == Health::Healthy)
                .map(|&i| p.names[i].clone())
                .filter(|n| c.rpcs[n].headers.is_empty())
                .take(2)
                .collect();
            if fix {
                changes.push((p.chain.clone(), best.clone(), proposal));
            } else if *best != c.rpc {
                println!(
                    "  {}",
                    ui::dim(&format!("best: {best} (run with --fix to activate)"))
                );
            }
        }
    }
    if fix {
        let mut write_failover = false;
        for (chain, best, proposal) in &changes {
            println!(
                "{chain}: active → {best}; proposed failover: {}",
                if proposal.is_empty() {
                    "-".into()
                } else {
                    proposal.join(", ")
                }
            );
        }
        if changes.iter().any(|(_, _, p)| !p.is_empty()) {
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
            for (chain, best, proposal) in &changes {
                edit::set_active(&mut tx.doc, chain, best)?;
                if write_failover {
                    let list: Vec<String> =
                        proposal.iter().filter(|n| *n != best).cloned().collect();
                    edit::set_failover(&mut tx.doc, chain, &list)?;
                }
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
    let entry = net::block_on(chainlist::find(&net::cache_dir()?, c.id, refresh))?;
    let known: Vec<&str> = c
        .rpcs
        .values()
        .map(|e| e.url.trim_end_matches('/'))
        .collect();
    let fresh: Vec<String> = chainlist::public_rpcs(&entry)
        .into_iter()
        .filter(|u| u.starts_with("http") && !known.contains(&u.trim_end_matches('/')))
        .collect();
    if fresh.is_empty() {
        println!("Every public chainlist RPC for '{chain}' is already configured");
        return Ok(0);
    }
    let probed = probe_urls(c.id, &fresh);
    let results: Vec<ProbeResult> = probed.iter().map(|(_, r)| r.clone()).collect();
    let order = rank(&results, &vec![None; results.len()]);
    let items: Vec<String> = order
        .iter()
        .map(|&i| match &probed[i] {
            (url, r) if r.is_ok() => format!("{url}  {}ms", r.latency.as_millis()),
            (url, r) => format!(
                "{url}  {}",
                r.error
                    .as_ref()
                    .map(|e| e.class().to_string())
                    .unwrap_or_default()
            ),
        })
        .collect();
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
            edit::add_endpoint(
                &mut tx.doc,
                &chain,
                &ep,
                &Endpoint::new(url.as_str()),
                false,
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
