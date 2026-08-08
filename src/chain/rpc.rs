use super::RpcEndpoint;
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use anyhow::Result;
use std::time::{Duration, Instant};

/// Global deadline for a single RPC health probe in interactive sweeps.
pub const CHECK_DEADLINE: Duration = Duration::from_secs(4);

/// Test whether an (already-expanded) RPC endpoint serves the expected chain
/// id, sending the endpoint's custom headers if any. No sweep deadline:
/// explicit single-URL validation keeps the provider's own 10s connect
/// timeout. The single definition of "is this RPC healthy".
///
/// Error messages deliberately omit the URL (it may embed API keys) and
/// header values (they may be credentials); callers that report failures to
/// the user should attach the *raw* (unexpanded) URL as context themselves.
pub async fn check_url(endpoint: &RpcEndpoint, expected_chain_id: u64) -> Result<()> {
    let provider = create_provider(endpoint).await?;
    let chain_id = provider
        .get_chain_id()
        .await
        // Provider errors may repeat credential-bearing URL paths. Keep the
        // diagnostic at this seam URL-free; callers attach a redacted URL.
        .map_err(|_| anyhow::anyhow!("Failed to query the RPC chain ID"))?;
    if chain_id != expected_chain_id {
        anyhow::bail!(
            "Chain ID mismatch: expected {}, got {}",
            expected_chain_id,
            chain_id
        );
    }
    Ok(())
}

/// One health probe under CHECK_DEADLINE, with measured latency.
pub async fn probe(endpoint: &RpcEndpoint, expected_chain_id: u64) -> (bool, Duration) {
    let start = Instant::now();
    let healthy = tokio::time::timeout(CHECK_DEADLINE, check_url(endpoint, expected_chain_id))
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);
    (healthy, start.elapsed())
}

pub struct ProbeResult {
    pub index: usize,
    pub healthy: bool,
    pub latency: Duration,
}

/// Probe endpoints concurrently, yielding each result as it lands
/// (completion order). The receiver yields exactly `endpoints.len()`
/// results, then closes.
pub fn probe_urls(
    endpoints: &[RpcEndpoint],
    expected_chain_id: u64,
) -> tokio::sync::mpsc::Receiver<ProbeResult> {
    let (tx, rx) = tokio::sync::mpsc::channel(endpoints.len().max(1));
    for (index, endpoint) in endpoints.iter().cloned().enumerate() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let (healthy, latency) = probe(&endpoint, expected_chain_id).await;
            let _ = tx
                .send(ProbeResult {
                    index,
                    healthy,
                    latency,
                })
                .await;
        });
    }
    rx
}

/// Picker ordering: healthy probes fastest-first, then unhealthy ones in
/// their original order.
pub fn rank_by_health(results: &[ProbeResult]) -> Vec<usize> {
    let mut healthy: Vec<&ProbeResult> = results.iter().filter(|r| r.healthy).collect();
    healthy.sort_by_key(|r| r.latency);
    let mut unhealthy: Vec<&ProbeResult> = results.iter().filter(|r| !r.healthy).collect();
    unhealthy.sort_by_key(|r| r.index);
    healthy
        .into_iter()
        .chain(unhealthy)
        .map(|r| r.index)
        .collect()
}

/// Collecting wrapper: one health flag per input endpoint, in input order.
pub async fn check_urls(endpoints: &[RpcEndpoint], expected_chain_id: u64) -> Vec<bool> {
    let mut results = vec![false; endpoints.len()];
    let mut rx = probe_urls(endpoints, expected_chain_id);
    while let Some(result) = rx.recv().await {
        results[result.index] = result.healthy;
    }
    results
}

