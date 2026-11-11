# Chainz v2: frozen interfaces (Phase 0)

The four streams (Gateway, Dashboard, Address book, Forks) build on these.
Don't change a frozen signature without agreeing it across streams first.
The rustdoc on each item is the reference. This page is the map.

Status: **frozen** means streams depend on it as written. **Provisional**
means the Gateway stream owns it and may change it, as long as it updates
the mock and the Dashboard stream at the same time.

## Crate layout

| Crate | Role | Async / network |
|---|---|---|
| `chainz-core` | config, secrets, migration, resolver, tokenizer, context, labels, runtime registry | none (sync; no tokio, reqwest or hyper) |
| `chainz-rpc` | JSON-RPC client, probes, ranking, health history, `Upstream`, control protocol, chainlist | tokio + reqwest (rustls) |
| `chainz-serve` | gateway: `run(Options)`, embeddable `Gateway::start(GatewayConfig)` | hyper 1 + tokio (multi-thread) |
| `chainz-tui` | dashboard (stub) | Dashboard stream adds ratatui |
| `chainz` (bin) | clap surface, handlers, pickers, output | builds a current-thread runtime only for network commands (`net::block_on`) |

`chainz-rpc` does not depend on `chainz-core`. The bin bridges the two with
`net::target(chainz, chain_name, chain, endpoint_name, endpoint) -> Result<Target>`,
which expands `${VAR}`s.

## Runtime registry (`chainz_core::runtime`), frozen

The directory is `$XDG_RUNTIME_DIR/chainz`. Without `XDG_RUNTIME_DIR` (macOS)
it falls back to the state dir, `$XDG_STATE_HOME/chainz`. The directory is
mode 0700.

```rust
pub struct Runtime { /* dir */ }
impl Runtime {
    pub fn resolve() -> io::Result<Self>;
    pub fn at(dir: impl Into<PathBuf>) -> Self;
    pub fn dir(&self) -> &Path;
    pub fn ensure_dir(&self) -> io::Result<()>;
    pub fn serve_lock_path(&self) -> PathBuf;   // serve.lock
    pub fn serve_json_path(&self) -> PathBuf;   // serve.json
    pub fn serve_sock_path(&self) -> PathBuf;   // serve.sock
    pub fn forks_dir(&self) -> PathBuf;         // forks/
    pub fn fork_path(&self, name: &str) -> PathBuf; // forks/<name>.json
    pub fn serve_running(&self) -> bool;        // non-blocking flock probe; creates nothing
    pub fn acquire_serve_lock(&self) -> io::Result<Option<ServeLock>>; // None = another gateway holds it
    pub fn read_serve_info(&self) -> io::Result<Option<ServeInfo>>;
    pub fn write_serve_info(&self, info: &ServeInfo) -> io::Result<()>; // atomic, 0600
    pub fn remove_serve_info(&self) -> io::Result<()>;
    pub fn gateway_url(&self, chain: &str, endpoint: Option<&str>) -> Option<String>;
    pub fn list_forks(&self) -> io::Result<Vec<ForkRecord>>;
    pub fn read_fork(&self, name: &str) -> io::Result<Option<ForkRecord>>;
    pub fn write_fork(&self, record: &ForkRecord) -> io::Result<()>;
    pub fn remove_fork(&self, name: &str) -> io::Result<()>;
}

pub struct ServeInfo { pub pid: u32, pub port: u16, pub token: String, pub started_at: u64, pub version: String }
impl ServeInfo { pub fn chain_url(&self, chain: &str, endpoint: Option<&str>) -> String } // http://127.0.0.1:<port>/<token>/<chain>[/<endpoint>]

pub struct ForkRecord { pub name: String, pub pid: u32, pub port: u16, pub parent: String,
                        pub chain_id: u64, pub fork_block: Option<u64>, pub started_at: u64 }
impl ForkRecord { pub fn rpc_url(&self) -> String } // http://127.0.0.1:<port>
```

**Exec seam.** `context::build` calls `runtime.gateway_url(chain, endpoint)`
unless `--direct` is set, the chain is a fork, or the endpoint is a websocket.
The seam returns `Some` only while `serve.lock` is held and `serve.json` is
present. The Gateway stream needs no exec changes: hold the lock, write
`serve.json`, and serve `/<token>/<chain>[/<endpoint>]`. `ServeInfo`'s `Debug`
redacts `token`.

**Forks.** The resolver checks configured chains first, then
`forks/<name>.json`. Live forks resolve as `ChainRef::Fork(ForkRecord)`, and
their context points at anvil with no explorer and no `ETH_KEYSTORE`. A fork
can't be named `ls` or `stop` (`names::RESERVED_FORK_NAMES`). The Forks stream
writes and removes records, and must remove stale ones (dead pid).

## Labels (`chainz_core::labels`), frozen

