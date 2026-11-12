//! Raw JSON-RPC forwarding to one chosen endpoint, with same-endpoint retries.
//!
//! The gateway and the fork proxy are built on [`Upstream`]. It never fails
//! over to another endpoint; that decision belongs to the caller, made from
//! the returned error's class.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::client::RpcClient;
use crate::error::RpcError;
use crate::target::Target;

/// A boxed, sendable future (keeps [`Upstream`] object-safe).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The JSON-RPC "limit exceeded" error code.
pub const LIMIT_EXCEEDED: i64 = -32005;

/// Which failures are retried on the same endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryOn {
    /// Connection errors.
    pub connect: bool,
    /// Per-attempt timeouts.
    pub timeout: bool,
    /// HTTP 502, 503 and 504.
    pub gateway_status: bool,
    /// HTTP 429 whose wait is at most [`RetryPolicy::max_retry_after`].
    pub rate_limited: bool,
    /// JSON-RPC `-32005` in a single (non-batch) 2xx response.
    pub limit_exceeded: bool,
}

impl RetryOn {
    /// Retry nothing.
    pub const NONE: RetryOn = RetryOn {
        connect: false,
        timeout: false,
        gateway_status: false,
        rate_limited: false,
        limit_exceeded: false,
    };
    /// Retry every transient failure.
    pub const ALL: RetryOn = RetryOn {
        connect: true,
        timeout: true,
        gateway_status: true,
        rate_limited: true,
        limit_exceeded: true,
    };
}

/// How [`Upstream::send`] retries on the same endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts, including the first (0 is treated as 1).
    pub max_attempts: u32,
    /// Wait before the first retry; doubles after each retry.
    pub backoff: Duration,
    /// Cap on the doubled backoff.
    pub max_backoff: Duration,
    /// A 429 asking to wait longer than this is returned immediately as
    /// [`RpcError::RateLimited`] instead of being waited out.
    pub max_retry_after: Duration,
    /// Per-attempt timeout.
    pub timeout: Duration,
    /// Which failures are retried.
    pub retry_on: RetryOn,
}

impl RetryPolicy {
    /// One attempt, no retries, 10 s timeout.
    pub fn none() -> Self {
        RetryPolicy {
            max_attempts: 1,
            backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(1),
            max_retry_after: Duration::ZERO,
            timeout: Duration::from_secs(10),
            retry_on: RetryOn::NONE,
        }
    }

    /// 3 attempts, every transient failure retried, backoff 100 ms doubling
    /// to 1 s, `Retry-After` waited out up to 2 s, 10 s per-attempt timeout.
    pub fn same_endpoint_transient() -> Self {
        RetryPolicy {
            max_attempts: 3,
            backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(1),
            max_retry_after: Duration::from_secs(2),
            timeout: Duration::from_secs(10),
            retry_on: RetryOn::ALL,
        }
    }

    /// Whether this policy retries `error` (attempts and `Retry-After`
    /// limits aside): the [`RetryOn`] flags applied to an error from
    /// [`RpcClient`]. Only 502, 503 and 504 count as gateway statuses.
    pub fn retries(&self, error: &RpcError) -> bool {
        let on = self.retry_on;
        match error {
            RpcError::Timeout => on.timeout,
            RpcError::Connect => on.connect,
            RpcError::RateLimited { .. } => on.rate_limited,
            RpcError::Http(status) => on.gateway_status && matches!(status, 502..=504),
            RpcError::Rpc { code, .. } => on.limit_exceeded && *code == LIMIT_EXCEEDED,
            _ => false,
        }
    }

