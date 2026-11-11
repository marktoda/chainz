//! Balances: every signer's native and built-in token balances on a chain
//! in one request. Multicall3 `aggregate3` where it is deployed, a JSON-RPC
//! batch of `eth_getBalance` and `eth_call` where it is not (or fails).
//! Only cached addresses are used; no key is ever unlocked.

use std::collections::HashSet;
use std::sync::Arc;

use chainz_core::Address;
use chainz_rpc::{BlockId, RpcError, Target};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;

use super::{Shared, Signals, abi, provider_slots, same_source};
use crate::model::{Fault, Holding, Sheet, Update, Via, now_ms};
use crate::plan::{ChainPlan, Plan};

/// Read every chain's balances every `balance_every`, and on a refresh.
///
/// Chains that share a provider are staggered. The first round also waits
/// for the head pollers' own staggered start to pass. When the plan
/// changes, only the chains it changed are read at once (all of them when
/// the signers changed); the others keep their schedule.
pub(super) async fn run(
    shared: Arc<Shared>,
    mut plan: watch::Receiver<Arc<Plan>>,
    signals: Arc<Signals>,
) {
    let stagger = shared.tuning.stagger;
    let mut current = plan.borrow_and_update().clone();
    let slots = provider_slots(&current);
    tokio::time::sleep(stagger * (slots.iter().max().copied().unwrap_or(0) + 1)).await;
    // `None`: every chain, and the interval restarts.
    let mut only: Option<HashSet<String>> = None;
    let mut next_round = Instant::now();
    loop {
        if only.is_none() {
            next_round = Instant::now() + shared.tuning.balance_every;
        }
        read_round(&shared, &current, only.as_ref()).await;
        tokio::select! {
            () = tokio::time::sleep_until(next_round) => only = None,
            () = signals.balances.notified() => only = None,
            changed = plan.changed() => {
                if changed.is_err() {
                    return;
                }
                let next = plan.borrow_and_update().clone();
                only = changed_chains(&current, &next);
                current = next;
            }
        }
    }
}

/// The chains of `next` whose balances `previous` does not answer for;
/// `None` when the signers changed, which affects every chain.
fn changed_chains(previous: &Plan, next: &Plan) -> Option<HashSet<String>> {
    if previous.holders() != next.holders() {
        return None;
    }
    let changed = next
        .chains
        .iter()
        .filter(|c| !previous.chain(&c.name).is_some_and(|p| same_source(p, c)))
        .map(|c| c.name.clone())
        .collect();
    Some(changed)
}

/// One round over `plan`'s chains (only those in `only`, when given).
async fn read_round(shared: &Arc<Shared>, plan: &Plan, only: Option<&HashSet<String>>) {
    let holders = Arc::new(plan.holders());
    if holders.is_empty() {
        return;
    }
    let mut round = JoinSet::new();
    for (chain, slot) in plan.chains.iter().zip(provider_slots(plan)) {
        if only.is_some_and(|only| !only.contains(&chain.name)) {
            continue;
        }
        let shared = shared.clone();
        let update = read_update(shared.clone(), chain.clone(), holders.clone());
        let delay = shared.tuning.stagger * slot;
        round.spawn(async move {
            tokio::time::sleep(delay).await;
            shared.send(update.await);
        });
    }
    while round.join_next().await.is_some() {}
}

/// One chain's balances, as an [`Update`].
pub(super) async fn read_update(
    shared: Arc<Shared>,
    chain: ChainPlan,
    holders: Arc<Vec<(String, Address)>>,
) -> Update {
    // Two requests at worst: the multicall, then its fallback.
    let deadline = shared.tuning.request_timeout * 2;
    let result = match shared.route(&chain).0 {
        Err(fault) => Err(fault),
        Ok(target) => tokio::time::timeout(deadline, read(&shared, &chain, &target, &holders))
            .await
            .unwrap_or(Err(RpcError::Timeout))
            .map_err(|e| Fault::from(&e)),
    };
    Update::Balances {
        chain: chain.name,
        at_ms: now_ms(),
        result,
    }
}

