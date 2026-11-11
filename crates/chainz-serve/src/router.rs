//! Routing: pick the endpoints a request may use, try them in order, and
//! record what happened.
//!
//! The rules, by request kind:
//!
//! | Request | Endpoints |
//! | --- | --- |
//! | read | the active endpoint, then each `failover` endpoint whose head is current |
//! | send, or any method not known to be a read | the active endpoint only |
//! | filter creation | the active endpoint only; the filter ID becomes sticky |
//! | filter use | the endpoint that created the filter |
//! | batch | the strictest rule among its calls |
//! | pinned (`/<token>/<chain>/<endpoint>`) | that endpoint only |
//!
//! A read moves to the next endpoint only on: a connection error (including
//! a response cut off mid-body), a timeout, HTTP 502/503/504, JSON-RPC
//! `-32005`, or a 429 whose wait would take the request past
//! [`MAX_WAIT`] on that endpoint.
//!
//! Every send to an endpoint, retries included, first takes a slot from
//! that endpoint's adaptive bucket, so requests that hit the same limit
//! come back spaced out rather than all at once. A request with no other
//! endpoint to go to queues in the bucket for up to [`MAX_QUEUE`]; past
//! that it is answered 429 at once, with the bucket's wait as
//! `Retry-After`.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use chainz_rpc::control::{RecordStatus, RequestRecord};
use chainz_rpc::upstream::{LIMIT_EXCEEDED, rpc_error_code};
use chainz_rpc::{ErrorClass, RetryPolicy, RpcError, Upstream};
use serde_json::{Value, json};

use crate::classify::{self, Kind, Needs, Parsed};
use crate::inspector::Inspector;
use crate::metrics::{Metrics, Sample};
use crate::state::{ChainState, EndpointState, Snapshot};
use crate::{now_ms, summary};

/// The longest a request waits out rate limits on one endpoint before a
/// read shifts to the next.
pub const MAX_WAIT: Duration = Duration::from_secs(2);
/// The longest a request with nowhere else to go waits for a slot in the
/// endpoint's bucket.
pub const MAX_QUEUE: Duration = Duration::from_secs(10);
/// First wait after a 429 without `Retry-After`; doubles each time.
const BACKOFF_429: Duration = Duration::from_millis(250);
/// Wait before the one same-endpoint retry of a read with nowhere to go.
const RETRY_PAUSE: Duration = Duration::from_millis(100);
/// Responses larger than this are not inspected for a JSON-RPC error.
const MAX_INSPECTED_BODY: usize = 16 * 1024;

/// What the HTTP layer sends back.
#[derive(Debug, Clone)]
pub struct Reply {
    /// HTTP status.
    pub status: u16,
    /// JSON body.
    pub body: Bytes,
    /// `Retry-After`, on a 429.
    pub retry_after: Option<Duration>,
}

impl Reply {
    /// A JSON-RPC error made by the gateway itself.
    pub fn error(status: u16, id: &Value, code: i64, message: &str) -> Reply {
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": format!("chainz serve: {message}")},
        });
        Reply {
            status,
            body: Bytes::from(body.to_string()),
            retry_after: None,
        }
    }
}

/// How one request may be routed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    /// Active endpoint, then eligible failover endpoints.
    Read(Needs),
    /// Active endpoint only.
    ActiveOnly,
    /// One named endpoint only.
    Sticky(String),
}

/// The request-independent state the router works with.
pub struct Router {
    snapshot: RwLock<Arc<Snapshot>>,
    upstream: Arc<dyn Upstream>,
    /// Metrics store.
    pub metrics: Metrics,
    /// Request inspector.
    pub inspector: Inspector,
    policy: RetryPolicy,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router").finish_non_exhaustive()
    }
}

impl Router {
    /// A router over `snapshot`, forwarding through `upstream` with
    /// `timeout` per attempt.
    pub fn new(
        snapshot: Snapshot,
        upstream: Arc<dyn Upstream>,
        inspector: Inspector,
        timeout: Duration,
    ) -> Self {
        Router {
            snapshot: RwLock::new(Arc::new(snapshot)),
            upstream,
            metrics: Metrics::default(),
            inspector,
            // One attempt per call: the router owns every retry decision.
            policy: RetryPolicy {
                timeout,
                ..RetryPolicy::none()
            },
        }
    }

