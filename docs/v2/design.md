# Chainz v2 — Design Proposal

Nov 11, 2026 · @Mark Toda

## Summary

Chainz v2 turns a chain-config manager into a **local multichain workbench**. It keeps the one thing v1 does perfectly, `chainz exec <chain> -- <tool>`, and builds around it a single source of truth for chains, endpoints, signers and addresses. Three new surfaces read from that truth: a local RPC gateway, a live dashboard, and disposable anvil forks.

The design is written as if from scratch. Breaking changes are allowed; the v1 config migrates automatically on first run.

**Goals**

- `exec` stays the centre and gets faster: under 5 ms of chainz overhead, and chainz replaces itself with the command rather than waiting on it.
- One small, legible data model: chains keyed by short names (`eth`, `base`, `arb`), endpoints, signers, labels.
- A hand-editable TOML config, with secrets either referenced or redacted.
- Name addresses once (`@usdc`, `@myrouter`) and use them everywhere: in `exec` arguments, and as reverse labels in every output.
- `chainz serve`: an opt-in local gateway with failover, rate-limit smoothing, metrics and a request inspector.
- `chainz top`: a live terminal UI showing chain heads, RPC health, wallet balances and gateway traffic.
- `chainz fork <chain>`: an anvil fork in one command, with a funded wallet, usable as a chain name.

**Non-goals**

- Running one command across many chains (`exec all`). Bash loops already cover it, and the design must not make them harder.
- Response caching in the gateway.
- Syncing config between machines. Each machine keeps its own keys and config.
- Reimplementing `cast`. Chainz supplies context, not a second Ethereum CLI.
- A preloaded registry of protocol deployments (Permit2, Universal Router). The address book ships major tokens only.

## First principles: what to keep, what to change

V1 is a careful codebase: about 8,000 lines with transactional config writes, cross-process locking, lazy key resolution and redaction everywhere. A rewrite keeps those guarantees. What it changes is the model and the surface, which grew one feature at a time.

**Keep (carried forward as invariants)**

- Keys are touched only when a command asks for them. `@wallet` never unlocks a key backend.
- Credentials never appear in ordinary output, `Debug` impls or error chains.
- Config writes are validated, atomic and serialized across processes.
- Per-endpoint headers are scoped to their URL.
- Chain lookup by ID, name, alias or unambiguous prefix.

**Change**

| v1 | Problem | v2 |
| --- | --- | --- |
| Chains are a JSON array; each has `name`, `selected_rpc`, `rpc_urls` | Names copied from chainlist ("Ethereum Mainnet") are long. The selected endpoint is a URL string repeated in two places. | TOML table keyed by short name: `[chains.eth]`. Endpoints are named, and the selected one is a name. |
| Exec spawns the child and waits | Signals and exit codes are relayed second-hand, and a parent process lingers | `execvp`: chainz becomes the command |
| Every command takes an exclusive config lock | Parallel `exec` calls in many terminals serialize on the lock | Reads take no lock: the atomic rename is enough. Writes lock. |
| `@token` expansion is a string replace over a HashMap | Order is undefined. `@key` matches inside `@keystore`, and adding user tokens would collide. | One tokenizer with a fixed grammar (see Exec contract) |
| Missing verifier settings are exported as `UNDEFINED` | Tools treat the literal as a real value | Unset variables are not exported |
| `${VAR}` is expanded from config, then again from the environment | A value can be expanded twice | One pass, no re-scan: built-ins, then environment, then `secrets.toml` |
| alloy pulled in for `eth_chainId` and address derivation | 14 MB binary, about 3 minutes for a release build | A small JSON-RPC client over reqwest, plus `k256` and `sha3` |
| Doctor, the wizards and listing each probe RPCs their own way | No shared notion of endpoint health | One `rpc` module, used by doctor, top, serve and fork |
| Network-facing features bolted onto the CLI | No place for long-running processes | A runtime directory for live processes (gateway, forks) and their discovery |

## Mental model

V2 has five nouns, and every command is a verb on one of them. A **context** is what `exec` hands a tool: one chain, one endpoint, optionally one signer, plus the labels for that chain.

| Concept | What it is | Lives in | Example |
| --- | --- | --- | --- |
| Chain | An EVM network: ID, short name, aliases, explorer | config | `eth` (1), aliases `ethereum`, `mainnet` |
| Endpoint | A named RPC URL on one chain, with optional headers; exactly one is active | config | `eth.infura`, `eth.uni-gw` |
| Signer | A named key reference with a cached address (keyring, encrypted, 1Password, Foundry keystore, plaintext) | config, secret in its backend | `default`, `funder` |
| Label | A name for an address: global, per chain, or built in (tokens) | config plus built-in registry | `@usdc`, `@myrouter` |
| Runtime | Live local processes chainz started: the gateway and forks | `$XDG_RUNTIME_DIR/chainz/` | `serve` on :18545, `base-fork` on :8601 |

Forks are real chains while they run. `base-fork` resolves like any configured chain, but it lives in the runtime registry rather than in config, so it disappears when the fork stops.

**How a chain reference resolves**, first match wins:

1. Explicit argument: `chainz exec base -- …`.
2. `$CHAINZ_CHAIN`, set inside `chainz shell` and by `eval "$(chainz env base)"`.
3. `default` in the config, set with `chainz use`. Because this default is machine-wide and shared by every terminal, `exec` prints `chainz: base (default)` to stderr whenever the chain came from here.
4. The interactive picker, but only on a TTY. Otherwise it is an error.

Within step 1, a reference matches in this order: an exact chain ID, an exact name or alias (case-insensitive), a live fork's exact name, then an unambiguous prefix. A configured chain always beats a fork of the same name, and a fork whose supervisor is gone never resolves. Destructive commands (`rm`) accept only an exact name or ID.

**How an endpoint is chosen for `exec`:** if `chainz serve` is running, the gateway URL for that chain is used. Otherwise the chain's active endpoint is used. `--direct` skips the gateway, and `--rpc <name>` picks a specific endpoint.

## CLI surface

The frequent actions are top-level verbs; management lives under nouns. You type `exec` dozens of times a day and `chain add` a few times a year, so the first is short and the second is explicit. Every listing command takes `--json`, and every command that touches secrets takes `--reveal`, never anything else.

