//! `doctor --tokens`: check the built-in token registry against the
//! chains themselves. For every entry on a configured chain: the address
//! has code, and `symbol()` and `decimals()` return what the registry says.

use super::open;
use crate::{net, ui};
use anyhow::Result;
use chainz_core::Chainz;
use chainz_core::labels::{BuiltinToken, LabelIndex, LabelSource, builtin_tokens};
use chainz_core::model::Chain;
use chainz_core::names;
use chainz_rpc::{BlockId, RpcClient, RpcError, Target};
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;

/// `symbol()`.
const SYMBOL: [u8; 4] = [0x95, 0xd8, 0x9b, 0x41];
/// `decimals()`.
const DECIMALS: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];
/// Waits before each further attempt of a request that failed in transit.
const BACKOFF: [Duration; 4] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
/// The longest `Retry-After` honoured.
const MAX_WAIT: Duration = Duration::from_secs(10);

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

    // Your own labels that hide a built-in token on a configured chain.
    let index = LabelIndex::new(config);
    for token in builtin_tokens() {
        if let Some((chain, _)) = config.chain_by_id(token.chain_id)
            && let Ok(label) = index.resolve(&token.name, token.chain_id)
            && label.source != LabelSource::Token
            && label.address != token.address
        {
            println!(
                "{}",
                ui::warn(&format!(
                    "{chain}: your @{} ({}) shadows the built-in token",
                    token.name, label.address
                ))
            );
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
            return net::target(chainz, name, chain, ep, endpoint)
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
    if let Err(e) = retry(|| client.check_chain_id(target, chain_id)).await {
        return Outcome::Unreachable(e.to_string());
    }
    let mut verdicts = Vec::with_capacity(tokens.len());
    for token in tokens {
        verdicts.push(check_token(client, target, token).await);
    }
    Outcome::Checked(verdicts)
}

async fn check_token(
    client: &RpcClient,
    target: &Target,
    token: &BuiltinToken,
) -> Result<(), String> {
    let address = token.address.to_string();
    let code: String = retry(|| client.request(target, "eth_getCode", json!([address, "latest"])))
        .await
        .map_err(|e| format!("eth_getCode failed: {e}"))?;
    if code.trim_start_matches("0x").is_empty() {
        return Err("no code at this address".to_string());
    }
    let symbol = retry(|| client.call(target, &address, &SYMBOL, BlockId::Latest))
        .await
        .map_err(|e| format!("symbol() failed: {e}"))?;
    let decimals = retry(|| client.call(target, &address, &DECIMALS, BlockId::Latest))
        .await
        .map_err(|e| format!("decimals() failed: {e}"))?;
    compare(token, &symbol, &decimals)
}

/// Compare raw `symbol()` and `decimals()` return data with the registry.
fn compare(token: &BuiltinToken, symbol: &[u8], decimals: &[u8]) -> Result<(), String> {
    let mut wrong = Vec::new();
    match decode_string(symbol) {
        Some(s) if s == token.symbol => {}
        Some(s) => wrong.push(format!(
            "symbol() is {s:?}, the registry says {:?}",
            token.symbol
        )),
        None => wrong.push("symbol() returned no string".to_string()),
    }
    match decode_u8(decimals) {
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

/// Run `call`, trying again with a growing wait after transient failures
/// (timeouts, connection errors, rate limits, 5xx). A rate limit's
/// `Retry-After` is honoured up to [`MAX_WAIT`].
async fn retry<T, F, Fut>(call: F) -> Result<T, RpcError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, RpcError>>,
{
    let mut waits = BACKOFF.iter();
    loop {
        match call().await {
            Err(e) if transient(&e) => match waits.next() {
                Some(wait) => {
                    let asked = match e {
                        RpcError::RateLimited { retry_after } => retry_after,
                        _ => None,
                    };
                    tokio::time::sleep(asked.map_or(*wait, |a| a.clamp(*wait, MAX_WAIT))).await
                }
                None => return Err(e),
            },
            other => return other,
        }
    }
}

fn transient(error: &RpcError) -> bool {
    match error {
        RpcError::Timeout | RpcError::Connect | RpcError::RateLimited { .. } => true,
        RpcError::Http(status) => *status >= 500,
        _ => false,
    }
}

/// A big-endian ABI word as a `usize`, if it fits.
fn word(data: &[u8], at: usize) -> Option<usize> {
    let word = data.get(at..at.checked_add(32)?)?;
    if word[..24].iter().any(|b| *b != 0) {
        return None;
    }
    usize::try_from(u64::from_be_bytes(word[24..].try_into().ok()?)).ok()
}

/// Decode an ABI `string` return value. Also accepts the `bytes32` that a
/// few old tokens return (NUL padded).
fn decode_string(data: &[u8]) -> Option<String> {
    let bytes = if data.len() == 32 {
        let end = data.iter().position(|b| *b == 0).unwrap_or(32);
        &data[..end]
    } else {
        let offset = word(data, 0)?;
        let len = word(data, offset)?;
        let start = offset.checked_add(32)?;
        data.get(start..start.checked_add(len)?)?
    };
    String::from_utf8(bytes.to_vec()).ok()
}

/// Decode an ABI `uint8` return value.
fn decode_u8(data: &[u8]) -> Option<u8> {
    if data.len() != 32 {
        return None;
    }
    u8::try_from(word(data, 0)?).ok()
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
    fn decodes_abi_strings_and_bytes32() {
        assert_eq!(decode_string(&abi_string("USDC")).as_deref(), Some("USDC"));
        assert_eq!(
            decode_string(&abi_string("USD₮0")).as_deref(),
            Some("USD₮0")
        );
        assert_eq!(decode_string(&abi_string("")).as_deref(), Some(""));
        let mut bytes32 = [0u8; 32];
        bytes32[..3].copy_from_slice(b"MKR");
        assert_eq!(decode_string(&bytes32).as_deref(), Some("MKR"));
        // Truncated, empty, or with an offset or length that points nowhere.
        assert_eq!(decode_string(&[]), None);
        assert_eq!(decode_string(&abi_string("USDC")[..66]), None);
        let mut far = abi_string("USDC");
        far[..32].fill(0xff);
        assert_eq!(decode_string(&far), None);
        let mut long = abi_string("USDC");
        long[32..64].fill(0xff);
        assert_eq!(decode_string(&long), None);
        let mut not_utf8 = abi_string("USDC");
        not_utf8[64] = 0xff;
        assert_eq!(decode_string(&not_utf8), None);
    }

    #[test]
    fn decodes_uint8() {
        assert_eq!(decode_u8(&abi_u8(6)), Some(6));
        assert_eq!(decode_u8(&abi_u8(18)), Some(18));
        assert_eq!(decode_u8(&[]), None);
        let mut big = abi_u8(0);
        big[30] = 1; // 256
        assert_eq!(decode_u8(&big), None);
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
    fn only_transport_errors_are_retried() {
        assert!(transient(&RpcError::Timeout));
        assert!(transient(&RpcError::Http(503)));
        assert!(transient(&RpcError::RateLimited { retry_after: None }));
        assert!(!transient(&RpcError::Http(404)));
        assert!(!transient(&RpcError::Rpc {
            code: 3,
            message: "execution reverted".into()
        }));
        assert!(!transient(&RpcError::ChainMismatch {
            expected: 1,
            actual: 2
        }));
    }
}
