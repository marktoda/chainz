# Chainz v2: contracts

What chainz promises to the outside (file formats, environment variables,
the CLI, JSON output, the control protocol) and what each crate offers the
others. Only public items another crate uses are listed; `pub(crate)` and
test-only items are not contracts. The rustdoc on each item is the
reference; this page is the map. Changing one of these means changing
every caller (and the docs) in the same commit.

## Crate layout

| Crate | Role | Async / network |
|---|---|---|
| `chainz-os` | OS primitives: private dirs and files, atomic writes, `flock` locks and probes, file stamps, the clock (no chainz dependencies) | none |
| `chainz-core` | config, secrets, migration, resolver, routes, tokenizer, exec contract, context, labels, display redaction, runtime registry | none (sync; no tokio, reqwest or hyper) |
| `chainz-rpc` | JSON-RPC client, probes, endpoint health and ranking, health history, `HttpUpstream`, ABI helpers, control protocol, chainlist | tokio + reqwest (rustls) |
| `chainz-serve` | gateway: `run(paths, runtime, state_dir, Options, on_event)`, embeddable `Gateway::start(GatewayConfig)`, the fork's `PinnedProxy` | hyper 1 + tokio (multi-thread) |
| `chainz-tui` | the `top` dashboard: `run(chainz, runtime, store, Options)` | ratatui + tokio |
| `chainz` (bin) | clap surface, handlers, pickers, output, fork supervisor, background launch | builds a current-thread runtime only for network commands (`net::block_on`) |

Every crate depends on `chainz-os`. `chainz-rpc` does not depend on
`chainz-core`; serve, tui and the bin join them. `chainz-tui`'s tests use
`chainz-serve`'s `testing` feature.

## Files