```
# hot path
chainz exec [CHAIN] [--signer S] [--rpc NAME] [--direct] [--expose-key] [--explain] -- CMD...   (alias: x)
chainz shell [CHAIN] [--signer S] [--rpc NAME] [--direct]     # subshell with the context exported
chainz env [CHAIN] [--signer S] [--rpc NAME] [--direct] [--shell zsh|bash|fish]   # eval "$(chainz env base)"
chainz use [CHAIN | --clear]         # set the default chain
chainz ls [--json] [--reveal]        # alias of `chain ls`

# live surfaces
chainz top [--once [--json]] [--reveal]
chainz serve [--port 18545] [--detach] [--log FILE]
chainz serve status [--json] | stop | log [-f] [--chain C] [--method M]
chainz fork CHAIN [--block N] [--port P] [--name NAME] [--chain-id N] [--fund [SPEC] | --no-fund] [--detach]
                                     # SPEC: usdc=10000,weth=5; `ls` and `stop` are reserved names
chainz fork ls [--json] | stop NAME|--all

# setup and editing
chainz init                          # TTY wizard: chains, a signer, an Etherscan key, a default
chainz update [CHAIN] [flags]        # same as `chain set`
chainz edit [CHAIN]                  # $EDITOR on config.toml (or one chain's table), validated on save

# management
chainz chain ls [--json] [--reveal]
chainz chain add [CHAIN_ID|NAME] [--name N] [--url URL]... [--signer S | --no-signer] [--no-check] [--refresh]   # with --url: an ID or built-in short name, offline
chainz chain show CHAIN [--json] [--reveal]
chainz chain set [CHAIN] [--signer S | --no-signer] [--rpc NAME|URL] [--header 'k: v']... [--clear-headers]
                 [--failover A,B | --no-failover] [--explorer E | --no-explorer] [--api-key-stdin]
                 [--name NEW] [--alias A]... [--rm-alias A]... [--no-check]   # no flags on a TTY: a menu
chainz chain rm CHAIN
chainz rpc ls [CHAIN] [--json] [--reveal]   # endpoints, with latest health and its age
chainz rpc add CHAIN URL [--name N] [--header 'k: v']... [--failover | --activate] [--no-check]
chainz rpc rm CHAIN NAME
chainz rpc bench [CHAIN] [--fix [-y]]       # rank and tag endpoints; --fix activates the best and proposes a failover list; exit 1 when a chain has no live endpoint
chainz rpc discover CHAIN [--refresh] [-y]  # pull public RPCs from chainlist, probe, offer to add
chainz signer add NAME [--stdin] [--backend keyring|encrypted|1password|keystore|plaintext]
                  [--path P] [--vault V --item I] [--address A] [--no-attach]
chainz signer ls [--json] | rm NAME [--unassign] [-y] | migrate [NAME|--all] [--to keyring|encrypted]
chainz addr add NAME ADDRESS [--chain C | --all-chains]
chainz addr ls [--chain C] [--builtin [--all]] [--json] | rm NAME [--chain C] | find 0x… [--json]
chainz addr import PATH... [--dry-run] [--prefix P] [--only A,B] [--force] [--json]
chainz label [--chain C]             # stdin filter: 0x… → name(0x…)
chainz var set NAME [VALUE | --stdin] | get NAME [--reveal] | ls [-v] [--reveal] [--json] | rm NAME
chainz doctor [--fix] [-v] [--tokens]     # one cause on many chains is one line; -v lists each
chainz config path [--all] | migrate [--dry-run]
chainz completions SHELL
```

**Renames from v1:** `key` becomes `signer` (it holds keystore and 1Password references, not only raw keys). `list` becomes `ls`, with `list` kept as an alias. `update` is `chain set`: one verb for every chain setting, flag-driven or, with no flags on a TTY, v1's menu (RPC / Signer / Explorer / Name / Aliases / Done), applied in one write. `--show-secrets` and `--show` both become `--reveal`. `init` stays as a TTY wizard; off a TTY it prints the quick start, as `chainz` with no arguments does on an empty config. The first signer attaches to every chain without one, and `chain add` attaches the only signer and the `etherscan` explorer when it exists.

**Interactive vs scripted:** pickers open where the design needs a choice (`exec`/`shell`/`env`/`use` with no chain, `chain add` with no chain, `chain set` with no chain, `addr add` with no chain context); off a TTY those fail with a precise usage message (exit 2 for a usage error). `--yes` means "accept without asking" everywhere. Every `rm` (`chain`, `rpc`, `addr`, `var`, `signer`) goes through one confirmation (`prompt::confirm_removal`): asked on a TTY (default no), `--yes` required off one. No command ever blocks on a hidden prompt in a pipeline.

## Config v2

The config is one hand-editable TOML file, kept in the order you wrote it. Secrets are referenced as `${VAR}` and held in a separate secrets file. Explorers are defined once and attached to chains by name, so one Etherscan v2 key can serve many chains. A chain uses an explorer only if it names one.

```toml
# ~/.config/chainz/config.toml
version = 2
default = "eth"

[explorers.etherscan]              # defined once, attached per chain
api = "https://api.etherscan.io/v2/api?chainid=${CHAIN_ID}"
key = "${ETHERSCAN_API_KEY}"

[chains.eth]
id = 1
aliases = ["ethereum", "mainnet"]
signer = "default"
explorer = "etherscan"
rpc = "infura"                     # the active endpoint, by name
failover = ["llama"]               # gateway may retry reads here; nothing else
[chains.eth.rpcs]
infura = "https://mainnet.infura.io/v3/${INFURA_KEY}"
uni-gw = { url = "https://gw.example.org/rpc/1", headers = { x-internal-service-secret = "${UNI_GW_SECRET}" } }
llama = "https://eth.llamarpc.com"
llama-ws = "wss://eth.llamarpc.com"   # usable with exec --direct; never routed by the gateway

[chains.base]
id = 8453
explorer = "etherscan"
rpc = "infura"
rpcs.infura = "https://base-mainnet.infura.io/v3/${INFURA_KEY}"

[signers.default]
backend = "keyring"
address = "0x0123…4567"

[signers.deployer]
backend = "keystore"               # Foundry keystore, unlocked by cast/forge itself
path = "~/.foundry/keystores/deployer"
address = "0xabcd…ef01"

[addresses.myrouter]               # per chain
base = "0x…"
eth = "0x…"
[addresses.my-safe]
"*" = "0x5afe…"                    # every chain: written only with --all-chains
```