```rust
pub const TOKENS_TOML: &str;                 // include_str!("tokens.toml")
pub struct BuiltinToken { pub chain_id: u64, pub name: String, pub address: Address,
                          pub symbol: String, pub decimals: u8, pub source: String }
pub fn builtin_tokens() -> &'static [BuiltinToken];
pub fn builtin_token(chain_id: u64, name: &str) -> Option<&'static BuiltinToken>;

pub enum LabelSource { User, Signer, Token }
pub enum LabelScope { Chain(u64), AllChains }
pub struct Label { pub name: String, pub address: Address, pub source: LabelSource, pub scope: LabelScope }
pub enum LabelError { Unknown { name: String, suggestion: Option<String> },
                      NotOnChain { name: String, chain_id: u64 } }

pub struct LabelIndex { /* … */ }
impl LabelIndex {
    pub fn new(config: &Config) -> Self;
    pub fn resolve(&self, name: &str, chain_id: u64) -> Result<Label, LabelError>;
    pub fn contains(&self, name: &str) -> bool;
    pub fn reverse(&self, chain_id: u64, address: &Address) -> Option<&Label>;
    pub fn reverse_all(&self, chain_id: u64, address: &Address) -> Vec<&Label>;
    pub fn names(&self) -> impl Iterator<Item = &str>;
    pub fn labels_on(&self, chain_id: u64) -> Vec<Label>;
}
```

Precedence is: user label for the chain, then user `"*"`, then signer, then
built-in token. In config, `[addresses]` is `label -> chain name | "*" ->
address`. For example, `[addresses.myrouter]` with `base = "0x…"`. Label
names follow `names::is_valid_label_name` and can't use a `CONTEXT_TOKENS`
name. The Address book stream owns `addr …`, `label`, and `doctor --tokens`.

Added by the Address book stream (additive; `LabelIndex` is unchanged):

```rust
// chainz_core::edit, inside `transact`
pub const ALL_CHAINS: &str = "*";
pub enum SetAddress { Added, Unchanged, Replaced(Address) }
pub fn check_label_name<'a>(name: &str, signers: impl IntoIterator<Item = &'a str>) -> Result<()>;
pub fn set_address(doc: &mut DocumentMut, name: &str, scope: &str, address: &Address, force: bool) -> Result<SetAddress>;
pub fn remove_address(doc: &mut DocumentMut, name: &str, scope: Option<&str>) -> Result<Vec<(String, String)>>;

// chainz_core::addrbook
pub fn all_labels(config: &Config) -> Vec<Label>;             // every source, every chain
pub fn find(config: &Config, address: &Address) -> Vec<Label>; // `addr find`
pub struct Labeler { /* … */ }                                 // `chainz label`
impl Labeler {
    pub fn for_chain(config: &Config, chain_id: u64) -> Self;
    pub fn any_chain(config: &Config) -> Self;   // "*" labels, signers, names that agree on every chain
    pub fn name(&self, address: &Address) -> Option<&str>;
    pub fn rewrite(&self, text: &[u8], out: &mut Vec<u8>);    // 0x… → name(0x…)
    pub fn filter(&self, input: impl BufRead, output: impl Write) -> io::Result<()>;
}
```

`Labeler::name` skips a built-in token that a user label shadows on that
chain, so a name it returns always resolves back to the same address.

## Upstream (`chainz_rpc::upstream`), frozen

```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub trait Upstream: Send + Sync {
    fn send<'a>(&'a self, target: &'a Target, body: Bytes, policy: &'a RetryPolicy)
        -> BoxFuture<'a, Result<UpstreamResponse, RpcError>>;
}
pub struct RetryOn { pub connect: bool, pub timeout: bool, pub gateway_status: bool,
                     pub rate_limited: bool, pub limit_exceeded: bool } // + RetryOn::NONE / ::ALL
pub struct RetryPolicy { pub max_attempts: u32, pub backoff: Duration, pub max_backoff: Duration,
                         pub max_retry_after: Duration, pub timeout: Duration, pub retry_on: RetryOn }
impl RetryPolicy { pub fn none() -> Self; pub fn same_endpoint_transient() -> Self; }
pub struct UpstreamResponse { pub status: u16, pub body: Bytes, pub attempts: u32,
                              pub latency: Duration, pub retry_after: Option<Duration> }
pub struct HttpUpstream; impl HttpUpstream { pub fn new(client: RpcClient) -> Self }
pub fn rpc_error_code(body: &[u8]) -> Option<i64>;
pub const LIMIT_EXCEEDED: i64 = -32005;
```

`Upstream` retries on the same endpoint only. Failover to another endpoint is
the gateway's job. `Target::new(chain_id, name, url, &[(String, String)])`
holds the expanded URL and headers, and its `Debug` redacts them.
`Retry-After` is clamped to `chainz_rpc::client::MAX_RETRY_AFTER` (60 s).

## Control protocol (`chainz_rpc::control`, unix only), provisional

The control protocol is HTTP/1.1 over `serve.sock`, mode 0600:

