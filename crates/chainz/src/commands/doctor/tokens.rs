//! `doctor --tokens`: check the built-in token registry against the
//! chains themselves. For every entry on a configured chain: the address
//! has code, and `symbol()` and `decimals()` return what the registry says.

use crate::commands::open;
use crate::{net, ui};
use anyhow::Result;
use chainz_core::Chainz;
use chainz_core::labels::{BuiltinToken, builtin_tokens};
use chainz_core::model::Chain;
use chainz_core::names;
use chainz_rpc::abi::{self, DECIMALS, SYMBOL};
use chainz_rpc::{BlockId, RetryPolicy, RpcClient, Target};
use serde_json::json;
use std::collections::BTreeMap;

/// How each request is retried: patiently, since a public endpoint asked
/// about many tokens in a row often needs it.
fn retry_policy() -> RetryPolicy {
    RetryPolicy::patient()
}

/// One chain's entries and how they fared.
enum Outcome {
    /// The chain is not in the config: nothing to ask.
    Skipped,
    /// The chain could not be asked at all.
    Unreachable(String),
    /// One verdict per token, in registry order.
    Checked(Vec<Result<(), String>>),
}

pub fn run() -> Result<i32> {
    let chainz = open()?;
    let config = chainz.config();
    let mut by_chain: BTreeMap<u64, Vec<&'static BuiltinToken>> = BTreeMap::new();
    for token in builtin_tokens() {
        by_chain.entry(token.chain_id).or_default().push(token);
    }

    // Resolve an endpoint per chain up front; the checks run concurrently.
    let mut outcomes: BTreeMap<u64, Outcome> = BTreeMap::new();
    let mut jobs = Vec::new();
    for (&id, tokens) in &by_chain {
        let Some((name, chain)) = config.chain_by_id(id) else {
            outcomes.insert(id, Outcome::Skipped);
            continue;
        };
        match endpoint(&chainz, name, chain) {
            Ok((provider, target)) => jobs.push(Job {
                provider,
                chain_id: id,
                target,
                tokens: tokens.clone(),
            }),
            Err(e) => {
                outcomes.insert(id, Outcome::Unreachable(e));
            }
        }
    }
    outcomes.extend(net::block_on(check_chains(jobs)));

    println!("{}", ui::section("tokens"));
    let (mut ok, mut failed, mut skipped) = (0, 0, 0);
    for (id, tokens) in &by_chain {
        let chain = match config.chain_by_id(*id) {
            Some((name, _)) => name.to_string(),
            None => names::short_name(*id).map_or_else(|| id.to_string(), str::to_string),
        };
        match &outcomes[id] {
            Outcome::Skipped => {
                skipped += tokens.len();
                println!(
                    "{}",
                    ui::warn(&format!(
                        "{chain} ({id}): not configured, {} entries not checked",
                        tokens.len()
                    ))
                );
            }
            Outcome::Unreachable(why) => {
                failed += tokens.len();
                println!(
                    "{}",
                    ui::fail(&format!(
                        "{chain} ({id}): {why}; {} entries not checked",
                        tokens.len()
                    ))
                );
            }
            Outcome::Checked(verdicts) => {
                for (token, verdict) in tokens.iter().zip(verdicts) {
                    let what = format!("{chain} @{} {}", token.name, token.address);
                    match verdict {
                        Ok(()) => {
                            ok += 1;
                            println!(
                                "{}",
                                ui::success(&format!(
                                    "{what} {} ({} decimals)",
                                    token.symbol, token.decimals
                                ))
                            );
                        }
                        Err(why) => {
                            failed += 1;
                            println!("{}", ui::fail(&format!("{what}: {why}")));
                        }
                    }
                }
            }
        }
    }

    println!();
    let total = ok + failed + skipped;
    let summary = format!("{total} registry entries: {ok} ok, {failed} failed, {skipped} skipped");
    if failed > 0 {
        println!("{}", ui::fail(&summary));
        Ok(1)
    } else {
        println!("{}", ui::success(&summary));
        Ok(0)
    }
}

/// One chain to check.
struct Job {
    /// The endpoint's provider (its host's registrable name).
    provider: String,
    chain_id: u64,
    target: Target,
    tokens: Vec<&'static BuiltinToken>,
}

/// The endpoint to ask, with its provider: the chain's active one, or
/// (when that is a websocket, which the client cannot use) its first HTTP
/// endpoint, failover list first. Errors never include a URL.
fn endpoint(chainz: &Chainz, name: &str, chain: &Chain) -> Result<(String, Target), String> {
    let candidates = std::iter::once(&chain.rpc)
        .chain(chain.failover.iter())
        .chain(chain.rpcs.keys());
    for ep in candidates {
        if let Some(endpoint) = chain.rpcs.get(ep)
            && !endpoint.is_websocket()
        {
            return net::target(chainz, name, chain.id, ep, endpoint)
                .map(|target| (names::endpoint_name_for_url(&endpoint.url), target))
                .map_err(|e| format!("{e:#}"));
        }
    }
    Err("no HTTP endpoint".to_string())
}

