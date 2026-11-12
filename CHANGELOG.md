# Changelog

## 2.0.0 - Unreleased

A ground-up rewrite as a local multichain workbench. The design is in
`docs/v2/design.md` and the crate contracts in `docs/v2/interfaces.md`.
Builds of `main` before the release had a few commands and flags that
changed before 2.0.0; they are listed under Removed and Changed.

### Migrating from 0.x

- The first v2 run migrates `~/.config/chainz/config.json` into `config.toml`
  plus `secrets.toml` (mode `0600`), then renames the old file to
  `config.v1.json.bak`. Preview with `chainz config migrate --dry-run`.
- Chains get short names (`eth`, `base`, `arb`, `op`, `uni`, …). Old chainlist
  names remain aliases.
- Endpoints are named after their host; `selected_rpc` becomes `rpc = "<name>"`.
  Known URL credentials (Infura, Alchemy) and verifier keys move into
  `${VARIABLES}`; anything else is listed by the migration and by `doctor`.
- Keys become signers and keep their backend and cached address. Keyring
  entries need no re-entry.
- Commands are regrouped by noun: `add`/`show`/`remove` become
  `chain add/show/rm`, `update` becomes `chain set` (`chainz update` still
  works), `key …` becomes `signer …`, and RPC management moves to `rpc …`.
  `chainz list` and `list`/`remove` under each noun still work (`chain
  remove`, not `chainz remove`). `init` is still the guided setup.
- `CHAIN_NAME`, `@chainname`, `@verification_url` and `@verifier_api_key` are
  deprecated aliases with a warning for one release. `@key` is removed; use
  `--expose-key`.

### Added

- `exec` replaces itself with the command (`execvp`) and costs about 3 ms.
  `exec`/`env`/`shell --rpc NAME|URL` take an endpoint name or a URL for
  that run only (not saved), by the same rule as `chain set --rpc`.
  New tokens: address labels (`@usdc`, `@myrouter`), signer names (`@funder`),
  cross-chain labels (`@usdc:arb`), and `--explain`. `env` prints exports.
- `chain set [CHAIN]`, also `chainz update`: every chain setting in one
  atomic write. `--signer`/`--no-signer`, `--rpc NAME|URL` (a new URL is
  added, checked, its secrets moved, and activated), `--header` and
  `--clear-headers`, `--failover`/`--no-failover`, `--explorer`/`--no-explorer`
  (`etherscan` is built in), `--api-key-stdin`, `--name`, `--alias` and
  `--rm-alias`. With no flags on a terminal it opens v1's menu (RPC, Signer,
  Explorer, Name, Aliases, Done) and applies the choices together; with no
  chain it opens a chain picker first. Off a terminal it needs flags.
- `init`: on a terminal, a guided setup built from the `chain add`,
  `signer add` and `chain set` flows (chains, a signer, an Etherscan key, a
  default chain). Off a terminal it prints the quick start, as `chainz`
  and `chainz ls` do while no chain is configured.
- `edit [CHAIN]`: `config.toml`, or one chain's table, in `$EDITOR`,
  validated on save and refused if the file changed in the meantime.
- Auto-attach: the first signer `signer add` creates is attached to every
  chain without one (`--no-attach` skips it). `chain add` attaches the only
  signer (`--signer S` picks another, `--no-signer` none) and the
  `etherscan` explorer when it is configured and Etherscan's v2 API covers
  the chain (a built-in list: not anvil's 31337 or custom IDs, which print
  `explorer: none (etherscan doesn't cover chain ID 31337)`). `init` attaches
  it on the same terms; `chain set --explorer etherscan` on a chain it
  doesn't cover still attaches it, with a warning.
- `serve`: an opt-in local RPC gateway with approved-only failover,
  rate-limit smoothing, a request inspector (`serve log -f`), metrics, and a
  control socket. `exec` routes through it automatically while it runs.
- `top`: a live dashboard of chain heads, endpoint health, signer balances
  and gateway traffic, with `--once --json` for scripts.
