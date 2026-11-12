//! Which endpoint a fork pins ([`pin_active`], or [`find_archive`] for
//! `--block`), and through what ([`upstream`]): the endpoint itself or a
//! running gateway pinned to it.

use crate::{net, ui};
use anyhow::{Result, anyhow, bail};
use chainz_core::Chainz;
use chainz_core::model::Chain;
use chainz_core::route::RouteOpts;
use chainz_core::runtime::Runtime;
use chainz_rpc::{Capabilities, RpcClient, Target, probe_capabilities};

/// The endpoint a fork pins without `--block`: the chain's active one,
/// which must be HTTP and serve the chain's ID.
pub async fn pin_active(
    chainz: &Chainz,
    parent: &str,
    chain: &Chain,
    client: &RpcClient,
) -> Result<String> {
    let name = &chain.rpc;
    let endpoint = chain
        .rpcs
        .get(name)
        .ok_or_else(|| anyhow!("chain '{parent}' has no endpoint '{name}'"))?;
    if !endpoint.is_http() {
        bail!(
            "chain '{parent}' is on websocket endpoint '{name}', which a fork can't use; activate an HTTP endpoint with `chainz chain set {parent} --rpc NAME`"
        );
    }
    let target = net::target(chainz, parent, chain.id, name, endpoint)?;
    client
        .check_chain_id(&target, chain.id)
        .await
        .map_err(|e| anyhow!("can't fork '{parent}' from endpoint '{name}': {e}"))?;
    Ok(name.clone())
}

/// The endpoint a fork pins with `--block`: the first archive-capable HTTP
/// endpoint in [`Chain::endpoints_by_preference`] order (cached capability
/// tags first, then a fresh probe). The fork refuses to start when none
/// qualifies.
pub async fn find_archive(
    chainz: &Chainz,
    parent: &str,
    chain: &Chain,
    block: u64,
    client: &RpcClient,
) -> Result<String> {
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
            return Ok(name.clone());
        }
    }

    eprintln!(
        "{}",
        ui::dim(&format!(
            "--block {block}: probing {} endpoint(s) of '{parent}' for archive state…",
            candidates.len()
        ))
    );
    let probes = candidates.iter().map(|(_, target)| {
        let (client, target, id) = (client.clone(), target.clone(), chain.id);
        async move {
            match client.check_chain_id(&target, id).await {
                Ok(()) => Some(probe_capabilities(&client, &target).await),
                Err(_) => None,
            }
        }
    });
    let probed: Vec<Option<Capabilities>> = net::join_indexed(probes.collect()).await?;
    let mut chosen = None;
    for ((name, _), capabilities) in candidates.iter().zip(probed) {
        let Some(capabilities) = capabilities else {
            continue;
        };
        if chosen.is_none() && capabilities.archive == Some(true) {
            chosen = Some(name.clone());
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