async fn read(
    shared: &Shared,
    chain: &ChainPlan,
    target: &Target,
    holders: &[(String, Address)],
) -> Result<Sheet, RpcError> {
    let known_absent = || {
        let known = shared.multicall.lock().unwrap_or_else(|e| e.into_inner());
        known.get(&chain.id) == Some(&false)
    };
    if !known_absent() {
        match multicall(shared, chain, target, holders).await {
            Ok(Some(values)) => return Ok(sheet(Via::Multicall3, chain, holders, values)),
            // An empty return means no code at the Multicall3 address:
            // remember it, so later rounds go straight to the batch.
            Ok(None) => {
                let mut known = shared.multicall.lock().unwrap_or_else(|e| e.into_inner());
                known.insert(chain.id, false);
            }
            // An endpoint that is unreachable or rate limiting will not
            // answer the batch either; do not hit it twice.
            Err(e @ (RpcError::Timeout | RpcError::Connect | RpcError::RateLimited { .. })) => {
                return Err(e);
            }
            // A rejected or malformed call falls back for this round only.
            Err(_) => {}
        }
    }
    let values = batch(shared, chain, target, holders).await?;
    Ok(sheet(Via::Batch, chain, holders, values))
}

/// One `aggregate3` call: per holder `getEthBalance`, then `balanceOf` on
/// every token. `Ok(None)` when Multicall3 is not deployed on the chain.
async fn multicall(
    shared: &Shared,
    chain: &ChainPlan,
    target: &Target,
    holders: &[(String, Address)],
) -> Result<Option<Vec<Option<u128>>>, RpcError> {
    let multicall3 = abi::multicall3();
    let mut calls = Vec::with_capacity(holders.len() * (1 + chain.tokens.len()));
    for (_, holder) in holders {
        calls.push((multicall3, abi::get_eth_balance(holder)));
        for token in &chain.tokens {
            calls.push((token.address, abi::balance_of(holder)));
        }
    }
    let data = abi::encode_aggregate3(&calls);
    let returned = shared
        .client
        .call(target, abi::MULTICALL3, &data, BlockId::Latest)
        .await?;
    if returned.is_empty() {
        return Ok(None);
    }
    abi::decode_aggregate3(&returned, calls.len())
        .map(Some)
        .ok_or_else(|| RpcError::InvalidResponse("malformed aggregate3 result".into()))
}

/// The same reads as one JSON-RPC batch.
async fn batch(
    shared: &Shared,
    chain: &ChainPlan,
    target: &Target,
    holders: &[(String, Address)],
) -> Result<Vec<Option<u128>>, RpcError> {
    let mut calls: Vec<(&str, Value)> = Vec::new();
    for (_, holder) in holders {
        calls.push(("eth_getBalance", json!([holder.to_lower_hex(), "latest"])));
        for token in &chain.tokens {
            let call = json!({
                "to": token.address.to_lower_hex(),
                "data": abi::hex(&abi::balance_of(holder)),
            });
            calls.push(("eth_call", json!([call, "latest"])));
        }
    }
    let results = shared.client.batch(target, &calls).await?;
    // A batch whose every entry failed is a failure, not a row of blanks.
    if let Some(Err(first)) = results.first()
        && results.iter().all(Result::is_err)
    {
        return Err(first.clone());
    }
    Ok(results
        .into_iter()
        .zip(&calls)
        .map(|(result, (method, _))| {
            let value = result.ok()?;
            let text = value.as_str()?;
            if *method == "eth_getBalance" {
                abi::quantity(text)
            } else {
                let word = abi::unhex(text)?;
                (word.len() >= 32).then(|| abi::uint(&word[..32]))
            }
        })
        .collect())
}

/// Lay a flat list of values (per holder: native, then each token) out as
/// a sheet.
fn sheet(
    via: Via,
    chain: &ChainPlan,
    holders: &[(String, Address)],
    values: Vec<Option<u128>>,
) -> Sheet {
    let stride = 1 + chain.tokens.len();
    Sheet {
        via,
        holders: holders
            .iter()
            .zip(values.chunks(stride))
            .map(|((signer, _), row)| Holding {
                signer: signer.clone(),
                native: row[0],
                tokens: row[1..].to_vec(),
            })
            .collect(),
    }
}
