# Changelog

## 2.0.0 - Unreleased

A ground-up rewrite as a local multichain workbench. The design is in
`docs/v2/design.md`.

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
- Commands are regrouped by noun: `add`/`list`/`show`/`remove` become
  `chain add/ls/show/rm`, `key …` becomes `signer …`, and RPC management
  moves to `rpc …`. `ls` and `list` still work.
- `CHAIN_NAME`, `@chainname`, `@verification_url` and `@verifier_api_key` are
  deprecated aliases with a warning for one release. `@key` is removed; use
  `--expose-key`.
- Windows is no longer supported.

### Added

- `exec` replaces itself with the command (`execvp`) and costs about 3 ms.
  New tokens: address labels (`@usdc`, `@myrouter`), signer names (`@funder`),
  cross-chain labels (`@usdc:arb`), and `--explain`. `env` prints exports.
- `serve`: an opt-in local RPC gateway with approved-only failover,
  rate-limit smoothing, a request inspector (`serve log -f`), metrics, and a
  control socket. `exec` routes through it automatically while it runs.
- `top`: a live dashboard of chain heads, endpoint health, signer balances
  and gateway traffic, with `--once --json` for scripts.
- `fork`: anvil forks in one command, funded signer, usable as a chain name,
  with replay guards and orphan-proof supervision.
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

### Changed

- `completions <shell>` prints a loader that calls back into chainz instead
  of a static script; re-source it from your shell rc (see README).
- The config is hand-editable TOML that keeps your ordering;
  `config edit` and `chain edit` validate on save.
- Read-only commands no longer take the config lock.
- The workspace is split into `chainz-core`, `chainz-rpc`, `chainz-serve`,
  `chainz-tui` and the `chainz` binary. The exec path cannot reach the
  network, and CI checks that with `strace`.

### Fixed

- `chain add NAME` and `rpc discover` paired probe results with the wrong
  URLs when chainlist listed an invalid one, so they could activate or add a
  dead endpoint.
- `exec`: a missing command exits 127 and an unexecutable one 126 (was 1);
  `--expose-key --explain` no longer unlocks the key.
- A foreground `fork` reports how it was stopped: 130 after Ctrl-C, 143
  after SIGTERM or `fork stop`, 129 after SIGHUP (was 0). `fork stop` waits
  up to 10 s for the supervisor, and `fork ls` asks every fork for its
  height at once, with a 5 s timeout.
- `addr rm --chain` accepts a chain alias as well as the exact name or ID.
- `signer add`/`signer migrate` roll back a keyring entry they created if
  the config write fails, and report a rollback failure.
- `serve stop` and `fork stop` never signal a PID that isn't a single
  process; serve and fork logs are created `0600`.

### Changed

- `chain add --rpc` and `rpc add` move known URL keys and credential headers
  into `secrets.toml` as they write, and print what they moved.
- `signer rm` asks before deleting key material; off a terminal it needs
  `--yes`. `signer migrate --all` tries every signer and reports each
  failure.
- `rpc bench --fix --yes` writes the proposed failover lists without asking;
  `rpc discover --yes` adds every endpoint that answered. Off a terminal,
  both report instead of prompting.
- `doctor` (not `doctor --tokens`) notes labels that shadow a built-in token;
  `doctor --tokens --fix` is rejected as a usage error.
- Secrets passed as arguments (`signer add --key`, `var set NAME VALUE`)
  warn that they are visible in shell history and `ps`.
- `--json` for `addr ls`, `addr find` (exit 1 when nothing is found),
  `fork ls` and `serve status`. `rpc ls` shows header values (redacted
  unless `--reveal`).
- Uptimes read `42s`, `12m05s`, `2h03m`, `3d04h` everywhere.
- v1 spellings are accepted: `list`/`remove` aliases on every `ls`/`rm`,
  `--type private-key`/`one-password`, `var get --show`. A fork can't be
  named `list`.

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
