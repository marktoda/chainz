# Chainz

A CLI tool for managing EVM chain configurations

## Features

- Interactive chain discovery and configuration
- Dynamic RPC health checking and failover
- Private key management
- Multiple RPC support per chain
- Environment variable interpolation

## Installation

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

# execute a command for a given chain
chainz exec ethereum -- cast block-number
21532741

# Switch to a chain
chainz use ethereum
source .env
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
Testing all RPCs...
✓ ethereum: 2/3 RPCs working
✓ optimism: 1/1 RPCs working
✗ arbitrum: 0/1 RPCs working

? Select chain to update
? What would you like to update?
> RPC URL
  Key
  Verification API Key
```

### Using Chains

Switch to a chain and set up environment:

```bash
> chainz use ethereum
Chain: ethereum
├─ ID: 1
├─ RPC: https://rpc.com
└─ Wallet: 0x123...789

> source .env
> echo $ETH_RPC_URL
https://rpc.com
```

Execute commands with chain context:

```bash
> chainz exec 1 -- cast block-number
21532741

> chainz exec ethereum -- cast balance @wallet
1.5 ETH

> chainz exec 10 -- forge script Deploy
```

### Managing Keys

Add and manage private keys:

```bash
> chainz key add deployer
Enter private key: ****
Added key 'deployer'

> chainz key list
Stored keys:
- default: 0x123...789
- deployer: 0xabc...def
```

### Custom Variables

Set and use custom variables:

```bash
> chainz var set ALCHEMY_KEY abc123
> chainz var set ETHERSCAN_KEY def456

> chainz var list
ALCHEMY_KEY=abc123
ETHERSCAN_KEY=def456
```

## Configuration

### Config File

Configs are stored at `$HOME/.chainz.json`:

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
      "type": "PrivateKey",
      "value": "0x123..."
    }
  }
}
```

### TODO:

- test onepassword key type
- fix keychain type
- clean up globalvariables structuring
