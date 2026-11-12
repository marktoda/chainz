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
use chainz_rpc::control::{MetricsSnapshot, RecordStatus, RequestRecord};
use chainz_rpc::error::{LIMIT_EXCEEDED, SERVER_ERROR};
use chainz_rpc::upstream::rpc_error_code;
use chainz_rpc::{ErrorClass, HttpUpstream, RetryPolicy, RpcError};
use serde_json::Value;

use crate::classify::{self, Kind, Needs, Parsed};
use crate::events::EventRing;
use crate::http::{GATEWAY, Reply};
use crate::inspector::Inspector;
use crate::metrics::{Metrics, Sample};
use crate::state::{ChainState, EndpointState, Snapshot};
use crate::summary;
use chainz_os::{now_ms, now_secs};

/// The longest a request waits out rate limits on one endpoint before a
/// read shifts to the next: as long as a client is told to wait when no
/// upstream named a wait.
pub const MAX_WAIT: Duration = crate::http::DEFAULT_RETRY_AFTER;
/// The longest a request with nowhere else to go waits for a slot in the
/// endpoint's bucket.
pub const MAX_QUEUE: Duration = Duration::from_secs(10);
/// First wait after a 429 without `Retry-After`; doubles each time.
const BACKOFF_429: Duration = Duration::from_millis(250);
/// Wait before the one same-endpoint retry of a read with nowhere to go.
const RETRY_PAUSE: Duration = Duration::from_millis(100);
/// Responses larger than this are not inspected for a JSON-RPC error.
const MAX_INSPECTED_BODY: usize = 16 * 1024;

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
    upstream: HttpUpstream,
    metrics: Metrics,
    inspector: Inspector,
}

/// A chain to route to, with the snapshot it came from.
pub struct ChainHandle {
    snapshot: Arc<Snapshot>,
    chain: Arc<ChainState>,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router").finish_non_exhaustive()
    }
}

