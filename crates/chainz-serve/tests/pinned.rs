//! The pinned (single-endpoint) proxy against a scripted mock upstream on
//! loopback.
mod common;

use chainz_rpc::{HttpUpstream, RetryPolicy, RpcClient, Target};
use chainz_serve::PinnedProxy;
use common::{Mock, R, http};
use serde_json::json;

async fn proxy_to(mock: &Mock) -> PinnedProxy {
    let target = Target::new(1, "pinned", &mock.url(), &[]).unwrap();
    PinnedProxy::start(
        HttpUpstream::new(RpcClient::new()),
        target,
        RetryPolicy::none(),
    )
    .await
    .unwrap()
}

const BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_only_tokened_loopback_json_posts() {
    let a = Mock::named("a").await;
    let proxy = proxy_to(&a).await;
    let url = proxy.url();
    let path = url.split_once(&format!(":{}", proxy.port())).unwrap().1;
    let token = path.trim_start_matches('/');
    assert_eq!(token.len(), 64);
    let host = format!("127.0.0.1:{}", proxy.port());
    let json_type = ("Content-Type", "application/json");

    let ok = http(
        proxy.port(),
        "POST",
        path,
        &[("Host", &host), json_type],
        BODY,
    )
    .await;
    assert_eq!((ok.status, ok.json()["result"].as_str()), (200, Some("a")));
    let ok = http(
        proxy.port(),
        "POST",
        path,
        &[("Host", "localhost"), json_type],
        BODY,
    )
    .await;
    assert_eq!(ok.status, 200);

    let wrong = format!("/{}", "0".repeat(64));
    type Case<'a> = (&'a str, &'a str, Vec<(&'a str, &'a str)>, u16);
    let cases: Vec<Case> = vec![
        ("POST", &wrong, vec![("Host", &host), json_type], 403),
        ("POST", "/", vec![("Host", &host), json_type], 403),
        ("POST", path, vec![("Host", "evil.example"), json_type], 403),
        ("POST", path, vec![("Host", "[::1]"), json_type], 403),
        ("POST", path, vec![json_type], 403),
        ("GET", path, vec![("Host", &host)], 405),
        (
            "POST",
            path,
            vec![("Host", &host), ("Content-Type", "text/plain")],
            415,
        ),
    ];
    for (method, path, headers, expected) in &cases {
        let reply = http(proxy.port(), method, path, headers, BODY).await;
        assert_eq!(reply.status, *expected, "{method} {path} {headers:?}");
        assert!(!reply.head.contains("access-control"));
    }
    assert_eq!(a.calls().len(), 2, "a refused request reached upstream");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_failures_are_http_errors_without_the_url() {
    let a = Mock::start("a", |_| R::Http(429, Some("3"))).await;
    let proxy = proxy_to(&a).await;
    let path = format!("/{}", proxy.url().rsplit('/').next().unwrap());
    let host = format!("127.0.0.1:{}", proxy.port());
    let headers = [
        ("Host", host.as_str()),
        ("Content-Type", "application/json"),
    ];
    let reply = http(proxy.port(), "POST", &path, &headers, BODY).await;
    assert_eq!(reply.status, 429);
    assert!(reply.head.contains("retry-after: 3"), "{}", reply.head);
    assert!(!reply.body.contains("127.0.0.1"), "{}", reply.body);
    // The same answer the gateway gives a rate limit.
    assert_eq!(reply.json()["error"]["code"], json!(-32005));

    let mut b = Mock::named("b").await;
    let proxy = proxy_to(&b).await;
    b.kill().await;
    let path = format!("/{}", proxy.url().rsplit('/').next().unwrap());
    let host = format!("127.0.0.1:{}", proxy.port());
    let headers = [
        ("Host", host.as_str()),
        ("Content-Type", "application/json"),
    ];
    let reply = http(proxy.port(), "POST", &path, &headers, BODY).await;
    assert_eq!(reply.status, 502);

    // An upstream 4xx is passed on, as the gateway does.
    let c = Mock::start("c", |_| R::Http(403, None)).await;
    let proxy = proxy_to(&c).await;
    let path = format!("/{}", proxy.url().rsplit('/').next().unwrap());
    let host = format!("127.0.0.1:{}", proxy.port());
    let headers = [
        ("Host", host.as_str()),
        ("Content-Type", "application/json"),
    ];
    let reply = http(proxy.port(), "POST", &path, &headers, BODY).await;
    assert_eq!(reply.status, 403);
    assert_eq!(reply.json()["error"]["code"], json!(-32000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_releases_the_port() {
    let a = Mock::named("a").await;
    let proxy = proxy_to(&a).await;
    let port = proxy.port();
    proxy.shutdown().await;
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
    );
    // Dropping works too.
    let proxy = proxy_to(&a).await;
    let port = proxy.port();
    drop(proxy);
    common::eventually(|| std::net::TcpStream::connect(("127.0.0.1", port)).is_err()).await;
}
