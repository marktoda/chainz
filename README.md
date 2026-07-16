# Chainz

A CLI tool for managing EVM chain configurations

## Features

- Interactive chain discovery and configuration (backed by [chainlist](https://chainid.network))
- RPC health checking when adding or updating chains
- Private key management (plaintext, encrypted, 1Password, keyring)
- Multiple RPC support per chain
- Environment variable interpolation
- Command execution with chain-specific variable expansion

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

# Execute a command for a given chain
chainz exec ethereum -- cast block-number
21532741

# Open a subshell with chain environment
chainz exec ethereum -- bash
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

Execute commands with chain-specific variables expanded:

```bash
> chainz exec 1 -- cast block-number
21532741

> chainz exec ethereum -- cast balance @wallet
1.5 ETH

> chainz exec 10 -- forge script Deploy
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

Open a subshell with all chain variables in the environment:

```bash
> chainz exec ethereum -- bash
$ echo $ETH_RPC_URL
https://rpc.com
$ exit
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

Configs are stored at `$HOME/.chainz.json` with owner-only permissions
(`0600`), since the file can contain private keys:

```json
{
  "chains": [
    {
      "name": "ethereum",
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
  }
}
```

## License

MIT — see [LICENSE](LICENSE).
