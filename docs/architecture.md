# Architecture

How chainz is put together, and the rules the code keeps that a reader of
one file can't see. What each command does is in the README and `--help`;
the API of each crate is its rustdoc.

## Crates

```
                 chainz (bin)
        ┌─────────┼──────────┐
   chainz-tui     │     chainz-serve
        └────┬────┴─────┬────┘
        chainz-core  chainz-rpc        (rpc does not depend on core)
             └────┬─────┘
             chainz-os
```

| Crate | Owns |
| --- | --- |
| `chainz-os` | private files and dirs, atomic durable writes, `flock` locks, file stamps, the clock |
| `chainz-core` | config model, load and save, secrets, migration, chain resolution and routes, the `@token` grammar, the exec contract, labels, redaction, the runtime registry |
| `chainz-rpc` | JSON-RPC client, probes, endpoint health and ranking, forwarding, the control protocol, chainlist |
| `chainz-serve` | the gateway and a fork's pinned proxy |
| `chainz-tui` | `chainz top` |
| `chainz` | the CLI: clap, handlers, prompts, output, the fork supervisor |

Every crate depends on `chainz-os`, and `chainz-tui`'s tests use
`chainz-serve`'s `testing` feature (so serve publishes before tui).
`chainz-core` is synchronous: no tokio, reqwest or hyper. `chainz-rpc`
knows nothing about the config; callers resolve an endpoint (with `${VAR}`s
expanded) and hand it in. The five library crates are published only so
`cargo install chainz` works; they have no stable API.

## Invariants

- **The hot path is offline.** `exec`, `env` and `ls` call only core (and
  `ls` reads the recorded health file). They start no runtime, open no
  socket and spawn no thread before `execvp`. The crate split enforces it at
  compile time; `crates/chainz/tests/e2e/hot_path.rs` checks it at run time.
- **Keys are unlocked only for `--expose-key`.** Everything else (`@wallet`,
  `ETH_FROM`, balances, fork funding, keystore signers) uses the cached
  address.
- **Secrets never reach ordinary output**, `Debug`, error chains or the
  gateway log. There is one redaction policy (core), and it fails closed.
  `--reveal` is the only way past it.
- **No hidden prompts.** Off a terminal chainz never asks; it reports and
  names the flag to pass.
- **Old names are unknown names.** Removed commands and flags get no hidden
  aliases.

## Config and write protocol

`config.toml` holds no secrets once `doctor` is clean; values live in
`secrets.toml` and are referenced as `${NAME}`. A template resolves in one
pass, with no re-scan: built-ins (`CHAIN_ID`, `CHAIN`), then the
environment, then `secrets.toml`. Inside a chainz context (`CHAINZ_CHAIN`
set) the exec contract's own variables are not read from the environment,
so a nested `chainz exec eth` in a `chainz exec base` shell can't pick up
base's keys. An unresolved reference is an error.

- **Reads take no lock.** Every file is replaced by an atomic rename, so a
  reader sees a whole file.
- **Writes** take `config.lock`, re-read both files, apply the change,
  validate the result and write. `secrets.toml` goes first, then
  `config.toml`, each fsynced and renamed, so a new secret exists before
  anything references it. A crash in between leaves at most an unused
  secret, which `doctor` lists.
- **Edits keep the file.** Writes go through `toml_edit`: comments,
  ordering and unchanged values stay as you wrote them.
- **Adding key material is part of the write.** If the config write fails,
  the keyring entry it created is deleted again.
- **Migration from 0.4** runs once, under the write lock, re-checking that
  `config.toml` is still missing, so parallel first runs migrate once.
  It writes both new files before renaming `config.json` to
  `config.v1.json.bak`.

## Runtime files and the control protocol

Live processes publish themselves in `$XDG_RUNTIME_DIR/chainz/` (the state
dir when that is unset), every file mode 0600 in a 0700 dir. Liveness is
always a held `flock`, never a PID check.

| File | Meaning |
| --- | --- |
| `serve.lock` | held by the gateway for its whole life; `exec` probes it with one non-blocking shared lock |
| `serve.json` | the gateway's port and token (format 1), written once it listens, removed first on shutdown |
| `serve.sock` | the control socket |
| `forks/<name>.lock`, `forks/<name>.json` | held by a fork's supervisor; its record (format 1) |
| `ports/<port>.lock` | a fork's port, reserved before anvil starts |

