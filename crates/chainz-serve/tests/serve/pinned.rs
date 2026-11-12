//! The pinned (single-endpoint) proxy against a scripted mock upstream on
//! loopback.
use crate::common::{Answer, Mock, Response, eventually, http, named};
use chainz_core::runtime::GatewayAddr;
use chainz_rpc::{RpcClient, Target};
use chainz_serve::PinnedProxy;
use serde_json::json;

async fn proxy_to(mock: &Mock) -> PinnedProxy {
    let target = Target::new(1, "pinned", &mock.url(), &[]).unwrap();
    PinnedProxy::start(RpcClient::new(), target).await.unwrap()
}

/// The proxy's port and token, read back from its URL.
fn addr_of(proxy: &PinnedProxy) -> GatewayAddr {
    let uri: hyper::Uri = proxy.url().parse().unwrap();
    GatewayAddr {
        port: uri.port_u16().unwrap(),
        token: uri.path().trim_matches('/').to_string(),
    }
}

/// POST `body` to the proxy as a local client does (loopback `Host`, JSON).
async fn post(proxy: &PinnedProxy, body: &str) -> Response {
    let addr = addr_of(proxy);
    let host = format!("127.0.0.1:{}", addr.port);
    let headers = [
        ("Host", host.as_str()),
        ("Content-Type", "application/json"),
    ];
    http(addr.port, "POST", &addr.path(None, None), &headers, body).await
}

const BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_only_tokened_loopback_json_posts() {
    let a = named("a");
    let proxy = proxy_to(&a).await;
    let addr = addr_of(&proxy);
    assert_eq!(proxy.url(), addr.base_url());
    let path = &addr.path(None, None);
    assert_eq!(addr.token.len(), 64);
    let host = format!("127.0.0.1:{}", addr.port);
    let json_type = ("Content-Type", "application/json");

    let ok = post(&proxy, BODY).await;
    assert_eq!((ok.status, ok.json()["result"].as_str()), (200, Some("a")));
    let ok = http(
        addr.port,
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
        let reply = http(addr.port, method, path, headers, BODY).await;
        assert_eq!(reply.status, *expected, "{method} {path} {headers:?}");
        assert!(!reply.head.contains("access-control"));
    }
    assert_eq!(a.calls().len(), 2, "a refused request reached upstream");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_failures_are_http_errors_without_the_url() {
    let a = Mock::start_probed(|_| Answer::Http(429, Some("3")));
    let proxy = proxy_to(&a).await;
    let reply = post(&proxy, BODY).await;
    assert_eq!(reply.status, 429);
    assert!(reply.head.contains("retry-after: 3"), "{}", reply.head);
    assert!(!reply.body.contains("127.0.0.1"), "{}", reply.body);
    // The same answer the gateway gives a rate limit.
    assert_eq!(reply.json()["error"]["code"], json!(-32005));

    let mut b = named("b");
    let proxy = proxy_to(&b).await;
    b.kill();
    let reply = post(&proxy, BODY).await;
    assert_eq!(reply.status, 502);

    // An upstream 4xx is passed on, as the gateway does.
    let c = Mock::start_probed(|_| Answer::Http(403, None));
    let proxy = proxy_to(&c).await;
    let reply = post(&proxy, BODY).await;
    assert_eq!(reply.status, 403);
    assert_eq!(reply.json()["error"]["code"], json!(-32000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_limit_exceeded_answer_is_retried_on_the_same_endpoint() {
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let a = Mock::start_probed(move |_| {
        match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => Answer::Error(-32005),
            _ => Answer::Result(json!("a")),
        }
    });
    let proxy = proxy_to(&a).await;
    let reply = post(&proxy, BODY).await;
    assert_eq!(
        (reply.status, reply.json()["result"].as_str()),
        (200, Some("a"))
    );
    assert_eq!(a.calls().len(), 2);

    // Retries run out: the last -32005 answer, as the endpoint sent it.
    let b = Mock::start_probed(|_| Answer::Error(-32005));
    let proxy = proxy_to(&b).await;
    let reply = post(&proxy, BODY).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["error"]["message"], json!("mock error"));
    assert_eq!(b.calls().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_releases_the_port() {
    let a = named("a");
    let proxy = proxy_to(&a).await;
    let port = addr_of(&proxy).port;
    drop(proxy);
    eventually(|| std::net::TcpStream::connect(("127.0.0.1", port)).is_err()).await;
}