**Secrets** go in `~/.config/chainz/secrets.toml` (mode `0600`): the values `chainz var set` writes, plus plaintext keys if you opt into that backend. One template syntax covers everything: `${NAME}` resolves first from the built-ins (`${CHAIN_ID}`, `${CHAIN}`), then from the process environment (so a one-off override works), then from `secrets.toml`. Built-ins come first on purpose: chainz exports `CHAIN_ID` into child shells, so letting the environment win would make a nested `chainz exec eth` inside `chainz shell base` build base's URLs. For the same reason, inside a chainz context (`CHAINZ_CHAIN` is set, as in `chainz shell` or after `eval "$(chainz env …)"`) the environment is skipped for the exec contract's own variables (`ETHERSCAN_API_KEY`, `VERIFIER_API_KEY`, `ETH_RPC_URL` and the rest of the table under Exec contract): there they hold the enclosing chain's values, and a nested `chainz exec eth` in `chainz shell celo` must not send celo's explorer key to Etherscan. It is a single pass with no re-scan. An unresolved `${NAME}` is an error, never passed through literally.

The main config holds no secrets **once `doctor` is clean**: `doctor` warns about any endpoint URL or explorer key that embeds a literal credential, and `doctor --fix` offers to move it into a variable.

**File layout**

| Path | Holds | Lifetime |
| --- | --- | --- |
| `$XDG_CONFIG_HOME/chainz/config.toml` | chains, endpoints, explorers, signer references, labels, settings | permanent, hand-editable |
| `$XDG_CONFIG_HOME/chainz/secrets.toml` | variable values, plaintext keys | permanent, `0600` |
| `$XDG_STATE_HOME/chainz/` | endpoint health history and capabilities, gateway and fork logs | rotated |
| `$XDG_RUNTIME_DIR/chainz/` (falls back to the state dir) | `serve.json`, `serve.lock`, `serve.sock`, `forks/*.{json,lock}`, `ports/*.lock` | cleared on reboot |
| `$XDG_CACHE_HOME/chainz/` | chainlist | rebuildable |

The token registry is compiled into the binary, so it needs no cache entry.

**Migration from v1** runs once, automatically, the first time v2 finds `config.json` and no `config.toml`:

1. **Race safety:** migration takes the config write lock, re-checks that `config.toml` is still absent, then writes. A bash loop that starts 17 `exec` calls at once migrates exactly once; the others wait on the lock, then read the TOML.
2. **Chain names:** long chainlist names map to short names through a built-in table keyed by chain ID (1 → `eth`, 10 → `op`, 42161 → `arb`, 130 → `uni`, …). Unknown IDs fall back to a slug of the name. The old name becomes an alias, so `chainz exec "OP Mainnet"` keeps working.
3. **Endpoints:** every one of the 149 is kept and named after its host (`infura`, `uniswap`, `llamarpc`), with `-2`, `-3` suffixes on collision. The 16 header endpoints keep their headers as inline tables; the 24 websocket URLs keep a `-ws` suffix. `selected_rpc` becomes `rpc = "<name>"`. `failover` starts empty, so nothing new is retried until you choose.
4. **Credentials:** literal keys in known URL shapes (Infura `/v3/<key>`, Alchemy `/v2/<key>`) are extracted into variables such as `${INFURA_KEY}`. Other inline credentials stay where they are and are listed in the migration summary and by `doctor`.
5. **Explorers:** the 8 chains with a literal verification key get their key moved to a variable. Chains that share an Etherscan-style URL and key are collapsed into one named explorer. The 9 chains with no verifier get none: migration never adds an explorer.
6. **Signers and variables:** `key_name` becomes `signer`. Key records keep their backend and cached address, so keyring entries need no re-entry. The 4 variables move to `secrets.toml`.
7. **Write order:** `secrets.toml` first, then `config.toml`, each with fsync and an atomic rename. Only then is `config.json` renamed to `config.v1.json.bak`. A crash at any point leaves either v1 or a complete v2. The run ends with a one-line summary, such as `migrated 17 chains, 149 endpoints, 3 signers, 4 variables; 2 inline credentials need attention`.

`chainz config migrate --dry-run` prints the resulting TOML without writing anything.

## Exec contract

`exec` resolves a context, rewrites `@tokens` in the arguments, exports the environment, then replaces itself with the command. That contract is versioned: tools such as `rl.ts` may depend on these exact variable names.

**Environment**, exported only when the value exists:

| Variable | Value | Notes |
| --- | --- | --- |
| `ETH_RPC_URL` | gateway URL for the chain, else the active endpoint expanded | read natively by cast and forge |
| `ETH_RPC_HEADERS` | `name: value` pairs | direct mode only; the gateway adds headers itself |
| `CHAIN_ID`, `CHAINZ_CHAIN` | `8453`, `base` | `CHAINZ_CHAIN` also drives prompt integrations |
| `ETHERSCAN_API_KEY` | the chain's explorer key | read natively by forge verify and cast |
| `VERIFIER_URL`, `VERIFIER_API_KEY` | explorer API URL and key | exported only for non-Etherscan explorers, so forge's built-in Etherscan v2 handling is left alone |
| `ETH_FROM`, `WALLET_ADDRESS` | the signer's cached address | `ETH_FROM` is read by cast only; forge scripts take `--sender @wallet`. No key unlock either way |
| `ETH_KEYSTORE` | keystore signer's file path | Foundry prompts for the password; key material never passes through chainz |
| `RAW_PRIVATE_KEY` | the private key | only with `--expose-key`, and never on a fork |
| `CHAIN_NAME` | same as `CHAINZ_CHAIN` | v1 name, kept for one release |

**Token grammar.** A token is `@` followed by a name made of `[a-z0-9_-]` (matched case-insensitively), with an optional `:chain` qualifier. The qualifier always means a chain, never anything else.

- **Where it starts:** the `@` must start the argument or follow a character outside the name alphabet, so `user@host` and `foo@bar.com` never expand.
- **Where it ends:** the end of the argument, or a character other than `/` or `.`. So `npm i @uniswap/v3-core` stays literal even if you have a label named `uniswap`.
- **Escape:** `@@` produces a literal `@`.
- **Unknown names:** an argument that is entirely an unknown token, such as `@udsc`, is an error with a suggestion (`did you mean @usdc?`). An unknown token inside a longer argument passes through unchanged.

| Token | Expands to |
| --- | --- |
| `@rpc`, `@chainid`, `@chain` | RPC URL, chain ID, short name |
| `@wallet` | the chain's signer address (cached; no unlock) |
| `@funder`, `@deployer` | any signer's address: signers are labels automatically |
| `@explorer`, `@explorer_key` | explorer API URL and key |
| `@usdc`, `@myrouter` | label resolved on this chain |
| `@usdc:base`, `@wallet:arb` | the same, on another chain |
| `@chainname`, `@verification_url`, `@verifier_api_key` | v1 names, kept as deprecated aliases for one release; each use prints a warning |

