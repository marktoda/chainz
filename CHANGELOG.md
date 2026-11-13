# Changelog

## 2.0.0 - 2026-11-12

A rewrite of chainz as a local multichain workbench: `exec` is faster and
hands tools more context, and a gateway (`serve`), a dashboard (`top`) and
anvil forks (`fork`) are new. 2.0 is the release after 0.4; there is no
1.x, and the `@key` removal 0.4 announced for 1.0 happens here. Linux and
macOS only.

### Upgrading from 0.4

The first run of 2.0 migrates `config.json` into `config.toml` and
`secrets.toml` and renames the old file to `config.v1.json.bak`; the
[README](https://github.com/marktoda/chainz#upgrading-from-04) lists what
migration does. The old command spellings are gone (each is an unknown command or
flag, exit 2). "0.4" is the v0.4.0 release; builds of main after it also
had `--header` and `--clear-headers` on `add`/`update --rpc-url`, which in
2.0 are `update CHAIN --header H` and `--clear-headers` (2.0 `add` takes no
headers).

| 0.4 | 2.0 |
| --- | --- |
| `list` | `ls` |
| `show CHAIN` | `ls CHAIN` |
| `list --verbose` | `ls CHAIN`, one chain at a time |
| `list --show-secrets`, `show --show-secrets` | `ls --reveal`, `ls CHAIN --reveal` |
| `remove CHAIN` | `rm CHAIN` (asks first; `--yes` off a terminal) |
| `add --chain-id ID --name NAME --rpc-url URL` | `add ID --name NAME --url URL` |
| `add … --key NAME` | `add … --signer NAME` |
| `add … --verification-url URL --verification-api-key[-stdin]` | `add`, then `update CHAIN --explorer NAME --api-key-stdin` |
| `add --force` (replace a chain) | `rm CHAIN --yes`, then `add` |
| `update --rpc-url URL` | `update --rpc URL` |
| `update --key NAME`, `--no-key` | `update --signer NAME`, `--no-signer` |
| `update --verification-url URL` | `update --explorer NAME` (`etherscan` is built in; define others as `[explorers.NAME]` with `chainz edit`) |
| `update --verification-api-key[-stdin]` | `update --api-key-stdin` |
| `update --clear-verification` | `update --no-explorer` |
| `update --refresh` | gone (`add --refresh` remains) |
| `exec -k NAME`, `exec --key NAME` | `exec --signer NAME` |
| `@key` | `exec --expose-key` (exports `RAW_PRIVATE_KEY`) |
| `shell CHAIN` | `exec CHAIN` with no command |
| `use` with no chain (a picker) | `use CHAIN`; `use` alone prints the default |
| `key add NAME --key KEY` | `printf %s "$KEY" \| chainz signer add NAME --stdin` |
| `key add NAME --type TYPE` | `signer add NAME --backend BACKEND` (`private-key` is `plaintext`, `one-password` is `1password`) |
| `key list` | `signer ls` |
| `key remove NAME [--force]` | `signer rm NAME [--unassign]` |
| `key migrate` | `signer migrate` |
| `var get NAME [--show]` | `var ls NAME [--reveal]` |
| `var list [--show]` | `var ls [--reveal]` |
| `var list --json` (values shown) | `var ls --json --reveal` |
| `var remove NAME` | `var rm NAME` |
| `completions SHELL > FILE` | delete the file you generated (0.4's static completions still offer `list`, `key`, …), then `source <(chainz completions SHELL)` at shell startup |

What `exec` exports changed too:

- `CHAIN_NAME`, `@chainname`, `@verification_url` and `@verifier_api_key`
  still work, with a warning, and will be removed in a later release. Use
  `CHAINZ_CHAIN`, `@chain`, `@explorer` and `@explorer_key`.
  `CHAIN_NAME` now holds the short name.
- The explorer key is `ETHERSCAN_API_KEY`, which forge and cast read.
  `VERIFIER_URL` and `VERIFIER_API_KEY` are exported only for explorers
  other than Etherscan.
- A value that isn't set is not exported (0.4 exported `UNDEFINED`).
- New: `ETH_FROM` (the signer's address), `ETH_KEYSTORE` (keystore
  signers) and `CHAINZ_CHAIN` (now from `exec` too, not only `shell`).

### Added

See the [README](https://github.com/marktoda/chainz#readme) for each.

- `exec` replaces itself with the command (`execvp`), adding about 3 ms.
  Labels (`@usdc`) and signer names (`@funder`) are tokens, `@name:chain`
  reads another chain, `--rpc NAME|URL` picks an endpoint for one run, and
  `--explain` shows the context without running anything. `x` is short for
  `exec`; `env` prints the context as shell exports.
- Named endpoints per chain: `update --rpc`, `--add-rpc`, `--rm-rpc` and
  `--failover`, and per-endpoint HTTP headers (`--header`); `ls CHAIN`
  shows each endpoint's latest health.
- `doctor CHAIN` probes, ranks and tags a chain's endpoints; `--fix`
  activates the best, and `--discover` adds public ones from chainlist.
- `edit [CHAIN]` opens the config, or one chain's table, in `$EDITOR` and
  validates it on save.
- Explorers are defined once and attached to chains by name; `etherscan`
  (Etherscan's v2 API) is built in.
- The first signer attaches to every chain without one, and `add` attaches
  the only signer and the `etherscan` explorer.
- An address book: `label add/ls/rm`, built-in major-token labels, `label
  0x…` to look an address up, `… | chainz label` to annotate any output,
  and `label import` for Foundry broadcast output.
- `serve`: a local RPC gateway with approved-only failover, rate-limit
  smoothing, a request inspector and metrics. `exec` uses it while it runs.
- `top`: a live dashboard of chain heads, endpoint health, signer balances
  and gateway traffic.
- `fork`: an anvil fork in one command, funded and usable as a chain name.
- Foundry keystore signers (`signer add --backend keystore`).
- Dynamic shell completions for chains, endpoints, signers, labels,
  variables and `@tokens`.
- `--json` on every listing.

### Changed

- The config is TOML, kept in your order with your comments, and secrets
  live in `secrets.toml` as `${NAME}` variables. Reads take no lock, so
  parallel `exec`s no longer wait on each other.
- `${NAME}` resolves in one pass: built-ins (`${CHAIN_ID}`, `${CHAIN}`),
  then the environment, then `secrets.toml`. An unresolved name is an
  error.
- Writing a URL with a known API key (Infura, Alchemy) or a credential
  header moves the secret into a variable, and says so.
- Every removal (`rm`, `signer rm`, `label rm`, `var rm`) asks on a
  terminal and needs `--yes` off one; `rm` names what goes with the chain.
  0.4 removed without asking.
- `exec` says on stderr when it falls back to the default chain.
- `--reveal` is the only flag that shows secrets (was `--show-secrets` and
  `--show`). Only the credential-shaped parts of a URL or header are
  masked, and `var ls --json` is masked too.
- Errors print as `error: …`. Usage errors, including a missing argument
  or `--yes` off a terminal, exit 2; `exec` exits 127 when the command doesn't exist
  and 126 when it can't run. See the README for every exit code.
- Commands that write say what changed (`active endpoint: alchemy →
  infura`), or "No changes".

### Removed

- Windows support.
- `@key`; use `--expose-key`.
- The 0.4 command and flag spellings above, with no hidden aliases.

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
