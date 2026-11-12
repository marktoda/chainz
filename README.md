# Chainz

A local multichain workbench for EVM development. Chainz keeps one source of
truth for your chains, endpoints, signers and addresses, and hands that
context to the tools you already use:

```bash
chainz exec base -- cast balance @wallet
chainz exec arb  -- forge script Deploy --broadcast
```

Around that sit three optional extras: a local RPC gateway (`serve`), a live
dashboard (`top`) and disposable anvil forks (`fork`).

> **Upgrading from 0.4?** Your `config.json` is migrated automatically the
> first time 2.0 runs. See [Upgrading from 0.4](#upgrading-from-04).

## Install

Linux and macOS. Needs Rust 1.90 or later:

```bash
cargo install chainz --locked
```

or with Nix: `nix profile install github:marktoda/chainz`. Forks also need
[Foundry](https://getfoundry.sh)'s `anvil` on `PATH`.

## Quick start

With nothing configured, `chainz` prints:

```
Quick start:
  chainz signer add deployer               # store a key (OS keyring by default)
  chainz add base                          # add a chain from chainlist; the signer attaches
  chainz exec base -- cast block-number    # run a tool with the chain's context
On a terminal, `chainz init` walks through the same steps.
```

`signer add` asks for the key on a terminal; in a script, pipe it:
`printf %s "$KEY" | chainz signer add deployer --stdin`. The first signer
attaches to every chain that has none, and `add` attaches the only signer
and, once it is configured, the `etherscan` explorer. More setup:

```bash
printf %s "$KEY" | chainz var set INFURA_KEY --stdin
chainz update base --rpc 'https://base-mainnet.infura.io/v3/${INFURA_KEY}'
printf %s "$ETHERSCAN_KEY" | chainz update base --explorer etherscan --api-key-stdin
chainz use base                 # the default chain, for `chainz exec -- …`
chainz ls                       # every chain
chainz ls base                  # one chain, with each endpoint's health
chainz doctor                   # check everything
```

Then load [completions](#completions).

A chain can be named by its short name, an alias, its chain ID, an
unambiguous prefix or a live fork's name. Well-known long names
(`ethereum`, `arbitrum`, `optimism`…) find the configured chain with that
ID, so `arbitrum` means your `arb`. `rm` takes only an exact name or ID.

## exec: run a tool with chain context

`chainz exec CHAIN -- CMD…` (or `chainz x`) resolves the chain, exports its
context, rewrites `@tokens` in the arguments, then **replaces itself** with
the command (`execvp`): signals, exit status and TTYs behave as if you had
run the command directly. Chainz adds about 3 ms.

| Variable | Value |
| --- | --- |
| `ETH_RPC_URL` | the active endpoint, or the gateway URL while `serve` runs |
| `ETH_RPC_HEADERS` | endpoint headers (direct connections only) |
| `CHAIN_ID`, `CHAINZ_CHAIN` | chain ID and short name |
| `ETHERSCAN_API_KEY` | the chain's explorer key |
| `VERIFIER_URL`, `VERIFIER_API_KEY` | for explorers other than Etherscan |
| `ETH_FROM`, `WALLET_ADDRESS` | the signer's cached address |
| `ETH_KEYSTORE` | the keystore path, for keystore signers |
| `RAW_PRIVATE_KEY` | only with `--expose-key` |

| Token | Expands to |
| --- | --- |
| `@rpc`, `@chainid`, `@chain` | endpoint URL, chain ID, short name |
| `@wallet`, `@funder` | the chain's signer's address, or a named signer's |
| `@explorer`, `@explorer_key` | verifier URL and key |
| `@usdc`, `@myrouter` | a label on this chain |
| `@usdc:arb` | a label on another chain |
| `@@` | a literal `@` |

An argument that is only an unknown token is an error with a suggestion
(`@udsc`: did you mean `@usdc`?). Inside a longer argument (`foo@bar.com`,
`@uniswap/v3-core`) an unknown token is left alone.

```bash
chainz exec base -- cast call @usdc 'balanceOf(address)(uint256)' @wallet
chainz exec eth --signer funder -- cast send @wallet --value 1ether
chainz exec op --rpc llama -- cast block-number                  # another endpoint
chainz exec op --rpc http://127.0.0.1:8545 -- cast block-number  # a URL, this run only
chainz exec base --explain -- forge script Deploy                # show, don't run
chainz x base                       # no command: a subshell with the context
eval "$(chainz env base)"           # or export it into this shell
```

Only `--expose-key` unlocks a key; everything else uses the signer's cached
address.

## Chains and endpoints

`update` changes any of a chain's settings in one atomic write. With no
flags on a terminal it opens a menu.

```bash
chainz update eth --rpc infura                      # activate an endpoint by name
chainz update eth --rpc https://gw/rpc/1 --header 'x-key: ${GW}'   # add, check, activate
chainz update eth --add-rpc llama=https://eth.llamarpc.com          # add a spare
chainz update eth --rpc llama --rm-rpc infura       # replace the active endpoint
chainz update eth --failover llama,ankr             # what the gateway may fall back to
chainz update eth --signer deployer --explorer etherscan
chainz update eth --name ethereum --alias main
chainz edit eth                                     # the chain's table in $EDITOR
```

`doctor` checks files, config, credentials, signers and every chain's
active endpoint, and exits 1 while a problem is left. Given a chain, it
probes, ranks and tags all of that chain's endpoints.

```bash
chainz doctor --fix            # fix file modes, move literal credentials into variables, swap dead endpoints
chainz doctor base --fix       # activate the best endpoint, propose a failover list
chainz doctor base --discover  # add public endpoints from chainlist that answer
```

## Labels

```bash
chainz label add myrouter 0x… --chain base
chainz label add my-safe 0x… --all-chains
chainz label ls --builtin                  # yours, plus the built-in tokens
chainz label 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913   # what is this?
cast logs … | chainz label --chain base    # 0x… → usdc(0x…)
chainz label import .                      # contracts from Foundry's broadcast/
```

Built-in labels cover major tokens (`weth`, `usdc`, `usdce`, `usdt`, `wbtc`,
`dai`) where they exist; your own labels take precedence. `label import`
names each contract a `forge script --broadcast` run deployed, on the chain
it ran on; `--dry-run` shows the plan.

## Signers

| Backend | Key lives in |
| --- | --- |
| `keyring` (default) | the OS keyring (`encrypted` where there is none) |
| `encrypted` | `secrets.toml`, password-encrypted |
| `1password` | a 1Password item (`--vault`, `--item`) |
| `keystore` | a Foundry keystore, unlocked by cast or forge, never by chainz |
| `plaintext` | `secrets.toml`, only if you ask for it |

A Foundry keystore made by `cast wallet new` has no address field; pass
`--address "$(cast wallet address --keystore PATH)"`. `signer migrate`
moves plaintext keys into safe storage. Give keys and variable values on
stdin (`--stdin`): an argument works but warns, since it shows in shell
history and `ps`.

## serve: local RPC gateway

```bash
chainz serve            # foreground, with a live summary line
chainz serve --detach   # background; then `serve status`, `serve stop`
chainz serve log -f --chain base --method eth_call
```

A loopback proxy on `127.0.0.1:18545`. While it runs, `exec` goes through
it (`--direct` bypasses it), and you get:

- **Failover you approve.** Reads fall back only to the endpoints in the
  chain's `failover` list. Transactions are never retried or sent
  elsewhere.
- **Rate-limit smoothing** per endpoint, waiting out short `Retry-After`s.
- **An inspector**: every request, with addresses shown as labels
  (`eth_call → @usdc.balanceOf`). `--log FILE` appends JSONL.
- **Metrics**: requests, errors, p50 and p95 latency, 429s and failovers per
  chain, endpoint and method.

Each start mints a random token for its URLs
(`http://127.0.0.1:18545/<token>/base`); endpoint secrets and headers never
leave the gateway. Config changes apply live.

## top: live dashboard

```bash
chainz top                 # interactive
chainz top --once --json   # one snapshot, for scripts
```

Tabs for chains (head, block age, base fee, active endpoint and latency),
one chain's endpoints (health and capability tags; `s` activates one),
signer balances, and gateway traffic while `serve` runs.

## fork: disposable anvil forks

```bash
$ chainz fork base --fund usdc=10000,weth=5
base-fork  http://127.0.0.1:8601  block 38,112,904  chain 8453

$ chainz exec base-fork -- cast send @usdc 'transfer(address,uint256)' @myrouter 1e6 --unlocked
$ chainz fork ls
$ chainz fork stop base-fork        # or Ctrl-C, or --all
```

- A fork is a chain name while it runs, by name only: `exec 8453` is always
  the real chain.
- Anvil reads state through one pinned endpoint: the active one, or
  `--rpc NAME|URL`. `--block N` needs an archive endpoint.
- Your signer gets ETH by default; `--fund` adds tokens, `--no-fund` skips
  it.
- A fork shares its parent's chain ID, so a signature made for it would be
  valid on the real chain. Chainz refuses `--expose-key` on forks: send
  with `--unlocked`, or give the fork its own ID with `--chain-id N`.
- Anvil dies with chainz, even on SIGKILL (Linux).

## Configuration

| Path | Holds |
| --- | --- |
| `$XDG_CONFIG_HOME/chainz/config.toml` | chains, endpoints, explorers, signer references, labels |
| `$XDG_CONFIG_HOME/chainz/secrets.toml` | variable values and plaintext keys (mode `0600`) |
| `$XDG_STATE_HOME/chainz/` | endpoint health, gateway and fork logs |
| `$XDG_RUNTIME_DIR/chainz/` | the running gateway and forks |
| `$XDG_CACHE_HOME/chainz/` | the chainlist download |

`$XDG_CONFIG_HOME` is `~/.config` when unset. `chainz doctor` lists every
path. The config is plain TOML that keeps your comments and ordering;
`chainz edit` validates it on save:

```toml
version = 2
default = "eth"

[explorers.etherscan]
api = "https://api.etherscan.io/v2/api?chainid=${CHAIN_ID}"
key = "${ETHERSCAN_API_KEY}"

[chains.eth]
id = 1
aliases = ["ethereum", "mainnet"]
signer = "default"
explorer = "etherscan"
rpc = "infura"
failover = ["llama"]
[chains.eth.rpcs]
infura = "https://mainnet.infura.io/v3/${INFURA_KEY}"
gw = { url = "https://gw.example.org/rpc/1", headers = { x-secret = "${GW_SECRET}" } }
llama = "https://eth.llamarpc.com"

[signers.default]
backend = "keyring"
address = "0x…"

[addresses.myrouter]
base = "0x…"
```

`${NAME}` resolves from the built-ins (`${CHAIN_ID}`, `${CHAIN}`), then the
environment, then `secrets.toml` (`chainz var set`). An unresolved name is
an error. Inside a chainz context (an `exec` subshell, or after
`eval "$(chainz env …)"`) the variables `exec` exports are not read from
the environment, so a nested `chainz exec` never inherits the outer chain's
keys. Writing a URL with a known API key (Infura, Alchemy) or a credential
header moves the secret into a variable and says so; `doctor --fix` does
the same for what is already there.

## Scripting

- Off a terminal chainz never prompts: it fails and names the flag to pass.
  Removals (`rm`, `signer rm`, `label rm`, `var rm`) ask on a terminal and
  need `--yes` (`-y`) off one.
- Every listing takes `--json`.
- Secrets are masked unless you pass `--reveal`, and only the parts that
  could be secret: `https://mainnet.infura.io/v3/[REDACTED]`.
- Exit codes: 0 on success; 1 for an error, or a problem the command
  reports (`doctor`); 2 for a usage error, including a missing argument off
  a terminal; from `exec`, the command's own status, 126 when it can't run
  and 127 when it doesn't exist; 130 for a cancelled prompt. A foreground
  `fork` exits 130 after Ctrl-C and 143 after `fork stop`.

## Completions

Completions are dynamic: chain names, endpoints, signers, labels, variables
and `exec`'s `@tokens` come from your config as you type. Generate the
loader at shell startup, so it follows upgrades:

```bash
source <(chainz completions zsh)     # ~/.zshrc, after compinit
source <(chainz completions bash)    # ~/.bashrc
chainz completions fish | source     # ~/.config/fish/config.fish
```

`chainz completions --help` covers elvish and PowerShell.

## Upgrading from 0.4

2.0 is the release after 0.4; there is no 1.x. The first time it runs
(`chainz ls` is a harmless way to trigger it), it migrates `config.json`
once:

- Chains get short names (`eth`, `op`, `arb`, `uni`, …); the old names stay
  as aliases, so `chainz exec "OP Mainnet"` still works.
- Every endpoint is kept and named after its host. The failover list starts
  empty.
- Known URL credentials (Infura, Alchemy) and verifier keys move into
  variables in `secrets.toml`; anything else is reported.
- Signers keep their backend and address; keyring entries need no
  re-entry.
- `config.json` is renamed to `config.v1.json.bak` once the new files are
  written, so you can go back.

Several commands and flags were renamed, and the old spellings are gone.
The [CHANGELOG](CHANGELOG.md#upgrading-from-04) maps each one.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) and
[docs/architecture.md](docs/architecture.md).

## License

MIT