async fn create_provider(endpoint: &RpcEndpoint) -> Result<DynProvider> {
    if endpoint.headers.is_empty() {
        // Header-free endpoints keep the connection-string path (ws:// support).
        let provider = tokio::time::timeout(
            Duration::from_secs(10),
            ProviderBuilder::new().connect(&endpoint.url),
        )
        .await
        .map_err(|_| anyhow::anyhow!("RPC connection timed out"))?
        .map_err(|_| anyhow::anyhow!("Failed to initialize the RPC connection"))?;
        return Ok(provider.erased());
    }

    let url: reqwest::Url = endpoint
        .url
        .parse()
        // The URL may embed credentials; the diagnostic stays URL-free.
        .map_err(|_| anyhow::anyhow!("Invalid RPC URL"))?;
    let mut header_map = reqwest::header::HeaderMap::new();
    for (name, value) in &endpoint.headers {
        let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| anyhow::anyhow!("Invalid RPC header name '{name}'"))?;
        // Header values are credentials: name the header, never echo the value.
        let header_value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| anyhow::anyhow!("Invalid value for RPC header '{name}'"))?;
        header_map.insert(header_name, header_value);
    }
    let client = reqwest::Client::builder()
        .default_headers(header_map)
        .build()
        .map_err(|_| anyhow::anyhow!("Failed to initialize the RPC connection"))?;
    Ok(ProviderBuilder::new().connect_reqwest(client, url).erased())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn result(index: usize, healthy: bool, ms: u64) -> ProbeResult {
        ProbeResult {
            index,
            healthy,
            latency: Duration::from_millis(ms),
        }
    }

    #[test]
    fn rank_healthy_fastest_first_then_unhealthy_in_order() {
        let results = vec![
            result(0, false, 4000),
            result(1, true, 150),
            result(2, true, 20),
            result(3, false, 4000),
        ];
        assert_eq!(rank_by_health(&results), vec![2, 1, 0, 3]);
    }

    #[tokio::test]
    async fn probe_urls_reports_every_url() {
        // connection-refused fails fast; no network needed
        let urls: Vec<RpcEndpoint> = vec!["http://localhost:1".into(), "http://localhost:2".into()];
        let mut rx = probe_urls(&urls, 1);
        let mut seen = Vec::new();
        while let Some(result) = rx.recv().await {
            assert!(!result.healthy);
            seen.push(result.index);
        }
        seen.sort();
        assert_eq!(seen, vec![0, 1]);
    }

    #[tokio::test]
    async fn probe_urls_empty_input_closes_immediately() {
        let mut rx = probe_urls(&[], 1);
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn rpc_error_chain_never_repeats_the_url() {
        let error = check_url(&"http://user:password@127.0.0.1:1/literal-secret".into(), 1)
            .await
            .unwrap_err();
        let diagnostic = format!("{error:#}");
        for secret in ["user", "password", "literal-secret"] {
            assert!(!diagnostic.contains(secret), "{diagnostic}");
        }
    }

    #[tokio::test]
    async fn check_url_sends_custom_headers() -> anyhow::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            // echo back the request's JSON-RPC id so alloy accepts the response
            let id: u64 = request
                .rfind("\"id\":")
                .map(|i| {
                    request[i + 5..]
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect::<String>()
                        .parse()
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            let body = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":"0x1"}}"#);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            request
        });

        let mut headers = std::collections::BTreeMap::new();
        headers.insert("x-test-header".to_string(), "expected-value".to_string());
        let endpoint = crate::chain::RpcEndpoint::with_headers(format!("http://{addr}"), headers);

        check_url(&endpoint, 1).await?;

        let request = server.await?;
        let request_lower = request.to_lowercase();
        assert!(
            request_lower.contains("x-test-header: expected-value"),
            "header missing from request: {request_lower}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_header_value_error_never_echoes_the_value() {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("x-test-header".to_string(), "bad\u{0000}value".to_string());
        let endpoint =
            crate::chain::RpcEndpoint::with_headers("http://127.0.0.1:1".to_string(), headers);
        let error = check_url(&endpoint, 1).await.unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("x-test-header"), "{diagnostic}");
        assert!(!diagnostic.contains("bad"), "{diagnostic}");
    }
}
