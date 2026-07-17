# Chainz

A CLI tool for managing EVM chain configurations

## Features

- Interactive chain discovery and configuration (backed by [chainlist](https://chainid.network), cached locally)
- Short chain names with aliases and prefix matching (`chainz exec eth`)
- RPC health checking (`chainz doctor`, with `--fix` failover to healthy RPCs)
- Private key management (plaintext, encrypted, 1Password, keyring)
- Multiple RPC support per chain and a configurable default chain
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

# Remove a chain
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
✓ https://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}
✓ https://eth.llamarpc.com
✗ https://mainnet.infura.io/v3/${INFURA_KEY}

Key Configuration
══════════════════════════════════════════════════════
? Select a key
> default (0x123...789)
  deployer (0xabc...def)
  Add new key

Chain added: ethereum (ChainId: 1)
```

### Managing Chains

List configured chains and their status:

```bash
> chainz list
Chain: ethereum
├─ ID: 1
├─ Active RPC: https://eth-mainnet.g.alchemy.com/v2/...
├─ Verification Key: 0xabc...def
└─ Key Name: default

Chain: optimism
├─ ID: 10
├─ Active RPC: https://opt-mainnet.g.alchemy.com/v2/...
├─ Verification Key: None
└─ Key Name: deployer
```

Update chain configuration:

```bash
> chainz update
? Select chain to update
? What would you like to update?
> RPC URL
  Key
  Verification API Key
```

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
private keys are never injected. Bash prompts get a `(⛓ ethereum)` prefix
automatically; for zsh/starship, add e.g.:

```toml
# starship.toml
[env_var.CHAINZ_CHAIN]
format = "\\(⛓ $env_value\\) "
```

### Managing Keys

Add and manage private keys:

```bash
> chainz key add deployer
? Select key type
> Private Key
  Encrypted Key
  One Password
  Keyring
Enter private key: ****
Added key 'deployer'

> chainz key list
Stored keys:
- default: 0x123...789
- deployer: 0xabc...def
```

For scripting, `key add` is fully non-interactive when the key is passed on
the command line (`--key` implies `--type private-key`):

```bash
> chainz key add ci-key --key 0xac09...ff80
Added key 'ci-key'
```

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
  ⚠ 'default' is stored as a plaintext private key — consider re-adding it with --type encrypted or --type keyring

Key references
  ✓ all chains reference existing keys

RPC health
  ✓ ethereum (https://eth.llamarpc.com)
  ✗ optimism (https://dead.example.com)

Fixing RPCs
  ✓ optimism: switched to https://mainnet.optimism.io
```

### Scripting

`list` and `key list` support `--json` (key material is never included):

```bash
> chainz list --json | jq '.[].name'
> chainz key list --json | jq '.[] | {name, address}'
```

### Custom Variables

Set and use custom variables for RPC URL interpolation:

```bash
> chainz var set ALCHEMY_KEY abc123
> chainz var set ETHERSCAN_KEY def456

> chainz var list
Variables:
  ALCHEMY_KEY = abc123
  ETHERSCAN_KEY = def456
```

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