    /// Run `call` (one request through [`RpcClient`], say) and retry it per
    /// this policy: up to `max_attempts` in all, waiting the backoff between
    /// attempts, or a 429's `Retry-After` when it is at most
    /// `max_retry_after` (a longer one returns the error at once). The
    /// per-attempt `timeout` is the client's business, not applied here.
    pub async fn run<T, F, Fut>(&self, mut call: F) -> Result<T, RpcError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, RpcError>>,
    {
        let max_attempts = self.max_attempts.max(1);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let error = match call().await {
                Err(error) if attempt < max_attempts && self.retries(&error) => error,
                other => return other,
            };
            let wait = match error {
                RpcError::RateLimited {
                    retry_after: Some(wait),
                } if wait > self.max_retry_after => return Err(error),
                RpcError::RateLimited {
                    retry_after: Some(wait),
                } => wait,
                _ => self.backoff_for(attempt),
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// The backoff before retry number `retry` (1-based).
    fn backoff_for(&self, retry: u32) -> Duration {
        let factor = 1u32
            .checked_shl(retry.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.backoff
            .checked_mul(factor)
            .unwrap_or(self.max_backoff)
            .min(self.max_backoff)
    }
}

/// A 2xx response from the upstream.
#[derive(Debug, Clone)]
pub struct UpstreamResponse {
    /// The HTTP status of the final attempt.
    pub status: u16,
    /// The raw response body (JSON-RPC errors stay in here).
    pub body: Bytes,
    /// Attempts made, including the successful one.
    pub attempts: u32,
    /// Total time across all attempts and waits.
    pub latency: Duration,
    /// The last `Retry-After` seen on a 429 along the way, if any.
    pub retry_after: Option<Duration>,
}

/// Send one raw JSON-RPC body (single or batch) to one chosen endpoint,
/// retrying on that same endpoint per `policy`. Never fails over to another
/// endpoint: that is the caller's decision, made from the returned error
/// class.
///
/// Semantics: POST with `content-type: application/json` and the target's
/// headers. A 429 whose `Retry-After` is at most `policy.max_retry_after`
/// (missing = use backoff) is waited out and retried while attempts remain; a
/// longer wait returns `Err(RateLimited)` immediately. Exhausted retries
/// return the last error. Non-retryable non-2xx is `Err(Http(status))`. 2xx is
/// `Ok`, except that `-32005` in a single response is retried when
/// `retry_on.limit_exceeded` and attempts remain.
pub trait Upstream: Send + Sync {
    /// Forward `body` to `target`.
    fn send<'a>(
        &'a self,
        target: &'a Target,
        body: Bytes,
        policy: &'a RetryPolicy,
    ) -> BoxFuture<'a, Result<UpstreamResponse, RpcError>>;
}

/// The production [`Upstream`] over [`RpcClient`]'s connection pool.
#[derive(Debug, Clone)]
pub struct HttpUpstream {
    client: RpcClient,
}

impl HttpUpstream {
    /// Forward through `client`'s pool.
    pub fn new(client: RpcClient) -> Self {
        HttpUpstream { client }
    }
}

impl Upstream for HttpUpstream {
    fn send<'a>(
        &'a self,
        target: &'a Target,
        body: Bytes,
        policy: &'a RetryPolicy,
    ) -> BoxFuture<'a, Result<UpstreamResponse, RpcError>> {
        Box::pin(self.send_inner(target, body, policy))
    }
}

impl HttpUpstream {
    async fn send_inner(
        &self,
        target: &Target,
        body: Bytes,
        policy: &RetryPolicy,
    ) -> Result<UpstreamResponse, RpcError> {
        target.require_http()?;
        let start = Instant::now();
        let max_attempts = policy.max_attempts.max(1);
        let on = policy.retry_on;
        let mut last_retry_after = None;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let can_retry = attempt < max_attempts;
            let backoff = policy.backoff_for(attempt);
            let outcome = self
                .client
                .post_raw(target, body.clone(), Some(policy.timeout))
                .await;
            let wait = match outcome {
                Ok(raw) if (200..300).contains(&raw.status) => {
                    if can_retry
                        && on.limit_exceeded
                        && rpc_error_code(&raw.body) == Some(LIMIT_EXCEEDED)
                    {
                        backoff
                    } else {
                        return Ok(UpstreamResponse {
                            status: raw.status,
                            body: raw.body,
                            attempts: attempt,
                            latency: start.elapsed(),
                            retry_after: last_retry_after,
                        });
                    }
                }
                Ok(raw) if raw.status == 429 => {
                    let retry_after = raw.retry_after;
                    if retry_after.is_some() {
                        last_retry_after = retry_after;
                    }
                    let wait = retry_after.unwrap_or(backoff);
                    if !(can_retry && on.rate_limited) || wait > policy.max_retry_after {
                        return Err(RpcError::RateLimited { retry_after });
                    }
                    wait
                }
                Ok(raw) => {
                    let gateway = matches!(raw.status, 502..=504);
                    if !(can_retry && gateway && on.gateway_status) {
                        return Err(RpcError::Http(raw.status));
                    }
                    backoff
                }
                Err(e) => {
                    let retryable = match e {
                        RpcError::Timeout => on.timeout,
                        RpcError::Connect => on.connect,
                        _ => false,
                    };
                    if !(can_retry && retryable) {
                        return Err(e);
                    }
                    backoff
                }
            };
            tokio::time::sleep(wait).await;
        }
    }
}