- `fork`: anvil forks in one command, funded signer, usable as a chain name,
  with replay guards and orphan-proof supervision. Each fork reserves its
  port (`ports/<port>.lock` in the runtime dir) before anvil starts, so
  parallel forks never pick the same port; an automatically chosen port
  that is taken moves on to the next one. `--port` is used exactly or fails.
  `--rpc NAME|URL` pins the upstream (a URL for this fork only, not saved)
  instead of the active endpoint, or with `--block` the first archive one.
- Address book: `addr add/ls/rm/find`, built-in major-token labels on every
  configured chain, and `label` to annotate addresses in any output.
  `addr import` labels the contracts in Foundry broadcast output
  (`run-latest.json`) on the chain each run used, with `--dry-run`,
  `--prefix`, `--only`, `--force` and `--json`.
- `rpc bench` and `rpc discover` probe, rank and tag endpoints.
  `doctor --tokens` verifies the built-in token registry on chain.
- Foundry keystore signers (`--backend keystore`), exported as `ETH_KEYSTORE`.
- Dynamic shell completions: `source <(chainz completions zsh)` (or bash,
  fish, elvish, powershell) completes chain names, aliases and live forks,
  the typed chain's endpoints for `--rpc`, signers, labels, variables, and
  `exec`'s `@tokens`, including `@label:chain`, from the current config.
- Per-endpoint HTTP headers (`rpc add --header "name: value"`), applied by
  probes and the gateway, and exported as `ETH_RPC_HEADERS` on direct
  connections. (This was unreleased in 0.x; v1 configs that use it migrate.)
- `--json` for `ls`, `chain show`, `rpc ls` (rows carry `websocket`),
  `signer ls`, `var ls`, `addr ls`, `addr find` (exit 1 when nothing is
  found), `addr import`, `fork ls` and `serve status`.
- `config path --all` lists every chainz path, the gateway and fork logs
  included.

### Changed

- The config is hand-editable TOML that keeps your ordering and comments;
  `edit` validates on save. Read-only commands no longer take the config
  lock.
- `use` with no chain prints the default chain (`default: base`, or
  `default: none`) and how to change it, on a terminal too; it no longer
  opens a picker or fails off a terminal.
- `completions <shell>` prints a loader that calls back into chainz instead
  of a static script; re-source it from your shell rc (see README).
- Flags, one meaning each: `chain add --url URL` (was `--rpc`; repeatable,
  the first becomes active), `rpc add --activate` (was `--use`), and
  `signer rm --unassign` (was `--force`) removes the signer from the chains
  that use it. `--yes` (`-y`, on every `rm`, `rpc bench --fix` and
  `rpc discover`) always means "accept without asking", `--detach` always
  means "run in the background", and `--reveal` is the only flag that shows
  secrets.
- One confirmation policy for every removal: `chain rm`, `rpc rm`,
  `addr rm`, `var rm` and `signer rm` ask on a terminal and need `--yes`
  (`-y`) off one. `chain rm` names what goes with the chain (its labels,
  and the default) and reports it. `signer rm` asks one question (accepting
  also unassigns it), and off a terminal one error names every flag it
  needs (`--unassign --yes`); after unassigning it says which chains have
  no signer now and how to attach one. `signer migrate --all` tries every signer and reports each
  failure. Moving a keyring signer to another backend deletes its old
  keyring entry.
- `rpc bench --fix --yes` writes the proposed failover lists without asking;
  `rpc discover --yes` adds every endpoint that answered. Off a terminal,
  both report instead of prompting.
- `chain add --url`, `chain set --rpc URL`, `rpc add` and `rpc discover`
  move known URL keys and credential headers into `secrets.toml` as they
  write, and print what they moved. `rpc add` refuses a URL the chain
  already has (use `chain set --rpc`) and any scheme but http(s) and ws(s),
  even with `--no-check`.
- Secrets passed as arguments (`signer add --key`, `chain set --api-key`,
  `var set NAME VALUE`) warn that they are visible in shell history and
  `ps`. The two flags are hidden from help, which shows the stdin form.
  Explorer API keys are trimmed however they are entered.
