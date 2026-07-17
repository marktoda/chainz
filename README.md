# Chainz

A CLI tool for managing EVM chain configurations

## Features

- Interactive chain discovery and configuration (backed by [chainlist](https://chainid.network), cached locally)
- Short chain names with aliases and prefix matching (`chainz exec eth`)
- RPC health checking (`chainz doctor`, with `--fix` failover to healthy RPCs)
- Safe-by-default private key management (OS keyring, encrypted, 1Password, explicit plaintext)
- Multiple RPC support per chain and a configurable default chain
- RPC-only chain configurations when signing is not needed
- Environment variable interpolation
- Command execution with chain-specific variable expansion
- Shell completions and `--json` output for scripting

## Installation

Requires Rust 1.88+.

```bash
git clone https://github.com/marktoda/chainz.git
cd chainz
cargo install --path .
```

## Quick Start

```bash
# Initialize with interactive wizard
chainz init

# Add a new chain
chainz add

# List configured chains
chainz list

# Inspect one chain in detail
chainz show ethereum

# Removal requires an exact primary name or chain ID
chainz remove ethereum

# Execute a command for a given chain (prefix matching works)
chainz exec eth -- cast block-number
21532741

# Set a default chain, then omit it
chainz use ethereum
chainz exec -- cast block-number

# Check config and RPC health; switch dead RPCs to healthy ones
chainz doctor --fix

# Open a subshell with chain environment (prompt shows the chain)
chainz shell ethereum

# Install shell completions (zsh example)
chainz completions zsh > ~/.zfunc/_chainz
```

## Usage

### Adding Chains

Chainz provides an interactive wizard for adding chains:

```bash
> chainz add
Chain Selection
══════════════════════════════════════════════════════
? Type to search and select a chain
> ethereum (1)
  optimism (10)
  arbitrum (42161)
  polygon (137)
  ...

RPC Configuration
══════════════════════════════════════════════════════
Testing RPCs...
13 of 18 RPCs healthy
? Select an RPC
> RPC 1 · https://…alchemy.com/… (120ms)
  RPC 2 · https://…llamarpc.com (184ms)

Key Configuration
══════════════════════════════════════════════════════
? Select a key
> default (0x123...789)
  deployer (0xabc...def)
  Add new key

Chain added: ethereum (ChainId: 1)
```

### Managing Chains

`list` is a compact index; the active RPC is redacted so credential-bearing
URLs are safe to display. The `*` marks the default chain:

```bash
> chainz list
  CHAIN      ID  RPC                             KEY
* ethereum    1  https://…llamarpc.com           default
  optimism   10  https://…optimism.io            —
* default chain
```

Use `show` for one chain's details, or `list --verbose` for all details.
Both support the explicit `--show-secrets` escape hatch:

```bash
chainz show ethereum
chainz show ethereum --json
chainz list --verbose
```

Update interactively, or target a chain and make a direct change:

```bash
> chainz update
? Select chain to update
? What would you like to update?
> RPC URL
  Key
  Verification
  Rename
  Save and finish

> chainz update ethereum --name eth --no-key
> chainz update eth --rpc-url https://eth.llamarpc.com
```

Chains may omit a key entirely. This is useful for read-only RPC commands;
`@wallet`, `@key`, and `--expose-key` fail with a clear message until a key is
attached.

### Executing Commands

Execute commands with chain-specific variables expanded. Chains can be
referenced by name, alias, unambiguous prefix, or chain ID:

```bash
> chainz exec 1 -- cast block-number
21532741

> chainz exec eth -- cast balance @wallet
1.5 ETH

> chainz exec 10 -- forge script Deploy
```

Set a default chain to omit the chain argument entirely:

```bash
> chainz use ethereum
Default chain set to 'ethereum'
> chainz exec -- cast block-number
21532741
```

Available expansions:
- `@wallet` — Wallet address
- `@rpc` — RPC URL
- `@chainid` — Chain ID
- `@chainname` — Chain name
- `@key` — Private key

`@key` is retained for compatibility but exposes the private key in child
process arguments. Prefer env-only exposure when a tool accepts environment
variables:

```bash
chainz exec ethereum --expose-key -- sh -c 'PRIVATE_KEY="$RAW_PRIVATE_KEY" forge script Deploy'
```

Chainz resolves a key once and injects only what was requested: using
`@wallet` does not put `RAW_PRIVATE_KEY` in the child environment. If a shell
expands `$RAW_PRIVATE_KEY` into a downstream command argument, that downstream
argument can still be visible to other processes.

Override the key for a single command:

```bash
> chainz exec ethereum -k deployer -- forge script Deploy
```

### Chain Shells

`chainz shell [chain]` opens your `$SHELL` with the chain's environment
(`ETH_RPC_URL`, `CHAIN_ID`, `CHAIN_NAME`, `VERIFIER_*`, `CHAINZ_CHAIN`) —
private keys are never injected. Shells that honor an inherited `PS1` get
a `(⛓ ethereum)` prefix, but interactive shells often reset `PS1` from rc
files, so this isn't reliable everywhere. For a prompt that always works
(zsh, starship, or a customized bash), key off `CHAINZ_CHAIN` instead, e.g.:

```toml
# starship.toml
[env_var.CHAINZ_CHAIN]
format = "\\(⛓ $env_value\\) "
```

### Managing Keys

Add and manage private keys. Without `--type`, Chainz uses the OS keyring when
available and otherwise uses password-encrypted storage. Plaintext requires an
explicit `--type private-key`:

```bash
> chainz key add deployer
Enter private key: ****
Added key 'deployer'

> chainz key list
Stored keys:
- default (0x123...789, keyring)
- deployer (0xabc...def, encrypted)
```

Read key material from stdin in scripts so it does not enter shell history or
process listings:

```bash
> printf '%s\n' "$PRIVATE_KEY" | chainz key add ci-key --stdin
Added key 'ci-key'
```

On a headless machine without an OS keyring, the safe encrypted fallback needs
an interactive password prompt and exits clearly instead of hanging. Scripts
that deliberately accept plaintext storage can opt in explicitly:

```bash
printf '%s\n' "$PRIVATE_KEY" | chainz key add ci-key --stdin --type private-key
```

Migrate one key, or every plaintext key, into safe storage:

```bash
chainz key migrate default
chainz key migrate --all --to encrypted
```

Removing an attached key is blocked by default. Use `--force` to detach it
from every referencing chain before removal.

### Health Checks

`chainz doctor` checks key storage, key references, and RPC connectivity for
every chain (concurrently). With `--fix`, any dead selected RPC is switched to
a healthy alternative from that chain's RPC list. Exits nonzero when failures
are found, so it can gate scripts. Interactive RPC tests and `doctor` probes
time out after 4 seconds per endpoint; results stream in live and pickers
list healthy endpoints fastest-first:

```bash
> chainz doctor --fix
Configuration
  ✓ configuration invariants hold

Keys
  ⚠ 'default' is stored as a plaintext private key — migrate with `chainz key migrate default`

Key references
  ✓ all chains reference existing keys

RPC health
  ✓ ethereum (https://eth.llamarpc.com)
  ✗ optimism (https://dead.example.com)

Fixing RPCs
  ✓ optimism: switched to https://mainnet.optimism.io
```

### Scripting

`list`, `show`, and `key list` support `--json`. Credential-bearing URLs are redacted
and key material is never included by default. `list --show-secrets` is the
explicit escape hatch for trusted interactive use:

```bash
> chainz list --json | jq '.[].name'
> chainz show ethereum --json | jq '.selected_rpc'
> chainz key list --json | jq '.[] | {name, address}'
```

### Custom Variables

Set and use custom variables for RPC URL interpolation. Stdin avoids placing
values in shell history; values are redacted from ordinary output:

```bash
> printf '%s\n' "$ALCHEMY_KEY" | chainz var set ALCHEMY_KEY --stdin
> printf '%s\n' "$ETHERSCAN_KEY" | chainz var set ETHERSCAN_KEY --stdin

> chainz var list
Variables:
  ALCHEMY_KEY = [REDACTED]
  ETHERSCAN_KEY = [REDACTED]

> chainz var get ALCHEMY_KEY --show
ALCHEMY_KEY = abc123
```

`chainz var list --json` is an explicitly revealing machine-readable form;
use it only in trusted scripting contexts.

## Configuration

### Config File

Configs are stored at `$XDG_CONFIG_HOME/chainz/config.json` (defaulting to
`~/.config/chainz/config.json`) with owner-only permissions (`0600`), since
the file can contain private keys. A legacy `~/.chainz.json` is migrated to
the new location automatically on first run.

```json
{
  "chains": [
    {
      "name": "ethereum",
      "aliases": ["Ethereum Mainnet"],
      "chain_id": 1,
      "rpc_urls": [
        "https://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}",
        "https://eth.llamarpc.com"
      ],
      "selected_rpc": "https://eth.llamarpc.com",
      "verification_api_key": "abc123",
      "key_name": "default"
    }
  ],
  "variables": {
    "ALCHEMY_KEY": "def456"
  },
  "keys": {
    "default": {
      "address": "0x0123456789abcdef0123456789abcdef01234567",
      "type": "EncryptedKey",
      "value": "<base64>",
      "nonce": "<base64>",
      "salt": "<base64>",
      "version": 1,
      "kdf_memory_kib": 19456,
      "kdf_iterations": 2,
      "kdf_parallelism": 1
    }
  },
  "default_chain": "ethereum"
}
```

The chainlist used by `chainz add` is cached at `~/.cache/chainz/chains.json`
for 24 hours; pass `--refresh` to `add`/`update` to force a re-download.

## License

MIT — see [LICENSE](LICENSE).
