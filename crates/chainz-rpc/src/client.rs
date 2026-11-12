//! JSON-RPC 2.0 client over one shared connection pool.

use std::time::Duration;

use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::error::RpcError;
use crate::hex;
use crate::target::Target;

/// The `User-Agent` of every request chainz makes.
pub(crate) const USER_AGENT: &str = concat!("chainz/", env!("CARGO_PKG_VERSION"));
/// Default whole-request timeout for [`RpcClient::new`].
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Connect timeout used by every [`RpcClient`].
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The fields of a block chainz reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    /// Block number.
    pub number: u64,
    /// Unix seconds, when the node sent one.
    pub timestamp: Option<u64>,
    /// EIP-1559 base fee in wei, on chains that have one.
    pub base_fee_per_gas: Option<u128>,
}

impl BlockHeader {
    /// Read a block as JSON: `None` for `null`, an error for anything
    /// without a block number. Quantities past `u64` (or `u128`) saturate.
    fn from_json(value: &Value) -> Result<Option<BlockHeader>, RpcError> {
        if value.is_null() {
            return Ok(None);
        }
        let quantity = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_str)
                .and_then(|s| hex::parse_quantity_saturating(s).ok())
        };
        let saturate = |n: u128| u64::try_from(n).unwrap_or(u64::MAX);
        let number = quantity("number")
            .ok_or_else(|| RpcError::InvalidResponse("block without a number".into()))?;
        Ok(Some(BlockHeader {
            number: saturate(number),
            timestamp: quantity("timestamp").map(saturate),
            base_fee_per_gas: quantity("baseFeePerGas"),
        }))
    }
}

/// A block selector for block-scoped calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockId {
    /// `"latest"`.
    Latest,
    /// `"finalized"`.
    Finalized,
    /// A specific block number.
    Number(u64),
}

impl BlockId {
    /// The JSON-RPC parameter form: a tag string or a hex quantity.
    pub(crate) fn to_param(self) -> Value {
        Value::String(match self {
            BlockId::Latest => "latest".into(),
            BlockId::Finalized => "finalized".into(),
            BlockId::Number(n) => hex::quantity(n),
        })
    }
}

/// A raw HTTP response, before any status classification.
pub(crate) struct RawResponse {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub body: Bytes,
}

/// JSON-RPC 2.0 over one shared reqwest pool (rustls; HTTP/2 when the server
/// offers it via ALPN).
///
/// Cloning is cheap and shares the pool. Errors are classified into
/// [`RpcError`] and never include the endpoint URL or header values.
#[derive(Clone)]
pub struct RpcClient {
    http: reqwest::Client,
}

impl std::fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcClient").finish_non_exhaustive()
    }
}

impl Default for RpcClient {
    fn default() -> Self {
        Self::new()
    }
}

impl RpcClient {
    /// A client with a 10 s request timeout and a 5 s connect timeout.
    pub fn new() -> Self {
        Self::with_timeout(DEFAULT_TIMEOUT)
    }

    /// A client with the given whole-request timeout (connect timeout stays
    /// 5 s, or `timeout` if shorter).
    pub fn with_timeout(timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(CONNECT_TIMEOUT.min(timeout))
            // A redirect would carry the target's headers (API keys
            // included) to whatever host it names: answer it as an HTTP
            // status instead.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(USER_AGENT)
            .build()
            .expect("reqwest client with rustls builds");
        RpcClient { http }
    }

    /// POST `body` as JSON to the target and read the whole response.
    /// Only transport errors are returned as `Err`; statuses are not judged.
    pub(crate) async fn post_raw(
        &self,
        target: &Target,
        body: Bytes,
    ) -> Result<RawResponse, RpcError> {
        target.require_http()?;
        let resp = self
            .http
            .post(target.url().clone())
            .headers(target.headers().clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| RpcError::from_reqwest(&e))?;
        let status = resp.status().as_u16();
        let retry_after = parse_retry_after(resp.headers());
        let body = resp.bytes().await.map_err(|e| RpcError::from_reqwest(&e))?;
        Ok(RawResponse {
            status,
            retry_after,
            body,
        })
    }