- Redaction fails closed and masks only what may be a secret. URLs show
  `${VAR}` templates, hosts and ordinary paths verbatim; user info, the
  fragment, known Infura/Alchemy keys, credential-shaped host labels, and
  any path segment or query value that is not a known-safe form (empty, a
  template, a number, a version such as `v2`, or a short lowercase word)
  print as `[REDACTED]`, and a URL that does not parse is masked whole.
  Header values show only templates and an allow-list of auth schemes and
  media types (`Bearer`, `Basic`, `Token`, `Digest`, `application/json`,
  `text/plain`, `gzip`, `identity`). `exec --explain`, `ls`, `chain show`,
  `rpc ls`, `rpc discover`, the chain picker, `top` and `Debug` output all
  use this one policy; `--reveal` prints values as stored.
- `signer ls` has a header row.
- Chain resolution: an exact chain ID, name or alias wins over a fork of the
  same name, then a live fork's exact name, then a prefix. A fork resolves
  only while its supervisor runs.
- `serve status` shows the gateway's own totals for the last 60 s, overall
  and per chain, with the true median latency (it used to average the
  per-method medians); `--json` adds `totals_60s` and `chain_totals_60s`.
  Against a gateway that sends no totals both show `-` (`null` in JSON).
- Both proxies (the gateway and a fork's pinned proxy) accept request bodies
  up to 64 MiB (the gateway took 32 MiB) and answer an upstream failure the
  same way: 429 with `Retry-After` and JSON-RPC code -32005 for a rate
  limit, 504 for a timeout, the upstream's own 4xx/5xx, else 502. The fork
  proxy used to answer -32000 and 502.
- `top`: the RPC column, `y`, the Endpoints tab's marker and JSON
  `chains[].rpc`/`endpoints[].active` all name the endpoint the gateway
  actually serves from (the Endpoints tab adds `(config: X)` when it
  differs from the configured one). The Balances `VIA` column reads
  `multicall3` or `batch`, as in JSON. Feed errors read `class(code)`, as in
  `serve log`. Gateway, request-feed and health-history failures show as
  warnings (on stderr with `--once`), and notices from a config reload
  (such as a permissions fix) appear in the footer. A gateway that sends
  no traffic totals shows `-` (`null` in JSON). Native symbols come from
  one table (gnosis is xDAI), and a chain task that crashes under `--once`
  reports the class `internal`.
- `doctor` (not `doctor --tokens`) notes labels that shadow a built-in token
  and says why `secrets.toml` does not parse; `doctor --tokens --fix` is a
  usage error.
- The gateway inspector shows the node's transaction hash for
  `eth_sendRawTransactionSync` as well, and only prints an `@label` that
  expands back to the same address.
- Exit codes: 0 on success, also when stdout is closed early (`chainz … |
  head`); 1 for an error, or a result the command reports itself (`doctor`
  problems, `addr find` with no match, `serve status` with no gateway); 2
  for every usage error (including a missing chain off a terminal for
  `exec`, `shell`, `env`, `addr add`, `chain set` and `chain add`,
  `fork stop` with no name, `signer add` or `var set` without the key or
  value off a terminal, and removed commands); 126 and 127 from `exec`
  when the command can't run or doesn't exist; 130 when a prompt is
  cancelled; and for a foreground `fork`, 130 after Ctrl-C, 143 after
  SIGTERM or `fork stop`, 129 after SIGHUP.
- Uptimes read `42s`, `12m05s`, `2h03m`, `3d04h` everywhere.
- v1 spellings are accepted: `list`/`remove` aliases on every `ls`/`rm`,
  `chainz update`, `key …`, `--type private-key`/`one-password`, and
  `var get --show`. A fork can't be named `list`.
- The workspace is six crates: `chainz-os` (OS primitives: private files,
  atomic writes, locks, file stamps, the clock), `chainz-core`, `chainz-rpc`
  (which does not depend on core), `chainz-serve`, `chainz-tui` and the
  `chainz` binary. The exec path cannot reach the network, and CI checks
  that with `strace`. The internals were restructured before release so
  each rule has one owner and no crate or module depends on itself.
- Errors print as `error: …`, the prefix clap's usage errors (and cargo
  and Foundry) use; they printed `Error: …`.