The config schema (`config.toml`, `secrets.toml`) is in
[design.md, Config v2](design.md#config-v2). Hand edits are welcome: writes
keep unchanged keys' text and comments. `secrets.toml` and every file
below are mode 0600 in 0700 directories.

| File | Written by | Meaning |
| --- | --- | --- |
| `$XDG_CONFIG_HOME/chainz/config.lock` | any config write | exclusive `flock` serializing writes (reads take no lock) |
| `$XDG_STATE_HOME/chainz/health.json` (+ `health.lock`) | probes (`doctor`, `doctor CHAIN`, `top`) | per-endpoint sample ring (`HealthStore::CAPACITY` = 64) and capability tags; a corrupt file is moved aside to `health.json.corrupt` |
| `$XDG_STATE_HOME/chainz/serve.log`, `forks/<name>.log` | `serve --detach`, fork supervisor | process output (`paths::serve_log`, `paths::fork_log`) |
| `$XDG_CACHE_HOME/chainz/` | `add`, `doctor CHAIN --discover` | the chainlist cache (rebuildable; `--refresh`) |
| runtime dir: `serve.lock`, `serve.json`, `serve.sock`, `forks/<name>.{lock,json}`, `ports/<port>.lock` | gateway, fork supervisor | see [Runtime registry](#runtime-registry-chainz_coreruntime) |

`edit --path` prints the config path, and `doctor` lists every other one.

## Environment variables

| Variable | Read by | Meaning |
| --- | --- | --- |
| `CHAINZ_CHAIN` | resolver, templates | the chain context (an `exec` subshell, `eval "$(chainz env …)"`); step 2 of chain selection. Inside a context, the exec contract's own variables are not read from the environment |
| exec contract (`ETH_RPC_URL`, `CHAIN_ID`, …) | exported by `exec` (and its subshell) and `env` | the table in [design.md, Exec contract](design.md#exec-contract); every name is a const in `chainz_core::contract`, listed in `CONTRACT_VARS` and unset before a context sets its own |
| any `${NAME}` | templates | after the built-ins (`CHAIN_ID`, `CHAIN`), before `secrets.toml` |
| `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_RUNTIME_DIR`, `HOME` | `chainz_core::paths` | file locations; without `XDG_RUNTIME_DIR` the runtime dir is the state dir |
| `CHAINZ_DISABLE_KEYRING` | `chainz_core::signer` | set: the OS keyring is unavailable (tests, perf gate) |
| `CHAINZ_COMPLETE` | bin `complete` | a shell completion request (set by the `completions` loader) |
| `CHAINZ_DETACHED` | bin `sys` | set in a `--detach` child |
| `VISUAL`, `EDITOR` | `edit` | the editor, in that order |
| `SHELL`, `PS1` | `exec` with no command, `env` | the subshell and export syntax |
| `NO_COLOR` | every styled output: bin `ui` (`console`), `top` | no colour (the test sandbox sets it) |
| `CHAINZ_BLESS`, `CHAINZ_REQUIRE_TOOLS` | tests | `1` rewrites golden files; set, a missing anvil or strace fails the test instead of skipping it |

## CLI

The command tree is [design.md, CLI surface](design.md#cli-surface) and
`chainz <command> --help`. Removed commands and flags get no hidden aliases,
and no v1 spelling is accepted: an old name is an unknown command (exit 2). Usage lines in
errors are rendered from the clap definition.

Exit codes: 0 on success, also when stdout closes early (`chainz … | head`);
1 for an error (`error: …` on stderr) or a result the command reports
itself (`doctor` found problems, `label 0x…` matched nothing, `serve status`
found no gateway, a fork's anvil died); 2 for a usage error, a missing
argument off a terminal included (always `cli::usage_error(path, message)`,
which prints the subcommand's usage line); 126 and 127
when `exec`'s command can't run or doesn't exist; 130 when a prompt is
cancelled; a foreground `fork` exits 130 after SIGINT, 143 after SIGTERM or
`fork stop`, 129 after SIGHUP.

## JSON output

Every `--json` is pretty-printed JSON on stdout. Secret values are
redacted ([Display policy](#other-seams)) unless `--reveal`. Golden files
(`crates/chainz/tests/golden/`, `crates/chainz-tui/tests/golden/`) pin
these shapes.

| Command | Shape |
| --- | --- |
| `ls` | `[ChainView]`, where `ChainView` is `{name, id, aliases, rpc, rpc_url, failover, signer, explorer, default, endpoints: [{name, url, active, failover, websocket, headers?}]}` |
| `ls CHAIN` | one `ChainView`, each endpoint with its latest health: `{…, latency_ms, head, probed_at_ms, error, tags}` (`probed_at_ms` is when that sample was taken) |
| `signer ls` | `[{name, backend, address, chains}]` |
| `var ls [NAME…]` | `[{name, value, used_by}]` (`value` is `[REDACTED] (N chars)` unless `--reveal`; `used_by` lists every config path, which the text summarises unless `-v`) |
| `label ls` | `[{name, scope, chain_id, address, source, shadowed}]` |
| `label 0x… --json` | the same rows, for one address (exit 1 when empty) |
| `label import` | `[{chain, chain_id, name, address, action, reason?, previous?, contract, file}]`; `action` is `added`, `unchanged`, `overwritten` or `skipped` |
| `fork ls` | `[{name, pid, port, parent, chain_id, fork_block, started_at, url, block, uptime_secs}]` (the command's own rows, not the registry format) |
| `serve status` | `{running, version, pid, port, uptime_secs, chains, last_60s, totals_60s, chain_totals_60s}`: `chains` is `Status.chains`, `last_60s` the 60 s `MetricsRow`s, `totals_60s`/`chain_totals_60s` the window's `MetricsTotals` (absent from a gateway that sends none). Stopped: only `{running: false}`, exit 1 |
| `top --once --json` | `{at, gateway: {running, port}, chains: [{name, id, aliases, rpc, via_gateway, status, head, block_timestamp, block_age_secs, block_time_secs, base_fee_wei, base_fee_gwei, latency_ms, p50_ms, lag_blocks, error, endpoints: [{name, url, active, websocket, status, tags, latency_ms, p50_ms, head, lag_blocks, error, probed_at_ms}]}], balances: [{signer, address, chains: [{chain, id, via, native, tokens, error}]}], traffic}` |

In `top`'s JSON, `chains[].rpc` and `endpoints[].active` name the endpoint
the gateway serves the chain from when the head was read through it, else
the configured active one. `via` is `multicall3` or `batch`. An amount is
`{symbol, decimals, raw, formatted, saturated}` (`symbol` always present).
An `error` is `{class, message}` with the `ErrorClass` name as `class`,
or `internal` for a crashed `--once` task. `traffic` is `null` without a gateway; its window
totals are `null` from a gateway that sends none.

## Control protocol (`chainz_rpc::control`)

HTTP/1.1 over `serve.sock` in the runtime dir, mode 0600. Paths are
`ControlRoute::{Status, Metrics, Events, Reload}.path()`, and each route's
`method()`:

| Path | Method | Body |
|---|---|---|
| `/_chainz/status` | GET | `Status { version, pid, port, uptime_secs, chains: Vec<ChainStatus { chain, chain_id, active, failover, head }> }` |
| `/_chainz/metrics` | GET | `MetricsSnapshot { at_ms, windows: Vec<MetricsWindow { window_secs, rows: Vec<MetricsRow { chain, endpoint, method, requests, errors: BTreeMap<ErrorClass,u64>, p50_ms, p95_ms, rate_limited, failovers, bytes }>, totals?: MetricsTotals, chains?: BTreeMap<chain, MetricsTotals> }> }` with `MetricsTotals { requests, errors, rate_limited, failovers, p50_ms, p95_ms, bytes }` |
| `/_chainz/events[?backlog=N][&after=ID][&follow=false]` | GET | NDJSON stream of `RequestRecord { id, at_ms, chain, method, params_summary, upstream, attempts, latency_ms, status: RecordStatus::{Ok, Error{class, code}}, response_bytes }`. First the last `N` buffered records with an ID above `after` (default 0; the gateway keeps 10,000), oldest first, then live records above `after`. `follow=false` ends the stream after the backlog. A malformed value is 400. |
| `/_chainz/reload` | POST | empty → 204; 500 with the cause as plain text when the new config is invalid (the old one stays in force) |

A known path with the wrong method is 405 (with `Allow`); anything else is
404. Fields marked `?` came after 2.0 and are optional both ways: absent from
a 2.0 gateway, ignored by a 2.0 client. `totals` and `chains` are the window's
rows together with true percentiles (not averages of the rows'). Windows are
named `Window::{Short, Long}` (60 s, 900 s); find one with
`MetricsSnapshot::window(Window)`. `RecordStatus` displays as `ok`,
`class(code)` or `class`.

The client is `ControlClient::new(socket)` with async `status()`,
`metrics()`, `events(EventsOptions { backlog: Backlog::{Last(n), All},
after_id, follow }) -> mpsc::Receiver<RequestRecord>`, and `reload()` (a
refused reload's error quotes the gateway's reason). `Backlog::All` goes out
as the largest count, which a 2.0 gateway reads the same way, and the client
drops records at or below `after_id` itself, for a 2.0 gateway that ignores
it. Each call gives up after 5 s, so a wedged gateway can't hang
`serve status` or the dashboard; only the body of an events stream is
open-ended. `RequestRecord` never holds params, URLs, or bodies.
`params_summary` is already redacted. NDJSON lines are framed by
`write_line(&value)` and `read_line(&mut buf)`; `EventsOptions::from_query`
parses the events query on the server side.

The server is `chainz-serve`'s: the gateway runs it over its live state. With
the `testing` cargo feature (enable it in `[dev-dependencies]`),
`chainz_serve::testing::ControlServer::start(path, status, metrics).await`
runs the same handler over canned state, with `push_event` (kept for
`backlog`, IDs as given), `set_metrics`, `reload_count` (every reload
succeeds), and `event_subscribers`. The dashboard's tests run against it.

What the real gateway puts in these types:

- `MetricsRow` counts upstream **attempts** per (chain, endpoint, method), so
  a request that failed over shows on both endpoints. `failovers` counts
  requests that moved on *from* that endpoint. `rate_limited` counts 429s
  plus requests the endpoint's own bucket turned away unsent (those are not
  in `requests`). A batch is one attempt under the method `batch(n)`.
  Windows are 60 s and 900 s, and every window carries `totals` and
  `chains`, even with no traffic.
- `RequestRecord` is one per client request. `upstream` is the endpoint that
  answered (or failed last), `attempts` spans endpoints, and a JSON-RPC
  error in a single response is `Error { class: Rpc, code }`.
- `ChainStatus.head` is the head high-water mark: the best head over the
  last three probe rounds. It is `None` for a chain with no `failover`
  list: only failover pools are probed.

`SocketPath::new(path)` gives a bindable/connectable path for sockets deeper
than `sockaddr_un` allows (Linux: via `/proc/self/fd`). The client and the
server both use it.

## OS primitives (`chainz_os`)

```rust
pub const PRIVATE_DIR_MODE: u32 = 0o700; pub const PRIVATE_FILE_MODE: u32 = 0o600;
pub fn is_private_mode(mode: u32) -> bool;                  // no group/other bits
pub fn create_private_dir(dir: &Path) -> io::Result<()>;
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()>; // atomic, fsynced, 0600
pub fn read_private(path: &Path) -> io::Result<Option<PrivateText { text, warning }>>; // tightens to 0600
pub fn open_private_log(path: &Path, mode: LogMode) -> io::Result<File>; // LogMode::{Append, Truncate}
// locks: liveness is "holds the lock"
pub fn probe_lock(path: &Path) -> io::Result<bool>;          // one shared, non-blocking attempt, released at once
pub fn try_lock_file(path: &Path, contention: Contention) -> io::Result<Option<File>>; // exclusive; None = held
pub enum Contention { GiveUp, OutwaitProbes }               // retry for PROBE_GRACE (100 ms) past probes
pub fn lock_file(path: &Path) -> io::Result<File>;           // exclusive, blocking
pub struct FileStamp; impl FileStamp { pub fn of(path: &Path) -> Option<FileStamp> } // compare with ==
pub fn now_ms() -> u64; pub fn now_secs() -> u64;
```

One lock protocol serves `serve.lock`, the fork locks and the port
reservations: readers `probe_lock` (two probes never contend), owners
`try_lock_file(.., OutwaitProbes)`, so a probe landing as an owner starts
does not make it fail. A probe's I/O error is an error, never "held". The
config write lock and `health.lock` use `lock_file`.

## Routes (`chainz_core::route`)

Core decides where a chain's requests go, and each consumer turns the
answer into a `chainz_rpc::Target` with one line:

```rust
pub struct RouteOpts { pub endpoint: Option<String>, pub direct: bool }
pub struct Route { pub chain_id: u64, pub chain: String, pub endpoint: String, pub url: String,
                   pub headers: Vec<(String, String)>, pub display: String,
                   pub via_gateway: bool, pub websocket: bool }  // Debug redacts url and headers
impl Chainz {
    pub fn route(&self, chain: &str, opts: &RouteOpts, runtime: &Runtime) -> Result<Route>;
    pub fn direct_route(&self, chain_name: &str, chain_id: u64, endpoint_name: &str,
                        endpoint: &Endpoint) -> Result<Route>; // never the gateway; chain need not be configured
}
impl Route { pub fn via_gateway(info: &ServeInfo, chain: &str, chain_id: u64, endpoint: &str, pin: bool) -> Route }
impl Chain { pub fn endpoints_by_preference(&self, http_only: bool) -> Vec<(&str, &Endpoint)> } // active, failover, rest
pub fn uses_gateway(websocket: bool, direct: bool) -> bool;

// the adapter (bin net::to_target, tui plan::target)
Target::new(route.chain_id, &route.endpoint, &route.url, &route.headers)
```

The rule: through a running gateway (pinned to the endpoint when one is
named) unless `direct` or the endpoint is a websocket; otherwise the
endpoint itself, `${VAR}`s expanded. `RouteOpts::endpoint` is `--rpc
NAME|URL`, read by `Chain::rpc_choice` (the rule `update --rpc` uses): a
URL the chain has no endpoint for goes straight to it, for that route only,
never written and never through the gateway (`exec`/`env --rpc`
and `fork --rpc`). `direct_route` is the one public way
to expand an endpoint; the gateway, `top`, probes and forks all use it.

## Runtime registry (`chainz_core::runtime`)

The directory is `$XDG_RUNTIME_DIR/chainz`. Without `XDG_RUNTIME_DIR` (macOS)
it falls back to the state dir, `$XDG_STATE_HOME/chainz`. The directory is
mode 0700.

```rust
pub const LOOPBACK: Ipv4Addr;            // 127.0.0.1: every server binds it
pub const DEFAULT_PORT: u16 = 18545;     // the gateway's
pub fn loopback_url(port: u16) -> String; // http://127.0.0.1:<port>

pub struct GatewayAddr { pub port: u16, pub token: String }   // Debug redacts token
impl GatewayAddr {
    pub fn path(&self, chain: Option<&str>, endpoint: Option<&str>) -> String; // /<token>[/<chain>[/<endpoint>]]
    pub fn base_url(&self) -> String;                                      // http://127.0.0.1:<port>/<token>
    pub fn url(&self, chain: &str, endpoint: Option<&str>) -> String;
    pub fn parse_path(path: &str) -> Option<AddrPath<'_>>; // at most one trailing '/', no empty segment
}
pub struct AddrPath<'a> { pub token: &'a str, pub chain: Option<&'a str>, pub endpoint: Option<&'a str> }

pub struct Runtime { /* dir */ }
impl Runtime {
    pub fn resolve() -> io::Result<Self>;
    pub fn at(dir: impl Into<PathBuf>) -> Self;
    pub fn dir(&self) -> &Path;
    pub fn ensure_dir(&self) -> io::Result<()>;
    pub fn serve_json_path(&self) -> PathBuf;   // serve.json
    pub fn serve_sock_path(&self) -> PathBuf;   // serve.sock
    pub fn forks_dir(&self) -> PathBuf;         // forks/
    // the gateway
    pub fn gateway(&self) -> io::Result<GatewayState>;  // one probe_lock, then serve.json; creates nothing
    pub fn acquire_serve_lock(&self) -> io::Result<Option<ServeLock>>; // None = another gateway holds it
    // forks
    pub fn claim_fork(&self, name: &str) -> io::Result<Option<ForkClaim>>; // None = a live fork has it
    pub fn fork(&self, name: &str) -> io::Result<Option<ForkRecord>>;     // live and ready only
    pub fn fork_live(&self, name: &str) -> io::Result<bool>;
    pub fn reserve_port(&self, port: u16) -> io::Result<Option<PortReservation>>; // None: another chainz has it
    pub fn forks(&self) -> io::Result<Vec<ForkEntry>>;                    // read-only, with liveness
    pub fn reap_forks(&self, stop_orphan: impl FnMut(&ForkRecord)) -> io::Result<Vec<String>>;
    pub fn reap_fork(&self, name: &str, stop_orphan: impl FnMut(&ForkRecord)) -> io::Result<bool>;
}

pub enum GatewayState { Down, Starting, Up(ServeInfo) } // Starting: lock held, serve.json not (or no longer) out
impl ServeLock { pub fn publish(&self, info: &ServeInfo) -> io::Result<Published> } // dropping Published removes serve.json

pub struct ServeInfo { pub format: u32, pub pid: u32, pub port: u16, pub token: String,
                       pub started_at: u64, pub version: String }
impl ServeInfo { pub const FORMAT: u32 = 1; pub fn new(addr: &GatewayAddr, started_at: u64) -> Self;
                 pub fn addr(&self) -> GatewayAddr }

pub struct ForkRecord { pub format: u32, pub name: String, pub pid: u32, pub anvil_pid: u32, pub port: u16,
                        pub parent: String, pub chain_id: u64, pub fork_block: Option<u64>,
                        pub started_at: u64, pub ready: bool }
impl ForkRecord { pub const FORMAT: u32 = 1; pub fn rpc_url(&self) -> String } // http://127.0.0.1:<port>
pub struct ForkEntry { pub name: String, pub live: bool, pub supervisor: Option<u32>,
                       pub record: Result<Option<ForkRecord>, String> }
impl ForkEntry { pub fn ready(&self) -> Option<&ForkRecord> }
impl ForkClaim { pub fn name(&self) -> &str; pub fn register(&self, record: &ForkRecord) -> io::Result<()> }
impl PortReservation { pub fn port(&self) -> u16 }
```

| File | Written by | Meaning |
| --- | --- | --- |
| `serve.lock` | gateway | exclusive `flock` for the gateway's whole life |
| `serve.json` | gateway | `ServeInfo` (format 1, 0600), published once both listeners are bound, withdrawn first on shutdown |
| `serve.sock` | gateway | control socket (0600) |
| `forks/<name>.lock` | fork supervisor | exclusive `flock` for the fork's whole life (`O_CLOEXEC`: anvil never inherits it); holds the supervisor's PID |
| `forks/<name>.json` | fork supervisor | `ForkRecord` (format 1): written when anvil spawns, `ready` once the fork serves |
| `ports/<port>.lock` | fork supervisor | exclusive `flock` on anvil's port, taken before anvil spawns and held for the fork's whole life: two chainz processes never pick one port |

Records carry a `format`; a reader refuses a format it does not know (an
error, never "not running"). Liveness is always the lock, never a PID check.
`acquire_serve_lock` removes a dead gateway's `serve.json` and fails if it
can't. Fork files are only removed by the lock's holder: the supervisor on
exit, or a reaper that took a dead fork's lock (`reap_forks` calls
`stop_orphan` first, so the bin can stop an anvil the supervisor left). A
claimant checks that the file it locked is still the one at the path.

**Exec seam.** `context::build` routes through `Chainz::route`: the gateway
URL while `Runtime::gateway()` is `Up`, unless `--direct` is set, the
chain is a fork, or the endpoint is a websocket. An unreadable `serve.json`
under a held lock fails `exec` (`--direct` bypasses it). The gateway
serves `/<token>/<chain>[/<endpoint>]`, parsed by `GatewayAddr::parse_path`;
`<chain>` is an ID, name or alias in any case (`resolve::exact`), never a
prefix.

**Forks.** The resolver checks configured chains first (exact ID, name,
alias), then a live, ready fork's exact name, then prefixes. Live forks
resolve as `ChainRef::Fork(ForkRecord)`, and their context points at anvil
with no explorer and no `ETH_KEYSTORE`. The name rules are
`chainz_core::forks`: `check_fork_name` (a valid chain name, not a
`RESERVED_FORK_NAMES` subcommand, not a configured chain's name or alias),
`default_fork_name(parent)` (`<parent>-fork`) and `check_forkable` (chains
anvil can't fork). A bin test keeps `RESERVED_FORK_NAMES` equal to the
`fork` subcommands and aliases.

**Logs.** `paths::serve_log()` (`$XDG_STATE_HOME/chainz/serve.log`) and
`paths::fork_log(name)` (`…/forks/<name>.log`); `doctor` lists
them.

**Background launch** (bin `sys`). `launch_detached(log, LogMode)`
re-executes the full command line minus `--detach` in a new session, with
`CHAINZ_DETACHED=1` set, stdin empty, stderr the log, and stdout a pipe.
The child prints what the user should see, then calls `detached_ready()`
(stdout onto the log), which closes the pipe; the parent's
`Detached::wait_ready(timeout)` returns what it printed. `serve --detach`
is ready once `serve.json` is out; `fork --detach` after its summary.

## Gateway and proxies (`chainz_serve`)

```rust
pub fn run(paths: StorePaths, runtime: Runtime, state_dir: Option<PathBuf>,
           options: Options, on_event: impl FnMut(Event<'_>) -> Result<()>) -> Result<()>;
pub struct Options { pub port: u16, pub log: Option<PathBuf>, pub ticks: bool }
pub enum Event<'a> { Started(&'a Gateway), Notice(&'a str), Tick(&'a Gateway), Stopping { drain: Duration } }
pub const SHUTDOWN_BUDGET: Duration;              // drain + slack + log writer wait
pub struct AlreadyRunning(pub Option<ServeInfo>); // the one "already running" error
impl Gateway {
    pub async fn start(config: GatewayConfig) -> Result<Gateway>; // GatewayConfig::new(paths, runtime, state_dir)
    pub fn addr(&self) -> GatewayAddr; pub fn port(&self) -> u16;
    pub fn status(&self) -> Status;
    pub fn summary(&self) -> MetricsTotals;       // the 60 s window's totals
    pub fn warnings(&self) -> Vec<String>;        // start-up warnings
    pub fn notices(&self) -> broadcast::Receiver<String>;
    pub async fn reload(&self) -> Result<()>; pub async fn shutdown(self);
}
```

`run` resolves nothing itself (the bin passes the paths, and a state dir it
already warned about) and reports through `on_event`; the bin presents
everything. `serve stop`'s timeout derives from `SHUTDOWN_BUDGET`. The
gateway reloads the config on SIGHUP, a control `reload`, or a change of
`StorePaths::stamp()`, and refreshes capability tags when `health.json`
changes.

**Loopback auth.** For any local server that hands a child process a
tokenised `http://127.0.0.1:<port>/<token>…` URL (the gateway; the fork
proxy). The token is 32 random bytes as 64 lowercase hex digits, compared
in constant time. Both servers run the same checks, in order: a `Host` of
`127.0.0.1` or `localhost` (any case), with their port or no port, and
nothing else: not `[::1]` (the servers listen on IPv4 only), and not a
missing `Host` (403); `POST` (405); `application/json` (415); the token
(403); a body of at most 64 MiB (413). No CORS headers are ever sent.

**Failures.** Both servers answer an upstream failure the same way: 429
with `Retry-After` (the upstream's, else 2 s) and JSON-RPC code -32005; 504
for a timeout; another upstream 4xx/5xx passed through; else 502; always
with a JSON-RPC error body that never names the URL (`chainz serve: …` or
`chainz proxy: upstream: …`). Both drain in-flight requests for up to 5 s
on shutdown.

`chainz_serve::PinnedProxy` is a single-endpoint server built on these
rules, for a fork's `--fork-url`: every body goes to one `Target` through an
`HttpUpstream` under the given `RetryPolicy`, never anywhere else. It
accepts only `/<token>` (a trailing `/` is allowed).

```rust
impl PinnedProxy {
    pub async fn start(upstream: HttpUpstream, target: Target, policy: RetryPolicy)
        -> io::Result<PinnedProxy>;      // binds 127.0.0.1:0
    pub fn url(&self) -> String;         // http://127.0.0.1:<port>/<token>
    pub fn addr(&self) -> &GatewayAddr;  // port and token
    pub async fn shutdown(self);         // drains like the gateway; dropping stops at once
}
```

## Dashboard (`chainz_tui`)

```rust
pub fn run(chainz: Chainz, runtime: Runtime, store: HealthStore, options: Options) -> Result<i32>;
pub struct Options { pub reveal: bool, pub mode: Mode }
pub enum Mode { Live { shell: Option<ShellFn> }, Once { json: bool, warn: WarnFn } }
pub type ShellFn = Box<dyn FnMut(&str) -> io::Result<()>>; // `e`: a shell for that chain
pub type WarnFn = Box<dyn FnMut(&str)>;                    // --once warnings (the bin's ui::warn)
```

The bin loads the health store (`net::health_store`, which warns about a
missing state dir or an unreadable history) and hands it in with `Chainz`
and `Runtime`. `Mode::Live` needs a terminal on stdin and stdout. Live
warnings and core notices from a reload go to the footer. The config
watcher compares `StorePaths::stamp()`.

## Labels (`chainz_core::labels`)

```rust
pub struct BuiltinToken { pub chain_id: u64, pub name: String, pub address: Address,
                          pub symbol: String, pub decimals: u8, pub source: String }
pub fn builtin_tokens() -> &'static [BuiltinToken];
pub fn builtin_token(chain_id: u64, name: &str) -> Option<&'static BuiltinToken>;
pub fn builtin_token_at(chain_id: u64, address: &Address) -> Option<&'static BuiltinToken>;
pub const TOKEN_ORDER: [&str; 6];            // weth, usdc, usdce, usdt, dai, wbtc (display order)
pub const NATIVE_DECIMALS: u8 = 18;
pub fn native_symbol(chain_id: u64) -> &'static str; // ETH, BNB, POL, xDAI…
pub const ALL_CHAINS: &str = "*";
pub fn scope_phrase(scope: &str) -> String;    // "on every chain" / "on eth": every message about a label scope

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
    pub fn reverse(&self, chain_id: u64, address: &Address) -> Option<&Label>; // only one that resolves back
    pub fn names(&self) -> impl Iterator<Item = &str>;
    pub fn labels(&self) -> impl Iterator<Item = Label> + '_; // every source, every chain
    pub fn labels_on(&self, chain_id: u64) -> Vec<Label>;
    pub fn shadowed(&self, label: &Label) -> Option<Label>;   // the label that wins on its chain
    pub fn shadowed_tokens(&self) -> impl Iterator<Item = (&'static BuiltinToken, Label)> + '_;
}
```

`Config::chain_display(id)` names a chain in output: configured name, else
the well-known short name, else `chain <id>`. `ChainRef::chain_id(&config)`
is the chain ID labels resolve on (a fork's own).

Precedence is: user label for the chain, then user `"*"`, then signer, then
built-in token. In config, `[addresses]` is `label -> chain name | "*" ->
address`. For example, `[addresses.myrouter]` with `base = "0x…"`. Label
names follow `names::check_label_name(name, signers)` and can't be a
context, deprecated or removed token (`names::context_tokens()` lists the
context ones).

The address book (`label …`, the stdin filter, `doctor --tokens`) is built on:

```rust
// chainz_core::tx::Tx, inside `transact`
pub enum SetAddress { Added, Unchanged, Replaced(Address) }
pub enum Overwrite { Refuse, Replace }
pub fn set_address(&mut self, name: &str, scope: &str, address: &Address, overwrite: Overwrite) -> Result<SetAddress>;
pub fn write_address(&mut self, write: &broadcast::AddressWrite) -> Result<SetAddress>;
pub fn remove_address(&mut self, name: &str, scope: Option<&str>) -> Result<Vec<(String, String)>>;

// chainz_core::addrbook
pub fn find(config: &Config, address: &Address) -> Vec<Label>; // `label 0x…`
pub struct Labeler { /* … */ }                                 // `… | chainz label`
impl Labeler {
    pub fn for_chain(config: &Config, chain_id: u64) -> Self;
    pub fn any_chain(config: &Config) -> Self;   // "*" labels, signers, names that agree on every chain
    pub fn name(&self, address: &Address) -> Option<&str>;
    pub fn filter(&self, input: impl BufRead, output: impl Write) -> io::Result<()>; // 0x… → name(0x…), padded topics too
}
```

`Labeler::name` skips a built-in token that a user label shadows on that
chain, so a name it returns always resolves back to the same address.

`label import` reads Foundry broadcast output with `chainz_core::broadcast`:

```rust
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
label on `config.chain_by_id(run.chain_id)`, named in kebab case. It never
uses a fork. It checks names with `names::check_label_name`, drops repeats
of the same (chain, name, address), and suffixes later different addresses
with `names::unique_name`. The command plans again inside `transact`, under
the lock (or `preview` for a dry run), and applies every planned write with
`tx.write_address(&write)`.

## Upstream (`chainz_rpc::upstream`)

```rust
pub struct RetryPolicy { pub max_attempts: u32, pub backoff: Duration, pub max_backoff: Duration,
                         pub max_retry_after: Duration }
impl RetryPolicy {
    pub fn none() -> Self;                     // one attempt
    pub fn same_endpoint_transient() -> Self;  // the fork proxy: 3 attempts, 0.1–1 s, Retry-After ≤ 2 s
    pub fn patient() -> Self;                  // token metadata: 5 attempts, 0.5–4 s, Retry-After ≤ 10 s
    pub fn retries(&self, error: &RpcError) -> bool;        // max_attempts > 1 and transient or rate-limited
    pub async fn run<T, F, Fut>(&self, call: F) -> Result<T, RpcError>; // backoff + Retry-After
}
pub struct HttpUpstream; impl HttpUpstream {
    pub fn new(client: RpcClient) -> Self;
    pub async fn send(&self, target: &Target, body: Bytes, policy: &RetryPolicy)
        -> Result<Bytes, RpcError>;            // the 2xx body
}
pub fn rpc_error_code(body: &[u8]) -> Option<i64>;
```

`RpcError::is_transient()` (timeout, connect, a body cut off mid-read, HTTP
502–504) and `RpcError::is_rate_limited()` (HTTP 429, JSON-RPC
`error::LIMIT_EXCEEDED` = -32005) are the only retry predicates: the gateway
fails over on exactly these, and a policy with `max_attempts > 1` retries
them (`max_attempts: 1` is the one "don't retry" switch). A body cut off
mid-read is `RpcError::BodyRead` (class `invalid`). Such a policy also
retries a 2xx single response carrying -32005, and returns it as it is when
the retries run out. `error::SERVER_ERROR` names -32000.

`HttpUpstream` retries on the same endpoint only. `RetryPolicy` is the retry
loop for callers without a rate-limit bucket (the pinned fork proxy,
`doctor`). The gateway sends with `RetryPolicy::none()` and retries in
`Router::try_endpoint`, where its buckets and failover live. `Target::new(chain_id, name, url, &[(String,
String)])` holds the expanded URL and headers, and its `Debug` redacts them.
`Retry-After` is clamped to `chainz_rpc::client::MAX_RETRY_AFTER` (60 s).
`RpcClient` never follows a redirect: a 3xx is `RpcError::Http(status)`, so
a target's headers never reach another host.

## Endpoint health (`chainz_rpc::health`)

```rust
pub const ENDPOINT_LAG: u64 = 20;  // lagging its peers (ranking, doctor, dashboard)
pub const CHAIN_LAG: u64 = 3;      // a chain's head is this far behind or more (dashboard)
pub const FAILOVER_LAG: u64 = 2;   // a failover endpoint may take a read (gateway)
pub const P50_WINDOW: usize = 16;  // successful samples behind p50
pub fn p50<'a>(samples: impl IntoIterator<Item = &'a HealthSample>) -> Option<Duration>; // last P50_WINDOW successes
pub fn median(values: impl IntoIterator<Item = u32>) -> Option<u32>; // the lower median for even counts
pub trait Observation { fn head(&self) -> Option<u64>; fn error(&self) -> Option<ErrorClass>;
                        fn latency(&self) -> Duration; } // ProbeResult, HealthSample, EndpointProbe
pub enum Health { Healthy, Lagging { behind: u64 }, Failing(ErrorClass) }
pub fn classify<O: Observation>(observations: &[O]) -> Vec<Health>;
pub struct Ranking { pub health: Vec<Health>, pub order: Vec<usize> }
impl Ranking { pub fn new(obs) -> Self; pub fn with_p50(obs, p50: &[Option<Duration>]) -> Self;
               pub fn best(&self) -> Option<usize>; pub fn best_healthy(&self) -> Option<usize>; }
pub struct EndpointProbe { pub chain_id: u64, pub endpoint: String, pub result: ProbeResult,
                           pub capabilities: Option<Capabilities> }
pub fn record(store: &mut HealthStore, probes) -> io::Result<()>; // samples + capabilities, saved
```

`p50` and `median` are the one median rule: `doctor CHAIN` ranks with
`health::p50(&store.samples(..))`, and the dashboard uses both.
`HealthSample::from_probe` builds the one sample a probe gives.
`HealthStore::load(dir)` returns an error for an unreadable store; a corrupt
`health.json` is moved aside to `health.json.corrupt` (never overwritten) and
reported. Callers warn and go on with `HealthStore::new(dir)` (empty, still
saves) or `HealthStore::empty()` (no state dir). `HealthStore::path(dir)`
names the file. `ErrorClass::phrase()` is the class in a sentence
("connection failed", "timed out").

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
  are replaced in place, new keys are appended. `tx.config()` is the typed
  view of the edit so far. Hand edits (`edit [CHAIN]`) go through
  `Chainz::config_text` + `replace_text(expected, new)` and
  `Chainz::chain_snippet` + `replace_chain_snippet`, which refuse if the file
  changed meanwhile.
  `chainz.preview(|tx| …)` runs the same closure on the files as they are
  and validates the result but writes nothing (dry runs, confirmations).
  `Chainz::peek()` (`LoadMode::Peek`) is the side-effect-free load for shell
  completion: no v1 migration, no permission changes, no validation.
  `StorePaths::stamp() -> StoreStamp` stamps every file a config is read
  from; the gateway and the dashboard compare stamps to see a change.
  `migrate::status(&paths)` says whether a v1 file is waiting; `migrate::migrate_locked(&WriteLock)` takes the held lock
  as proof. A migration that runs inside `transact` is reported like one on
  first load.
- Endpoints: `tx.add_endpoint_url(chain, name, &endpoint, EndpointRole)` is
  the one way to add an endpoint from a user's URL (`update --add-rpc`,
  `doctor CHAIN --discover`, `add`, `update --rpc URL`): `Chain::new_endpoint`
  checks the URL (`Endpoint::check_url`: http(s) or ws(s), a host) and names
  it, refuses a URL the chain already has (`Chain::endpoint_at`,
  `Endpoint::same_url(a, b, &vars)`: a trailing `/` ignored, and every
  `${NAME}` that `secrets.toml`'s `[vars]` holds expanded on both sides, so a
  pasted URL matches the stored one whose key moved into a variable;
  `ChainEdit::rpc_choice` and `add`'s own dedupe use the same rule), and
  literal secrets move into variables.
- Chain edits: `tx.edit_chain(name, &ChainEdit) -> Result<EditReport>` applies
  one patch (`update` flags or its menu): signer, added endpoints
  (`add_rpcs`, from `ChainEdit::parse_add_rpc`; `ChainEdit::added_endpoints`
  lists them, so the caller probes exactly what the write adds), removed
  endpoints (`remove_rpcs`, after the activation; the active one can't go), `rpc` (an endpoint name,
  or a URL: an existing one activates, a new one is added with its secrets
  extracted and activated; `ChainEdit::rpc_choice` says which, by
  `Chain::rpc_choice(name, rpc)` (in `model`, shared with routes):
  `RpcChoice::{Existing(name), New(name, endpoint)}`, so the caller probes
  exactly that and the menu shows it), headers and `clear_headers` on that endpoint
  (else the one added endpoint, else the active one), failover, explorer (`Explorer::ETHERSCAN` creates the
  built-in `Explorer::etherscan()`:
  `api = "https://api.etherscan.io/v2/api?chainid=${CHAIN_ID}"`,
  `key = "${ETHERSCAN_API_KEY}"`, the URL v1 migration writes), `api_key`
  (trimmed, stored in the explorer's variable, `ETHERSCAN_API_KEY` or
  `<EXPLORER>_API_KEY`), aliases, and the rename last. Each rule shares its
  check and message with validation (`Config::check_signer`,
  `Chain::check_failover`, `names::check_alias`). `EditReport` lists the
  changes (`Chain::changes(old, new)`, the one way a chain change is
  described, `edit CHAIN` included: `active endpoint: a → b`, side effects
  such as `failover: b, c → c`, and a chain ID change; then the explorer
  key line, `replaced the value of ${VAR}` when it held another, and the
  rename), the added endpoints, the created explorer, the key variable and
  the moved secrets, and `warnings` (an `etherscan` explorer on a chain
  `Explorer::etherscan_covers` rejects: `Explorer::etherscan_gap(id)`);
  `EditReport::is_empty` is a no-op edit, which `update` reports as "No changes". Checking a new URL's chain ID is the caller's (network)
  job. `tx.insert_chain(name, &chain, Attach)` checks name, ID and signer and,
  with `Attach::Defaults`, attaches the only signer and
  `[explorers.etherscan]` if configured and it covers the chain ID
  (`Explorer::etherscan_covers`, over the built-in `ETHERSCAN_CHAIN_IDS`)
  (`Attach::ExplorerOnly` for `--no-signer`, `Attach::Nothing` keeps the
  chain as given; `Attached` says what was added, and `explorer_skipped`
  why etherscan was not). `tx.attach_signer_where_missing(signer)` gives a first
  signer to every chain without one and returns them.
- Notices: core never prints. A load or write that has something to say (a
  v1 migration, a file whose loose permissions could not be tightened)
  queues a `store::Notice` on the `Chainz`; take them with `take_notices()`
  or route them as they happen with `set_notice_sink(fn(&Notice))`. The bin
  prints each `Display` line as `chainz: …`; serve adds them to the
  snapshot warnings; the dashboard shows them in its footer.
- Signers: `signer::provision(&dyn KeyBackend, &dyn Prompter, name, key, backend)`
  stores a key and returns a `Provision` (`Provision::reference(Signer)` for
  keystore and 1Password references, `Backend::is_reference`).
  `Chainz::install_signer` writes the record and key material in one
  transaction, abandons the provision (deletes the keyring entry it created)
  if that fails, and deletes a replaced signer's old keyring entry.
  `Chainz::remove_signer` is the reverse (`tx.remove_signer(name,
  Users::{Keep, Unassign})`; `Unassign` is `signer rm --unassign`; it
  returns the chains left without a signer). `signer::keystore_signer_address`
  is the keystore rule (its `address` field, else `--address`; a missing
  file or no address at all is an error). `tx.remove_chain(name)` returns
  `RemovedChain { endpoints, labels, was_default }`, which `rm`
  previews (`Chainz::preview`) to confirm and then reports. `KeyBackend` is external storage only;
  `Prompter` (hidden prompts) is implemented by the binary
  (`prompt::Terminal`, or the init flow's `AskPrompter`).
  `signer::safe_default(keys)` is the default backend (keyring, else
  encrypted). `CHAINZ_DISABLE_KEYRING` makes the keyring unavailable.
- Resolver: `resolve::select(config, Some(&runtime), explicit, chainz.env()) -> Result<Option<(ChainRef, Source)>>`.
  It checks the explicit argument, then `$CHAINZ_CHAIN`, then the default;
  `resolve::NO_CHAINS` is the one "no chains configured" message.
  Management commands use `resolve::configured(config, query, Policy) ->
  Result<(&str, &Chain)>` (no forks): `Policy::Prefix` (name, alias, ID or
  unambiguous prefix), `Policy::Exact` (name or ID: `rm`) or
  `Policy::ExactOrAlias` (`label rm --chain`). `resolve::exact` (ID, name or
  alias, any case) is the gateway's. Every policy but `Exact` then tries
  `resolve::implicit_alias(config, query)`: a `names::LONG_NAMES` entry
  (`arbitrum` → 42161) names the configured chain with that ID; fork names
  may not take one.
- Context: `context::build(&chainz, &runtime, &ChainRef, &ExecOptions) -> Result<Context>`
  (with `expose_key`, it refuses a fork or a chain without a signer), then
  `Context::unlock_key(&chainz, keys, prompter)` for `--expose-key`,
  `Context::exports() -> Exports { set, unset, warnings }` computed once, and
  `Exports::apply(&mut Command)`, which removes every contract variable and
  sets this context's (it refuses an `--expose-key` context whose key was
  never unlocked). Each `EnvVar` carries its own secrecy (`display()`
  redacts). `Context::explain()` is the safe view `--explain` prints.
  `Expander::new(&chainz, &runtime, &ctx, direct).expand_args(&argv)`.
  `contract::CONTRACT_VARS` lists every name exec owns, each a const in
  `contract` (a leaf); tests keep `exports()` and `names`' context tokens
  tied to their tables.
- Environment seam: `contract::Env` is where core reads the environment
  (`Env::process()`); `Chainz::env()` gives it to `resolve::select`, and
  template resolution and the context read it.
- Variables: `${NAME}` resolves to the built-ins (`CHAIN_ID`, `CHAIN`), then
  the environment, then `secrets.toml [vars]`.
- Display policy: `redact::url(value, reveal)` and `redact::header(value,
  reveal)` are the one policy for every view of a URL or header value
  (`exec --explain`, listings, the dashboard, `Debug`). A URL shows its
  `${VAR}` templates, hosts and ordinary paths; it masks user info, the
  fragment, known Infura/Alchemy keys, credential-shaped host labels, and
  every path segment or query value that is not a known-safe form (empty,
  a template, a number, a version such as `v2`, or a short lowercase word),
  as `redact::REDACTED`. An unparseable URL is masked whole. A header value
  shows only templates and an allow-list of auth schemes and media types;
  every other word is masked. `redact::summarize` is the terse host label
  of the compact `ls`. `extract` owns the credential shapes it moves into
  variables; `extract::manual_credential(url)` is the detector for what
  only a person can move (migration notes and `doctor`). The bin's
  `view::EndpointView` applies `url`/`header` once for `ls CHAIN` and `ls
  --json`.
- Shared helpers, so crates don't keep private copies:
  `chainz_core::fmt::{group, uptime, short_address}` (thousands separators,
  `42s`/`12m05s`/`2h03m`/`3d04h`, `short_address(&Address)` → `0x1234…abcd`);
  `Address::from_bytes` / `as_bytes` (the field is private). Model types are
  imported from `chainz_core::model` only (no root re-exports; the root
  re-exports `Address`, `Chainz`, `LoadMode`, `StorePaths`, `StoreStamp`
  and `Tx`). `chainz_rpc` re-exports its common items at the root (import
  them from there): `RpcClient`, `BlockHeader`, `BlockId`, `RpcError`,
  `ErrorClass`, `Target`, `HttpUpstream`, `RetryPolicy`, `Health`,
  `Ranking`, `EndpointProbe`, `classify`, `HealthSample`, `HealthStore`,
  `ProbeResult`, `probe`, `probe_all`, `Capabilities`, `probe_capabilities`.
  `chainz_rpc::hex::{encode, decode, parse_quantity, parse_quantity_saturating}`;
  `chainz_rpc::abi` (selectors and `function_name`, `MULTICALL3`,
  `call_with_address`, `word`/`uint`, `encode_aggregate3`/`decode_aggregate3`,
  `decode_string`/`decode_u8`, and exact `units::{check, scale, format_exact}`);
  `RpcClient::get_block(target, BlockId) -> Option<BlockHeader { number,
  timestamp, base_fee_per_gas }>`; `chainz_rpc::chainlist` (the bin fetches
  through `net::chainlist`, which warns once about a cache it couldn't
  write).

## Bin crate: adding a command handler

1. The clap surface lives in `crates/chainz/src/cli.rs`: one `…Args`
   struct per command, documented positionals, and conversions to core
   options (`From<&ContextArgs> for ExecOptions`, `From<BackendArg> for
   Backend`) done there, or in the handler when they need I/O. Declare
   value completion on the argument
   (`#[arg(add = ArgValueCompleter::new(complete::signers))]`), never by
   command path. Removed commands and flags get no hidden aliases.
2. Put the handler in a module, for example `commands/edit.rs` with
   `pub fn run(args: &EditArgs) -> anyhow::Result<i32>` (commands with
   subcommands take their `…Command` enum by value), and route it in
   `commands::dispatch` (that module only dispatches). Shared helpers live
   outside `commands/`: `load` (open the store, notices to stderr),
   `select` (chain picker, `$CHAINZ_CHAIN`/default), `args` (address and
   header parsing), `prompt` (`is_tty`, `confirm_or_yes(ask, yes, msg,
   Consent::{Destructive, Proposal})`, the `Ask` seam that interactive
   flows take so tests script them with `prompt::script::Script`;
   `Ask::choose` reports Esc as `None`, `Ask::select` as a cancellation),
   `net` (`block_on`, `join_indexed`,
   `to_target`, `health_store`, `chainlist`), `probe`, `view`
   (`EndpointView`, `marker`, `table`, `QUICK_START`).
   The return value is the exit code: 0, or the code of a failure the
   handler reported itself (`doctor`, `label 0x…`, `serve status`). An `Err`
   prints `error: …` and exits 1, or with the code of a `ui::ExitCode` in
   its chain (exec uses 127/126) or a clap usage error (2);
   `ui::cancelled()` exits 130 and a broken stdout pipe exits 0, all in
   `main` through `ui::report`/`ui::exit_code`.
3. Network work goes through `net::block_on(async { … })`, which creates a
   current-thread tokio runtime. A long-running server (`serve`, `top`) can
   build a multi-threaded runtime in its own crate. `exec` (and its subshell),
   `env` and `ls` must stay free of runtimes, sockets, and threads. The Linux strace
   test (`crates/chainz/tests/strace.rs`) enforces this.
4. Tests go in `crates/chainz/tests/cli.rs` (or a new file with
   `mod common;`) through `common::Sandbox::new()`. Its `.cmd()` /
   `.process()` clear the environment down to `PATH`, `TMPDIR`,
   `RUST_BACKTRACE` and `LLVM_PROFILE_FILE`, then set `HOME`, all four XDG
   dirs, `CHAINZ_DISABLE_KEYRING` and `NO_COLOR`, with stdin empty.
   `.seed(&[("eth", 1)])` writes chains on a dead local RPC with a plaintext
   `default` signer; `.run`/`.ok` run the binary. `common::MockRpc::start(handler)`
   is a threaded keep-alive JSON-RPC mock (batches too) that records every
   request. Output that users or scripts read is pinned by golden files
   (`tests/golden.rs`; `CHAINZ_BLESS=1` rewrites them). Never run the binary
   outside a sandbox, and never guess a free port: use port 0
   (`serve --port 0`, `anvil --port 0`) or let `fork` pick, then read the
   port back. Run `cargo test` with `TMPDIR` pointed at a scratch directory.
5. Before committing, run `cargo fmt`,
   `cargo clippy --workspace --all-targets -- -D warnings`,
   `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`, and
   `cargo test --workspace`. The perf gate is `scripts/perf-gate.sh V1 V2`.
