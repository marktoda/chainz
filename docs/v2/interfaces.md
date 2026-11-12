# Chainz v2: crate contracts

The seams between the crates: what each one offers the others. The rustdoc
on each item is the reference; this page is the map. Changing one of these
means changing every caller in the same commit.

## Crate layout

| Crate | Role | Async / network |
|---|---|---|
| `chainz-sys` | OS primitives: private dirs and files, atomic writes, `flock`, file stamps, the clock (no chainz dependencies) | none |
| `chainz-core` | config, secrets, migration, resolver, tokenizer, context, labels, runtime registry | none (sync; no tokio, reqwest or hyper) |
| `chainz-rpc` | JSON-RPC client, probes, ranking, health history, `Upstream`, control protocol, chainlist | tokio + reqwest (rustls) |
| `chainz-serve` | gateway: `run(Options)`, embeddable `Gateway::start(GatewayConfig)` | hyper 1 + tokio (multi-thread) |
| `chainz-tui` | the `top` dashboard | ratatui + tokio |
| `chainz` (bin) | clap surface, handlers, pickers, output | builds a current-thread runtime only for network commands (`net::block_on`) |

`chainz-rpc` does not depend on `chainz-core`. The bin bridges the two with
`net::target(chainz, chain_name, chain_id, endpoint_name, endpoint) -> Result<Target>`,
which expands `${VAR}`s through
`Chainz::expand_endpoint(chain_name, chain_id, endpoint_name, &Endpoint) -> Result<ExpandedEndpoint>`
(the same call the gateway and `top` use; the chain need not be configured
yet).

## Runtime registry (`chainz_core::runtime`)

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
present. The gateway holds the lock, writes `serve.json`, and serves
`/<token>/<chain>[/<endpoint>]`. `ServeInfo`'s `Debug` redacts `token`.

**Forks.** The resolver checks configured chains first, then
`forks/<name>.json`. Live forks resolve as `ChainRef::Fork(ForkRecord)`, and
their context points at anvil with no explorer and no `ETH_KEYSTORE`. A fork
can't be named `ls`, `list` or `stop` (`names::RESERVED_FORK_NAMES`). `fork` writes
and removes records, and removes stale ones (dead pid).

## Labels (`chainz_core::labels`)

```rust
pub const TOKENS_TOML: &str;                 // include_str!("tokens.toml")
pub struct BuiltinToken { pub chain_id: u64, pub name: String, pub address: Address,
                          pub symbol: String, pub decimals: u8, pub source: String }
pub fn builtin_tokens() -> &'static [BuiltinToken];
pub fn builtin_token(chain_id: u64, name: &str) -> Option<&'static BuiltinToken>;
pub fn builtin_token_at(chain_id: u64, address: &Address) -> Option<&'static BuiltinToken>;
pub const TOKEN_ORDER: [&str; 6];            // weth, usdc, usdce, usdt, dai, wbtc (display order)
pub const NATIVE_DECIMALS: u8 = 18;
pub fn native_symbol(chain_id: u64) -> &'static str; // ETH, BNB, POL, xDAI…

pub enum LabelSource { User, Signer, Token }  // Display: user, signer, built-in
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
    pub fn labels(&self) -> impl Iterator<Item = Label> + '_; // every source, every chain
    pub fn labels_on(&self, chain_id: u64) -> Vec<Label>;
    pub fn shadowed(&self, label: &Label) -> Option<Label>; // the label that wins on its chain
}
```

