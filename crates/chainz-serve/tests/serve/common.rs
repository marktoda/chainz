//! The gateway in-process against [`Mock`] upstreams, with a sandboxed
//! config: [`Rig`] starts one, [`http`] talks to it as a raw client would.

use std::path::Path;
use std::time::Duration;

use chainz_core::StorePaths;
use chainz_core::runtime::Runtime;
use chainz_rpc::control::ControlClient;
use chainz_serve::{Gateway, GatewayConfig};
pub use chainz_testkit::http::Response;
pub use chainz_testkit::{Answer, Mock};
use serde_json::{Value, json};

/// An upstream answering every call with its own name. It answers health
/// probes too (see [`Mock::start_probed`]).
pub fn named(name: &'static str) -> Mock {
    Mock::start_probed(move |_| Answer::Result(json!(name)))
}

/// One raw HTTP/1.1 request to 127.0.0.1:`port` (`Connection: close`).
pub async fn http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Response {
    chainz_testkit::http::send_async(&format!("127.0.0.1:{port}"), method, path, headers, body)
        .await
}

/// A sandboxed config plus a gateway running against it.
pub struct Rig {
    pub dir: tempfile::TempDir,
    pub gateway: Option<Gateway>,
    pub port: u16,
}

impl Rig {
    pub fn paths(dir: &Path) -> StorePaths {
        StorePaths::in_dir(dir.join("config"))
    }

    pub fn runtime_at(dir: &Path) -> Runtime {
        Runtime::at(dir.join("runtime"))
    }

    pub fn runtime(&self) -> Runtime {
        Rig::runtime_at(self.dir.path())
    }

    /// Write `config.toml` the way chainz does (atomically, mode 0600).
    pub fn write_config(dir: &Path, toml: &str) {
        chainz_os::write_private(Rig::paths(dir).config(), toml.as_bytes()).unwrap();
    }

    /// The default test settings: fast probes and config checks.
    pub fn config(dir: &Path) -> GatewayConfig {
        let mut config = GatewayConfig::new(
            Rig::paths(dir),
            Rig::runtime_at(dir),
            Some(dir.join("state")),
        );
        config.port = 0;
        config.timeout = Duration::from_secs(2);
        config.probe_interval = Duration::from_millis(20);
        config.watch_interval = Duration::from_millis(50);
        config
    }

    pub async fn start(toml: &str) -> Rig {
        Rig::start_with(toml, |_| {}).await
    }

    pub async fn start_with(toml: &str, tweak: impl FnOnce(&mut GatewayConfig)) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        Rig::write_config(dir.path(), toml);
        let mut config = Rig::config(dir.path());
        tweak(&mut config);
        let gateway = Gateway::start(config).await.unwrap();
        let port = gateway.addr().port;
        Rig {
            dir,
            gateway: Some(gateway),
            port,
        }
    }

    pub fn gateway(&self) -> &Gateway {
        self.gateway.as_ref().unwrap()
    }

    /// The path part of the gateway URL for `chain`.
    pub fn path(&self, chain: &str, endpoint: Option<&str>) -> String {
        self.gateway().addr().path(Some(chain), endpoint)
    }

    pub fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// POST a JSON body the way a well-behaved client does.
    pub async fn post(&self, path: &str, body: &Value) -> Response {
        http(
            self.port,
            "POST",
            path,
            &[("Host", &self.host()), ("Content-Type", "application/json")],
            &body.to_string(),
        )
        .await
    }

    /// One JSON-RPC call to `chain`.
    pub async fn call(&self, chain: &str, method: &str, params: Value) -> Response {
        self.post(
            &self.path(chain, None),
            &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
        )
        .await
    }

    pub fn control(&self) -> ControlClient {
        ControlClient::new(self.runtime().serve_sock_path())
    }

    /// Wait until `chain`'s head high-water mark is at least `head` and
    /// the gateway holds a current head for each of `pool` (the mocks it
    /// probes, all already serving the head waited for).
    pub async fn wait_for_head(&self, chain: &str, head: u64, pool: &[&Mock]) {
        let start: Vec<usize> = pool.iter().map(|m| m.probes()).collect();
        eventually(|| {
            self.gateway()
                .status()
                .chains
                .iter()
                .any(|c| c.chain == chain && c.head.is_some_and(|h| h >= head))
        })
        .await;
        // Two more probes of each: the first was received after the call
        // began, so it saw the new head, and a probe round only starts
        // once the previous round's results are stored.
        probed_twice_since(pool, &start).await;
    }

    pub async fn shutdown(&mut self) {
        if let Some(gateway) = self.gateway.take() {
            gateway.shutdown().await;
        }
    }
}

/// Wait until each of `mocks` has been probed at least twice more than
/// `start` (their probe counts at some earlier point, in order).
pub async fn probed_twice_since(mocks: &[&Mock], start: &[usize]) {
    eventually(|| mocks.iter().zip(start).all(|(m, s)| m.probes() >= s + 2)).await;
}

/// Poll `condition` for up to 5 s.
#[track_caller]
pub fn eventually(condition: impl FnMut() -> bool) -> impl Future<Output = ()> {
    let caller = std::panic::Location::caller();
    async move {
        chainz_testkit::wait_until_async(
            &format!("the condition at {caller}"),
            Duration::from_secs(5),
            condition,
        )
        .await
    }
}

/// A one-chain config: `eth` (ID 1) with the given active endpoint,
/// failover list and endpoints.
pub fn eth_config(active: &str, failover: &[&str], endpoints: &[(&str, String)]) -> String {
    let failover = failover
        .iter()
        .map(|f| format!("\"{f}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let mut toml = format!(
        "version = 2\n[chains.eth]\nid = 1\naliases = [\"mainnet\"]\nrpc = \"{active}\"\nfailover = [{failover}]\n[chains.eth.rpcs]\n"
    );
    for (name, url) in endpoints {
        toml.push_str(&format!("{name} = \"{url}\"\n"));
    }
    toml
}