    /// POST a JSON payload and return the parsed body of a 2xx response.
    async fn post_json(&self, target: &Target, payload: &Value) -> Result<Value, RpcError> {
        let body = Bytes::from(serde_json::to_vec(payload).expect("JSON value serializes"));
        let raw = self.post_raw(target, body).await?;
        check_status(&raw)?;
        serde_json::from_slice(&raw.body)
            .map_err(|_| RpcError::InvalidResponse("response is not valid JSON".into()))
    }

    /// Call one method and deserialize its `result`.
    pub async fn request<T: DeserializeOwned>(
        &self,
        target: &Target,
        method: &str,
        params: Value,
    ) -> Result<T, RpcError> {
        let payload = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let resp = self.post_json(target, &payload).await?;
        let result = extract_result(resp)?;
        serde_json::from_value(result)
            .map_err(|e| RpcError::InvalidResponse(format!("unexpected result: {e}")))
    }

    /// Send several calls as one JSON-RPC batch.
    ///
    /// The outer `Err` is a transport-level failure (or a single error object
    /// for the whole batch); otherwise one result per call, in call order,
    /// matched by `id` regardless of the order the server answered in.
    pub async fn batch(
        &self,
        target: &Target,
        calls: &[(&str, Value)],
    ) -> Result<Vec<Result<Value, RpcError>>, RpcError> {
        let payload: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (method, params))| {
                json!({"jsonrpc": "2.0", "id": i, "method": method, "params": params})
            })
            .collect();
        let resp = self.post_json(target, &Value::Array(payload)).await?;
        let items = match resp {
            Value::Array(items) => items,
            // Some servers answer a rejected batch with a single error object.
            other @ Value::Object(_) => {
                return Err(extract_result(other)
                    .err()
                    .unwrap_or_else(|| RpcError::InvalidResponse("expected a batch".into())));
            }
            _ => return Err(RpcError::InvalidResponse("expected a batch".into())),
        };
        let mut out: Vec<Option<Result<Value, RpcError>>> = vec![None; calls.len()];
        for item in items {
            let Some(id) = item.get("id").and_then(Value::as_u64) else {
                continue;
            };
            if let Some(slot) = usize::try_from(id).ok().and_then(|i| out.get_mut(i)) {
                *slot = Some(extract_result(item));
            }
        }
        Ok(out
            .into_iter()
            .map(|r| {
                r.unwrap_or_else(|| {
                    Err(RpcError::InvalidResponse(
                        "batch response is missing an id".into(),
                    ))
                })
            })
            .collect())
    }

    /// `eth_chainId`.
    pub async fn chain_id(&self, t: &Target) -> Result<u64, RpcError> {
        let v: Value = self.request(t, "eth_chainId", json!([])).await?;
        hex::value_u64(&v)
    }

    /// `eth_chainId`, failing with [`RpcError::ChainMismatch`] unless it
    /// equals `expected`.
    pub async fn check_chain_id(&self, t: &Target, expected: u64) -> Result<(), RpcError> {
        expect_chain(expected, self.chain_id(t).await?)
    }

    /// `eth_blockNumber`.
    pub async fn block_number(&self, t: &Target) -> Result<u64, RpcError> {
        let v: Value = self.request(t, "eth_blockNumber", json!([])).await?;
        hex::value_u64(&v)
    }

    /// `eth_getBlockByNumber` without transactions; `None` when the node
    /// has no such block.
    pub async fn get_block(
        &self,
        t: &Target,
        block: BlockId,
    ) -> Result<Option<BlockHeader>, RpcError> {
        let v: Value = self
            .request(t, "eth_getBlockByNumber", json!([block.to_param(), false]))
            .await?;
        BlockHeader::from_json(&v)
    }

    /// `eth_getBalance` in wei.
    pub(crate) async fn get_balance(
        &self,
        t: &Target,
        address: &str,
        block: BlockId,
    ) -> Result<u128, RpcError> {
        let v: Value = self
            .request(t, "eth_getBalance", json!([address, block.to_param()]))
            .await?;
        match v.as_str() {
            Some(s) => hex::parse_quantity(s),
            None => Err(RpcError::InvalidResponse("expected a hex quantity".into())),
        }
    }

    /// `eth_call` with `to` and `data`; returns the raw return data.
    pub async fn call(
        &self,
        t: &Target,
        to: &str,
        data: &[u8],
        block: BlockId,
    ) -> Result<Vec<u8>, RpcError> {
        let tx = json!({"to": to, "data": hex::encode(data)});
        let v: String = self
            .request(t, "eth_call", json!([tx, block.to_param()]))
            .await?;
        hex::decode(&v)
    }
}