- `rpc ls` is an aligned table with a header (`NAME HEALTH CHECKED URL`):
  long endpoint names no longer push columns apart, rows have no trailing
  spaces, `CHECKED` says how old the health sample is (`3m ago`), and a
  legend explains `●` active and `◐` failover. `--json` rows add
  `probed_at_ms`. The endpoint lists of `chain show` and `rpc bench` size
  the name column to the longest name too.
- `var ls` has a header row and summarises what uses each variable
  (`24 endpoints on 17 chains`, `explorer etherscan`); `-v` lists every
  config path under it, and `--json` keeps the full `used_by` list.
- `doctor` prints one line for one cause on many chains (`'quicknode' on
  19 chains: literal credential in the URL …`, `active endpoint 'alchemy'
  is failing (connection failed) on 17 chains`); `-v` lists each finding
  under it. Without `--fix`, the closing line says what `--fix` would do
  (`would move 3 credentials, switch 2 active endpoints`) or that it can't
  repair these.
- `ls` shows each chain's aliases and endpoint count (`EPS`). The `RPC`
  column names the active endpoint and adds its host only when the name
  doesn't already say it (`public …base.org`, but `infura`). `--json` is
  unchanged.
- `top --once` leaves chains that are down (with no balances read) out of
  each signer's Balances table and names them in one line instead
  (`balances skipped on down chains: arb`, or a count past three). With no
  chains it no longer prints an empty Balances table per signer.
  `--json` is unchanged.
- `addr ls` has a header row (`NAME CHAIN ADDRESS SOURCE`). `addr ls
  --builtin` lists the built-in tokens on your configured chains only;
  `--all` lists every chain's.

- Writes say what changed: `chain set` and `update` print each change as
  a transition (`active endpoint: alchemy → infura`) with its side effects
  (`failover: llama, ankr → ankr` when the new active endpoint was in
  failover), and "No changes" when every value was already set (they used
  to print "Updated chain"). `rpc rm` says when the endpoint also left
  failover, `use` with the current default says so, `--api-key-stdin`
  says when it replaced the value of the shared key variable, and `edit
  CHAIN` lists what changed, a chain ID change included.

- A write that leaves a chain without a signer says so and names the fix
  (`chainz chain set CHAIN --signer <name>`): `chain add` when there are
  several signers to choose from, and `signer rm --unassign`.

### Removed

- Windows support.
- `@key`; use `--expose-key`.
- From pre-release builds of `main`: `chain rename` and `chain alias` (use
  `chain set --name`, `--alias`, `--rm-alias`), `chain edit` and
  `config edit` (use `edit [CHAIN]`), and `rpc use` (use
  `chain set --rpc NAME`). Removed commands and flags have no hidden
  aliases and exit 2.

### Fixed

- `init` dead ends: with no chains it skips the Explorer and Default steps
  and ends with how to add one, not "Done"; a key that doesn't work (a
  typo, a password mismatch) is asked again instead of ending the wizard;
  and when chainlist is unreachable it offers to add the chain by ID and
  RPC URL.
- `signer add --backend keystore` refuses a `--path` that doesn't exist, and
  a keystore with no address field (as `cast wallet new` writes) without
  `--address`, saying how to get it (`cast wallet address --keystore
  PATH`). It used to add a signer with no cached address, or none at all
  that could work.
- `chain add NAME --url URL` downloaded chainlist to resolve the name. With
  `--url` the chain is now a chain ID or a built-in short name (`arb`,
  `base`…), resolved offline; another name is a usage error asking for the
  ID (and `--name`).
- `rpc bench` showed dead endpoints with a `0ms` latency and ranked them by
  how fast they failed. They now come last in config order, with `–` and
  what went wrong (`✗ connection failed`); each chain says how many
  endpoints answered, a chain with no healthy endpoint says so (with
  `--fix`: nothing to fix), and the exit code is 1 when a chain has no
  endpoint that answered.
