# Chainz

A simple CLI tool for managing chain configurations

## Installation

```bash
git clone https://github.com/marktoda/chainz.git
cd chainz
cargo install --path .
```

## Usage

### Add a new chain
```bash
> chainz add -h
chainz-add 0.1.0
Add a new chain

USAGE:
    chainz add [OPTIONS] --name <name> --rpc-url <rpc-url> --verification-api-key <verification-api-key>

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information

OPTIONS:
    -n, --name <name>
    -p, --private-key <private-key>
    -r, --rpc-url <rpc-url>
    -v, --verification-api-key <verification-api-key>

> chainz add --name mainnet -r https://mainnet.infura.io/v3/{INFURA_KEY} -v {ETHERSCAN_API_KEY}
mainnet (ChainId: 1)
Wallet: 0x0000000000000000000000000000000000000000 (Balance: 80072519714480901)
```

### List chains
```bash
> chainz list -h
chainz-list 0.1.0
List all chains

USAGE:
    chainz list

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information


> chainz list
optimism (ChainId: 10)
Wallet: 0x0000000000000000000000000000000000000000 (Balance: 1336192881671202)

arbitrum (ChainId: 42161)
Wallet: 0x0000000000000000000000000000000000000000 (Balance: 10959312699843000)
```

### Switch to a chain
```bash
> chainz use -h
chainz-use 0.1.0
Use a chain by name or chainid. Writes to a local .env which can be sourced

USAGE:
    chainz use [FLAGS] <name-or-id>

FLAGS:
    -h, --help       Prints help information
    -p, --print
    -V, --version    Prints version information

ARGS:
    <name-or-id>

> chainz use mainnet && source .env

> echo $FOUNDRY_RPC_URL
https://mainnet.infura.io/v3/{INFURA_KEY}
> echo $FOUNDRY_PRIVATE_KEY
{PRIVATE_KEY}
> echo $FOUNDRY_VERIFICATION_API_KEY
{ETHERSCAN_API_KEY}
```

### Set global defaults
```bash
> chainz set --default-private-key {key}
> chainz set --env-prefix FOUNDRY
```

## TODOs
- Import config from 1password etc.