impl Router {
    /// A router over `snapshot`, forwarding through `upstream` (whose
    /// client sets the per-attempt timeout).
    pub fn new(snapshot: Snapshot, upstream: HttpUpstream, inspector: Inspector) -> Self {
        Router {
            snapshot: RwLock::new(Arc::new(snapshot)),
            upstream,
            metrics: Metrics::default(),
            inspector,
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

    /// A chain of the current snapshot by name, alias or ID.
    pub fn chain(&self, reference: &str) -> Option<ChainHandle> {
        let snapshot = self.snapshot();
        let chain = snapshot.chain(reference)?.clone();
        Some(ChainHandle { snapshot, chain })
    }

    /// Both metrics windows at unix time `now`.
    pub fn metrics(&self, now: u64) -> MetricsSnapshot {
        self.metrics.snapshot(now)
    }

    /// The records behind `/_chainz/events`.
    pub fn events(&self) -> &EventRing {
        self.inspector.events()
    }

    /// Stop the request log; its writer drains and exits.
    pub fn close_log(&self) {
        self.inspector.close_log();
    }

    /// Route one request body for `chain`, optionally pinned to an endpoint.
    pub async fn handle(&self, chain: ChainHandle, pin: Option<&str>, body: Bytes) -> Reply {
        let ChainHandle { snapshot, chain } = chain;
        let chain = &chain;
        let started = Instant::now();
        let at_ms = now_ms();
        chain.touch(at_ms / 1000);
        let parsed = match classify::parse(&body) {
            Ok(parsed) => parsed,
            Err((code, message)) => return Reply::error(400, &Value::Null, code, message),
        };
        let method = summary::method_label(&parsed);
        let mut params_summary = summary::summarize(&parsed, chain.id(), &snapshot.labels);
        let id = parsed.reply_id();

        let candidates = match self.candidates(chain, &parsed, pin) {
            Ok(candidates) => candidates,
            Err(message) => {
                self.inspector.push(RequestRecord {
                    id: 0,
                    at_ms,
                    chain: chain.name().to_string(),
                    method,
                    params_summary,
                    upstream: pin.unwrap_or(chain.active_name()).to_string(),
                    attempts: 0,
                    latency_ms: 0,
                    status: RecordStatus::Error {
                        class: ErrorClass::Unsupported,
                        code: None,
                    },
                    response_bytes: 0,
                });
                return Reply::error(400, &id, SERVER_ERROR, &message);
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
                let upstream = format!("endpoint '{}'", outcome.endpoint);
                (Reply::failure(&error, &id, GATEWAY, &upstream), status)
            }
        };
        self.inspector.push(RequestRecord {
            id: 0,
            at_ms,
            chain: chain.name().to_string(),
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
        if let Some(pin) = pin {
            return Ok(vec![chain.endpoint(pin)?]);
        }
        match plan(chain, parsed)? {
            Plan::Sticky(name) => Ok(vec![chain.endpoint(&name)?]),
            Plan::ActiveOnly => Ok(vec![chain.active()?]),
            Plan::Read(needs) => {
                let active = chain.active()?;
                let mut pool = vec![active.clone()];
                pool.extend(chain.current_failovers().cloned());
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
        let mut trip = Trip {
            chain,
            method,
            body,
            attempts: 0,
        };
        let mut outcome = None;
        for (i, endpoint) in candidates.iter().enumerate() {
            let has_next = i + 1 < candidates.len();
            // A read with one candidate and nowhere to go may be retried
            // there once.
            let retry_here = idempotent && candidates.len() == 1;
            let (result, shift) = self
                .try_endpoint(&mut trip, endpoint, has_next, retry_here)
                .await;
            outcome = Some(Outcome {
                result,
                endpoint: endpoint.name().to_string(),
                attempts: trip.attempts,
            });
            if !shift {
                break;
            }
        }
        outcome.unwrap_or_else(|| Outcome {
            result: Err(RpcError::Unsupported("no endpoint available".into())),
            endpoint: String::new(),
            attempts: trip.attempts,
        })
    }

    /// Send to one endpoint, waiting out rate limits up to [`MAX_WAIT`]
    /// (up to [`MAX_QUEUE`] in the bucket when there is no next endpoint).
    /// Returns the answer and whether the request should move on to the
    /// next endpoint (only ever true when `has_next`).
    async fn try_endpoint(
        &self,
        trip: &mut Trip<'_>,
        endpoint: &EndpointState,
        has_next: bool,
        retry_here: bool,
    ) -> (Result<Answer, RpcError>, bool) {
        let Trip {
            chain,
            method,
            ref body,
            ref mut attempts,
        } = *trip;
        let record = |sample: Sample| {
            self.metrics
                .record(now_secs(), chain.name(), endpoint.name(), method, &sample);
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
            match endpoint.acquire(Instant::now(), budget) {
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
                        now_secs(),
                        chain.name(),
                        endpoint.name(),
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
                // One attempt per call: the router owns every retry
                // decision (rate-limit buckets, backoff, failover).
                .send(endpoint.target(), body.clone(), &RetryPolicy::none())
                .await;
            let latency_ms = millis(sent.elapsed());
            match result {
                Ok(body) => {
                    // The one parse of the body: the error code, if any.
                    let code = (body.len() <= MAX_INSPECTED_BODY)
                        .then(|| rpc_error_code(&body))
                        .flatten();
                    let limit_exceeded = code == Some(LIMIT_EXCEEDED);
                    let shift = limit_exceeded && has_next;
                    record(Sample {
                        latency_ms,
                        error: limit_exceeded.then_some(ErrorClass::Rpc),
                        bytes: body.len() as u64,
                        failover: shift,
                        ..Sample::default()
                    });
                    if !limit_exceeded {
                        endpoint.on_success();
                    }
                    let answer = Answer { body, code };
                    return (Ok(answer), shift);
                }
                Err(RpcError::RateLimited { retry_after }) => {
                    endpoint.on_rate_limited(Instant::now());
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
                    // A transient failure may move a read on (`-32005` and
                    // long 429 waits are handled above).
                    let transient = error.is_transient();
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
        let known = |call: &classify::Call| classify::lookup(&call.method);
        let interesting = calls.iter().any(|c| {
            classify::kind(&c.method) == Kind::FilterCreate
                || known(c).is_some_and(|m| m.uninstalls_filter || m.raw_tx)
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
            let method = known(call);
            if classify::kind(&call.method) == Kind::FilterCreate {
                if let Some(filter) = result.as_str() {
                    chain.remember_filter(filter, endpoint);
                }
            } else if method.is_some_and(|m| m.uninstalls_filter)
                && let Some(filter) = classify::filter_id(call)
            {
                chain.forget_filter(&filter);
            }
            // The node's hash is authoritative (blob transactions hash
            // differently from their network encoding). The `…Sync` form
            // answers with the receipt.
            if matches!(parsed, Parsed::Single(_))
                && method.is_some_and(|m| m.raw_tx)
                && let Some(hash) = result
                    .as_str()
                    .or_else(|| result.get("transactionHash")?.as_str())
                    .and_then(tx_hash)
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

/// One request on its way through its candidate endpoints.
struct Trip<'a> {
    chain: &'a ChainState,
    method: &'a str,
    body: Bytes,
    /// Upstream attempts so far, across endpoints.
    attempts: u32,
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
        Some(endpoint) if active_only && endpoint != chain.active_name() => Err(format!(
            "batch mixes a filter created on endpoint '{endpoint}' with calls bound to the active endpoint '{}'",
            chain.active_name()
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