    /// The current snapshot.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replace the snapshot (reload).
    pub fn set_snapshot(&self, snapshot: Snapshot) {
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(snapshot);
    }

    /// Route one request body for `chain`, optionally pinned to an endpoint.
    pub async fn handle(
        &self,
        snapshot: &Snapshot,
        chain: &Arc<ChainState>,
        pin: Option<&str>,
        body: Bytes,
    ) -> Reply {
        let started = Instant::now();
        let at_ms = now_ms();
        chain.touch(at_ms / 1000);
        let parsed = match classify::parse(&body) {
            Ok(parsed) => parsed,
            Err((code, message)) => return Reply::error(400, &Value::Null, code, message),
        };
        let method = summary::method_label(&parsed);
        let mut params_summary = summary::summarize(&parsed, chain.id, &snapshot.labels);
        let id = parsed.reply_id();

        let candidates = match self.candidates(chain, &parsed, pin) {
            Ok(candidates) => candidates,
            Err(message) => {
                self.inspector.push(RequestRecord {
                    id: 0,
                    at_ms,
                    chain: chain.name.clone(),
                    method,
                    params_summary,
                    upstream: pin.unwrap_or(&chain.active_name).to_string(),
                    attempts: 0,
                    latency_ms: 0,
                    status: RecordStatus::Error {
                        class: ErrorClass::Unsupported,
                        code: None,
                    },
                    response_bytes: 0,
                });
                return Reply::error(400, &id, -32000, &message);
            }
        };

        let idempotent = parsed
            .calls()
            .iter()
            .all(|c| classify::kind(&c.method) == Kind::Read);
        let outcome = self
            .attempt(chain, &candidates, idempotent, &method, body)
            .await;
        let (reply, status) = match outcome.result {
            Ok(Answer { body, code }) => {
                if code.is_none() {
                    self.after_success(
                        chain,
                        &parsed,
                        &body,
                        &outcome.endpoint,
                        &mut params_summary,
                    );
                }
                let status = match code {
                    None => RecordStatus::Ok,
                    code => RecordStatus::Error {
                        class: ErrorClass::Rpc,
                        code,
                    },
                };
                let reply = Reply {
                    status: 200,
                    body,
                    retry_after: None,
                };
                (reply, status)
            }
            Err(error) => {
                let status = RecordStatus::Error {
                    class: error.class(),
                    code: None,
                };
                (failure_reply(&error, &outcome.endpoint, &id), status)
            }
        };
        self.inspector.push(RequestRecord {
            id: 0,
            at_ms,
            chain: chain.name.clone(),
            method,
            params_summary,
            upstream: outcome.endpoint,
            attempts: outcome.attempts,
            latency_ms: millis(started.elapsed()),
            status,
            response_bytes: reply.body.len() as u64,
        });
        reply
    }

    /// The endpoints this request may use, in order.
    fn candidates(
        &self,
        chain: &ChainState,
        parsed: &Parsed,
        pin: Option<&str>,
    ) -> Result<Vec<Arc<EndpointState>>, String> {
        let named = |name: &str| match chain.endpoints.get(name) {
            Some(Ok(endpoint)) => Ok(endpoint.clone()),
            Some(Err(reason)) => Err(format!(
                "endpoint '{name}' of chain '{}' can't be routed: {reason}",
                chain.name
            )),
            None => Err(format!("chain '{}' has no endpoint '{name}'", chain.name)),
        };
        if let Some(pin) = pin {
            return Ok(vec![named(pin)?]);
        }
        let active = || {
            chain.active.clone().map_err(|reason| {
                format!(
                    "active endpoint '{}' of chain '{}' can't be routed: {reason}",
                    chain.active_name, chain.name
                )
            })
        };
        match plan(chain, parsed)? {
            Plan::Sticky(name) => Ok(vec![named(&name)?]),
            Plan::ActiveOnly => Ok(vec![active()?]),
            Plan::Read(needs) => {
                let active = active()?;
                let mut pool = vec![active.clone()];
                pool.extend(
                    chain
                        .failover
                        .iter()
                        .filter(|e| chain.head_is_current(e))
                        .cloned(),
                );
                if needs.any() {
                    // Tagged endpoints only; the active endpoint also
                    // qualifies while its tags are unknown.
                    pool.retain(|e| match e.supports(needs) {
                        Some(supported) => supported,
                        None => Arc::ptr_eq(e, &active),
                    });
                    if pool.is_empty() {
                        pool.push(active);
                    }
                }
                Ok(pool)
            }
        }
    }

