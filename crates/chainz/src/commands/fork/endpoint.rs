//! Which endpoint a fork pins, and through what: the endpoint itself or a
//! running gateway pinned to it.

use crate::{net, ui};
use anyhow::{Result, anyhow, bail};
use chainz_core::Chainz;
use chainz_core::model::Chain;
use chainz_core::route::RouteOpts;
use chainz_core::runtime::Runtime;
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
    for (name, endpoint) in chain.endpoints_by_preference(true) {
        match net::target(chainz, parent, chain.id, name, endpoint) {
            Ok(target) => candidates.push((name.to_string(), target)),
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

/// The upstream the fork's proxy forwards to: the endpoint itself, or,
/// when a gateway is running, the gateway pinned to that endpoint (so fork
/// traffic shows in its inspector). Returns the route's name too. A gateway
/// whose record can't be read is said, and the fork goes direct.
pub fn upstream(
    chainz: &Chainz,
    runtime: &Runtime,
    parent: &str,
    endpoint: &str,
) -> Result<(Target, &'static str)> {
    let pinned = |direct| RouteOpts {
        endpoint: Some(endpoint.to_string()),
        direct,
    };
    let route = match chainz.route(parent, &pinned(false), runtime) {
        Ok(route) => route,
        Err(e) => {
            eprintln!(
                "{}",
                ui::warn(&format!("not using the running gateway: {e:#}"))
            );
            chainz.route(parent, &pinned(true), runtime)?
        }
    };
    let name = if route.via_gateway {
        "gateway"
    } else {
        "proxy"
    };
    Ok((net::to_target(&route)?, name))
}
