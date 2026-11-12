//! Raw JSON-RPC forwarding to one chosen endpoint, and the one retry loop.
//!
//! The gateway and the fork proxy forward through [`HttpUpstream`]. It
//! never fails over to another endpoint; that decision belongs to the
//! caller, made from the returned error ([`RpcError::is_transient`],
//! [`RpcError::is_rate_limited`]).

use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;

use crate::client::{RpcClient, check_status};
use crate::error::{LIMIT_EXCEEDED, RpcError};
use crate::target::Target;

/// Which failures a [`RetryPolicy`] retries on the same endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOn {
    /// Retry nothing.
    Nothing,
    /// Retry every transient failure ([`RpcError::is_transient`]) and
    /// every rate limit ([`RpcError::is_rate_limited`]; a 429 only while
    /// its wait is at most [`RetryPolicy::max_retry_after`]).
    Transient,
}

/// How [`RetryPolicy::run`] (and so [`HttpUpstream::send`]) retries on the
/// same endpoint. Per-attempt timeouts are the [`RpcClient`]'s.
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
    /// Which failures are retried.
    pub retry_on: RetryOn,
}

impl RetryPolicy {
    /// One attempt, no retries.
    pub fn none() -> Self {
        RetryPolicy {
            max_attempts: 1,
            backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(1),
            max_retry_after: Duration::ZERO,
            retry_on: RetryOn::Nothing,
        }
    }

    /// 3 attempts, every transient failure retried, backoff 100 ms doubling
    /// to 1 s, `Retry-After` waited out up to 2 s.
    pub fn same_endpoint_transient() -> Self {
        RetryPolicy {
            max_attempts: 3,
            backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(1),
            max_retry_after: Duration::from_secs(2),
            retry_on: RetryOn::Transient,
        }
    }

    /// For many requests in a row to one public endpoint: up to five
    /// attempts on transient failures, waiting 0.5, 1, 2 then 4 s, or a
    /// rate limit's `Retry-After` up to 10 s.
    pub fn patient() -> Self {
        RetryPolicy {
            max_attempts: 5,
            backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(4),
            max_retry_after: Duration::from_secs(10),
            retry_on: RetryOn::Transient,
        }
    }

    /// Whether this policy retries `error` (attempts and `Retry-After`
    /// limits aside).
    pub fn retries(&self, error: &RpcError) -> bool {
        match self.retry_on {
            RetryOn::Nothing => false,
            RetryOn::Transient => error.is_transient() || error.is_rate_limited(),
        }
    }

    /// Run `call` (one request through [`RpcClient`], say) and retry it per
    /// this policy: up to `max_attempts` in all, waiting the backoff between
    /// attempts, or a 429's `Retry-After` when it is at most
    /// `max_retry_after` (a longer one returns the error at once).
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

/// Forwards raw JSON-RPC bodies (single or batch) to one chosen endpoint
/// over [`RpcClient`]'s connection pool.
#[derive(Debug, Clone)]
pub struct HttpUpstream {
    client: RpcClient,
}

impl HttpUpstream {
    /// Forward through `client`'s pool (and under its timeouts).
    pub fn new(client: RpcClient) -> Self {
        HttpUpstream { client }
    }

    /// Send `body` to `target`, retrying on that same endpoint per
    /// `policy` ([`RetryPolicy::run`]), and return the 2xx response body.
    /// Never fails over to another endpoint.
    ///
    /// POST with `content-type: application/json` and the target's
    /// headers. A 429 is [`RpcError::RateLimited`], another non-2xx is
    /// [`RpcError::Http`]. A 2xx body is returned as it is, JSON-RPC errors
    /// included, except that a single response carrying [`LIMIT_EXCEEDED`]
    /// is retried when the policy retries rate limits (the last one is
    /// returned as it is).
    pub async fn send(
        &self,
        target: &Target,
        body: Bytes,
        policy: &RetryPolicy,
    ) -> Result<Bytes, RpcError> {
        target.require_http()?;
        // Only a policy that retries rate limits looks into the body.
        let inspect = policy.retries(&limit_exceeded());
        // The last 2xx body that carried LIMIT_EXCEEDED, as it was sent.
        let limited: Mutex<Option<Bytes>> = Mutex::new(None);
        let (client, body, limited_ref) = (&self.client, &body, &limited);
        let result = policy
            .run(move || async move {
                let raw = client.post_raw(target, body.clone()).await?;
                check_status(&raw)?;
                if inspect && rpc_error_code(&raw.body) == Some(LIMIT_EXCEEDED) {
                    *limited_ref.lock().unwrap_or_else(|e| e.into_inner()) = Some(raw.body);
                    return Err(limit_exceeded());
                }
                Ok(raw.body)
            })
            .await;
        match result {
            Err(error) if error == limit_exceeded() => limited
                .into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .ok_or(error),
            other => other,
        }
    }
}

/// The error a [`LIMIT_EXCEEDED`] answer stands for while it is retried.
fn limit_exceeded() -> RpcError {
    RpcError::Rpc {
        code: LIMIT_EXCEEDED,
        message: "limit exceeded".into(),
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
            retry_on: RetryOn::Transient,
        }
    }

    #[test]
    fn only_transient_errors_are_retried() {
        let all = RetryPolicy::same_endpoint_transient();
        assert!(all.retries(&RpcError::Timeout));
        assert!(all.retries(&RpcError::Connect));
        assert!(all.retries(&RpcError::Http(503)));
        assert!(all.retries(&RpcError::BodyRead));
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

    #[test]
    fn patient_waits_longer() {
        let p = RetryPolicy::patient();
        assert_eq!(p.max_attempts, 5);
        let waits: Vec<_> = (1..5).map(|r| p.backoff_for(r).as_millis()).collect();
        assert_eq!(waits, [500, 1000, 2000, 4000]);
        assert!(p.retries(&RpcError::Timeout));
        assert!(!p.retries(&RpcError::Rpc {
            code: 3,
            message: "execution reverted".into()
        }));
    }
}