    /// Try each candidate in order; the last one's answer is final.
    async fn attempt(
        &self,
        chain: &ChainState,
        candidates: &[Arc<EndpointState>],
        idempotent: bool,
        method: &str,
        body: Bytes,
    ) -> Outcome {
        let mut attempts = 0;
        let mut outcome = None;
        for (i, endpoint) in candidates.iter().enumerate() {
            let has_next = i + 1 < candidates.len();
            // A read with one candidate and nowhere to go may be retried
            // there once.
            let retry_here = idempotent && candidates.len() == 1;
            let (result, shift) = self
                .try_endpoint(
                    chain,
                    endpoint,
                    has_next,
                    retry_here,
                    method,
                    &body,
                    &mut attempts,
                )
                .await;
            outcome = Some(Outcome {
                result,
                endpoint: endpoint.name.clone(),
                attempts,
            });
            if !shift {
                break;
            }
        }
        outcome.unwrap_or_else(|| Outcome {
            result: Err(RpcError::Unsupported("no endpoint available".into())),
            endpoint: String::new(),
            attempts,
        })
    }

    /// Send to one endpoint, waiting out rate limits up to [`MAX_WAIT`]
    /// (up to [`MAX_QUEUE`] in the bucket when there is no next endpoint).
    /// Returns the answer and whether the request should move on to the
    /// next endpoint (only ever true when `has_next`).
    #[allow(clippy::too_many_arguments)]
    async fn try_endpoint(
        &self,
        chain: &ChainState,
        endpoint: &EndpointState,
        has_next: bool,
        retry_here: bool,
        method: &str,
        body: &Bytes,
        attempts: &mut u32,
    ) -> (Result<Answer, RpcError>, bool) {
        let record = |sample: Sample| {
            self.metrics.record(
                now_ms() / 1000,
                &chain.name,
                &endpoint.name,
                method,
                &sample,
            );
        };
        // How long this request may wait for a bucket slot in all.
        let allowance = if has_next { MAX_WAIT } else { MAX_QUEUE };
        let mut waited = Duration::ZERO;
        let mut backoff = BACKOFF_429;
        let mut retried = false;
        loop {
            // Every send takes a slot, retries included: requests that
            // waited out the same 429 don't all retry at once.
            let budget = allowance.saturating_sub(waited);
            match endpoint.limiter.try_acquire(Instant::now(), budget) {
                Ok(wait) => {
                    if !wait.is_zero() {
                        tokio::time::sleep(wait).await;
                        waited += wait;
                    }
                }
                Err(wait) => {
                    // The bucket alone says the wait is too long: move on,
                    // or tell the client how long to wait. Nothing was sent,
                    // so this is no upstream attempt.
                    self.metrics.record_refusal(
                        now_ms() / 1000,
                        &chain.name,
                        &endpoint.name,
                        method,
                        has_next,
                    );
                    let error = RpcError::RateLimited {
                        retry_after: Some(wait),
                    };
                    return (Err(error), has_next);
                }
            }
            *attempts += 1;
            let sent = Instant::now();
            let result = self
                .upstream
                .send(&endpoint.target, body.clone(), &self.policy)
                .await;
            let latency_ms = millis(sent.elapsed());
            match result {
                Ok(response) => {
                    // The one parse of the body: the error code, if any.
                    let code = (response.body.len() <= MAX_INSPECTED_BODY)
                        .then(|| rpc_error_code(&response.body))
                        .flatten();
                    let limit_exceeded = code == Some(LIMIT_EXCEEDED);
                    let shift = limit_exceeded && has_next;
                    record(Sample {
                        latency_ms,
                        error: limit_exceeded.then_some(ErrorClass::Rpc),
                        bytes: response.body.len() as u64,
                        failover: shift,
                        ..Sample::default()
                    });
                    if !limit_exceeded {
                        endpoint.limiter.on_success();
                    }
                    let answer = Answer {
                        body: response.body,
                        code,
                    };
                    return (Ok(answer), shift);
                }
                Err(RpcError::RateLimited { retry_after }) => {
                    endpoint.limiter.on_rate_limited(Instant::now());
                    let wait = retry_after.unwrap_or(backoff);
                    let stay = waited + wait <= MAX_WAIT;
                    record(Sample {
                        latency_ms,
                        error: Some(ErrorClass::RateLimited),
                        rate_limited: true,
                        failover: !stay && has_next,
                        ..Sample::default()
                    });
                    if !stay {
                        return (Err(RpcError::RateLimited { retry_after }), has_next);
                    }
                    // Wait on the same endpoint: read-your-writes stays
                    // intact for nonce reads and receipt polling.
                    tokio::time::sleep(wait).await;
                    waited += wait;
                    backoff *= 2;
                }
                Err(error) => {
                    let transient = is_transient(&error);
                    record(Sample {
                        latency_ms,
                        error: Some(error.class()),
                        failover: transient && has_next,
                        ..Sample::default()
                    });
                    // One more try here (not after a timeout: that already
                    // took long enough).
                    if transient && retry_here && !retried && error != RpcError::Timeout {
                        retried = true;
                        tokio::time::sleep(RETRY_PAUSE).await;
                        continue;
                    }
                    return (Err(error), transient && has_next);
                }
            }
        }
    }