**Names.** Context tokens (`rpc`, `chainid`, `chain`, `wallet`, `explorer`, `explorer_key`) and signer names are reserved: a label can't use them. Built-in *token* labels (`usdc`, `weth`) can be shadowed by your own label. A label that exists but has no address on the current chain is an error naming the chain. `@key` is removed and fails with a pointer to `--expose-key`. `chainz exec --explain …` prints the context, the exported variable names and the rewritten argv (secrets redacted), then exits.

**Process model.** Exec calls `execvp`: same PID, signals go straight to the command, and its exit status is the shell's exit status. Chainz runs on Linux and macOS only. Before the exec, chainz holds no lock and no open sockets.

**Overhead budget.** The target is a median under 5 ms from start to `execvp` on a warm cache (v1 measures about 3.8 ms; process startup alone is about 1.3 ms, so the headroom is small). CI does not enforce an absolute number, which would flake. It enforces two things: the median must be at most 85% of v1's in the same CI job, and a structural test runs exec under `strace` and asserts no sockets, no threads, and no reads outside the config, secrets and runtime files.

`--expose-key` is outside the budget: reaching the keyring over D-Bus takes as long as it takes. Checking for a running gateway is one non-blocking `flock` attempt.

## RPC layer

One `rpc` module gives every feature the same client, the same idea of endpoint health, and the same ranking. Doctor, `rpc bench`, the dashboard, the gateway and forks are all thin users of it.

- **Client:** a JSON-RPC 2.0 client over a shared `reqwest` connection pool (HTTP/2, rustls), with per-endpoint headers, batch requests and typed helpers: `chain_id`, `check_chain_id`, `block_number`, `get_block`, `get_balance`, `call`. Errors are `Timeout`, `Connect`, `RateLimited { retry_after }`, `Http(status)`, `Rpc { code, message }`, `ChainMismatch`, `InvalidResponse`, `BodyRead` (a body cut off mid-read), `InvalidEndpoint` or `Unsupported`; `is_transient` and `is_rate_limited` are the only retry predicates. Error text never includes the URL or header values. The client never follows a redirect.
- **Probe:** a health probe is `eth_chainId` plus `eth_blockNumber` in one batch, under a 4-second deadline. It records latency, head height and the error class. An endpoint more than 20 blocks behind the best head for its chain is marked `lagging`, a state v1 couldn't detect.
- **Capabilities** (probed by `rpc bench`, cached in the state dir): archive (`eth_getBalance` at block 1), `debug_traceTransaction`, `trace_block`, the largest `eth_getLogs` range accepted, and websocket support. `rpc ls` and the dashboard's Endpoints tab show these as tags; the gateway and forks route by them.
- **Ranking:** healthy before lagging before failing; healthy and lagging endpoints by p50 latency over the recent window. Failing endpoints are not ranked: they come last in config order, and `rpc bench` shows them with no latency and what went wrong, prints "N of M answered" per chain, and exits 1 when some chain has no endpoint that answered. `rpc bench --fix` and `doctor --fix` set `rpc = <best>` through the normal transactional config write.
- **Health history:** probes append to a small ring buffer per endpoint in the state dir (`health.json`; a corrupt file is moved aside to `health.json.corrupt`, never overwritten). The dashboard reads it to draw sparklines and `rpc bench` ranks by its p50; the gateway reads only the capability tags from it.

## Gateway: `chainz serve`

`chainz serve` is an opt-in local JSON-RPC proxy: one authenticated loopback URL per chain, failover only across endpoints you have approved, smoothing of rate limits, and a record of every call. It runs only when you start it. While it runs, `exec` routes through it automatically, and `--direct` bypasses it. The default port is 18545, which keeps 8545 free for anvil.

**Routing**

- **URLs:** `http://127.0.0.1:18545/<token>/base` (name, alias or ID). `/<token>/base/llama` pins one endpoint, which is what `exec --rpc llama` uses while the gateway runs.
- **Auth, always on:** a random token is generated at each start and stored in `serve.json` (mode `0600`), so `exec` picks it up and other local users cannot. The server also rejects any `Host` other than `127.0.0.1` or `localhost`, accepts only `POST` with `application/json`, and sends no CORS headers, so a web page cannot reach it through DNS rebinding or a simple cross-origin request.
- **Failover pool, opt-in per chain:** the active endpoint is always tried first. Only the endpoints in the chain's `failover = ["llama", "publicnode"]` list are ever tried after it. A request meant for a private or header-authenticated endpoint therefore never leaks to an arbitrary public RPC. Migration leaves the list empty, and `chainz rpc bench` suggests one.
- **Consistent heads:** while serving, the gateway probes each pool every 5 s and keeps a per-chain head high-water mark: the best head over the last three probe rounds, so one failed probe doesn't lower it and one bogus head ages out. A failover target must be within 2 blocks of that mark, so `latest` does not visibly go backwards.
- **Capability-aware:** `debug_*` and `trace_*` calls, and reads pinned to blocks older than 128, go only to endpoints tagged with that capability, or to the active endpoint when tags are unknown.
- **Secrets stay upstream:** headers and `${VAR}` values are applied by the gateway. Child processes see only the tokenised loopback URL.
- **HTTP only:** `ws://` and `wss://` endpoints (24 of the 149 in the current config) remain usable with `exec --direct` but are not routed. Routing them is planned for 2.1 (see Decisions).

**Failover rules**, by request kind:

| Request | Retried on the next failover endpoint when | Why |
| --- | --- | --- |
| Reads (`eth_call`, `eth_getLogs`, `eth_getBalance`, …) | connect error, timeout, HTTP 502/503/504, RPC error `-32005` (limit exceeded), or a 429 whose wait would exceed 2 s | idempotent; this enumerated list is the whole set of retryable errors |
| `eth_sendRawTransaction`, `eth_sendTransaction` | never; sent to the active endpoint only | a private endpoint's transaction must not be broadcast elsewhere |
| Filters and subscriptions (`eth_newFilter`, `eth_getFilterChanges`) | never; sticky to the endpoint that created the filter | filter IDs are local to each node |
| JSON-RPC batches | the whole batch, under the strictest rule among its methods | keeps batch semantics simple |

**Rate-limit smoothing.** Each endpoint has an adaptive token bucket (additive increase, multiplicative decrease). On a 429 the gateway halves that endpoint's rate (once per second at most: a burst of 429s from concurrent requests is one signal) and waits out `Retry-After` on the **same** endpoint when the wait is 2 s or less. Staying put keeps read-your-writes intact for nonce reads and receipt polling. Only a longer wait moves a read to the failover pool. Every send, retries included, takes a slot from the bucket, so requests that waited out the same 429 come back spaced out. A request with no other endpoint to go to queues in the bucket for up to 10 s; past that it gets a 429 at once, with the bucket's wait (rounded up) as `Retry-After`. The client sees fewer 429s and no thundering-herd retries.