async fn check_chains(jobs: Vec<Job>) -> Vec<(u64, Outcome)> {
    // Chains on one provider share its rate limit: those go one after
    // another, and only different providers run side by side.
    let mut by_provider: BTreeMap<String, Vec<Job>> = BTreeMap::new();
    for job in jobs {
        by_provider
            .entry(job.provider.clone())
            .or_default()
            .push(job);
    }
    let client = RpcClient::new();
    let mut set = tokio::task::JoinSet::new();
    for (_, jobs) in by_provider {
        let client = client.clone();
        set.spawn(async move {
            let mut out = Vec::with_capacity(jobs.len());
            for job in jobs {
                let outcome = check_chain(&client, &job.target, job.chain_id, &job.tokens).await;
                out.push((job.chain_id, outcome));
            }
            out
        });
    }
    let mut out = Vec::new();
    while let Some(joined) = set.join_next().await {
        out.extend(joined.expect("token checks do not panic"));
    }
    out
}

/// Check one chain's tokens in turn (one endpoint, no bursts).
async fn check_chain(
    client: &RpcClient,
    target: &Target,
    chain_id: u64,
    tokens: &[&'static BuiltinToken],
) -> Outcome {
    let retry = retry_policy();
    if let Err(e) = retry.run(|| client.check_chain_id(target, chain_id)).await {
        return Outcome::Unreachable(e.to_string());
    }
    let mut verdicts = Vec::with_capacity(tokens.len());
    for token in tokens {
        verdicts.push(check_token(client, target, &retry, token).await);
    }
    Outcome::Checked(verdicts)
}

async fn check_token(
    client: &RpcClient,
    target: &Target,
    retry: &RetryPolicy,
    token: &BuiltinToken,
) -> Result<(), String> {
    let address = token.address.to_string();
    let code: String = retry
        .run(|| client.request(target, "eth_getCode", json!([address, "latest"])))
        .await
        .map_err(|e| format!("eth_getCode failed: {e}"))?;
    if code.trim_start_matches("0x").is_empty() {
        return Err("no code at this address".to_string());
    }
    let symbol = retry
        .run(|| client.call(target, &address, &SYMBOL, BlockId::Latest))
        .await
        .map_err(|e| format!("symbol() failed: {e}"))?;
    let decimals = retry
        .run(|| client.call(target, &address, &DECIMALS, BlockId::Latest))
        .await
        .map_err(|e| format!("decimals() failed: {e}"))?;
    compare(token, &symbol, &decimals)
}

/// Compare raw `symbol()` and `decimals()` return data with the registry.
fn compare(token: &BuiltinToken, symbol: &[u8], decimals: &[u8]) -> Result<(), String> {
    let mut wrong = Vec::new();
    match abi::decode_string(symbol) {
        Some(s) if s == token.symbol => {}
        Some(s) => wrong.push(format!(
            "symbol() is {s:?}, the registry says {:?}",
            token.symbol
        )),
        None => wrong.push("symbol() returned no string".to_string()),
    }
    match abi::decode_u8(decimals) {
        Some(d) if d == token.decimals => {}
        Some(d) => wrong.push(format!(
            "decimals() is {d}, the registry says {}",
            token.decimals
        )),
        None => wrong.push("decimals() returned no uint8".to_string()),
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(wrong.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chainz_core::labels::builtin_token;

    fn abi_string(s: &str) -> Vec<u8> {
        let mut out = vec![0u8; 64];
        out[31] = 32;
        out[63] = s.len() as u8;
        out.extend_from_slice(s.as_bytes());
        out.resize(64 + s.len().div_ceil(32) * 32, 0);
        out
    }

    fn abi_u8(n: u8) -> Vec<u8> {
        let mut out = vec![0u8; 32];
        out[31] = n;
        out
    }

    #[test]
    fn compare_names_each_mismatch() {
        let usdc = builtin_token(8453, "usdc").unwrap();
        assert_eq!(compare(usdc, &abi_string("USDC"), &abi_u8(6)), Ok(()));
        let err = compare(usdc, &abi_string("USDbC"), &abi_u8(18)).unwrap_err();
        assert!(
            err.contains("symbol() is \"USDbC\", the registry says \"USDC\""),
            "{err}"
        );
        assert!(
            err.contains("decimals() is 18, the registry says 6"),
            "{err}"
        );
        let err = compare(usdc, &[], &[]).unwrap_err();
        assert!(
            err.contains("no string") && err.contains("no uint8"),
            "{err}"
        );
    }

    #[test]
    fn retries_are_transient_only() {
        let retry = retry_policy();
        assert!(retry.retries(&chainz_rpc::RpcError::Timeout));
        assert!(retry.retries(&chainz_rpc::RpcError::Http(503)));
        assert!(!retry.retries(&chainz_rpc::RpcError::Rpc {
            code: 3,
            message: "execution reverted".into()
        }));
        assert_eq!(retry.max_attempts, 5);
    }
}