`Config::chain_display(id)` names a chain in output: configured name, else
the well-known short name, else `chain <id>`. `ChainRef::chain_id(&config)`
is the chain ID labels resolve on (a fork's own).

Precedence is: user label for the chain, then user `"*"`, then signer, then
built-in token. In config, `[addresses]` is `label -> chain name | "*" ->
address`. For example, `[addresses.myrouter]` with `base = "0x…"`. Label
names follow `names::check_label_name` and can't use a name from
`names::TOKENS` (context, deprecated and removed tokens).

The address book (`addr …`, `label`, `doctor --tokens`) is built on:

```rust
// chainz_core::labels / names
pub const ALL_CHAINS: &str = "*";
pub fn names::check_label_name<'a>(name: &str, signers: impl IntoIterator<Item = &'a str>) -> Result<()>;

// chainz_core::tx::Tx, inside `transact`
pub enum SetAddress { Added, Unchanged, Replaced(Address) }
pub enum Overwrite { Refuse, Replace }
pub fn set_address(&mut self, name: &str, scope: &str, address: &Address, overwrite: Overwrite) -> Result<SetAddress>;
pub fn remove_address(&mut self, name: &str, scope: Option<&str>) -> Result<Vec<(String, String)>>;

// chainz_core::addrbook
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

`addr import` reads Foundry broadcast output with `chainz_core::broadcast`:

```rust
pub fn kebab(contract: &str) -> String;                  // UniversalRouter → universal-router
pub fn find_runs(paths: &[PathBuf]) -> Result<Vec<PathBuf>>; // files as given; dirs → broadcast/**/run-latest.json
pub fn read(file: &Path) -> Result<Vec<Run>>;            // one Run per chain (multi-chain runs hold several)
pub fn parse(text: &str, file: &Path) -> Result<Vec<Run>>;
pub struct Run { pub file: PathBuf, pub chain_id: u64, pub contracts: Vec<Deployed> }
pub struct Deployed { pub contract: Option<String>, pub address: Option<Address>, pub problem: Option<String> }
pub struct Options { pub prefix: String, pub only: Vec<String>, pub force: bool }
pub struct AddressWrite { pub name: String, pub chain: String, pub address: Address }
pub enum Action { Add(AddressWrite), Unchanged, Overwrite(AddressWrite, Address), Skip(String) }
impl Action { pub fn write(&self) -> Option<&AddressWrite>; pub fn as_str(&self) -> &'static str; }
pub struct Planned { pub file: PathBuf, pub chain_id: u64, pub chain: Option<String>,
                     pub contract: Option<String>, pub name: Option<String>,
                     pub address: Option<Address>, pub action: Action }
pub fn plan(config: &Config, runs: &[Run], options: &Options) -> Vec<Planned>;
```

`Deployed::problem` covers a failed transaction, a transaction with no receipt
(when the file has receipts), and a missing name or address. `plan` puts each
label on `config.chain_by_id(run.chain_id)`. It never uses a fork. It checks
names with `names::check_label_name`, drops repeats of the same (chain, name,
address), and suffixes later different addresses with `names::unique_name`.
The command plans again inside `transact`, under the lock (or `preview` for a
dry run), and applies every planned write with `tx.write_address(&write)`.

## Upstream (`chainz_rpc::upstream`)

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
impl RetryPolicy {
    pub fn none() -> Self;
    pub fn same_endpoint_transient() -> Self;
    pub fn retries(&self, error: &RpcError) -> bool;        // is this error retried?
    pub async fn run<T, F, Fut>(&self, call: F) -> Result<T, RpcError>; // backoff + Retry-After
}
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
`RpcClient` never follows a redirect: a 3xx is `RpcError::Http(status)`, so
a target's headers never reach another host.

## Loopback auth (`chainz_serve::auth`)

For any local server that hands a child process a tokenised
`http://127.0.0.1:<port>/<token>…` URL (the gateway; the fork proxy).

```rust
pub const TOKEN_BYTES: usize = 32;                         // 256 bits
pub fn new_token() -> String;                              // 64 lowercase hex digits
pub fn token_matches(given: &str, expected: &str) -> bool; // constant-time
pub fn host_allowed(host: Option<&str>, port: u16) -> bool;
```

`host_allowed` accepts `127.0.0.1` or `localhost` (any case), with `port` or
no port, and nothing else: not `[::1]` (the servers listen on IPv4 only), and
not a missing `Host`.

`chainz_serve::PinnedProxy` is a single-endpoint server built on these
checks, for a fork's `--fork-url`: every body goes to one `Target` through an
`Upstream` under the given `RetryPolicy`, never anywhere else.

```rust
impl PinnedProxy {
    pub async fn start(upstream: Arc<dyn Upstream>, target: Target, policy: RetryPolicy)
        -> io::Result<PinnedProxy>;      // binds 127.0.0.1:0
    pub fn url(&self) -> String;         // http://127.0.0.1:<port>/<token>
    pub fn port(&self) -> u16;
    pub async fn shutdown(self);         // drains up to 5 s; dropping stops at once
}
```

It accepts only `POST` with `application/json` to `/<token>` (a trailing `/`
is allowed) from a loopback `Host`, and answers 403/405/415 otherwise. An
upstream failure is an HTTP error (429 with `Retry-After`, 504, or 502) with a
JSON-RPC error body that never names the URL.

## Control protocol (`chainz_rpc::control`, unix only)

The control protocol is HTTP/1.1 over `serve.sock`, mode 0600:

| Path | Method | Body |
|---|---|---|
| `/_chainz/status` | GET | `Status { version, pid, port, uptime_secs, chains: Vec<ChainStatus { chain, chain_id, active, failover, head }> }` |
| `/_chainz/metrics` | GET | `MetricsSnapshot { at_ms, windows: Vec<MetricsWindow { window_secs, rows: Vec<MetricsRow { chain, endpoint, method, requests, errors: BTreeMap<ErrorClass,u64>, p50_ms, p95_ms, rate_limited, failovers, bytes }> }> }` |
| `/_chainz/events[?backlog=N][&follow=false]` | GET | NDJSON stream of `RequestRecord { id, at_ms, chain, method, params_summary, upstream, attempts, latency_ms, status: RecordStatus::{Ok, Error{class, code}}, response_bytes }`. First the last `N` buffered records (default 0; the gateway keeps `EVENTS_BUFFER` = 10,000), oldest first, then live records. `follow=false` ends the stream after the backlog. |
| `/_chainz/reload` | POST | empty → 204; 500 when the new config is invalid (the old one stays in force) |

The client is `ControlClient::new(socket)` with async `status()`,
`metrics()`, `events() -> mpsc::Receiver<RequestRecord>`,
`events_with(EventsOptions { backlog, follow })`, and `reload()`. Each call
gives up after `CONTROL_TIMEOUT` (5 s; `.with_timeout(d)` changes it), so a
wedged gateway can't hang `serve status` or the dashboard; only the body of
an events stream is open-ended. `RequestRecord` never holds params, URLs, or
bodies. `params_summary` is already redacted.

`EventRing` is the buffer behind `/_chainz/events`: the last
`EVENTS_BUFFER` records plus a live broadcast, with `push` (as given),
`push_numbered` (assigns IDs 1, 2, …) and `subscribe(EventsOptions)`. The
gateway's inspector and the mock share it.

With the `mock` cargo feature (enable it in `[dev-dependencies]`),
`MockControlServer::start(path, status, metrics).await` provides
`push_event`, `set_status`, `set_metrics`, `reload_count`, and
`event_subscribers`, and keeps pushed events for `backlog` like the gateway.
The dashboard's tests run against it.

What the real gateway puts in these types:

- `MetricsRow` counts upstream **attempts** per (chain, endpoint, method), so
  a request that failed over shows on both endpoints. `failovers` counts
  requests that moved on *from* that endpoint. `rate_limited` counts 429s
  plus requests the endpoint's own bucket turned away unsent (those are not
  in `requests`). A batch is one attempt under the method `batch(n)`.
  Windows are 60 s and 900 s.
- `RequestRecord` is one per client request. `upstream` is the endpoint that
  answered (or failed last), `attempts` spans endpoints, and a JSON-RPC
  error in a single response is `Error { class: Rpc, code }`.
- `ChainStatus.head` is the head high-water mark: the best head over the
  last three probe rounds. It is `None` for a chain with no `failover`
  list: only failover pools are probed.

`SocketPath::new(path)` gives a bindable/connectable path for sockets deeper
than `sockaddr_un` allows (Linux: via `/proc/self/fd`). The client, the mock
and the gateway all use it.

## Other seams

- Config: `Chainz::open()` gives lock-free reads and auto-migrates v1 under
  the lock. `chainz.transact(|tx| { … })` holds the lock while it parses both
  files, runs your `Tx` methods, validates, and writes secrets then config.
  The writes are atomic, fsynced, and 0600. Only write config through
  `transact`. Each `Tx` method picks its own file (`set_var` and signer key
  material go to `secrets.toml`, everything else to `config.toml`). Records
  are written by serializing the model types (`Chain`, `Endpoint`,
  `Explorer`, `Signer`, `SignerSecret`) and merging them into the existing
  table: unchanged keys keep their exact text and comments, changed values
  are replaced in place, new keys are appended. `tx.config()` /
  `tx.secrets()` are the typed views of the edit so far. Hand edits go
  through `Chainz::config_text` + `replace_text(expected, new)` and
  `Chainz::chain_snippet` + `replace_chain_snippet`, which refuse if the file
  changed meanwhile.
  `chainz.preview(|tx| …)` runs the same closure on the files as they are
  and validates the result but writes nothing (dry runs, confirmations).
  `Chainz::peek()` (`LoadMode::Peek`) is the side-effect-free load for shell
  completion: no v1 migration, no permission changes, no validation.
  `migrate::migrate_locked(&WriteLock)` takes the held lock as proof; a
  migration that runs inside `transact` is reported like one on first load.
- Chain edits: `tx.edit_chain(name, &ChainEdit) -> Result<EditReport>` applies
  one patch (`chain set` flags or its menu): signer, `rpc` (an endpoint name,
  or a URL: an existing one activates, a new one is added with its secrets
  extracted and activated), headers and `clear_headers` on that endpoint (or
  the active one), failover, explorer (`etherscan` creates the built-in
  `Explorer::etherscan()`: `api = "https://api.etherscan.io/v2/api?chainid=${CHAIN_ID}"`,
  `key = "${ETHERSCAN_API_KEY}"`, the URL v1 migration writes), `api_key`
  (stored in the explorer's variable, `ETHERSCAN_API_KEY` or
  `<EXPLORER>_API_KEY`), aliases, and the rename last. `EditReport` lists the
  changes, the added endpoint, the created explorer, the key variable and
  the moved secrets. Checking a new URL's chain ID is the caller's (network)
  job. `tx.insert_chain(name, &chain, Attach)` checks name, ID and signer and,
  with `Attach::Defaults`, attaches the only signer and
  `[explorers.etherscan]` if configured (`Attach::ExplorerOnly` for
  `--no-signer`, `Attach::Nothing` keeps the chain as given; `Attached` says
  what was added). `tx.attach_signer_where_missing(signer)` gives a first
  signer to every chain without one and returns them.
- Notices: core never prints. A load or write that has something to say (a
  v1 migration, a file whose loose permissions could not be tightened)
  queues a `store::Notice` on the `Chainz`; take them with `take_notices()`
  or route them as they happen with `set_notice_sink(fn(&Notice))`. The bin
  prints each `Display` line as `chainz: …`; serve adds them to the
  snapshot warnings.
- Signers: `signer::provision(&dyn KeyBackend, &dyn Prompter, name, key, backend)`
  stores a key and returns a `Provision` (`Provision::reference(Signer)` for
  keystore and 1Password references). `Chainz::install_signer` writes the
  record and key material in one transaction, abandons the provision (deletes
  the keyring entry it created) if that fails, and deletes a replaced
  signer's old keyring entry. `Chainz::remove_signer` is the reverse.
  `KeyBackend` is external storage only; `Prompter` (hidden prompts) is
  implemented by the binary (`prompt::Terminal`). `CHAINZ_DISABLE_KEYRING`
  (`signer::DISABLE_KEYRING_VAR`) makes the keyring unavailable for tests and
  the perf gate.
- Resolver: `resolve::select(config, Some(&runtime), explicit, chainz.env()) -> Result<Option<(ChainRef, Source)>>`.
  It checks the explicit argument, then `$CHAINZ_CHAIN`, then the default.
  Management commands use `resolve::configured(config, query, Policy) ->
  Result<(&str, &Chain)>` (no forks): `Policy::Prefix` (name, alias, ID or
  unambiguous prefix), `Policy::Exact` (name or ID: `chain rm`) or
  `Policy::ExactOrAlias` (`addr rm --chain`).
- Context: `context::build(&chainz, &runtime, &ChainRef, &ExecOptions) -> Result<Context>`
  (with `expose_key`, it refuses a fork or a chain without a signer), then
  `Context::unlock_key(&chainz, keys, prompter)` for `--expose-key`,
  `Context::exports() -> Exports { set, unset, warnings }` computed once, and
  `Exports::apply(&mut Command)`, which removes every contract variable and
  sets this context's (it refuses an `--expose-key` context whose key was
  never unlocked). Each `EnvVar` carries its own secrecy (`display()`
  redacts). `Context::explain()` is the safe view `--explain` prints.
  `Expander::new(&chainz, &runtime, &ctx, direct).expand_args(&argv)`.
  `contract::CONTRACT_VARS` lists every name exec owns (`contract` is a leaf,
  with `CHAINZ_CHAIN`, `RAW_PRIVATE_KEY` and `ETHERSCAN_API_KEY`).
