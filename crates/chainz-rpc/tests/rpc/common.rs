//! Upstreams for the client tests: a [`Mock`] whose URL and headers carry
//! secrets, so every error can be checked for redaction.

use chainz_rpc::Target;
pub use chainz_testkit::{Mock, Reply, Request};

/// A secret embedded in every mock URL path, to check redaction.
pub const URL_SECRET: &str = "urlsekrit9f8e7d";
/// A secret header value, to check redaction.
pub const HEADER_SECRET: &str = "hdrsekrit1a2b3c";

/// The URL and headers a [`Mock`] is reached by, both secret.
pub trait Upstream {
    /// A target for `mock` on `chain_id`: a secret in the path and in an
    /// `X-Api-Key` header.
    fn target(&self, chain_id: u64) -> Target;
}

impl Upstream for Mock {
    fn target(&self, chain_id: u64) -> Target {
        Target::new(
            chain_id,
            "mock",
            &format!("{}/v3/{URL_SECRET}", self.url()),
            &[("X-Api-Key".into(), HEADER_SECRET.into())],
        )
        .unwrap()
    }
}

/// A target on a local port nothing listens on.
pub fn dead_target() -> Target {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    Target::new(
        1,
        "dead",
        &format!("http://127.0.0.1:{port}/v3/{URL_SECRET}"),
        &[],
    )
    .unwrap()
}

/// Assert that neither Display nor Debug of `e` leaks a secret.
pub fn assert_redacted(e: &impl std::fmt::Debug, display: &str) {
    let dbg = format!("{e:?}");
    for text in [&dbg, display] {
        assert!(!text.contains(URL_SECRET), "URL leaked: {text}");
        assert!(!text.contains("127.0.0.1"), "host leaked: {text}");
        assert!(!text.contains(HEADER_SECRET), "header leaked: {text}");
    }
}
