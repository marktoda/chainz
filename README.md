# Chainz

A local multichain workbench for EVM development. Chainz keeps one source of
truth for your chains, endpoints, signers and addresses, and hands that
context to the tools you already use:

```bash
chainz exec base -- cast balance @wallet
chainz exec arb  -- forge script Deploy --broadcast
chainz exec 1    -- bun tools/cli/rl.ts quote
```

Around that hot path sit three optional surfaces: a local RPC gateway
(`serve`), a live dashboard (`top`), and disposable anvil forks (`fork`).

> **Upgrading from 0.x?** v2 migrates `config.json` automatically on first
> run. See [Migrating from v1](#migrating-from-v1).

## Install

Requires Rust 1.90+. Linux and macOS.

```bash
cargo install chainz --locked
```

Forks need [Foundry](https://getfoundry.sh)'s `anvil` on `PATH`.

### Completions

Completions are dynamic: the shell asks chainz as you type, so chain names
and aliases, live forks, endpoints (of the chain on the line), signers,
labels, variables and `exec`'s `@tokens` (`@usdc`, `@usdc:arb`) come from
your current config. Load the loader at shell startup:

```bash
source <(chainz completions zsh)     # ~/.zshrc, after compinit
source <(chainz completions bash)    # ~/.bashrc
chainz completions fish | source     # ~/.config/fish/config.fish
```

Elvish (`eval (chainz completions elvish | slurp)`) and PowerShell
(`chainz completions powershell | Out-String | Invoke-Expression`) work too.
Generate the loader at startup rather than saving it, so it follows upgrades.
Completing never migrates, writes or unlocks anything.

## The five nouns

| Noun | What it is | Commands |
| --- | --- | --- |
| **Chain** | A network you work on, with a short name (`eth`, `base`, `arb`) and aliases | `chain ls/add/show/set/rm`, `update`, `use`, `edit` |
| **Endpoint** | A named RPC URL on a chain; one is active, some may be failover | `rpc ls/add/rm/bench/discover`, `chain set --rpc` |
| **Signer** | A reference to key material plus its cached address | `signer add/ls/rm/migrate` |
| **Address** | A label for a contract or wallet, per chain or everywhere | `addr add/ls/rm/find`, `label` |
| **Runtime** | Live processes chainz started: the gateway and forks | `serve`, `fork`, `top` |

Chain references accept a name, an alias, a chain ID, an unambiguous prefix,
or a live fork name. Destructive commands (`chain rm`) take only an exact
name or ID.

## Quick start

```
Quick start:
  chainz signer add deployer               # store a key (OS keyring by default)
  chainz chain add base                    # add a chain from chainlist; the signer attaches
  chainz exec base -- cast block-number    # run a tool with the chain's context
On a terminal, `chainz init` walks through the same steps.
```

The first signer you add attaches to every chain that has none, and
`chain add` attaches the only signer (`--no-signer` skips it) and the
`etherscan` explorer once it is configured. With `--url` (`chainz chain add
42161 --url https://…`, or a built-in short name such as `arb`) `chain add`
never downloads chainlist; any other name needs the chain ID and `--name`.
More setup:

```bash
chainz rpc add base https://base-mainnet.infura.io/v3/'${INFURA_KEY}' --activate
printf %s "$KEY" | chainz var set INFURA_KEY --stdin
printf %s "$ETHERSCAN_KEY" | chainz chain set base --explorer etherscan --api-key-stdin
chainz use base                       # default chain for `exec -- …`
chainz ls
chainz doctor
```

## Changing a chain: `chain set` (or `update`)

One command changes any of a chain's settings, in one atomic write:

```bash
chainz chain set eth --signer deployer            # or --no-signer
chainz chain set eth --rpc infura                 # activate an endpoint by name
chainz chain set eth --rpc https://gw/rpc/1 --header 'x-secret: ${GW}'
                                                  # a new URL is added, checked and activated
chainz chain set eth --rpc gw --clear-headers
chainz chain set eth --failover llama,ankr        # or --no-failover
chainz chain set eth --explorer etherscan         # built in; or --no-explorer
printf %s KEY | chainz chain set eth --api-key-stdin   # one key per explorer
chainz chain set eth --name ethereum --alias main --rm-alias mainnet
chainz update                                     # on a terminal: pick a chain, then a menu
```

With no flags on a terminal, `chain set` opens a menu (RPC, Signer,
Explorer, Name, Aliases, Done) and applies your choices together. Each
picker starts on the current value, so Enter keeps it; Esc in a picker goes
back to the menu, and Esc in the menu asks before discarding pending
changes. Off a terminal it needs flags. `chainz edit [CHAIN]` opens config.toml (or one
chain's table) in `$EDITOR` for everything else.

## exec: run a tool with chain context

`chainz exec CHAIN -- CMD…` resolves the chain, exports its context,
rewrites `@tokens` in the arguments, and then **replaces itself** with the
command (`execvp`). Signals, exit codes and TTYs behave exactly as if you had
run the command directly. Chainz overhead is about 3 ms.

**Environment**

| Variable | Value |
| --- | --- |
| `ETH_RPC_URL` | the active endpoint, or the gateway URL while `serve` runs |
| `ETH_RPC_HEADERS` | endpoint headers (direct connections only) |
| `CHAIN_ID`, `CHAINZ_CHAIN` | chain ID and short name |
| `ETHERSCAN_API_KEY` | the chain's explorer key |
| `VERIFIER_URL`, `VERIFIER_API_KEY` | for non-Etherscan explorers |
| `ETH_FROM`, `WALLET_ADDRESS` | the signer's cached address |
| `ETH_KEYSTORE` | Foundry keystore path, for keystore signers |
| `RAW_PRIVATE_KEY` | only with `--expose-key` |

**Tokens**

| Token | Expands to |
| --- | --- |
| `@rpc`, `@chainid`, `@chain` | endpoint URL, chain ID, short name |
| `@wallet`, `@funder` | the chain's signer address, or a named signer's |
| `@explorer`, `@explorer_key` | verifier URL and key |
| `@usdc`, `@myrouter` | an address-book label on this chain |
| `@usdc:arb` | a label on another chain |
| `@@` | a literal `@` |

An argument that is only an unknown token is an error with a suggestion
(`@udsc` → did you mean `@usdc`?); inside a longer argument
(`foo@bar.com`, `@uniswap/v3-core`) an unknown token is left as it is. `--explain` prints the context, exported
names and rewritten argv without running anything (with `--expose-key` it
shows `RAW_PRIVATE_KEY` redacted and never unlocks the key).

Exit codes follow the shell: the command's own status, 127 when the command
does not exist, 126 when it can't be executed, and 130 when you interrupt a
chainz prompt.

```bash
chainz exec base -- cast call @usdc 'balanceOf(address)(uint256)' @wallet
chainz exec eth --signer funder -- cast send @wallet --value 1ether
chainz exec op --rpc llama -- cast block-number
eval "$(chainz env base)"             # export into the current shell
chainz shell base                     # or open a subshell
```

## serve: local RPC gateway

```bash
chainz serve            # foreground, with a live summary line
chainz serve --detach   # background; `serve status`, `serve stop`
chainz serve log -f --chain base --method eth_call
```

An opt-in loopback proxy on `127.0.0.1:18545`. While it runs, `exec` routes
through it automatically (`--direct` bypasses it). It gives you:

- **Failover you approve.** The active endpoint is tried first. Reads retry
  only on the endpoints in the chain's `failover = [...]` list. Transactions
  are never retried or sent elsewhere.
- **Rate-limit smoothing.** Adaptive per-endpoint token buckets, and
  `Retry-After` honoured on the same endpoint for short waits.
- **An inspector.** Every request is recorded, with addresses shown as labels
  (`eth_call → @usdc.balanceOf`). `--log FILE` appends JSONL.
- **Metrics.** Request counts, errors, p50 and p95 latency, 429s and
  failovers per chain, endpoint and method.

Each start mints a random token; URLs look like
`http://127.0.0.1:18545/<token>/base`. Endpoint secrets and headers stay
inside the gateway. Config edits are picked up live.

## top: live dashboard

```bash
chainz top             # interactive
chainz top --once      # one snapshot; add --json for scripts
```

Four tabs: Chains (head block, block age and time, base fee, active
endpoint and latency, recent health), Endpoints (every endpoint of a chain,
with health and capability tags; `s` activates one), Balances (each
signer's native and major-token balances, one Multicall3 call per chain)
and Traffic (the gateway's live requests and totals, while `serve` runs).
Through the gateway, the endpoint shown is the one it actually serves
from. Forks are not shown.

## fork: disposable anvil forks

```bash
$ chainz fork base --fund usdc=10000,weth=5
base-fork  http://127.0.0.1:8601  block 38,112,904  chain 8453

$ chainz exec base-fork -- cast send @usdc 'transfer(address,uint256)' @myrouter 1e6 --unlocked
$ chainz fork ls
$ chainz fork stop base-fork        # or Ctrl-C, or --all
```

- The fork is a chain name while it runs, and only by name: `exec 8453`
  always means the real chain.
- Anvil reads state through a proxy pinned to one endpoint (and through the
  gateway when it runs). `--block N` requires an archive-capable endpoint.
- Your signer is funded with ETH by default; `--fund` adds tokens,
  `--no-fund` skips it.
- A fork shares its parent's chain ID, so chainz refuses `--expose-key` on
  forks: sign with `--unlocked`. Use `--chain-id N` if you need real
  signatures.
- Anvil dies with its supervisor, even on SIGKILL (Linux).
- `fork ls --json` lists forks with their URL and block height. A
  foreground fork stopped by Ctrl-C exits 130; by `fork stop` or SIGTERM,
  143.

## Address book

```bash
chainz addr add myrouter 0x… --chain base
chainz addr add my-safe 0x… --all-chains
chainz addr ls --chain base --builtin   # include built-in tokens
chainz addr find 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913
chainz addr ls --json                    # also `addr find --json`
cast logs … | chainz label --chain base  # 0x… → usdc(0x…)
chainz addr import .                     # label contracts from Foundry broadcast/
```

Built-in labels cover major tokens
(`weth`, `usdc`, `usdce`, `usdt`, `wbtc`, `dai`) where they exist. Your own
labels take precedence; `chainz doctor` notes each label that shadows a
built-in token. `chainz doctor --tokens` verifies the built-in registry on
chain.

`chainz addr import PATH…` labels the contracts a `forge script --broadcast`
deployed. PATH is a `run-latest.json` (or any run file, named explicitly) or a
directory: a project's `broadcast/` is searched for `run-latest.json`, skipping
history files, `dry-run/` and dependencies under `lib/`. Each label goes on the
configured chain with the run's chain ID (never a fork), named in kebab case
(`UniversalRouter` → `@universal-router`, `ERC20Mock` → `@erc20-mock`). It
imports `CREATE`/`CREATE2` transactions and `additionalContracts` whose
transaction succeeded. It skips, and reports, failed or pending transactions,
unnamed contracts, invalid or reserved names, and runs on chains you have not
configured.

```bash
chainz addr import ~/src/v4-periphery --dry-run    # show the plan, write nothing
chainz addr import broadcast/ --prefix v4- --only PoolManager,position-manager
chainz addr import run-latest.json --force --json  # replace labels that differ
```

A label that already holds the same address is left unchanged. One that holds a
different address is skipped unless you pass `--force`. When one import gives
different addresses the same name on a chain, the later ones get `-2`, `-3`,
and so on. All labels are written in one locked, atomic update.

## Signers

Chainz needs a signer's address often and its key almost never. Only
`--expose-key` unlocks a key; everything else uses the cached address.

| Backend | Key lives in |
| --- | --- |
| `keyring` (default) | the OS keyring (`encrypted` is the default where there is none) |
| `encrypted` | `secrets.toml`, password-encrypted |
| `1password` | a 1Password item (`--vault`, `--item`) |
| `keystore` | a Foundry keystore file, unlocked by cast or forge |
| `plaintext` | `secrets.toml`, explicit opt-in |

A `keystore` signer (`--path`) caches the keystore's `address` field;
chainz never decrypts it. Foundry's own keystores (`cast wallet new`) have
none, so pass `--address "$(cast wallet address --keystore PATH)"`. A path
that doesn't exist is refused.

`chainz signer migrate` moves plaintext keys into safe storage
(`--all` for every plaintext signer; a failure doesn't stop the rest).
`chainz signer rm` deletes the key material too. A signer that chains
still use needs `--unassign`, which removes it from those chains (and says
which are left without a signer). On a terminal one question covers both;
off one, pass `--unassign --yes`, and the error names every flag missing.

Pass keys and variable values on stdin (`--stdin`): a secret given as an
argument works but warns, since it lands in shell history and `ps`.

## Configuration

One hand-editable TOML file plus a separate secrets file:

| Path | Holds |
| --- | --- |
| `~/.config/chainz/config.toml` | chains, endpoints, explorers, signer references, labels |
| `~/.config/chainz/secrets.toml` | variable values and plaintext keys (mode `0600`) |
| `$XDG_STATE_HOME/chainz/` | endpoint health history, gateway and fork logs |
| `$XDG_RUNTIME_DIR/chainz/` | gateway and fork discovery files |

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
environment, then `secrets.toml`. Inside a chainz context (`chainz shell`,
`eval "$(chainz env …)"`) the exec contract's own variables are not read
from the environment, so a nested `chainz exec` never inherits the outer
chain's keys. An unresolved reference is an error.
`chainz edit` and `chainz edit eth` validate on save.
`chainz doctor` flags literal credentials in URLs, and `doctor --fix` moves
them into variables. `chain add --url`, `chain set --rpc URL` and `rpc add` do the same as they
write a URL with a known key (Infura, Alchemy) or a credential header, and
say what they moved.

`--yes` (`-y`) always means "accept without asking". Every `rm` (`chain rm`,
`rpc rm`, `addr rm`, `var rm`, `signer rm`) asks first on a terminal and
needs `--yes` off one; `chain rm` names what goes with the chain (its
labels, and the default). `rpc bench --fix --yes` also writes the proposed
failover lists, `rpc discover --yes` adds every endpoint that answered.
Off a terminal chainz never prompts: it reports and tells you which flag
to pass. Listing commands take `--json` (`ls`, `chain show`, `rpc ls`,
`signer ls`, `var ls`, `addr ls`, `addr find`, `addr import`, `fork ls`,
`serve status`, `top --once`). Secrets are shown only with `--reveal`;
otherwise only the credential-shaped parts of a URL or header are masked
(`https://mainnet.infura.io/v3/[REDACTED]`), and anything chainz can't
parse is masked whole.

Exit codes: 0 on success, 1 for an error, 2 for a usage error (a missing
argument off a terminal included, such as a chain with no default set),
130 for a cancelled prompt.

## Migrating from v1

The first time v2 runs and finds `config.json` without `config.toml`, it
migrates once, under a lock:

- Long chainlist names become short names (`eth`, `op`, `arb`, `uni`, …); the
  old names stay as aliases, so `chainz exec "OP Mainnet"` still works.
- Every endpoint is kept and named after its host. `failover` starts empty.
- Known URL credentials (Infura, Alchemy) and verifier keys move into
  variables in `secrets.toml`. Anything else is reported.
- Signers keep their backend and address; keyring entries need no re-entry.
- `config.json` is renamed to `config.v1.json.bak` only after both new files
  are written.

Preview first with `chainz config migrate --dry-run`. The v1 names
`CHAIN_NAME`, `@chainname`, `@verification_url` and `@verifier_api_key` keep
working for one release, with a warning. `@key` is gone; use `--expose-key`.
Some v1 spellings still work: `chainz list`, `list` and `remove` under
each noun (`chain remove eth`, not `chainz remove eth`), `chainz update`
(now `chain set`, with v1's menu when given no flags), `chainz init`,
`key …` (for `signer …`), `--type private-key`/`one-password`, and
`var get --show`. The other top-level v1 commands moved under `chain`:
`chain add`, `chain show`, `chain rm`.

## Development

```bash
nix develop                      # toolchain, Foundry, hyperfine, strace
cargo test --workspace
scripts/perf-gate.sh V1_BIN V2_BIN
```

The design is in [`docs/v2/design.md`](docs/v2/design.md); crate interfaces
are in [`docs/v2/interfaces.md`](docs/v2/interfaces.md).

## License

MIT