- Re-adding a URL whose key an earlier write moved into a variable (an
  Alchemy or Infura URL) no longer adds `alchemy-2`, `alchemy-3`…: endpoint
  URLs are compared with the variables expanded, so `rpc add` refuses it by
  name and `chain set --rpc URL` activates the endpoint that has it.
- The `chain set`/`update` menu: every picker starts on the current value
  (it started on the first item, so Enter switched the RPC or signer); Esc
  in a picker goes back to the menu with the pending edits kept (it
  cancelled the whole edit), and its hint says `ESC: back`; Esc in the menu
  asks before discarding pending changes; after an edit the cursor is on
  Done.
- Every command exits quietly (exit 0) when its reader goes away: `rpc ls
  | head`, `var ls | head` and `addr ls | head` panicked.
- `chain add NAME` and `rpc discover` paired probe results with the wrong
  URLs when chainlist listed an invalid one, so they could activate or add a
  dead endpoint. `chain add` with no chain off a terminal is a usage error
  before any chainlist download.
- `exec`: `--expose-key --explain` no longer unlocks the key.
- `exec` no longer resolves a fork whose supervisor is gone, and an
  unreadable gateway or fork record is an error rather than "not running".
- A foreground `fork` reports how it was stopped (it exited 0). `fork stop`
  waits up to 10 s for the supervisor, and `fork ls` asks every fork for
  its height at once, with a 5 s timeout.
- Starting `serve` while another command checked for a gateway could fail
  with "already running".
- A corrupt `health.json` is moved aside to `health.json.corrupt` and
  reported instead of being overwritten.
- `addr rm --chain` accepts a chain alias as well as the exact name or ID.
- `signer add`/`signer migrate` roll back a keyring entry they created if
  the config write fails, and report a rollback failure.
- `serve stop` and `fork stop` never signal a PID that isn't a single
  process; serve and fork logs are created `0600`, and the chainlist cache
  `0600` in a `0700` directory.
- Renaming a chain whose label scope clashes is an error instead of
  leaving the old key behind.

## 0.4.0 - 2026-07-16

### Security

- OS keyring storage is now the default, with password-encrypted storage as
  the interactive fallback. Plaintext storage requires `--type private-key`.
- Added `--stdin` for private keys and variables; argv inputs now warn.
- Added `chainz key migrate` and interactive plaintext migration through
  `doctor --fix`.
- Redact verification keys, variable values, and credential-bearing URLs from
  ordinary and JSON output unless `--show-secrets` is explicit.
- `@wallet` no longer injects a raw private key. `--expose-key` provides an
  env-only alternative to the compatibility `@key` expansion.
- Key debug formatting is redacted and key backend tests are hermetic.

### Reliability

- `init` now stages the complete wizard and preserves the existing config on
  cancellation or failure.
- Legacy migration and unsafe permission failures are no longer ignored.
- Config writes validate chain identities, RPC selection, defaults, variables,
  keys, and key references.
- Referenced keys cannot be removed, and chain replacement preserves the
  default-chain invariant.
- Encrypted records now persist their envelope version and Argon2 parameters;
  existing encrypted records remain compatible.
- Config read-modify-write operations are serialized across processes, and
  durable atomic replacement now uses a portable Unix/Windows implementation.
- External key provisioning and migration roll back newly created credentials
  when the corresponding config transaction cannot be committed.

### Tooling

- CI now tests Linux, macOS, and Windows and verifies the crates.io package.
- Added a protected, tag-gated crates.io and GitHub release workflow.
- Interactive workflows now use a testable prompt adapter.
- Narrowed the pre-1.0 Rust interface to the CLI entry point and serialized
  records under `chainz::model`; command implementation modules are private.

### Breaking changes

- Bare `key add --key` no longer silently creates plaintext storage.
- Variable values and verification keys are redacted by default.
- Duplicate chain IDs and colliding aliases are rejected.
- `@key` is deprecated and scheduled for removal in 1.0; use `--expose-key`.
- Pre-1.0 Rust model imports now live under `chainz::model`.