- Environment seam: `contract::Env` is where core reads the environment
  (`Env::process()`, or `Env::from_fn` in tests). `Chainz::with_env` sets it
  for template resolution and the context; `resolve::select` takes it.
- Variables: `${NAME}` resolves to the built-ins (`CHAIN_ID`, `CHAIN`), then
  the environment, then `secrets.toml [vars]`.
- Display policy: `redact::url(value, reveal)` and `redact::header(value,
  reveal)` print templates, hosts and ordinary paths verbatim and mask only
  credential-shaped parts (user info, key-like query values, long mixed
  alphanumeric segments, known Infura/Alchemy keys, the fragment) as
  `redact::REDACTED`; an unparseable URL fails closed. `extract` owns the
  credential shapes; `extract::manual_credential(url)` is the detector for
  what only a person can move (migration notes and `doctor`). `redact` and
  `summarize` stay as the stricter `Debug` and compact views.
- Shared helpers, so crates don't keep private copies:
  `chainz_core::fmt::{group, uptime, short_address}` (thousands separators,
  `42s`/`12m05s`/`2h03m`/`3d04h`, `short_address(&Address)` → `0x1234…abcd`);
  `Address::from_bytes` / `as_bytes` (the field is private). Model types are
  imported from `chainz_core::model` only (no root re-exports).
  `chainz_rpc::hex::{encode, decode, parse_quantity, parse_quantity_saturating}`;
  `chainz_rpc::erc20::{SYMBOL, DECIMALS, decode_string, decode_u8}`;
  `HealthStore::empty()` and `HealthStore::load_or_empty(Option<&Path>)` for
  callers that treat an unreadable store as empty.

