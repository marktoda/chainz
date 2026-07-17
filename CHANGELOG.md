# Changelog

## 0.4.0 - Unreleased

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
- Documented the pre-1.0 Rust API policy; CLI and config compatibility remain
  the supported interfaces.

### Breaking changes

- Bare `key add --key` no longer silently creates plaintext storage.
- Variable values and verification keys are redacted by default.
- Duplicate chain IDs and colliding aliases are rejected.
- `@key` is deprecated and scheduled for removal in 1.0; use `--expose-key`.