/// The JSON-RPC error code of a single-object response body, if any.
/// Batch (array) bodies and bodies without an error yield `None`.
pub fn rpc_error_code(body: &[u8]) -> Option<i64> {
    #[derive(serde::Deserialize)]
    struct Single {
        error: Option<ErrorObj>,
    }
    #[derive(serde::Deserialize)]
    struct ErrorObj {
        code: i64,
    }
    serde_json::from_slice::<Single>(body)
        .ok()?
        .error
        .map(|e| e.code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn quick(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_retry_after: Duration::from_millis(50),
            timeout: Duration::from_secs(1),
            retry_on: RetryOn::ALL,
        }
    }

    #[test]
    fn only_transient_errors_are_retried() {
        let all = RetryPolicy::same_endpoint_transient();
        assert!(all.retries(&RpcError::Timeout));
        assert!(all.retries(&RpcError::Connect));
        assert!(all.retries(&RpcError::Http(503)));
        assert!(all.retries(&RpcError::RateLimited { retry_after: None }));
        assert!(all.retries(&RpcError::Rpc {
            code: LIMIT_EXCEEDED,
            message: "limit".into()
        }));
        assert!(!all.retries(&RpcError::Http(404)));
        assert!(!all.retries(&RpcError::Http(500)));
        assert!(!all.retries(&RpcError::Rpc {
            code: 3,
            message: "execution reverted".into()
        }));
        assert!(!all.retries(&RpcError::ChainMismatch {
            expected: 1,
            actual: 2
        }));
        assert!(!RetryPolicy::none().retries(&RpcError::Timeout));
    }

    #[tokio::test]
    async fn run_retries_transient_errors_up_to_max_attempts() {
        let calls = AtomicU32::new(0);
        let ok = quick(3)
            .run(|| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                async move { if n < 2 { Err(RpcError::Timeout) } else { Ok(n) } }
            })
            .await;
        assert_eq!(ok, Ok(2));

        // Exhausted: the last error.
        calls.store(0, Ordering::SeqCst);
        let failed: Result<(), _> = quick(2)
            .run(|| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err(RpcError::Connect) }
            })
            .await;
        assert_eq!(failed, Err(RpcError::Connect));
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // Not transient: one attempt.
        calls.store(0, Ordering::SeqCst);
        let failed: Result<(), _> = quick(5)
            .run(|| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err(RpcError::Http(404)) }
            })
            .await;
        assert_eq!(failed, Err(RpcError::Http(404)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // A Retry-After beyond the policy's limit is not waited out.
        calls.store(0, Ordering::SeqCst);
        let long = RpcError::RateLimited {
            retry_after: Some(Duration::from_secs(60)),
        };
        let failed: Result<(), _> = quick(5)
            .run(|| {
                calls.fetch_add(1, Ordering::SeqCst);
                let long = long.clone();
                async move { Err(long) }
            })
            .await;
        assert_eq!(failed, Err(long));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn error_code_helper() {
        assert_eq!(
            rpc_error_code(br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32005,"message":"x"}}"#),
            Some(-32005)
        );
        assert_eq!(
            rpc_error_code(br#"{"jsonrpc":"2.0","id":1,"result":"0x1"}"#),
            None
        );
        assert_eq!(rpc_error_code(br#"[{"error":{"code":-32005}}]"#), None);
        assert_eq!(rpc_error_code(b"garbage"), None);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let p = RetryPolicy::same_endpoint_transient();
        assert_eq!(p.backoff_for(1), Duration::from_millis(100));
        assert_eq!(p.backoff_for(2), Duration::from_millis(200));
        assert_eq!(p.backoff_for(4), Duration::from_millis(800));
        assert_eq!(p.backoff_for(5), Duration::from_secs(1));
        assert_eq!(p.backoff_for(60), Duration::from_secs(1));
        assert_eq!(RetryPolicy::none().max_attempts, 1);
    }
}