Records carry a `format`; a reader refuses one it does not know. Only the
holder of a lock removes its files.

The control API is HTTP/1.1 on `serve.sock`, never on the TCP port. It is
what `serve status`, `serve log` and `top` read; its types are in
`chainz_rpc::control`.

| Path | Method | Answer |
| --- | --- | --- |
| `/_chainz/status` | GET | version, PID, port, uptime, each chain's active endpoint, failover list and head |
| `/_chainz/metrics` | GET | 60 s and 900 s windows: rows per (chain, endpoint, method), plus totals overall and per chain with true percentiles |
| `/_chainz/events?backlog=N\|all&after=ID&follow=false` | GET | NDJSON request records: the last `N` (or all) above `after`, then live ones unless `follow=false` |
| `/_chainz/reload` | POST | 204, or 500 with the reason when the new config is invalid (the old one stays) |

Clients give up after 5 s (an events body excepted), so a wedged gateway
can't hang them. A record never holds params, URLs or bodies.

## Gateway safety

- **Auth.** Each start mints a random token that every URL carries
  (`/<token>/<chain>[/<endpoint>]`). Only `Host: 127.0.0.1` or `localhost`,
  only `POST` with `application/json`, no CORS: a web page can't reach it by
  DNS rebinding. Headers and `${VAR}` values are applied inside the gateway,
  so a child process never sees an endpoint secret.
- **Approved failover only.** The active endpoint is tried first, then only
  the chain's `failover` list, so a request for a private endpoint never
  leaks to a public one. Migration leaves the list empty.
- **Sends are never retried** or sent anywhere but the active endpoint.
  Filters stay on the node that made them. A batch follows its strictest
  method.
- **`latest` doesn't go backwards.** A failover target must be within 2
  blocks of the head high-water mark (the best head over the last three
  probe rounds).
- **Rate limits.** Each endpoint has an adaptive token bucket. A 429 halves
  its rate; a `Retry-After` of 2 s or less is waited out on the same
  endpoint (keeping read-your-writes), and only a longer one moves a read
  to failover.
- **Shutdown** drains in-flight requests for up to 5 s.

## Forks

- Anvil forks through a proxy inside the supervisor, pinned to one endpoint
  for the fork's life (lazily fetched state must come from one node), and
  through the gateway when it runs. The proxy never fails over.
- Anvil's `--fork-url` carries a per-fork token, never the gateway's or an
  endpoint's secret, so its argv is safe in `ps`.
- A fork shares its parent's chain ID, so a transaction signed for it is
  valid on the real chain. On forks chainz refuses `--expose-key` and does
  not export `ETH_KEYSTORE`; anvil's `--unlocked` impersonation is the only
  signing path. Forks resolve by name only, so `exec 8453` is always the
  real chain.
- Anvil runs in its own process group with `PR_SET_PDEATHSIG` on Linux; on
  macOS the next `fork` or `fork ls` reaps an anvil whose supervisor died.

## Performance

CI enforces the hot path relative to 0.4 (built from `c964bce`), not in
absolute time, which would flake: `scripts/perf-gate.sh` fails when the
median of `exec 1 -- true` or `ls` (17 chains) is over 85% of 0.4's, and the
strace test fails on any socket or thread. The gateway's `latency` example
fails when it adds 1 ms or more to the p50 round trip.

## Decisions

- **`execvp`, not spawn and wait:** signals, exit codes and TTYs behave as
  if chainz weren't there. This is why Windows is not supported.
- **A workspace, not one crate:** the compile-time split is what keeps the
  hot path offline.
- **Exec routes through a running gateway** (`--direct` opts out), so the
  gateway helps without changing any command.
- **Failover starts empty:** nothing is retried on an endpoint you didn't
  approve.
- **`secrets.toml` is separate,** so `config.toml` can be shown, diffed and
  pasted.
- **The gateway is HTTP only.** Websocket endpoints work with `exec`
  (directly), not through the gateway.
- **Keep `chainz use`,** with a stderr notice whenever `exec` falls back to
  the default chain.
