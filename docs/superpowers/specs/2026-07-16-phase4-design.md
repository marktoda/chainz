# Phase 4 Design: Key Security Overhaul + Terminal Experience

Date: 2026-07-16
Status: approved (design discussion in-session)

## Overview

Two independent workstreams that make chainz safe to recommend and a joy to
use:

1. **Key security**: safe key storage becomes the default, with a migration
   path for existing plaintext keys.
2. **Terminal experience**: a polished line-based CLI — streaming feedback,
   progress indication, one coherent style system, and a first-class
   `chainz shell`.

Explicitly out of scope: de-alloying, any full-screen TUI, prompt-library
swaps, and changes to any existing `--json` contract.

## Part 1 — Key Security Overhaul

### Backend default ladder

When `init` or `key add` runs without an explicit `--type`:

1. Probe the OS **keyring** once per process (write + read + delete a canary
   entry under service `chainz`). If usable, it is the default.
2. If no keyring is available (e.g. headless Linux without a secret
   service), the default is **encrypted** (existing AES-256-GCM + Argon2).
3. **Plaintext requires explicit `--type private-key`** and prints a
   one-line warning naming the migrate command.

`init` follows the same ladder for its default key, whether the user pastes
one or accepts a generated wallet.

Behavior changes (documented in changelog):

- Bare `chainz key add <name> --key <k>` now stores to the resolved safe
  default instead of plaintext. Scripts that want plaintext must say
  `--type private-key`.
- In a non-TTY context where the encrypted fallback would need a password
  prompt, the command **errors clearly** (it never hangs waiting for input).
  Non-interactive plaintext remains available explicitly.

### Keyring storage shape

Default/migrated keyring entries standardize on `service = "chainz"`,
`username = <key name>` — created without prompting. The existing free-form
service/username entry remains available in the interactive picker.

### `chainz key migrate`

```
chainz key migrate <name> [--to keyring|encrypted]
chainz key migrate --all [--to keyring|encrypted]
```

- Reads the source key via the existing `Key::private_key()` (prompting for
  a password if the source is encrypted; using `op`/keyring as needed).
- Writes to the target backend (default: the resolved ladder default),
  replaces the entry in config, saves.
- `--all` migrates every plaintext key; per-key failures are reported and
  skipped, not fatal.
- `doctor`'s plaintext warning names the exact migrate command; `doctor
  --fix` offers interactive migration when a TTY is present and skips
  silently otherwise (scripted doctor runs stay non-interactive).

### `@key` argv hardening

When `@key` expands into command **arguments** (visible in `ps` and shell
history), print a one-line stderr warning steering to `$RAW_PRIVATE_KEY`.
Env-var use stays silent. No hard break.

### Testing (Part 1)

- Unit: migrate round-trip plaintext→encrypted (mock password prompt
  exists); plaintext→keyring soft-skips without keyring access (same
  pattern as the existing keyring test); ladder resolution with keyring
  probe stubbed.
- Integration: bare `key add --key` in the sandbox lands on a non-plaintext
  backend or errors clearly (never silently plaintext, never hangs);
  non-TTY encrypted fallback error is pinned; migrate output never contains
  key material (extends the existing leak-contract tests).

## Part 2 — Terminal Experience

### Style stack

- Adopt `console` + `indicatif`; **drop `colored`** (dialoguer already
  depends on `console`, so this removes a stack rather than adding one, and
  brings TTY detection + `NO_COLOR` support).
- New `src/ui.rs` module: the single output vocabulary — `header`,
  `success`, `warn`, `fail`, one glyph set (`✓ ✗ ⚠ ▸`), one palette.
  All wizard/doctor/display call sites migrate to it.
- `--json` output and non-TTY output remain byte-identical to today.

### Streaming RPC checks

- Probing becomes an event stream: `probe(url, chain_id) -> (healthy,
  latency)` is the single deadline-bearing health check; `probe_urls`
  fans it out and yields `ProbeResult { index, healthy, latency }` events
  in completion order (data layer, no TTY knowledge). `check_urls` remains
  as a collecting wrapper. A render layer consumes the stream.
- **Collapse:** the old `Rpc` struct (URL + live provider pair),
  `resolve_rpc`, `resolve_rpcs`, and `test_rpc` are deleted;
  `create_provider` becomes private. `select_rpc` takes plain URL strings.
- **Raw-vs-expanded boundary:** pickers, doctor output, and config storage
  always use raw URLs (`${VAR}` intact); expansion happens only inside
  probe calls and exec-time resolution. This fixes the old flow, which
  displayed expanded URLs on screen and stored them into `selected_rpc`
  (baking API keys into the config).
- `add` wizard: per-RPC lines update live (`⋯ → ✓ 89ms / ✗ timeout`) under
  an elapsed-time spinner; a **~4s global deadline** replaces the 10s worst
  case; unfinished probes render as timeouts; the picker opens immediately
  after, **sorted healthy-first by latency**, latencies shown.
- `doctor` health sweep and `--fix` probing use the same stream + renderer.

### Download progress

Chainlist fetch streams the response body with an `indicatif` byte-progress
bar (content-length known; spinner fallback otherwise). Applies to first
fetch and `--refresh`.

### `chainz shell [chain]`

- Spawns `$SHELL` (fallback `sh`) with the full chain env plus
  `CHAINZ_CHAIN=<name>`; styled entry banner and exit line; exit code
  passes through. Chain resolution matches `exec`: explicit argument >
  configured default > interactive picker.
- Prompt indication: prepend `(⛓ <chain>)` to `PS1` for bash-like shells;
  README documents a one-line starship/zsh snippet reading `$CHAINZ_CHAIN`.
  No native zsh prompt injection.
- Lazy-key rule applies: entering a shell never touches key backends.

### Testing (Part 2)

- Unit: stream sorter/formatter (healthy-first ordering, latency
  formatting, deadline handling) tested without a TTY.
- Integration: `shell` env/`CHAINZ_CHAIN`/exit-code passthrough (run `sh -c`
  as the shell); non-TTY renders remain stable for existing pinned outputs.

## Sequencing

1. `ui` module + streaming checker (Part 2 core) — doctor/migrate output
   from Part 1 wants the new vocabulary.
2. Part 1 key security on top.
3. Shell + download progress + polish passes.

## Risks / trade-offs accepted

- macOS Keychain re-prompts once per new (unsigned) binary after upgrades;
  goes away when Phase 2 ships signed release binaries.
- Encrypted fallback keeps a password prompt per signing use — accepted as
  the no-keyring trade; lazy resolution keeps read-only commands
  prompt-free on every backend.
- Bare `key add --key` behavior change is breaking for scripts that
  expected plaintext; called out in changelog with the explicit flag.