## Bin crate: adding a command handler

1. The clap surface lives in `crates/chainz/src/cli.rs`. Add flags only for
   your own command.
2. Put the handler in a module, for example `commands/serve.rs` with
   `pub fn run(...) -> anyhow::Result<i32>`, and route it in
   `commands::dispatch`. The return value is the exit code. An `Err` prints
   `Error: …` and exits 1, or with the code of a `ui::ExitCode` in its chain
(exec uses 127/126). `ui::cancelled()` exits 130.
3. Network work goes through `net::block_on(async { … })`, which creates a
   current-thread tokio runtime. A long-running server (`serve`, `top`) can
   build a multi-threaded runtime in its own crate. `exec`, `env`, `ls`, and
   `shell` must stay free of runtimes, sockets, and threads. The Linux strace
   test (`crates/chainz/tests/strace.rs`) enforces this.
4. Tests go in `crates/chainz/tests/cli.rs` (or a new file with
   `mod common;`) through `common::Sandbox::new()`. Its `.cmd()` /
   `.process()` clear the environment down to `PATH`, `TMPDIR`,
   `RUST_BACKTRACE` and `LLVM_PROFILE_FILE`, then set `HOME`, all four XDG
   dirs, `CHAINZ_DISABLE_KEYRING` and `NO_COLOR`, with stdin empty.
   `.seed(&[("eth", 1)])` writes chains on a dead local RPC with a plaintext
   `default` signer; `.run`/`.ok` run the binary. `common::MockRpc::start(handler)`
   is a threaded keep-alive JSON-RPC mock (batches too) that records every
   request. Never run the binary outside a sandbox, and never guess a free
   port: use port 0 (`serve --port 0`, `anvil --port 0`) or let `fork`
   pick, then read the port back. Run `cargo test` with `TMPDIR` pointed at
   a scratch directory.
5. Before committing, run `cargo fmt`,
   `cargo clippy --workspace --all-targets -- -D warnings`, and
   `cargo test --workspace`. The perf gate is `scripts/perf-gate.sh V1 V2`.