| Path | Method | Body |
|---|---|---|
| `/_chainz/status` | GET | `Status { version, pid, port, uptime_secs, chains: Vec<ChainStatus { chain, chain_id, active, failover, head }> }` |
| `/_chainz/metrics` | GET | `MetricsSnapshot { at_ms, windows: Vec<MetricsWindow { window_secs, rows: Vec<MetricsRow { chain, endpoint, method, requests, errors: BTreeMap<ErrorClass,u64>, p50_ms, p95_ms, rate_limited, failovers, bytes }> }> }` |
| `/_chainz/events[?backlog=N][&follow=false]` | GET | NDJSON stream of `RequestRecord { id, at_ms, chain, method, params_summary, upstream, attempts, latency_ms, status: RecordStatus::{Ok, Error{class, code}}, response_bytes }`. First the last `N` buffered records (default 0; the gateway keeps `EVENTS_BUFFER` = 10,000), oldest first, then live records. `follow=false` ends the stream after the backlog. |
| `/_chainz/reload` | POST | empty → 204; 500 when the new config is invalid (the old one stays in force) |

The client is `ControlClient::new(socket)` with async `status()`,
`metrics()`, `events() -> mpsc::Receiver<RequestRecord>`,
`events_with(EventsOptions { backlog, follow })`, and `reload()`.
`MockControlServer::start(path, status, metrics).await` provides
`push_event`, `set_status`, `set_metrics`, `reload_count`, and
`event_subscribers`. The Dashboard stream builds against the mock.
`RequestRecord` never holds params, URLs, or bodies. `params_summary` is
already redacted. The mock keeps pushed events for `backlog`, like the
gateway.

What the real gateway puts in these types:

- `MetricsRow` counts upstream **attempts** per (chain, endpoint, method), so
  a request that failed over shows on both endpoints. `failovers` counts
  requests that moved on *from* that endpoint. A batch is one attempt under
  the method `batch(n)`. Windows are 60 s and 900 s.
- `RequestRecord` is one per client request. `upstream` is the endpoint that
  answered (or failed last), `attempts` spans endpoints, and a JSON-RPC
  error in a single response is `Error { class: Rpc, code }`.
- `ChainStatus.head` is the head high-water mark. It is `None` for a chain
  with no `failover` list: only failover pools are probed.

`SocketPath::new(path)` gives a bindable/connectable path for sockets deeper
than `sockaddr_un` allows (Linux: via `/proc/self/fd`). The client, the mock
and the gateway all use it.

## Other stable seams

- Config: `Chainz::open()` gives lock-free reads and auto-migrates v1 under
  the lock. `chainz.transact(|tx| { … })` holds the lock while it parses both
  files, runs your `edit::*` calls on `tx.doc` / `tx.secrets`, validates, and
  writes secrets then config. The writes are atomic, fsynced, and 0600. Only
  write config through `transact`.
- Resolver: `resolve::select(config, Some(&runtime), explicit) -> Result<Option<(ChainRef, Source)>>`.
  It checks the explicit argument, then `$CHAINZ_CHAIN`, then the default.
  Management commands use `resolve_configured` (no forks) or `resolve_exact`
  (rm).
- Context: `context::build(&chainz, &runtime, &ChainRef, &ExecOptions) -> Result<Context>`,
  then `Context::env()`, `Expander::new(..).expand_args(&argv)`.
  `CONTRACT_VARS` lists every name exec owns. Exec sets the ones it has and
  removes the rest from the child env.
- Variables: `${NAME}` resolves to the built-ins (`CHAIN_ID`, `CHAIN`), then
  the process env, then `secrets.toml [vars]`.

## Bin crate: adding a command handler

1. The clap surface in `crates/chainz/src/cli.rs` is complete, including the
   stream commands. Keep its flags and add new ones only for your own command.
2. (Done: every stub is replaced and `commands/stubs.rs` is gone.) Replace your stub in `commands/stubs.rs` with a module, for example
   `commands/serve.rs` with `pub fn run(...) -> anyhow::Result<i32>`. Then
   change the route in `commands::dispatch`. The return value is the exit
   code. An `Err` prints `Error: …` and exits 1. `ui::cancelled()` exits 130.
3. Network work goes through `net::block_on(async { … })`, which creates a
   current-thread tokio runtime. A long-running server (`serve`, `top`) can
   build a multi-threaded runtime in its own crate. `exec`, `env`, `ls`, and
   `shell` must stay free of runtimes, sockets, and threads. The Linux strace
   test (`crates/chainz/tests/strace.rs`) enforces this.
4. Tests go in `crates/chainz/tests/cli.rs` (or a new file) through
   `Sandbox::new()`. Its `.cmd()` sets `HOME`, all four XDG dirs, and
   `CHAINZ_DISABLE_KEYRING`, and scrubs inherited context vars. `.seed(&[("eth", 1)])`
   writes chains on a dead local RPC with a plaintext `default` signer.
   `chain_id_rpc(id, n)` serves `n` `eth_chainId` answers. Never run the
   binary outside a sandbox. Run `cargo test` with `TMPDIR` pointed at a
   scratch directory.
5. Before committing, run `cargo fmt`,
   `cargo clippy --workspace --all-targets -- -D warnings`, and
   `cargo test --workspace`. The perf gate is `scripts/perf-gate.sh V1 V2`.
