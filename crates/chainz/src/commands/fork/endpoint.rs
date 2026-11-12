//! Which endpoint a fork pins, and through what: the endpoint itself or a
//! running gateway pinned to it.

use crate::{net, ui};
use anyhow::{Result, anyhow, bail};
use chainz_core::runtime::{Runtime, ServeInfo};
use chainz_core::{Chain, Chainz};
use chainz_rpc::{Capabilities, RpcClient, Target, probe_capabilities};

/// The one endpoint this fork uses for its whole life.
///
/// Without `block`: the chain's active endpoint. With `block`: an
/// archive-capable one (cached capability tags first, then a fresh probe),
/// preferring the active endpoint, then the failover list, then the rest.
/// The fork refuses to start when none qualifies.
pub async fn choose(
    chainz: &Chainz,
    parent: &str,
    chain: &Chain,
    block: Option<u64>,
    client: &RpcClient,
) -> Result<(String, Target)> {
    let Some(block) = block else {
        let name = &chain.rpc;
        let endpoint = chain
            .rpcs
            .get(name)
            .ok_or_else(|| anyhow!("chain '{parent}' has no endpoint '{name}'"))?;
        if endpoint.is_websocket() {
            bail!(
                "chain '{parent}' is on websocket endpoint '{name}', which a fork can't use; activate an HTTP endpoint with `chainz rpc use {parent} NAME`"
            );
        }
        let target = net::target(chainz, parent, chain.id, name, endpoint)?;
        client
            .check_chain_id(&target, chain.id)
            .await
            .map_err(|e| anyhow!("can't fork '{parent}' from endpoint '{name}': {e}"))?;
        return Ok((name.clone(), target));
    };

    let mut candidates: Vec<(String, Target)> = Vec::new();
    for name in archive_order(chain) {
        let endpoint = &chain.rpcs[&name];
        if endpoint.is_websocket() {
            continue;
        }
        match net::target(chainz, parent, chain.id, &name, endpoint) {
            Ok(target) => candidates.push((name, target)),
            Err(e) => eprintln!("{}", ui::warn(&format!("{e:#}"))),
        }
    }
    let mut store = net::health_store();
    for (name, target) in &candidates {
        let cached = store
            .capabilities(chain.id, name)
            .is_some_and(|c| c.archive == Some(true));
        if cached && client.check_chain_id(target, chain.id).await.is_ok() {
            return Ok((name.clone(), target.clone()));
        }
    }

    eprintln!(
        "{}",
        ui::dim(&format!(
            "--block {block}: probing {} endpoint(s) of '{parent}' for archive state…",
            candidates.len()
        ))
    );
    let mut probes = tokio::task::JoinSet::new();
    for (index, (_, target)) in candidates.iter().enumerate() {
        let (client, target, id) = (client.clone(), target.clone(), chain.id);
        probes.spawn(async move {
            let capabilities = match client.check_chain_id(&target, id).await {
                Ok(()) => Some(probe_capabilities(&client, &target).await),
                Err(_) => None,
            };
            (index, capabilities)
        });
    }
    let mut probed: Vec<Option<Capabilities>> = vec![None; candidates.len()];
    while let Some(joined) = probes.join_next().await {
        if let Ok((index, capabilities)) = joined {
            probed[index] = capabilities;
        }
    }
    let mut chosen = None;
    for ((name, target), capabilities) in candidates.iter().zip(probed) {
        let Some(capabilities) = capabilities else {
            continue;
        };
        if chosen.is_none() && capabilities.archive == Some(true) {
            chosen = Some((name.clone(), target.clone()));
        }
        store.set_capabilities(chain.id, name, capabilities);
    }
    let _ = store.save();
    chosen.ok_or_else(|| {
        anyhow!(
            "--block {block} needs an archive endpoint, and none of the {} HTTP endpoint(s) of '{parent}' serves historical state; add one with `chainz rpc add {parent} URL`",
            candidates.len()
        )
    })
}

/// Endpoint names in preference order: active, failover list, the rest.
fn archive_order(chain: &Chain) -> Vec<String> {
    let mut order: Vec<String> = Vec::with_capacity(chain.rpcs.len());
    let preferred = std::iter::once(&chain.rpc).chain(&chain.failover);
    for name in preferred.chain(chain.rpcs.keys()) {
        if chain.rpcs.contains_key(name) && !order.contains(name) {
            order.push(name.clone());
        }
    }
    order
}

/// The upstream the fork's proxy forwards to: `target` itself, or, when a
/// gateway is running, the gateway's URL pinned to the same endpoint (so
/// fork traffic shows in its inspector). Returns the route's name too.
pub fn route(
    runtime: &Runtime,
    parent: &str,
    chain_id: u64,
    endpoint: &str,
    target: Target,
) -> Result<(Target, &'static str)> {
    match gateway(runtime) {
        Some(info) => {
            let url = info.chain_url(parent, Some(endpoint));
            let target = Target::new(chain_id, endpoint, &url, &[])
                .map_err(|e| anyhow!("gateway URL: {e}"))?;
            Ok((target, "gateway"))
        }
        None => Ok((target, "proxy")),
    }
}

/// The gateway's discovery record, when one is running.
fn gateway(runtime: &Runtime) -> Option<ServeInfo> {
    if !runtime.serve_running() {
        return None;
    }
    runtime.read_serve_info().ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_candidates_prefer_active_then_failover() {
        let chain: Chain = chainz_core::Config::parse(
            r#"
[chains.base]
id = 8453
rpc = "b"
failover = ["c", "missing"]
rpcs.a = "https://a"
rpcs.b = "https://b"
rpcs.c = "https://c"
"#,
        )
        .unwrap()
        .chains
        .swap_remove("base")
        .unwrap();
        assert_eq!(archive_order(&chain), ["b", "c", "a"]);
    }
}
