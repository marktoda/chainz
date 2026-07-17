# Chainz

A CLI tool for managing EVM chain configurations

## Features

- Interactive chain discovery and configuration (backed by [chainlist](https://chainid.network), cached locally)
- Short chain names with aliases and prefix matching (`chainz exec eth`)
- RPC health checking (`chainz doctor`, with `--fix` failover to healthy RPCs)
- Private key management with safe keyring/encrypted defaults and migration
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
> RPC 1  120ms
  RPC 2  184ms

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

Use `show` for one chain's details, or `list --verbose` for all details:

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
`@wallet` and `@key` fail with a clear message until a key is attached.

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

Add and manage private keys. With no `--type`, Chainz uses the OS keyring
when available and otherwise offers encrypted storage. Plaintext storage
requires the explicit `--type private-key` option and prints a warning:

```bash
> chainz key add deployer
Enter private key: ****
Added key 'deployer'

> chainz key list
Stored keys:
- default (0x123...789)
- deployer (0xabc...def)
```

For scripting, `key add` is fully non-interactive with `--key` when the OS
keyring is available. Choose `--type encrypted` when a terminal password
prompt is acceptable:

```bash
> chainz key add ci-key --key 0xac09...ff80
Added key 'ci-key'
```

Move legacy plaintext keys to safe storage without changing chain references:

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
Keys
  ⚠ 'default' is stored as plaintext — run `chainz key migrate default`

Key references
  ✓ all chains reference existing keys

RPC health
  ✓ ethereum (https://eth.llamarpc.com)
  ✗ optimism (https://dead.example.com)

Fixing RPCs
  ✓ optimism: switched to https://mainnet.optimism.io
```

### Scripting

`list`, `show`, and `key list` support `--json` (key material is never included):

```bash
> chainz list --json | jq '.[].name'
> chainz show ethereum --json | jq '.selected_rpc'
> chainz key list --json | jq '.[] | {name, address}'
```

### Custom Variables

Set and use custom variables for RPC URL interpolation:

```bash
> chainz var set ALCHEMY_KEY abc123
> chainz var set ETHERSCAN_KEY def456

> chainz var list
Variables:
  ALCHEMY_KEY
  ETHERSCAN_KEY
```

Human-readable `set` and `list` output does not echo values. Use
`chainz var get NAME` for one value or the explicitly revealing
`chainz var list --json` form in trusted scripting contexts.

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
      "type": "EncryptedKey",
      "value": "<base64>",
      "nonce": "<base64>",
      "salt": "<base64>"
    }
  },
  "default_chain": "ethereum"
}
```

The chainlist used by `chainz add` is cached at `~/.cache/chainz/chains.json`
for 24 hours; pass `--refresh` to `add`/`update` to force a re-download.

## License

MIT — see [LICENSE](LICENSE).