**Metrics** per chain, endpoint and method over 1-minute and 15-minute windows: requests, errors by class, p50 and p95 latency, 429s, failovers and bytes. Each window also carries totals, overall and per chain, with true percentiles over its requests; `serve status` shows those.

**Inspector.** Every request becomes a record: time, chain, method, a short params summary (with addresses shown as labels, such as `eth_call → @usdc.balanceOf`), upstream endpoint, attempts, latency, status and response size. The last 10,000 records are kept in memory. `--log FILE` appends JSONL. `chainz serve log -f --chain base --method eth_call` tails it in the terminal.

**Control API** on a unix socket, `serve.sock` in the runtime dir (mode `0600`), never on the TCP port: `status` (version, uptime, chains), `metrics` (JSON snapshot), `events` (a stream of request records) and `reload`. This API is the contract `serve status`, `serve log` and the dashboard are built against; its types live in `chainz_rpc::control`; client tests run against the real control handler over canned state (`chainz-serve`'s `testing` feature). Clients give up after 5 s, so a wedged gateway can't hang them.

**Lifecycle.** It runs in the foreground with a live one-line summary, or with `--detach` as a background process.

- **Liveness:** the gateway holds an exclusive `flock` on `serve.lock` for its whole life. `exec` probes it with one shared, non-blocking `flock` that it releases at once. If the probe gets the lock, no gateway is running and `exec` goes direct. A gateway starting at that moment waits out the probe instead of failing. This replaces a PID check, which PID reuse can fool.
- **Config changes:** the gateway watches `config.toml` and `secrets.toml` and reloads on change (also on SIGHUP or a control `reload`), so `chainz chain set --rpc` takes effect immediately.
- **Shutdown:** `serve stop` sends SIGTERM, and in-flight requests drain for up to 5 seconds (the same drain as a fork's proxy).
- **Limits and errors:** both proxies take request bodies up to 64 MiB and answer an upstream failure the same way: 429 with `Retry-After` and JSON-RPC code -32005 for a rate limit, 504 for a timeout, the upstream's own 4xx or 5xx, else 502, with an error body that never names the URL.

## Dashboard: `chainz top`

`chainz top` is a full-screen ratatui view of every chain at once, with one tab per data source. It reads chains through the shared RPC layer, or through the gateway when one is running, so the dashboard's own polling shows up in the inspector like any other client.

```
 chainz top ─ [1] Chains  [2] Endpoints  [3] Balances  [4] Traffic          serve: on :18545
 CHAIN     ID      HEAD         AGE   BLOCK   BASE FEE   RPC          P50   HEALTH
▸eth       1       23,481,220   4s    12.0s   0.41 gwei  infura       48ms  ▁▂▁▁▃▁▁ ok
 base      8453    38,112,904   1s    2.0s    0.002      infura       61ms  ▁▁▁▂▁▁▁ ok
 uni       130     31,770,013   0s    1.0s    0.000      infura       55ms  ▁▁▅▇▂▁▁ lag 3
 zora      7777777 33,004,118   9s    2.0s    0.001      uniswap      240ms ▁▁▁▁▁▁▁ slow
 …
 enter: endpoints  b: balances  r: refresh  s: set rpc  e: exec shell  /: filter  q: quit
```

| Tab | Shows | Source and refresh |
| --- | --- | --- |
| Chains | head, block age, average block time, base fee, active RPC, p50 latency, sparkline, status | one request per chain (`eth_getBlockByNumber(latest)`), paced by the chain's block time, 3–12 s |
| Endpoints | every endpoint of the selected chain: latency, lag, error class, capability tags; `s` makes it the active one | probes from the RPC layer, every 15 s |
| Balances | each signer's native balance and major-token balances per chain | one Multicall3 `aggregate3` `eth_call` per chain for every signer and token (a JSON-RPC batch where Multicall3 is missing), every 30 s and on `r` |
| Traffic | when `serve` runs: requests per second, a live request feed with labels, top methods, errors and 429s per endpoint | the gateway's `/_chainz/events` and `/_chainz/metrics` on its control socket |

**Keys:** `1`–`4` switch tabs, `j`/`k` move, `h`/`l` switch chain or signer, `/` filters chains, `enter` drills into a chain, `s` sets the active RPC (a normal transactional config write), `e` drops into `chainz shell` for the selected chain and returns on exit, `y` copies the selected address or URL (redacted unless `--reveal`), `q` quits.

**Active endpoint.** When a chain's head is read through the gateway, the RPC column, `y`, the Endpoints tab's marker and the JSON snapshot name the endpoint the gateway serves from; otherwise the configured active one. The Endpoints tab adds `(config: X)` when the two differ. Gateway and feed failures and config-reload notices show in the footer (on stderr with `--once`).

The dashboard never unlocks a signer: balances use cached addresses only. It sends at most one request per chain per tick, plus bounded probes, so leaving it open all day stays well inside public RPC limits. Without a TTY, `chainz top --once --json` prints a single snapshot for scripts. `--once` as text leaves chains that are down out of the Balances tables and names them in one line (`balances skipped on down chains: …`); the JSON keeps every row.

**Pacing.** Every head poll is a billed request on a metered API key, and several chains usually share one key. So the poll interval has a 3 s floor even on chains with faster blocks, and a 12 s ceiling. After 5 minutes without a keypress every chain drops to the ceiling until the next key. Chains on one provider start 250 ms apart, and a failing endpoint is backed off.

## Address book and tokens

The address book holds a name for an address, scoped globally or per chain. It works in both directions: names become addresses in `exec` arguments, and addresses become names in every chainz output. It is filled from two sources: your own labels in config, and a built-in token registry.

**Data model.** A label is `name → {chain → address}`, with an optional `"*"` entry for "every chain". Names follow the token grammar: `[a-z0-9_-]`, case-insensitive.

- **Lookup on chain C:** your entry for C, then your `*` entry, then the built-in entry for C.
- **Shadowing:** your label can shadow a built-in token label, and `doctor` lists it as a note. Context tokens and signer names are reserved.
- **Scope when adding:** `addr add` is per chain by default, using the current chain context. Writing a `*` entry requires `--all-chains`, because routers, Safes and many deployments differ per chain.

**Built-in token registry.** A `tokens.toml` compiled into the binary, keyed by chain ID and covering every chain chainz knows a short name for. A token appears on a chain only where it really exists: `weth` (or the chain's wrapped native token, also `wnative`), `usdc` (native Circle USDC), `usdce` (bridged, where different), `usdt`, `dai`, `wbtc`.

- **Fields:** each entry carries `address`, `symbol`, `decimals` and `source`, a URL to the issuer's or chain's official address list.
- **Checking:** `doctor --tokens` verifies every entry that exists on chain: code present, and `symbol()` and `decimals()` match. Registry mistakes are caught before they cost you.

**Commands**

```
chainz addr add myrouter 0x… --chain base     # per chain (default: current chain context)
chainz addr add my-safe 0x5afe… --all-chains  # every chain, explicitly
chainz addr ls [--chain base] [--builtin [--all]]   # built-in tokens on your chains; --all: every chain
chainz addr find 0x833589fC…                  # → usdc on base (built-in)
chainz addr rm myrouter --chain base

# reverse labels as a pipe filter
chainz exec base -- cast logs … | chainz label --chain base
```

**Reverse labels** come from one index keyed by `(chain_id, address)`, built at load in well under 1 ms. The gateway inspector, the dashboard, `chain show` and `chainz label` all use it. `chainz label` rewrites every 20-byte hex address in stdin to `usdc(0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913)`. It keeps the full checksummed address, so the output stays copy-pasteable.

## Forks: `chainz fork`

`chainz fork base` starts anvil on a fork of base, funds your wallet, and registers `base-fork` as a chain name until the fork stops. Nothing about a fork is written to config.

```
$ chainz fork base --fund usdc=10000,weth=5
base-fork  http://127.0.0.1:8601  block 38,112,904  chain 8453
funded 0x0123…4567 (default): 1000 ETH, 10000 USDC, 5 WETH
$ chainz exec base-fork -- cast send @usdc 'transfer(address,uint256)' @myrouter 1e6 --unlocked
```

**Supervisor.** The `chainz fork` process supervises anvil as a child. Anvil gets `--fork-url http://127.0.0.1:<port>/<token>`, which points at a single-chain proxy inside the supervisor. The proxy always sits in front of anvil: it forwards to the pinned endpoint directly, or, when a gateway is running, through the gateway pinned to that same endpoint so fork traffic shows up in the inspector and metrics. The `<token>` is a per-fork token, never the gateway's token or an endpoint secret, so anvil's argv is safe to show in `ps`.

- **Upstream:** the proxy pins one endpoint for the fork's whole life, because anvil's lazily fetched state must come from one consistent source. With `--block N`, that endpoint must be archive-capable: it is chosen from capability tags, and the fork refuses to start if none qualifies. The proxy retries transient errors on that same endpoint but never fails over, since mixing nodes mid-fork corrupts state.
- **Unsupported chains:** chains anvil can't fork (zkSync Era, 324) are rejected up front with a clear message.
- **Orphans:** anvil runs in its own process group with `PR_SET_PDEATHSIG` on Linux. If the supervisor is SIGKILLed, anvil dies too. On macOS the next `chainz fork` or `fork ls` reaps any anvil whose supervisor is gone.
- **Stopping:** Ctrl-C, `chainz fork stop base-fork` or `--all` stops anvil and removes the registry entry.

**Registry.** In the runtime dir, the supervisor holds an exclusive `flock` on `forks/<name>.lock` for the fork's whole life (liveness is the lock, as for the gateway), and `forks/<name>.json` (format 1) holds `{format, name, pid, anvil_pid, port, parent, chain_id, fork_block, started_at, ready}`.

- **Lookup:** forks resolve **by name only**. A fork keeps its parent's chain ID, so `chainz exec 8453` always means real base and can never hit the fork by accident.
- **Names:** `--name` may not collide with a configured chain's name or alias, or with the reserved words `ls` and `stop`.
- **Ports and logs:** ports are allocated from 8601 upward. The supervisor holds `ports/<port>.lock` for anvil's port from before anvil starts until the fork stops, so parallel forks never pick the same port; an automatically chosen port that anything else holds moves on to the next. `--port P` is used exactly or fails. `--detach` runs the supervisor in the background, with logs in the state dir.
- **Replay protection:** because a fork shares the real chain ID, a transaction signed for the fork is valid on the real chain. On forks, chainz refuses `--expose-key` and does not export `ETH_KEYSTORE`, so the only signing path is anvil's `--unlocked` impersonation. Pass `--chain-id N` to give the fork its own ID if you need real signatures.

**Funding** (`--fund`, on by default for the chain's signer):

- Native balance: `anvil_setBalance`, 1000 units of the native token by default.
- Tokens: `anvil_dealERC20` for each `name=amount` pair, with names resolved through the address book and amounts scaled by the registry's decimals. When a token's balance slot can't be found, fork reports which token failed and continues.
- Sending: anvil starts with `--auto-impersonate`, and `ETH_FROM` is exported, so `cast send --unlocked` works from your real address without ever unlocking its key.

`chainz fork ls` lists live forks with their URL, parent, block height, uptime and PID. The dashboard does not show forks. Snapshots and fork state persistence are out of scope for v2.0.

## Signers and secrets

A signer is a reference to key material plus its cached public address. Chainz needs the address often and the key almost never. The v2 rule: **chainz unlocks a key only for `--expose-key`**. Every other feature (`@wallet`, `ETH_FROM`, balances, fork funding, keystore signing) works from the cached address.

| Backend | Key lives in | Unlock path | Default? |
| --- | --- | --- | --- |
| `keyring` | OS keyring (Secret Service, macOS Keychain) | chainz reads it on `--expose-key` | yes, when a keyring is available |
| `encrypted` | `secrets.toml`, AES-256-GCM with Argon2id | password prompt | fallback on headless machines |
| `keystore` | Foundry keystore file | Foundry itself, via `ETH_KEYSTORE`; chainz never decrypts it | new in v2 |
| `1password` | 1Password item, through `op` | `op read` |  |
| `plaintext` | `secrets.toml` | none | explicit opt-in only |

**Invariants carried from v1:**

- No secret in ordinary output, `Debug`, error chains or the gateway log. Request params in the inspector are summarised, and `eth_sendRawTransaction` payloads are recorded only as their transaction hash.
- Secrets go in through `--stdin` or a hidden prompt. Passing one as an argument still works, with a warning.
- Adding a key to a backend and committing it to config happen together: if the config write fails, the backend entry is rolled back.
- `--reveal` is the only flag that shows secret values, and it is never the default for JSON.

**New in v2:** secrets move out of `config.toml` into `secrets.toml`, so the main config can be shown, diffed and pasted safely. Through the gateway, child processes never receive endpoint secrets at all.

## Architecture

One binary, built from a small workspace with a strict dependency direction: a leaf of OS primitives; a synchronous core that knows nothing about the network; an RPC crate beside it that knows nothing about the config; then the live surfaces, which join the two. The split keeps the hot path (`exec`, `ls`, `env`) honest at compile time. Those commands call only `chainz-core` APIs, so they cannot start the async runtime, open a socket or spawn a thread, and the `strace` test enforces that at runtime.

```
                 chainz (bin)
        ┌─────────┼──────────┐
   chainz-tui     │     chainz-serve
        └────┬────┴─────┬────┘
        chainz-core  chainz-rpc        (rpc does not depend on core)
             └────┬─────┘
             chainz-os
```

Every crate also depends on `chainz-os` directly; `chainz-tui`'s tests use `chainz-serve`'s `testing` feature. Top, serve and fork share one RPC layer, and top reads serve through its control API. Exec talks only to the core and the runtime registry. Core decides where a chain's requests go (`chainz_core::route`), and each consumer turns that into an RPC `Target` in one line.

| Crate | Owns | Depends on |
| --- | --- | --- |
| `chainz-os` | private dirs and files, atomic durable writes, `flock` locks and probes, file stamps, the clock | libc |
| `chainz-core` | model (Chain, Endpoint, Signer, Label), config load and save via `toml_edit`, secrets, chain-reference resolution and routes, token grammar, exec contract, built-in tokens, label index, display redaction, runtime registry (serve and forks), v1 migration | os, serde, toml, toml\_edit, indexmap, k256, sha3, keyring, argon2, aes-gcm |
| `chainz-rpc` | JSON-RPC client, probes, endpoint health and ranking, capabilities, health history, `HttpUpstream`, ABI helpers, chainlist, the control protocol (types, framing, client) | os, reqwest, tokio |
| `chainz-serve` | gateway HTTP server, adaptive limiter, metrics, inspector, control handler, the fork's pinned proxy | os, core, rpc, hyper |
| `chainz-tui` | `chainz top` | os, core, rpc, ratatui |
| `chainz` (bin) | clap surface, pickers, output formatting, `exec`/`shell`/`env`, `fork` supervisor, background launch | all of the above |

**Config writes go through `toml_edit`**: each record is serialized from the typed model and merged into the existing table, so `chainz chain set --rpc` or the dashboard's `s` key changes one value and keeps your comments, ordering and formatting. Validation is unchanged from v1: one `validate`, run before every write, with an atomic rename.

**Locking and write order.** Reads take no lock: each file is replaced by an atomic rename, so a reader always sees a whole file. Writes take an exclusive file lock, re-read both files, apply the change, validate, and then write.

- **Two files, one order:** when both files change, `secrets.toml` is written first, then `config.toml`, each with fsync and an atomic rename. New secrets therefore exist before anything references them.
- **A crash between the two writes** leaves at most an unused secret behind, and `doctor` lists it.
- **Readers fail closed:** a reference to a missing variable is an error, never a literal `${NAME}` passed on.

**Performance budget**, enforced in CI:

| Metric | v1 today | v2 target | CI gate |
| --- | --- | --- | --- |
| `exec eth -- true`, chainz overhead | 3.8 ms | < 5 ms median (measured 3.1 ms) | ≤ 85% of v1 in the same job; no sockets or threads (strace) |
| `ls` with 17 chains | 3.1 ms | < 5 ms median (measured 2.0 ms) | ≤ 85% of v1 in the same job |
| `doctor`, 17 chains (active endpoints) | 157 ms | ≤ v1, network-bound | none |
| `top`, first paint | n/a | < 100 ms; data streams in | none |
| Gateway added latency, p50 | n/a | < 1 ms (measured 84 µs) | `chainz-serve`'s `latency` example against a local mock upstream, in the perf job; fails over 1 ms |
| Release binary | 14 MB | < 8 MB | size check |
| Clean release build | about 3 min | < 90 s | none |

## Delivery plan

The foundation is built serially and its interfaces are frozen. Then four features are built in parallel, each in its own worktree branched from the foundation. Integration and release come last.

**Phase 0: foundation (serial, on `revamp`)**

1. Workspace split; `chainz-core` model, TOML config with `toml_edit`, `secrets.toml`, validation, locking and write ordering.
2. v1 migration with `config migrate --dry-run`, tested against a fixture that mirrors the real config's shape: 17 chains, 149 endpoints including 16 with headers and 24 websocket, inline-key URLs, 8 literal verifier keys, 3 keyring signers, 4 variables.
3. The exec contract: resolver, token grammar, environment table with the v1 aliases, `execvp`, `--explain`, `env`, `shell`. The performance gate is switched on here, not at the end.
4. `chainz-rpc`: client, probe, ranking, capability tags, health history, and **`HttpUpstream`** (send one request to a chosen endpoint under a retry policy; a concrete type, not a trait), which both the gateway and the fork proxy use. Doctor and `rpc bench` move onto it.
5. **The whole clap tree as stubs.** Every command in the CLI surface exists and returns `not yet implemented`, so the streams fill in handlers without all editing one file.
6. **A minimal token registry:** `weth` and `usdc` for the 17 chains, enough for the dashboard and forks. The Address book stream extends it.
7. Interfaces: `LabelIndex` (resolve and reverse) and the `Runtime` registry (`serve.lock`, `serve.json`, `forks/*.json`) are frozen. The control API types (`Status`, `MetricsSnapshot`, `RequestRecord`) are **provisional** (tested against the real handler); the Gateway stream owns them and may change them, telling the Dashboard stream when it does.

**Phase 1: features (parallel)**

| Stream | Builds | Consumes | Done when |
| --- | --- | --- | --- |
| Gateway | `chainz-serve` over `HttpUpstream`: auth, failover pool, adaptive limiter, inspector, `serve log`, exec auto-routing | `HttpUpstream`, control types (owner) | `rl.ts` quotes run through it; killing the active upstream mid-run fails zero reads; a send never leaves the active endpoint |
| Dashboard | `chainz-tui`, all four tabs, `--once --json` | rpc, control API test server, label index, minimal tokens | all 17 chains render live; the traffic tab works against a real `serve` |
| Address book | `addr` commands, full `tokens.toml` with sources, `doctor --tokens`, `chainz label` | label index | `doctor --tokens` passes for every registry entry |
| Forks | `fork` supervisor and its pinned-endpoint proxy over `HttpUpstream`, registry, funding, `fork ls/stop`, replay guards | `HttpUpstream`, runtime, label index, minimal tokens | `fork base --fund usdc=1000` then `cast send --unlocked` works end to end; killing the supervisor leaves no anvil running |

**Phase 2: integration and release.** Merge the streams. Run the migration on the real config (dry run, compare, then migrate). Turn on the performance budget in CI. Rewrite the README around the five nouns. Publish 2.0.0 with a migration note in the changelog.

## Decisions

These were open questions, each with a proposed default. Every one shipped in 2.0 as its default.

- [x] **Exec auto-routes through a running gateway.** Decided: yes, with `--direct` to opt out. The alternative was opt-in per command (`--via-serve`).
- [x] **Failover starts empty after migration.** Decided: yes; nothing is retried on an endpoint you didn't approve. The alternative auto-filled `failover` with every public HTTP endpoint that has no credentials.
- [x] **Keep `chainz use`.** Decided: keep it, with the stderr notice whenever the default chain applies. The stricter alternative removed the global default, so `exec` would need an explicit chain or `$CHAINZ_CHAIN`.
- [x] **`secrets.toml` split.** Decided: variables and plaintext keys move out of the main config. The alternative kept everything in one file and relied on redaction.
- [x] **Signer naming.** Decided: `signer` replaces `key`, and `chainz key …` stays as a hidden alias.
- [x] **Workspace split vs one crate.** Decided: workspace. It enforces at compile time that the exec path cannot reach the network. One crate with features would have been simpler to publish.
- [x] **Gateway websockets** (`eth_subscribe`). Decided: HTTP only in 2.0. Websocket endpoints stay usable with `--direct`; routing them comes in 2.1.
- [x] **Windows.** Decided: Linux and macOS only in 2.0. The spawn-and-wait fallback and the Windows CI job are gone; v1 stays available for Windows.
- [x] **Foundry keystore backend** in 2.0. Decided: yes. It is small, and it removes the last reason to use `--expose-key` for Foundry.

## Self-review

An independent Fable 5.2 reviewer read the first draft against the v1 source, the shape of the real config, and Foundry 1.6 `--help` output. It returned 4 blockers, 10 should-fixes and 1 nit, and judged the draft not ready to sign off. All 4 blockers and 9 of the 10 should-fixes are folded into the sections above. I spot-checked its facts before acting: the config really has 149 endpoints (not the 23 the draft implied), 4 variables, 8 literal verifier keys and 9 chains with no verifier, and `anvil --fork-url` has no environment fallback.

| # | Severity | Finding | Response |
| --- | --- | --- | --- |
| 1 | Blocker | Gateway unauthenticated: other local users, or a web page via DNS rebinding, could spend keyed endpoints; 8545 clashes with anvil | Fixed in Gateway: a per-start token, a `Host` check, JSON-only POST, no CORS, the control API on a unix socket, default port 18545 |
| 2 | Blocker | Failover unsafe: a send could leak to a public RPC, a 20-block lag allowance breaks `latest`, shifting on 429 breaks read-your-writes, ws and capabilities ignored | Fixed in Gateway: an opt-in `failover` list per chain, sends only to the active endpoint, a 2-block high-water mark, wait-then-shift on 429, enumerated retryable errors, capability routing, ws excluded |
| 3 | Blocker | Migration loses or leaks data: literal keys in URLs and explorer keys, an undefined global explorer, verifiers newly added to 9 chains, ws and header endpoints unspecified, a race with parallel execs | Fixed in Config v2: named explorers attached per chain, key extraction plus `doctor` warnings, every endpoint kept, migration under lock with a re-check, defined write order |
| 4 | Blocker | Exec silently breaks v1 names (`CHAIN_NAME`, `WALLET_ADDRESS`, `@chainname`, `@verification_url`) | Fixed in Exec contract: kept as deprecated aliases with a warning; removed `@key` errors instead of passing through |
| 5 | Should-fix | Foundry facts wrong: anvil doesn't read `ETH_RPC_URL`; `ETH_FROM` is cast-only; a keystore path needs `ETH_KEYSTORE` | Fixed in Exec contract and Forks: the fork proxy URL is passed via `--fork-url`, `--sender @wallet` for forge, `ETH_KEYSTORE` |
| 6 | Should-fix | Gateway liveness by PID is fooled by PID reuse; config changes don't reach a running gateway | Fixed: an `flock` liveness check, a file watch with reload, `--rpc` pins through the gateway |
| 7 | Should-fix | Token grammar: `@uniswap/v3-core` would expand, typos pass silently, the `:qualifier` is overloaded | Fixed: `/` and `.` don't end a token, an unknown whole-argument token is an error, the qualifier always means a chain, signers are labels |
| 8 | Should-fix | Address book: global by default, truncated reverse labels, registry location and provenance unclear | Fixed: per chain by default with `--all-chains`, full addresses, compiled in with a `source` URL |
| 9 | Should-fix | Forks: a pinned block needs archive, failover corrupts state, zkSync can't fork, orphaned anvil, replay of real signatures | Fixed in Forks: one pinned archive endpoint, zkSync rejected, `PDEATHSIG`, signing restricted to unlocked impersonation |
| 10 | Should-fix | The global `default` chain risks wrong-chain sends | Partly: a stderr notice now; `use` is kept (see Decisions) |
| 11 | Should-fix | Two files are not atomic as a pair | Fixed in Architecture: secrets written first, fail closed on a missing variable |
| 12 | Should-fix | A 5 ms absolute CI gate will flake, and "links only core" means nothing in one binary | Fixed: a relative gate plus an `strace` structural test; the claim restated as compile-time API separation |
| 13 | Should-fix | The CLI tree is missing flags; `--fund` shape; `fork CHAIN` collides with `fork ls` | Fixed in CLI surface |
| 14 | Should-fix | Phase 0 freeze point: streams collide on the forwarder, the clap tree and tokens; control types frozen too early | Fixed in Delivery plan: one `HttpUpstream` type, clap stubs, a minimal token registry, provisional control types, perf gate in phase 0 |
| 15 | Nit | Over-engineered for a personal tool (capability probing, two metric windows, Windows); two template syntaxes | Mostly accepted: one `${…}` syntax; Windows is dropped (Linux and macOS only). Capability probing stays because fork pinning and gateway routing depend on it |


## Implementation notes

- **Perf baseline corrected.** The draft quoted v1 exec at 13–41 ms. That figure was inflated by the measurement harness; hyperfine on a warm cache gives about 3.8 ms for v1 and 3.1 ms for v2, against a 1.3 ms process-startup floor. The CI gate is therefore a ratio of 0.85, not 0.50, plus the strace structural test.
- **Forks always go through the proxy,** and through the gateway when one is running (see Forks). An earlier build passed the gateway URL, token included, to anvil's argv; that was fixed before merge.