    /// Bookkeeping after a successful answer: sticky filters, and the real
    /// transaction hash for a raw send.
    fn after_success(
        &self,
        chain: &ChainState,
        parsed: &Parsed,
        body: &Bytes,
        endpoint: &str,
        params_summary: &mut String,
    ) {
        let calls = parsed.calls();
        let interesting = calls.iter().any(|c| {
            matches!(classify::kind(&c.method), Kind::FilterCreate)
                || c.method == "eth_uninstallFilter"
                || c.method == "eth_sendRawTransaction"
        });
        if !interesting || body.len() > MAX_INSPECTED_BODY {
            return;
        }
        let Ok(response) = serde_json::from_slice::<Value>(body) else {
            return;
        };
        let responses: Vec<&Value> = match &response {
            Value::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        for call in calls {
            let Some(result) = responses
                .iter()
                .find(|r| r.get("id") == Some(&call.id))
                .and_then(|r| r.get("result"))
            else {
                continue;
            };
            match classify::kind(&call.method) {
                Kind::FilterCreate => {
                    if let Some(filter) = result.as_str() {
                        chain.remember_filter(filter, endpoint);
                    }
                }
                Kind::FilterUse if call.method == "eth_uninstallFilter" => {
                    if let Some(filter) = classify::filter_id(call) {
                        chain.forget_filter(&filter);
                    }
                }
                _ => {}
            }
            // The node's hash is authoritative (blob transactions hash
            // differently from their network encoding).
            if matches!(parsed, Parsed::Single(_))
                && call.method == "eth_sendRawTransaction"
                && let Some(hash) = result.as_str().and_then(tx_hash)
            {
                *params_summary = format!("tx {hash}");
            }
        }
    }
}

/// An upstream's 2xx answer.
struct Answer {
    body: Bytes,
    /// The JSON-RPC error code of a single response (bodies over
    /// [`MAX_INSPECTED_BODY`] are not inspected).
    code: Option<i64>,
}

/// What [`Router::attempt`] ended with.
struct Outcome {
    /// The upstream's answer, or the last error.
    result: Result<Answer, RpcError>,
    /// The endpoint that answered or failed last.
    endpoint: String,
    /// Upstream attempts across endpoints.
    attempts: u32,
}

/// A transaction hash as a node returns it (`0x` and 64 hex digits),
/// lowercased; `None` for anything else.
fn tx_hash(result: &str) -> Option<String> {
    let digits = result.strip_prefix("0x")?;
    (digits.len() == 64 && digits.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| result.to_ascii_lowercase())
}

fn millis(duration: Duration) -> u32 {
    u32::try_from(duration.as_millis()).unwrap_or(u32::MAX)
}

/// The enumerated errors on which a read may move to the next endpoint
/// (`-32005` and long 429 waits are handled where they are seen).
fn is_transient(error: &RpcError) -> bool {
    match error {
        RpcError::Connect | RpcError::Timeout => true,
        RpcError::Http(status) => matches!(status, 502..=504),
        // From `Upstream::send` this only means the response body could not
        // be read: the connection died mid-response.
        RpcError::InvalidResponse(_) => true,
        _ => false,
    }
}

/// The reply for a request no endpoint answered.
fn failure_reply(error: &RpcError, endpoint: &str, id: &Value) -> Reply {
    let (status, code) = match error {
        RpcError::RateLimited { .. } => (429, LIMIT_EXCEEDED),
        RpcError::Timeout => (504, -32000),
        RpcError::Http(status) if (400..600).contains(status) => (*status, -32000),
        _ => (502, -32000),
    };
    // `RpcError`'s text never includes a URL or a header value.
    let mut reply = Reply::error(status, id, code, &format!("endpoint '{endpoint}': {error}"));
    if let RpcError::RateLimited { retry_after } = error {
        reply.retry_after = Some(retry_after.unwrap_or(MAX_WAIT));
    }
    reply
}

/// Decide the routing rule for a request: the strictest among its calls.
fn plan(chain: &ChainState, parsed: &Parsed) -> Result<Plan, String> {
    let mut needs = Needs::default();
    let mut active_only = false;
    let mut sticky: Option<String> = None;
    for call in parsed.calls() {
        match classify::kind(&call.method) {
            Kind::Read => needs = needs.union(classify::needs(call, chain.head())),
            Kind::ActiveOnly | Kind::FilterCreate => active_only = true,
            Kind::FilterUse => {
                match classify::filter_id(call).and_then(|id| chain.filter_endpoint(&id)) {
                    Some(endpoint) => match &sticky {
                        Some(other) if *other != endpoint => {
                            return Err(
                                "batch uses filters created on different endpoints".to_string()
                            );
                        }
                        _ => sticky = Some(endpoint),
                    },
                    // Not created through this gateway: the active endpoint
                    // is the only one that could know it.
                    None => active_only = true,
                }
            }
        }
    }
    match sticky {
        Some(endpoint) if active_only && endpoint != chain.active_name => Err(format!(
            "batch mixes a filter created on endpoint '{endpoint}' with calls bound to the active endpoint '{}'",
            chain.active_name
        )),
        Some(endpoint) => Ok(Plan::Sticky(endpoint)),
        None if active_only => Ok(Plan::ActiveOnly),
        None => Ok(Plan::Read(needs)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_hash_is_checked_not_sliced() {
        let hash = format!("0x{}", "aB".repeat(32));
        assert_eq!(tx_hash(&hash), Some(hash.to_ascii_lowercase()));
        // 66 bytes, but a multi-byte character straddles byte 2: slicing
        // `[2..]` panicked here.
        let straddle = format!("a\u{e9}{}", "b".repeat(63));
        assert_eq!(straddle.len(), 66);
        assert_eq!(tx_hash(&straddle), None);
        for bad in [
            String::new(),
            "0x".to_string(),
            "0x12".to_string(),
            format!("0X{}", "a".repeat(64)),
            format!("0x{}", "g".repeat(64)),
            format!("0x{}", "a".repeat(65)),
            format!("0x{}\u{e9}", "a".repeat(62)),
        ] {
            assert_eq!(tx_hash(&bad), None, "{bad}");
        }
    }
}
