# Per-RPC Custom HTTP Headers

**Date:** 2026-08-07
**Status:** Approved

## Motivation

Some RPC endpoints (e.g. internal gateways) require authentication via a
custom HTTP header rather than a credential embedded in the URL:

```
curl https://gateway.example.com/rpc/1 \
  -H "content-type: application/json" \
  -H "x-internal-service-secret: <secret>" \
  -d '{"method":"eth_blockNumber",...}'
```

chainz has no way to attach headers to an RPC endpoint today, so such
endpoints can neither be health-checked by `doctor` nor used through
`exec`/`shell`.

chainz is primarily a config broker: it connects to RPCs itself only for
health probes, and otherwise hands the URL to downstream tools via
`ETH_RPC_URL`. Header support is therefore two features:

1. chainz's own probes send the headers.
2. `exec`/`shell` export headers to child tools. Foundry's `cast`/`forge`
   natively consume an `ETH_RPC_HEADERS` env var (comma-separated
   `"name: value"` pairs; verified via `cast --curl`), so the foundry
   ecosystem works with no downstream changes.

## Decisions

- **Scope: per RPC URL.** Headers belong to a specific endpoint. A chain's
  fallback RPCs never receive another endpoint's secret, and `doctor --fix`
  failover stays credential-safe.
- **CLI: repeatable `--header` flag** on `add`/`update`, scoped to
  `--rpc-url`. No new subcommand group.
- **Config: string-or-object `rpc_urls` entries.** Entries without headers
  serialize as plain strings (byte-identical to today, readable by older
  chainz versions); entries with headers serialize as `{url, headers}`
  objects. Existing configs load unchanged.

## Design

### 1. Data model (`src/chain/mod.rs`)

```rust
pub struct RpcEndpoint {
    pub url: String,
    pub headers: BTreeMap<String, String>, // BTreeMap → stable config diffs
}
```

- `ChainDefinition.rpc_urls: Vec<RpcEndpoint>` — one in-memory shape.
- Custom `Serialize`/`Deserialize` on `RpcEndpoint`:
  - deserialize accepts a JSON string (legacy) or `{url, headers}`;
  - serialize emits a string when `headers` is empty, else an object.
- `selected_rpc` stays a plain URL `String`. The `Config::validate()`
  invariant becomes "some entry's `.url` equals `selected_rpc`".
  `ChainDefinition::select_rpc()` keeps its upsert behavior.
- Validation:
  - header names: non-empty HTTP token characters;
  - header values: no `\r`/`\n` (header injection) and no commas (would
    corrupt the comma-joined `ETH_RPC_HEADERS` export — there is no escape
    syntax on the consumer side). `${VAR}` templates are exempt from the
    comma check at validate time and re-checked post-expansion at use time.

Example config:

```json
{
  "name": "ethereum",
  "chain_id": 1,
  "rpc_urls": [
    "https://eth.llamarpc.com",
    {
      "url": "https://entry-gateway.backend-prod.api.uniswap.org/rpc/1",
      "headers": { "x-internal-service-secret": "${UNI_GW_SECRET}" }
    }
  ],
  "selected_rpc": "https://entry-gateway.backend-prod.api.uniswap.org/rpc/1"
}
```

### 2. Secret handling & expansion (`src/config.rs`, `src/variables.rs`)

- `ChainInstance` gains `headers: HashMap<String, String>` — the *expanded*
  headers of the selected endpoint.
- `Chainz::get_chain()` expands each header value through the same `${VAR}`
  interpolation used for URLs (`GlobalVariables::expand_rpc_url`,
  generalized to `expand`). Recommended usage stores the secret in
  `chainz var set UNI_GW_SECRET --stdin` (or the environment) and
  references it as `${UNI_GW_SECRET}`; literal values also work.

### 3. CLI surface (`src/opt.rs`, add/update handlers)

- `chainz add ... --rpc-url URL --header "name: value"` — repeatable;
  requires `--rpc-url`; attaches to that URL.
- `chainz update <chain> --rpc-url URL --header "name: value"` — sets that
  URL's headers (replacing its previous header set) and selects it,
  matching today's `--rpc-url` semantics.
- `chainz update <chain> --rpc-url URL --clear-headers` — removes them.
- Parse rule: split on the first `:`, trim whitespace (same syntax as
  `curl -H` / `cast --rpc-headers`).
- stderr warning when a literal (non-`${VAR}`) header value is passed via
  argv, mirroring the `chainz var set` argv warning.

### 4. Probes (`src/chain/rpc.rs`)

- `check_url`, `probe`, `probe_urls`, `check_urls` accept expanded headers
  alongside the URL (`RpcEndpoint`-shaped data instead of bare `&str`).
  Callers (`doctor`, wizard RPC picker, add/update validation) expand
  per-endpoint headers before probing.
- `create_provider` builds a `reqwest::Client` with `default_headers` when
  the map is non-empty, wired into alloy via `Http::with_client(client,
  url)` → `RpcClient` → `ProviderBuilder::connect_client`. Header-free
  endpoints keep today's code path exactly.
- Invalid header name/value at client-build time fails with a message that
  names the header but never echoes the value (consistent with the existing
  "error chain never repeats the URL" rule).
- `doctor --fix` failover: each candidate endpoint is probed with *its own*
  headers, so failing over from a gateway to a public RPC never sends the
  secret to the wrong host.

### 5. `exec` / `shell` export (`src/variables.rs`, shell handler)

- When the selected endpoint has headers, `ChainVariables` sets
  `ETH_RPC_HEADERS` to the comma-joined `"name: value"` list (foundry's
  native format). `chainz shell` exports the same variable.
- Post-expansion guard: an expanded value containing a comma or newline is
  a hard error, not a silently-corrupted export.
- No `@headers` argv expansion (YAGNI; the env var covers the ecosystem and
  argv would leak the secret into process listings).

### 6. Display & redaction (`src/endpoint.rs` callers, `Debug` impls)

- Header **names** are public metadata (like query-param names today);
  header **values** are always `[REDACTED]` in `list`, `show`, and `Debug`
  output.
- `--show-secrets` reveals the raw *stored* value (`${UNI_GW_SECRET}` when
  a var is used — the indirection itself is a feature).
- `ChainDefinition`'s manual `Debug` impl extends to `RpcEndpoint`.

### 7. Testing

- Serde round-trips: legacy string entry loads and saves back as a string;
  entry with headers serializes as an object; mixed lists.
- Validation: bad header names; CR/LF/comma values rejected; selected-RPC
  invariant checked against `.url`.
- Probe headers actually sent: hand-rolled one-shot
  `tokio::net::TcpListener` responder in tests that asserts the header on
  the incoming request and replies with a canned `eth_chainId` response
  (no new dev-deps).
- `ChainVariables` sets `ETH_RPC_HEADERS` correctly and omits it when the
  endpoint has no headers.
- Redaction: header secrets absent from `Debug`, `list`, `show`, and error
  chains, following the existing redaction-test pattern.

## Out of scope

- Global per-host header rules (netrc-style matching).
- A `chainz rpc` subcommand group for fine-grained header management.
- Wizard prompts for headers.
- `@headers` argv expansion.
