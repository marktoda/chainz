# Contributing

Start with [docs/architecture.md](docs/architecture.md): the crate graph and
the rules every change keeps. Each crate's rustdoc
(`cargo doc --workspace --no-deps --open`) is the API reference.

## Setup and checks

```bash
nix develop            # optional: toolchain, Foundry, hyperfine, strace
scripts/check.sh       # fmt, clippy, rustdoc, tests: what CI runs
```

`scripts/check.sh lint` and `scripts/check.sh test` run half each. Tests
that need `anvil` or `strace` skip when it is missing; with
`CHAINZ_REQUIRE_TOOLS=1` they fail instead, as in CI.

The hot-path gate compares release builds against chainz 0.4 (commit
`c964bce`), as CI does:

```bash
scripts/perf-gate.sh path/to/0.4/chainz target/release/chainz
cargo run --release -p chainz-serve --example latency
```

## Never touch your real setup

The binary migrates and writes `~/.config/chainz`, and signers live in the
OS keyring. Never run a development build against them. Point `HOME` and
every XDG directory at a scratch directory and set `CHAINZ_DISABLE_KEYRING=1`:

```bash
s=$(mktemp -d)
export HOME=$s XDG_CONFIG_HOME=$s/config XDG_STATE_HOME=$s/state \
  XDG_CACHE_HOME=$s/cache XDG_RUNTIME_DIR=$s/runtime CHAINZ_DISABLE_KEYRING=1
```

Tests do this for you (see below). Never pick a port by guessing: bind
port 0 (`serve --port 0`, `anvil --port 0`) or let `fork` choose, then read
the port back.

## Adding a command

1. **Arguments** go in `crates/chainz/src/cli.rs`: one `…Args` struct per
   command, with every argument documented (the doc comment is the
   `--help` line). Declare completion on the argument
   (`add = ArgValueCompleter::new(…)`). Removed flags get no hidden alias.
2. **The handler** goes in `crates/chainz/src/commands/`, routed from
   `commands::dispatch`. It returns the exit code: 0, or the code of a
   result it reported itself (as `doctor` does). An `Err` prints
   `error: …` and exits 1; a usage error exits 2 and a cancelled prompt 130.
3. **Rules belong in the library crates.** Validation, resolution and
   formatting that another command or `top` could need go in `chainz-core`
   (or `chainz-rpc` for the network), not in the handler.
4. **Network work** runs inside `net::block_on`, a current-thread runtime
   built on demand. `exec`, `env` and `ls` must never reach it; the strace
   test fails if they open a socket or start a thread.
5. **Prompts** go through the `Ask` seam in `prompt`, so tests can script
   them. Off a terminal, ask nothing: fail with a usage error that names
   the flag.

## Tests

- **End to end:** `crates/chainz/tests/e2e/`, one test binary with a module
  per command (`add.rs`, `exec.rs`, `doctor.rs`, …). A test of one command
  goes in that command's module; `cli.rs` holds what every command shares
  (usage errors, removed spellings), `config.rs` the config files
  themselves, `hot_path.rs` the strace check of `exec`'s fast path. Every
  run of the binary goes through `Sandbox` in `e2e/common.rs`, which clears
  the environment, points `HOME` and the XDG dirs at a temp dir, disables
  the keyring and empties stdin. A gateway or process a test starts is
  wrapped in `StopGateway` or `ChildGuard`, so a failing test leaves
  nothing running.
- **Integration tests** of the library crates are one binary each too:
  `crates/chainz-rpc/tests/rpc/` and `crates/chainz-serve/tests/serve/`.
- **Golden files** pin output that people and scripts read
  (`crates/chainz/tests/golden/`, written by `e2e/golden.rs`, and
  `crates/chainz-tui/tests/golden/`). Mask what varies run to run (ports,
  latencies, ages) rather than sleeping it into shape.
  `CHAINZ_BLESS=1 cargo test` rewrites them; review the diff.
- **Unit tests** sit beside the code in each crate. `chainz-serve`'s
  `testing` feature runs the real control handler over canned state for
  clients' tests; `chainz-core`'s gives `TempStore`, `MemoryKeyBackend`
  and `ForkRecord::sample`.
- **`crates/chainz-testkit`** is the shared harness, a dev-dependency of
  every crate and never published (it depends on no chainz crate, and
  `cargo package` drops it): `Mock`, a JSON-RPC server answering per call
  or per request that records what it was sent; a raw HTTP client;
  `golden!`; `wait_until` and `wait_until_async` (poll, never sleep a
  guess); `short_tempdir` for unix sockets; `require_tool`, which prints
  `SKIPPED` when `anvil` or `strace` is missing; `ChildGuard`; ABI
  encoders with hand-written selectors; and the anvil test keys. Reach for
  it before writing another mock or helper.
- RPCs are local mocks; no test uses the network. A failure path that
  retries (a closed port, a 503) costs real backoff time: answer with an
  error the code doesn't retry instead.

## Docs

Each fact has one home: what a command does is its `--help`; how to use
chainz is the README; what changed for users is CHANGELOG.md (under the
next release); why the code is shaped this way is docs/architecture.md.
Update the one home in the same change.