/// `Err(ChainMismatch)` unless the endpoint's chain ID `actual` is the
/// `expected` one.
pub(crate) fn expect_chain(expected: u64, actual: u64) -> Result<(), RpcError> {
    if actual == expected {
        Ok(())
    } else {
        Err(RpcError::ChainMismatch { expected, actual })
    }
}

/// Map a non-2xx status to an error: 429 is `RateLimited`, others `Http`.
pub(crate) fn check_status(raw: &RawResponse) -> Result<(), RpcError> {
    match raw.status {
        200..=299 => Ok(()),
        429 => Err(RpcError::RateLimited {
            retry_after: raw.retry_after,
        }),
        s => Err(RpcError::Http(s)),
    }
}

/// Upper bound on a parsed `Retry-After`: callers wait on it, and the
/// value comes from an untrusted endpoint.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Parse `Retry-After` in its delta-seconds form, clamped to
/// [`MAX_RETRY_AFTER`]. Never panics on hostile values (`1e300`, `inf`).
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let v = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    parse_retry_after_value(v)
}

fn parse_retry_after_value(v: &str) -> Option<Duration> {
    let v = v.trim();
    let parsed = match v.parse::<u64>() {
        Ok(secs) => Duration::from_secs(secs),
        Err(_) => {
            let secs = v.parse::<f64>().ok().filter(|s| *s >= 0.0)?; // NaN, negative: ignore
            // Overflow (`1e300`, `inf`, huge integers) clamps instead of panicking.
            Duration::try_from_secs_f64(secs).unwrap_or(MAX_RETRY_AFTER)
        }
    };
    Some(parsed.min(MAX_RETRY_AFTER))
}

/// Take `result` out of a JSON-RPC response object, or turn `error` into
/// [`RpcError::Rpc`].
fn extract_result(mut resp: Value) -> Result<Value, RpcError> {
    let Some(obj) = resp.as_object_mut() else {
        return Err(RpcError::InvalidResponse(
            "response is not a JSON object".into(),
        ));
    };
    if let Some(err) = obj.get("error").filter(|e| !e.is_null()) {
        let code = err.get("code").and_then(Value::as_i64);
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return match code {
            Some(code) => Err(RpcError::Rpc {
                code,
                message: message.chars().take(512).collect(),
            }),
            None => Err(RpcError::InvalidResponse("malformed error object".into())),
        };
    }
    match obj.remove("result") {
        Some(v) => Ok(v),
        None => Err(RpcError::InvalidResponse(
            "response has neither result nor error".into(),
        )),
    }
}

#[cfg(test)]
mod retry_after_tests {
    use super::*;

    #[test]
    fn hostile_retry_after_values_are_clamped_not_panics() {
        assert_eq!(parse_retry_after_value("2"), Some(Duration::from_secs(2)));
        assert_eq!(
            parse_retry_after_value("0.5"),
            Some(Duration::from_millis(500))
        );
        for huge in [
            "1e300",
            "18446744073709551615",
            "99999999999999999999",
            "inf",
            "3600",
        ] {
            assert_eq!(
                parse_retry_after_value(huge),
                Some(MAX_RETRY_AFTER),
                "{huge}"
            );
        }
        for garbage in ["-1", "NaN", "soon", ""] {
            assert_eq!(parse_retry_after_value(garbage), None, "{garbage}");
        }
    }
}
