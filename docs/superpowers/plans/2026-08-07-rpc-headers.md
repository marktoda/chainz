# Per-RPC Custom HTTP Headers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** RPC endpoints can carry custom HTTP headers (e.g. `x-internal-service-secret`) that chainz sends in its own health probes and exports to downstream tools via `ETH_RPC_HEADERS`.

**Architecture:** A new `RpcEndpoint { url, headers }` type replaces raw strings in `ChainDefinition.rpc_urls`, serialized as a plain string when headers are empty (full backward compatibility) and `{url, headers}` otherwise. Headers flow through the existing `${VAR}` interpolation, into alloy probes via a custom `reqwest::Client`, and into `exec`/`shell` as foundry's native `ETH_RPC_HEADERS` env var.

**Tech Stack:** Rust 2024 edition, alloy 1.6 (`ProviderBuilder::connect_reqwest`), reqwest 0.12, serde, clap 4, tokio.

**Spec:** `docs/superpowers/specs/2026-08-07-rpc-headers-design.md`

## Global Constraints

- Rust edition 2024, rust-version 1.88; run `cargo fmt` before every commit; `cargo clippy --all-targets` must stay clean.
- No new dependencies (dev or runtime).
- Secret hygiene: header **values** must never appear in error messages, `Debug` output, redacted listings, or test failure output. Header **names** are public metadata. Follow the existing patterns in `src/endpoint.rs` and the `rpc_error_chain_never_repeats_the_url` test.
- Configs without headers must serialize byte-identically to today (plain string entries in `rpc_urls`).
- Every task: `cargo test` green before commit.
- Commits: conventional messages, `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` trailer. If `git commit` fails with a GPG signing timeout, retry once; if it fails again, leave the work staged and write the intended message to `.git/PENDING_COMMIT_MSG` (append with a `---` separator if the file already has content), then report this in your result. Do NOT disable signing.

---

### Task 1: `RpcEndpoint` type with string-or-object serde

**Files:**
- Modify: `src/chain/mod.rs`

**Interfaces:**
- Produces: `pub struct RpcEndpoint { pub url: String, pub headers: BTreeMap<String, String> }` with:
  - `RpcEndpoint::new(url: impl Into<String>) -> Self`
  - `RpcEndpoint::with_headers(url: impl Into<String>, headers: BTreeMap<String, String>) -> Self`
  - `impl From<String> for RpcEndpoint`, `impl From<&str> for RpcEndpoint`
  - `Clone`, `PartialEq`, `Eq`, custom `Debug` (redacts URL, shows header names only)
  - `Serialize`/`Deserialize`: JSON string ⇄ headerless endpoint; `{url, headers}` object ⇄ endpoint with headers
- Consumes: `crate::endpoint::redact` (exists)

- [ ] **Step 1: Write failing serde/debug tests**

Add to the existing `mod tests` in `src/chain/mod.rs`:

```rust
#[test]
fn rpc_endpoint_deserializes_string_and_object_forms() {
    let plain: RpcEndpoint = serde_json::from_str(r#""https://rpc.example.com""#).unwrap();
    assert_eq!(plain, RpcEndpoint::new("https://rpc.example.com"));

    let detailed: RpcEndpoint = serde_json::from_str(
        r#"{"url":"https://gw.example.com/rpc/1","headers":{"x-secret":"${GW_SECRET}"}}"#,
    )
    .unwrap();
    assert_eq!(detailed.url, "https://gw.example.com/rpc/1");
    assert_eq!(detailed.headers["x-secret"], "${GW_SECRET}");

    // headers key optional in object form
    let bare: RpcEndpoint = serde_json::from_str(r#"{"url":"https://rpc.example.com"}"#).unwrap();
    assert!(bare.headers.is_empty());
}

#[test]
fn rpc_endpoint_serializes_headerless_as_plain_string() {
    let plain = RpcEndpoint::new("https://rpc.example.com");
    assert_eq!(
        serde_json::to_string(&plain).unwrap(),
        r#""https://rpc.example.com""#
    );

    let mut headers = std::collections::BTreeMap::new();
    headers.insert("x-secret".to_string(), "value".to_string());
    let detailed = RpcEndpoint::with_headers("https://gw.example.com", headers);
    let json = serde_json::to_string(&detailed).unwrap();
    assert!(json.contains(r#""url":"https://gw.example.com""#), "{json}");
    assert!(json.contains(r#""x-secret":"value""#), "{json}");

    // round-trip preserves both forms
    let back: RpcEndpoint = serde_json::from_str(&json).unwrap();
    assert_eq!(back, detailed);
}

#[test]
fn rpc_endpoint_debug_redacts_url_and_header_values() {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("x-secret-name".to_string(), "header-secret".to_string());
    let endpoint = RpcEndpoint::with_headers(
        "https://user:password@private.rpc.example.com/path-secret",
        headers,
    );
    let output = format!("{endpoint:?}");
    for secret in ["password", "path-secret", "header-secret", "private"] {
        assert!(!output.contains(secret), "{output}");
    }
    assert!(output.contains("x-secret-name"), "{output}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib chain::tests -- rpc_endpoint`
Expected: compile error, `RpcEndpoint` not found.

- [ ] **Step 3: Implement `RpcEndpoint`**

In `src/chain/mod.rs`, after the `DEFAULT_KEY_NAME` const, add (note `use std::collections::BTreeMap;` at the top):

```rust
/// One RPC endpoint: a URL plus optional custom HTTP headers (for gateways
/// that authenticate via header rather than URL-embedded credential).
///
/// On the wire this is a plain JSON string when there are no headers — so
/// existing configs parse unchanged and header-free configs stay readable by
/// older chainz versions — and a `{url, headers}` object otherwise.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RpcEndpointWire", into = "RpcEndpointWire")]
pub struct RpcEndpoint {
    pub url: String,
    /// Raw (unexpanded) header values; may contain ${VAR} references.
    /// BTreeMap keeps serialization and env-export ordering deterministic.
    pub headers: BTreeMap<String, String>,
}

impl RpcEndpoint {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: BTreeMap::new(),
        }
    }

    pub fn with_headers(url: impl Into<String>, headers: BTreeMap<String, String>) -> Self {
        Self {
            url: url.into(),
            headers,
        }
    }
}

impl From<String> for RpcEndpoint {
    fn from(url: String) -> Self {
        Self::new(url)
    }
}

impl From<&str> for RpcEndpoint {
    fn from(url: &str) -> Self {
        Self::new(url)
    }
}

impl fmt::Debug for RpcEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcEndpoint")
            .field("url", &crate::endpoint::redact(&self.url))
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Wire format: legacy plain string or {url, headers} object.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RpcEndpointWire {
    Url(String),
    Detailed {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

impl From<RpcEndpointWire> for RpcEndpoint {
    fn from(wire: RpcEndpointWire) -> Self {
        match wire {
            RpcEndpointWire::Url(url) => Self::new(url),
            RpcEndpointWire::Detailed { url, headers } => Self::with_headers(url, headers),
        }
    }
}

impl From<RpcEndpoint> for RpcEndpointWire {
    fn from(endpoint: RpcEndpoint) -> Self {
        if endpoint.headers.is_empty() {
            Self::Url(endpoint.url)
        } else {
            Self::Detailed {
                url: endpoint.url,
                headers: endpoint.headers,
            }
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib chain::tests`
Expected: all PASS (including pre-existing `debug_redacts_chain_credentials`).

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo fmt && cargo clippy --all-targets
git add src/chain/mod.rs
git commit -m "feat: add RpcEndpoint type with string-or-object serde"
```

---

### Task 2: Switch `rpc_urls` to `Vec<RpcEndpoint>` across the crate

Pure mechanical migration — behavior must be identical (headers exist but are ignored). The crate will not compile between the type flip and the call-site fixes; that's expected within this task.

**Files:**
- Modify: `src/chain/mod.rs` (field type, `select_rpc`, helpers, tests)
- Modify: `src/config.rs:407-415` (validate), `src/config.rs:457-464` (normalize_legacy)
- Modify: `src/config/tests.rs:10,343-361`
- Modify: `src/doctor.rs:186-233` (fix_rpcs)
- Modify: `src/chain/wizard.rs:379-392,485-495,606-616` (rpc_urls construction sites)
- Modify: `src/chain/wizard/tests.rs:72`, `src/variables/tests.rs:192`
- Modify: `src/listing.rs:66` (ChainView), `src/lib.rs:24-29` (add `RpcEndpoint` to the `model` re-exports)
- Modify: `tests/cli.rs:76,859,1015,1065`

**Interfaces:**
- Produces on `ChainDefinition`:
  - `pub rpc_urls: Vec<RpcEndpoint>` (same serde field name)
  - `pub(crate) fn endpoint(&self, url: &str) -> Option<&RpcEndpoint>`
  - `pub(crate) fn selected_endpoint(&self) -> Option<&RpcEndpoint>`
  - `pub(crate) fn select_rpc(&mut self, rpc_url: String)` (unchanged signature; upserts a headerless entry, preserves an existing entry's headers)
  - `pub(crate) fn select_rpc_with_headers(&mut self, rpc_url: String, headers: BTreeMap<String, String>)` (upserts and **replaces** that entry's header set)
- Consumes: `RpcEndpoint` from Task 1.

- [ ] **Step 1: Write failing tests for the new helpers**

Add to `mod tests` in `src/chain/mod.rs`:

```rust
#[test]
fn select_rpc_preserves_headers_and_select_with_headers_replaces_them() {
    let mut chain = make_chain_def(None, None);
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("x-secret".to_string(), "v1".to_string());
    chain.select_rpc_with_headers("https://gw.example.com".to_string(), headers.clone());
    assert_eq!(chain.selected_rpc, "https://gw.example.com");
    assert_eq!(chain.selected_endpoint().unwrap().headers, headers);

    // re-selecting via plain select_rpc keeps the stored headers
    chain.select_rpc("https://eth.llamarpc.com".to_string());
    chain.select_rpc("https://gw.example.com".to_string());
    assert_eq!(chain.selected_endpoint().unwrap().headers, headers);

    // select_rpc_with_headers replaces the header set
    chain.select_rpc_with_headers(
        "https://gw.example.com".to_string(),
        std::collections::BTreeMap::new(),
    );
    assert!(chain.selected_endpoint().unwrap().headers.is_empty());
    // no duplicate entries were created
    assert_eq!(
        chain
            .rpc_urls
            .iter()
            .filter(|e| e.url == "https://gw.example.com")
            .count(),
        1
    );
}
```

- [ ] **Step 2: Flip the field type and add helpers**

In `src/chain/mod.rs`:

```rust
pub rpc_urls: Vec<RpcEndpoint>,
```

Replace `select_rpc` and add helpers:

```rust
/// The configured endpoint for `url`, if present.
pub(crate) fn endpoint(&self, url: &str) -> Option<&RpcEndpoint> {
    self.rpc_urls.iter().find(|endpoint| endpoint.url == url)
}

/// The endpoint record backing `selected_rpc`. `None` only on configs that
/// bypass validation (doctor's lenient load).
pub(crate) fn selected_endpoint(&self) -> Option<&RpcEndpoint> {
    self.endpoint(&self.selected_rpc)
}

/// Select an RPC while preserving the config invariant that the selected
/// endpoint is present in the chain's configured endpoint list. An entry
/// that already exists keeps its headers.
pub(crate) fn select_rpc(&mut self, rpc_url: String) {
    if self.endpoint(&rpc_url).is_none() {
        self.rpc_urls.push(RpcEndpoint::new(rpc_url.clone()));
    }
    self.selected_rpc = rpc_url;
}

/// Select an RPC and set its header set (replacing any previous headers).
pub(crate) fn select_rpc_with_headers(
    &mut self,
    rpc_url: String,
    headers: BTreeMap<String, String>,
) {
    match self.rpc_urls.iter_mut().find(|e| e.url == rpc_url) {
        Some(endpoint) => endpoint.headers = headers,
        None => self
            .rpc_urls
            .push(RpcEndpoint::with_headers(rpc_url.clone(), headers)),
    }
    self.selected_rpc = rpc_url;
}
```

In the same file's `Debug for ChainDefinition`, the `rpc_urls` field now uses `RpcEndpoint`'s own redacting Debug:

```rust
f.debug_struct("ChainDefinition")
    .field("name", &self.name)
    .field("aliases", &self.aliases)
    .field("chain_id", &self.chain_id)
    .field("rpc_urls", &self.rpc_urls)
```

(delete the local `let rpc_urls: Vec<_> = ...` redaction block; keep the rest of the impl unchanged). In the tests module, `make_chain_def` becomes:

```rust
rpc_urls: vec!["https://eth.llamarpc.com".into()],
```

and `debug_redacts_chain_credentials`'s line `chain.rpc_urls = vec![chain.selected_rpc.clone()];` becomes `chain.rpc_urls = vec![chain.selected_rpc.clone().into()];`.

- [ ] **Step 3: Fix every call site**

Run `cargo build --all-targets` and fix each error. Expected sites and their fixes:

`src/config.rs` validate (~line 410):

```rust
if !chain.rpc_urls.iter().any(|e| e.url == chain.selected_rpc) {
```

`src/config.rs` normalize_legacy (~line 461):

```rust
if !chain.selected_rpc.is_empty() && chain.endpoint(&chain.selected_rpc).is_none() {
    chain.rpc_urls.push(RpcEndpoint::new(chain.selected_rpc.clone()));
}
```

(add `RpcEndpoint` to the `crate::chain` import list at the top of `config.rs`.)

`src/doctor.rs` fix_rpcs (~lines 194-214): candidates become endpoints —

```rust
let candidates: Vec<&RpcEndpoint> = chain
    .rpc_urls
    .iter()
    .filter(|endpoint| endpoint.url != chain.selected_rpc)
    .collect();
let expanded: Vec<String> = candidates
    .iter()
    .map(|endpoint| chainz.config.globals.expand_rpc_url(&endpoint.url))
    .collect();
```

and below, `candidates[i].clone()` → `candidates[i].url.clone()` (both in `set_selected_rpc` and in the `redact` call). Import `RpcEndpoint` via `crate::chain::RpcEndpoint`. `check_urls` still takes `&[String]` in this task.

`src/chain/wizard.rs`:
- `handle_non_interactive` (~line 489): `rpc_urls: vec![rpc_url.clone().into()],`
- `handle_interactive` (~line 610): `rpc_urls: selected_chain.rpc.iter().map(|url| RpcEndpoint::new(url.clone())).collect(),` (import `RpcEndpoint` in the `super::` use list)
- `edit_interactively` RPC branch (~lines 379-392): preserve headers for URLs that survive the refresh —

```rust
let available_rpcs = fetch_chain_by_id(chain.chain_id, self.refresh)
    .await
    .map(|entry| entry.rpc)
    .unwrap_or_else(|_| chain.rpc_urls.iter().map(|e| e.url.clone()).collect());
let new_rpc = select_rpc(
    terminal,
    &chain.name,
    chain.chain_id,
    available_rpcs.clone(),
    &chainz.config.globals,
)
.await?;
chain.rpc_urls = available_rpcs
    .iter()
    .map(|url| {
        chain
            .endpoint(url)
            .cloned()
            .unwrap_or_else(|| RpcEndpoint::new(url.clone()))
    })
    .collect();
chain.select_rpc(new_rpc);
```

`src/listing.rs` ChainView (~line 66):

```rust
rpc_urls: chain.rpc_urls.iter().map(|e| present(&e.url)).collect(),
```

Test files — string literals gain `.into()` (resolves via `From<&str>`):
- `src/config/tests.rs:10`: `rpc_urls: vec!["https://rpc.example.com".into()],`
- `src/config/tests.rs:348`: `assert_eq!(config.chains[0].rpc_urls, vec![RpcEndpoint::from("https://rpc.example.com")]);` (import `RpcEndpoint`)
- `src/config/tests.rs:361`: `assert!(chain.endpoint(&chain.selected_rpc).is_some());`
- `src/chain/wizard/tests.rs:72`, `src/variables/tests.rs:192`: add `.into()` inside the `vec![]`.
- `src/lib.rs`: add `pub use crate::chain::RpcEndpoint;` inside the `pub mod model` block (the integration tests import via `chainz::model::{...}`).
- `tests/cli.rs:76,1015,1065`: `.into()` on each `rpc_urls` entry (in the `seed_config` helper at line 76 and the two inline `ChainDefinition` literals); `tests/cli.rs:859`: `config.chains[0].rpc_urls = vec![secret_url.into()];`.
- `tests/cli.rs:499` is a raw JSON config fixture with `"rpc_urls": ["https://eth.example.com"]` — leave it untouched; it now doubles as the legacy string-format compatibility test.

- [ ] **Step 4: Run the full suite**

Run: `cargo test`
Expected: all PASS. In particular `tests/cli.rs` (legacy string configs still parse) and `src/config/tests.rs` round-trips.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo fmt && cargo clippy --all-targets
git add -A src tests
git commit -m "refactor: store rpc_urls as RpcEndpoint entries"
```

---

### Task 3: Header validation in `Config::validate`

**Files:**
- Modify: `src/config.rs` (validate, inside the per-chain loop)
- Test: `src/config/tests.rs`

**Interfaces:**
- Produces: validation rules — header names are non-empty HTTP token chars; values contain no `\r`/`\n`; values contain no `,` unless they contain `${` (template exemption, re-checked post-expansion in Task 6). Endpoint URLs must be non-blank.
- Consumes: `RpcEndpoint` entries on `ChainDefinition.rpc_urls`.

- [ ] **Step 1: Write failing validation tests**

Add to `src/config/tests.rs` (use the file's existing `test_config()`/chain-builder helpers — read the top of the file first and follow its pattern for constructing a valid config):

```rust
#[test]
fn validate_rejects_bad_rpc_headers() {
    let cases: &[(&str, &str, &str)] = &[
        ("", "value", "empty name"),
        ("x sec", "value", "space in name"),
        ("x-ok", "line\nbreak", "newline in value"),
        ("x-ok", "carriage\rreturn", "CR in value"),
        ("x-ok", "a,b", "comma in literal value"),
    ];
    for (name, value, label) in cases {
        let mut config = test_config();
        let mut headers = std::collections::BTreeMap::new();
        headers.insert(name.to_string(), value.to_string());
        config.chains[0]
            .select_rpc_with_headers(config.chains[0].selected_rpc.clone(), headers);
        assert!(config.validate().is_err(), "expected rejection: {label}");
    }
}

#[test]
fn validate_accepts_good_rpc_headers_and_template_commas() {
    for value in ["plain-secret", "${GW_SECRET}", "${A},${B}"] {
        let mut config = test_config();
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("x-internal-service-secret".to_string(), value.to_string());
        config.chains[0]
            .select_rpc_with_headers(config.chains[0].selected_rpc.clone(), headers);
        assert!(config.validate().is_ok(), "expected acceptance: {value}");
    }
}
```

(If `test_config()` doesn't exist under that name, adapt to the file's actual helper — do not invent a second config builder.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config::tests -- headers`
Expected: FAIL (validate currently accepts everything).

- [ ] **Step 3: Implement validation**

In `src/config.rs`, inside `validate()`'s `for chain in &self.chains` loop (after the selected-RPC check), add:

```rust
for endpoint in &chain.rpc_urls {
    if endpoint.url.trim().is_empty() {
        anyhow::bail!("Chain '{}' has an RPC entry with an empty URL", chain.name);
    }
    for (name, value) in &endpoint.headers {
        if name.is_empty() || !name.bytes().all(is_header_name_byte) {
            anyhow::bail!(
                "Chain '{}' has an invalid RPC header name '{}'",
                chain.name,
                name
            );
        }
        // Never echo the value: it is a credential.
        if value.contains(['\r', '\n']) {
            anyhow::bail!(
                "Chain '{}' RPC header '{}' has a value containing line breaks",
                chain.name,
                name
            );
        }
        if value.contains(',') && !value.contains("${") {
            anyhow::bail!(
                "Chain '{}' RPC header '{}' has a value containing a comma, \
                 which cannot be exported via ETH_RPC_HEADERS",
                chain.name,
                name
            );
        }
    }
}
```

And at module level (near `normalize_legacy`):

```rust
/// RFC 9110 token characters, the legal alphabet for HTTP header names.
fn is_header_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: all PASS.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo fmt && cargo clippy --all-targets
git add src/config.rs src/config/tests.rs
git commit -m "feat: validate RPC endpoint headers in config"
```

---

### Task 4: Header expansion into `ChainInstance`

**Files:**
- Modify: `src/variables.rs` (GlobalVariables)
- Modify: `src/config.rs` (`Chainz::get_chain`)
- Modify: `src/chain/mod.rs` (`ChainInstance`)
- Test: `src/variables/tests.rs`

**Interfaces:**
- Produces:
  - `GlobalVariables::expand(&self, value: &str) -> String` — **rename** of `expand_rpc_url` (same interpolation, now also used for header values). Update ALL call sites: `src/config.rs:70`, `src/doctor.rs:148,201`, `src/chain/wizard.rs:74,173,323,479,558` (grep for `expand_rpc_url` to be exhaustive).
  - `GlobalVariables::expand_endpoint(&self, endpoint: &RpcEndpoint) -> RpcEndpoint` — expands URL and every header value.
  - `ChainInstance.headers: BTreeMap<String, String>` — the selected endpoint's **expanded** headers.
- Consumes: `RpcEndpoint`, `ChainDefinition::selected_endpoint()` (Task 2).

- [ ] **Step 1: Write failing tests**

Add to `src/variables/tests.rs` (follow the file's existing test style — it already builds `GlobalVariables` and `ChainInstance` fixtures):

```rust
#[test]
fn expand_endpoint_interpolates_url_and_header_values() {
    let mut globals = GlobalVariables::default();
    globals.add_rpc_expansion("GW_SECRET", "sekrit");
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("x-internal-service-secret".to_string(), "${GW_SECRET}".to_string());
    headers.insert("x-plain".to_string(), "as-is".to_string());
    let endpoint = crate::chain::RpcEndpoint::with_headers(
        "https://gw.example.com/${GW_SECRET}",
        headers,
    );

    let expanded = globals.expand_endpoint(&endpoint);

    assert_eq!(expanded.url, "https://gw.example.com/sekrit");
    assert_eq!(expanded.headers["x-internal-service-secret"], "sekrit");
    assert_eq!(expanded.headers["x-plain"], "as-is");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib variables::tests -- expand_endpoint`
Expected: compile error, `expand_endpoint` not found.

- [ ] **Step 3: Implement**

In `src/variables.rs`, rename `expand_rpc_url` → `expand` and add:

```rust
/// Expand a full endpoint: URL and every header value get the same
/// ${VAR} interpolation.
pub fn expand_endpoint(&self, endpoint: &crate::chain::RpcEndpoint) -> crate::chain::RpcEndpoint {
    crate::chain::RpcEndpoint {
        url: self.expand(&endpoint.url),
        headers: endpoint
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), self.expand(value)))
            .collect(),
    }
}
```

Update all `expand_rpc_url` call sites (list above; verify with `grep -rn expand_rpc_url src/`).

In `src/chain/mod.rs`, add the field to `ChainInstance` (with `use std::collections::BTreeMap;` already present from Task 1):

```rust
pub struct ChainInstance {
    pub definition: ChainDefinition,
    pub rpc_url: String,
    /// Expanded custom headers of the selected endpoint (empty when none).
    pub headers: BTreeMap<String, String>,
    pub key: Option<Key>,
}
```

In `src/config.rs`, `Chainz::get_chain` becomes:

```rust
pub fn get_chain(&self, name_or_id: &str) -> Result<ChainInstance> {
    let definition = self.config.get_chain(name_or_id)?.clone();
    let endpoint = definition
        .selected_endpoint()
        .cloned()
        .unwrap_or_else(|| RpcEndpoint::new(definition.selected_rpc.clone()));
    let expanded = self.config.globals.expand_endpoint(&endpoint);
    let key = definition
        .key_name
        .as_deref()
        .and_then(|name| self.config.keys.get(name))
        .cloned();
    Ok(ChainInstance {
        definition,
        rpc_url: expanded.url,
        headers: expanded.headers,
        key,
    })
}
```

Fix any `ChainInstance` construction in tests (`src/variables/tests.rs` fixture) by adding `headers: Default::default(),`.

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: all PASS.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo fmt && cargo clippy --all-targets
git add src/variables.rs src/variables/tests.rs src/config.rs src/chain/mod.rs
git commit -m "feat: expand endpoint headers into ChainInstance"
```

---

### Task 5: Probes send headers

**Files:**
- Modify: `src/chain/rpc.rs` (all public fns + `create_provider`)
- Modify: callers — `src/doctor.rs` (check_rpc_health, fix_rpcs), `src/chain/wizard.rs` (select_rpc, select_manual_rpc, apply_direct, handle_non_interactive, handle_interactive)

**Interfaces:**
- Produces (all take **expanded** endpoints):
  - `pub async fn check_url(endpoint: &RpcEndpoint, expected_chain_id: u64) -> Result<()>`
  - `pub async fn probe(endpoint: &RpcEndpoint, expected_chain_id: u64) -> (bool, Duration)`
  - `pub fn probe_urls(endpoints: &[RpcEndpoint], expected_chain_id: u64) -> Receiver<ProbeResult>`
  - `pub async fn check_urls(endpoints: &[RpcEndpoint], expected_chain_id: u64) -> Vec<bool>`
- Consumes: `RpcEndpoint`, `GlobalVariables::expand_endpoint` (Task 4), alloy `ProviderBuilder::connect_reqwest` (verified present in alloy 1.6.3).

- [ ] **Step 1: Write the failing header-sending test**

Add to `mod tests` in `src/chain/rpc.rs`:

```rust
#[tokio::test]
async fn check_url_sends_custom_headers() -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap();
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        // echo back the request's JSON-RPC id so alloy accepts the response
        let id: u64 = request
            .rfind("\"id\":")
            .map(|i| {
                request[i + 5..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let body = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":"0x1"}}"#);
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        request
    });

    let mut headers = std::collections::BTreeMap::new();
    headers.insert("x-test-header".to_string(), "expected-value".to_string());
    let endpoint = crate::chain::RpcEndpoint::with_headers(format!("http://{addr}"), headers);

    check_url(&endpoint, 1).await?;

    let request = server.await?;
    let request_lower = request.to_lowercase();
    assert!(
        request_lower.contains("x-test-header: expected-value"),
        "header missing from request: {request_lower}"
    );
    Ok(())
}

#[tokio::test]
async fn invalid_header_value_error_never_echoes_the_value() {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("x-test-header".to_string(), "bad\u{0000}value".to_string());
    let endpoint =
        crate::chain::RpcEndpoint::with_headers("http://127.0.0.1:1".to_string(), headers);
    let error = check_url(&endpoint, 1).await.unwrap_err();
    let diagnostic = format!("{error:#}");
    assert!(diagnostic.contains("x-test-header"), "{diagnostic}");
    assert!(!diagnostic.contains("bad"), "{diagnostic}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib chain::rpc`
Expected: compile error (`check_url` takes `&str`).

- [ ] **Step 3: Change signatures and implement the custom-client path**

In `src/chain/rpc.rs` (add `use crate::chain::RpcEndpoint;` — note this file is `chain/rpc.rs` so `use super::RpcEndpoint;` matches the existing style of `wizard.rs`):

```rust
/// Test whether an (already-expanded) RPC endpoint serves the expected chain
/// id, sending the endpoint's custom headers if any. ...(keep existing doc)...
pub async fn check_url(endpoint: &RpcEndpoint, expected_chain_id: u64) -> Result<()> {
    let provider = create_provider(endpoint).await?;
    // ... body unchanged ...
}

pub async fn probe(endpoint: &RpcEndpoint, expected_chain_id: u64) -> (bool, Duration) {
    let start = Instant::now();
    let healthy = tokio::time::timeout(CHECK_DEADLINE, check_url(endpoint, expected_chain_id))
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);
    (healthy, start.elapsed())
}

pub fn probe_urls(
    endpoints: &[RpcEndpoint],
    expected_chain_id: u64,
) -> tokio::sync::mpsc::Receiver<ProbeResult> {
    let (tx, rx) = tokio::sync::mpsc::channel(endpoints.len().max(1));
    for (index, endpoint) in endpoints.iter().cloned().enumerate() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let (healthy, latency) = probe(&endpoint, expected_chain_id).await;
            let _ = tx
                .send(ProbeResult {
                    index,
                    healthy,
                    latency,
                })
                .await;
        });
    }
    rx
}

pub async fn check_urls(endpoints: &[RpcEndpoint], expected_chain_id: u64) -> Vec<bool> {
    let mut results = vec![false; endpoints.len()];
    let mut rx = probe_urls(endpoints, expected_chain_id);
    // ... body unchanged ...
}

async fn create_provider(endpoint: &RpcEndpoint) -> Result<DynProvider> {
    if endpoint.headers.is_empty() {
        // Header-free endpoints keep the connection-string path (ws:// support).
        let provider = tokio::time::timeout(
            Duration::from_secs(10),
            ProviderBuilder::new().connect(&endpoint.url),
        )
        .await
        .map_err(|_| anyhow::anyhow!("RPC connection timed out"))?
        .map_err(|_| anyhow::anyhow!("Failed to initialize the RPC connection"))?;
        return Ok(provider.erased());
    }

    let url: reqwest::Url = endpoint
        .url
        .parse()
        // The URL may embed credentials; the diagnostic stays URL-free.
        .map_err(|_| anyhow::anyhow!("Invalid RPC URL"))?;
    let mut header_map = reqwest::header::HeaderMap::new();
    for (name, value) in &endpoint.headers {
        let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| anyhow::anyhow!("Invalid RPC header name '{name}'"))?;
        // Header values are credentials: name the header, never echo the value.
        let header_value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| anyhow::anyhow!("Invalid value for RPC header '{name}'"))?;
        header_map.insert(header_name, header_value);
    }
    let client = reqwest::Client::builder()
        .default_headers(header_map)
        .build()
        .map_err(|_| anyhow::anyhow!("Failed to initialize the RPC connection"))?;
    Ok(ProviderBuilder::new().connect_reqwest(client, url).erased())
}
```

Update the existing tests in this file: `probe_urls_reports_every_url` builds `vec!["http://localhost:1".into(), "http://localhost:2".into()]` (type annotation `Vec<RpcEndpoint>`), `probe_urls_empty_input_closes_immediately` passes `&[]`, and `rpc_error_chain_never_repeats_the_url` wraps its URL: `check_url(&"http://user:password@127.0.0.1:1/literal-secret".into(), 1)`.

- [ ] **Step 4: Update callers**

`src/doctor.rs` `check_rpc_health` (~line 148): probe the selected endpoint with its expanded headers —

```rust
let expanded = chainz.config.globals.expand_endpoint(
    &c.selected_endpoint()
        .cloned()
        .unwrap_or_else(|| RpcEndpoint::new(c.selected_rpc.clone())),
);
let raw = c.selected_rpc.clone();
let chain_id = c.chain_id;
let name = c.name.clone();
tokio::spawn(async move {
    let (healthy, latency) = crate::chain::rpc::probe(&expanded, chain_id).await;
    (name, healthy, raw, latency)
})
```

`src/doctor.rs` `fix_rpcs` (~line 199): expand full endpoints —

```rust
let expanded: Vec<RpcEndpoint> = candidates
    .iter()
    .map(|endpoint| chainz.config.globals.expand_endpoint(endpoint))
    .collect();
let health = check_urls(&expanded, chain.chain_id).await;
```

`src/chain/wizard.rs`:
- `select_rpc` (~line 74): these URLs come from chainlist with no headers, so:

```rust
let expanded: Vec<RpcEndpoint> = urls
    .iter()
    .map(|u| RpcEndpoint::new(globals.expand(u)))
    .collect();
```

- `select_manual_rpc` (~line 173): `check_url(&RpcEndpoint::new(globals.expand(&rpc_url)), chain_id)`
- `apply_direct` (~line 322), `handle_non_interactive` (~line 479), `handle_interactive` (~line 557): each currently calls `check_url(&chainz.config.globals.expand(rpc_url), chain_id)` — wrap as `check_url(&RpcEndpoint::new(chainz.config.globals.expand(rpc_url)), chain_id)`. (Task 6 upgrades these to carry real headers.)

- [ ] **Step 5: Run tests**

Run: `cargo test`
Expected: all PASS, including the new TcpListener test.

- [ ] **Step 6: fmt, clippy, commit**

```bash
cargo fmt && cargo clippy --all-targets
git add src/chain/rpc.rs src/doctor.rs src/chain/wizard.rs
git commit -m "feat: send custom endpoint headers in RPC health probes"
```

---

### Task 6: `--header` / `--clear-headers` CLI flags

**Files:**
- Modify: `src/opt.rs` (AddArgs, UpdateArgs)
- Modify: `src/chain/wizard.rs` (parse helper + wiring in apply_direct / handle_non_interactive / handle_interactive / has_direct_changes)
- Test: `src/chain/wizard/tests.rs`

**Interfaces:**
- Produces:
  - `AddArgs.headers: Vec<String>`, `UpdateArgs.headers: Vec<String>` (clap long `--header`, requires `rpc_url`)
  - `UpdateArgs.clear_headers: bool` (requires `rpc_url`, conflicts with `headers`)
  - `pub(crate) fn parse_rpc_headers(raw: &[String]) -> Result<BTreeMap<String, String>>` in `src/chain/wizard.rs`
- Consumes: `select_rpc_with_headers` (Task 2), `check_url(&RpcEndpoint, ...)` + `expand_endpoint` (Tasks 4-5).

- [ ] **Step 1: Write failing parse tests**

Add to `src/chain/wizard/tests.rs`:

```rust
#[test]
fn parse_rpc_headers_splits_on_first_colon_and_trims() {
    let parsed = super::parse_rpc_headers(&[
        "x-internal-service-secret: ${GW_SECRET}".to_string(),
        "authorization:Bearer abc:def".to_string(),
    ])
    .unwrap();
    assert_eq!(parsed["x-internal-service-secret"], "${GW_SECRET}");
    assert_eq!(parsed["authorization"], "Bearer abc:def");
}

#[test]
fn parse_rpc_headers_rejects_malformed_without_echoing_values() {
    for bad in ["no-colon-secret", ": value-secret", "name-only:", "name-only:   "] {
        let error = super::parse_rpc_headers(&[bad.to_string()]).unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(!diagnostic.contains("secret"), "{diagnostic}");
    }
    // duplicate names rejected
    let error = super::parse_rpc_headers(&[
        "x-dup: a".to_string(),
        "x-dup: b".to_string(),
    ])
    .unwrap_err();
    assert!(format!("{error:#}").contains("x-dup"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib chain::wizard::tests -- parse_rpc_headers`
Expected: compile error, `parse_rpc_headers` not found.

- [ ] **Step 3: Add flags and parser**

`src/opt.rs` — add to **both** `AddArgs` and `UpdateArgs`:

```rust
/// Custom HTTP header for the RPC URL, as "name: value" (repeatable).
/// Values may reference variables: --header "x-secret: ${MY_SECRET}"
#[arg(long = "header", value_name = "NAME: VALUE", requires = "rpc_url")]
pub headers: Vec<String>,
```

and to `UpdateArgs` only:

```rust
/// Remove all custom headers from the RPC URL given by --rpc-url
#[arg(long, requires = "rpc_url", conflicts_with = "headers")]
pub clear_headers: bool,
```

`src/chain/wizard.rs` — add near `read_verification_api_key`:

```rust
/// Parse repeatable `--header "name: value"` arguments. Splits on the first
/// colon so values may themselves contain colons ("Bearer x:y").
/// Error messages never echo the raw argument: it may contain a credential.
pub(crate) fn parse_rpc_headers(
    raw: &[String],
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut headers = std::collections::BTreeMap::new();
    for entry in raw {
        let Some((name, value)) = entry.split_once(':') else {
            anyhow::bail!("Invalid --header; expected \"name: value\"");
        };
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty() || value.is_empty() {
            anyhow::bail!("Invalid --header; expected \"name: value\"");
        }
        if !value.contains("${") {
            eprintln!(
                "Warning: header values in argv may be visible in shell history; prefer a ${{VAR}} reference set via `chainz var set`"
            );
        }
        if headers
            .insert(name.to_string(), value.to_string())
            .is_some()
        {
            anyhow::bail!("Duplicate --header '{}'", name);
        }
    }
    Ok(headers)
}
```

- [ ] **Step 4: Wire into add/update**

`UpdateArgs::has_direct_changes` (~line 296): add `|| !self.headers.is_empty() || self.clear_headers`.

`UpdateArgs::apply_direct` — replace the `--rpc-url` branch:

```rust
if let Some(rpc_url) = &self.rpc_url {
    let headers = if self.clear_headers {
        Some(std::collections::BTreeMap::new())
    } else if !self.headers.is_empty() {
        Some(parse_rpc_headers(&self.headers)?)
    } else {
        None // keep whatever headers the entry already has
    };
    let probe_endpoint = chainz.config.globals.expand_endpoint(&RpcEndpoint::with_headers(
        rpc_url.clone(),
        headers
            .clone()
            .or_else(|| chain.endpoint(rpc_url).map(|e| e.headers.clone()))
            .unwrap_or_default(),
    ));
    check_url(&probe_endpoint, chain.chain_id)
        .await
        .with_context(|| {
            format!("RPC check failed for {}", crate::endpoint::redact(rpc_url))
        })?;
    match headers {
        Some(headers) => chain.select_rpc_with_headers(rpc_url.clone(), headers),
        None => chain.select_rpc(rpc_url.clone()),
    }
}
```

`AddArgs::handle_non_interactive` — parse once at the top, probe with headers, store them:

```rust
let headers = parse_rpc_headers(&self.headers)?;
// ... existing key resolution ...
let probe_endpoint = chainz
    .config
    .globals
    .expand_endpoint(&RpcEndpoint::with_headers(rpc_url.clone(), headers.clone()));
check_url(&probe_endpoint, chain_id)
    .await
    .with_context(|| format!("RPC check failed for {}", crate::endpoint::redact(&rpc_url)))?;

let chain_def = ChainDefinition {
    // ... unchanged fields ...
    rpc_urls: vec![RpcEndpoint::with_headers(rpc_url.clone(), headers)],
    selected_rpc: rpc_url,
    // ...
};
```

`AddArgs::handle_interactive` — in the `if let Some(rpc_url) = &self.rpc_url` branch, probe with parsed headers exactly as above, and after `chain_def.select_rpc(selected_rpc)` replace with:

```rust
let headers = parse_rpc_headers(&self.headers)?;
if headers.is_empty() {
    chain_def.select_rpc(selected_rpc);
} else {
    chain_def.select_rpc_with_headers(selected_rpc, headers);
}
```

(Clap's `requires = "rpc_url"` guarantees `--header` only appears with `--rpc-url`, so in the headers-present case `selected_rpc` is exactly the flagged URL.)

- [ ] **Step 5: Run tests, then a manual smoke check**

Run: `cargo test`
Expected: all PASS.

Manual (no network needed — expect the probe failure path, proving flags parse):

```bash
cargo run -- add --name smoketest --chain-id 999999 \
  --rpc-url http://127.0.0.1:1 --header "x-test: ${NOPE_VAR}" 2>&1 | head -5
```

Expected: "RPC check failed for http://127.0.0.1:1/..." error (not a clap error).

- [ ] **Step 6: fmt, clippy, commit**

```bash
cargo fmt && cargo clippy --all-targets
git add src/opt.rs src/chain/wizard.rs src/chain/wizard/tests.rs
git commit -m "feat: add --header/--clear-headers flags to add and update"
```

---

### Task 7: Export `ETH_RPC_HEADERS` in exec/shell

**Files:**
- Modify: `src/variables.rs` (ChainVariables::new)
- Test: `src/variables/tests.rs`

**Interfaces:**
- Produces: `ETH_RPC_HEADERS` env var — comma-joined `"name: value"` pairs of the selected endpoint's expanded headers (foundry's native format; BTreeMap ordering makes it deterministic). Absent when the endpoint has no headers. Hard error if an expanded value contains `,`, `\r`, or `\n`.
- Consumes: `ChainInstance.headers` (Task 4). `chainz shell` and `chainz exec` both go through `ChainVariables::new` (`src/cli.rs:136,175`), so no cli.rs change is needed.

- [ ] **Step 1: Write failing tests**

Add to `src/variables/tests.rs` (reuse the file's existing `ChainInstance` fixture pattern from ~line 190):

```rust
#[test]
fn chain_variables_export_eth_rpc_headers() {
    let mut chain = test_chain_instance(); // adapt to the file's fixture helper
    chain.headers.insert("b-second".to_string(), "two".to_string());
    chain.headers.insert("a-first".to_string(), "one".to_string());

    let variables = ChainVariables::new(&chain, &[], false).unwrap();

    assert_eq!(
        variables.as_map()["ETH_RPC_HEADERS"],
        "a-first: one,b-second: two"
    );
}

#[test]
fn chain_variables_omit_eth_rpc_headers_when_empty() {
    let chain = test_chain_instance();
    let variables = ChainVariables::new(&chain, &[], false).unwrap();
    assert!(!variables.as_map().contains_key("ETH_RPC_HEADERS"));
}

#[test]
fn chain_variables_reject_unexportable_expanded_header_values() {
    for bad in ["has,comma", "has\nnewline"] {
        let mut chain = test_chain_instance();
        chain
            .headers
            .insert("x-bad".to_string(), bad.to_string());
        let error = ChainVariables::new(&chain, &[], false).unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("x-bad"), "{diagnostic}");
        assert!(!diagnostic.contains("has"), "{diagnostic}");
    }
}
```

(If no fixture helper exists, extract one from the existing test at ~line 190 rather than duplicating the `ChainInstance` literal.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib variables::tests -- eth_rpc_headers`
Expected: FAIL / compile error.

- [ ] **Step 3: Implement**

In `ChainVariables::new` (`src/variables.rs`), after the `basic_vars` loop:

```rust
// Export custom RPC headers in foundry's native ETH_RPC_HEADERS format
// ("name: value" pairs, comma-separated). Values were ${VAR}-expanded by
// get_chain; re-check exportability post-expansion since the config-time
// comma check exempts templates. Never echo the value: it is a credential.
if !chain.headers.is_empty() {
    let mut pairs = Vec::with_capacity(chain.headers.len());
    for (name, value) in &chain.headers {
        if value.contains([',', '\r', '\n']) {
            anyhow::bail!(
                "RPC header '{}' expands to a value with a comma or line break, \
                 which cannot be exported via ETH_RPC_HEADERS",
                name
            );
        }
        pairs.push(format!("{name}: {value}"));
    }
    env.insert("ETH_RPC_HEADERS".to_string(), pairs.join(","));
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: all PASS.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo fmt && cargo clippy --all-targets
git add src/variables.rs src/variables/tests.rs
git commit -m "feat: export ETH_RPC_HEADERS for exec and shell"
```

---

### Task 8: Header display in list/show

**Files:**
- Modify: `src/listing.rs`
- Test: `src/listing.rs` tests module

**Interfaces:**
- Produces on `ChainView`:
  - JSON: `rpc_headers: BTreeMap<String, BTreeMap<String, String>>` keyed by presented URL → header name → presented value (`[REDACTED]` unless revealed); `#[serde(skip_serializing_if = "BTreeMap::is_empty")]` so header-free chains keep today's exact JSON shape.
  - Human `show`/`verbose`: an `RPC Headers` tree line after `Active RPC`, only when the selected endpoint has headers, e.g. `├─ RPC Headers: x-internal-service-secret: [REDACTED]`.
- Consumes: `RpcEndpoint`, `selected_endpoint()` (Task 2).

- [ ] **Step 1: Write failing tests**

Add to `src/listing.rs` tests (extend the `chain()` fixture with a headered endpoint):

```rust
fn chain_with_headers(name: &str, id: u64) -> ChainDefinition {
    let mut definition = chain(name, id, None);
    let mut headers = std::collections::BTreeMap::new();
    headers.insert(
        "x-internal-service-secret".to_string(),
        "header-secret".to_string(),
    );
    definition.select_rpc_with_headers(
        "https://gw.example.com/rpc/1".to_string(),
        headers,
    );
    definition
}

#[test]
fn redacted_views_show_header_names_but_never_values() {
    let chain = chain_with_headers("ethereum", 1);
    for output in [
        show(&chain, None, SecretVisibility::Redacted),
        show_json(&chain, None, SecretVisibility::Redacted).unwrap(),
        verbose(std::slice::from_ref(&chain), None, SecretVisibility::Redacted),
    ] {
        assert!(output.contains("x-internal-service-secret"), "{output}");
        assert!(!output.contains("header-secret"), "{output}");
    }
}

#[test]
fn revealed_view_shows_header_values() {
    let chain = chain_with_headers("ethereum", 1);
    let json = show_json(&chain, None, SecretVisibility::Revealed).unwrap();
    assert!(json.contains("header-secret"));
}

#[test]
fn headerless_chains_keep_prior_json_shape() {
    let chain = chain("ethereum", 1, None);
    let json = show_json(&chain, None, SecretVisibility::Redacted).unwrap();
    assert!(!json.contains("rpc_headers"), "{json}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib listing`
Expected: FAIL.

- [ ] **Step 3: Implement**

`ChainView` gains two fields (import `std::collections::BTreeMap`):

```rust
#[serde(skip_serializing_if = "BTreeMap::is_empty")]
rpc_headers: BTreeMap<String, BTreeMap<String, String>>,
#[serde(skip)]
selected_rpc_headers: Vec<String>,
```

In `ChainView::new`, after `present`:

```rust
let present_header_value = |value: &str| {
    if reveal {
        value.to_string()
    } else {
        "[REDACTED]".to_string()
    }
};
```

and in the struct literal:

```rust
rpc_headers: chain
    .rpc_urls
    .iter()
    .filter(|endpoint| !endpoint.headers.is_empty())
    .map(|endpoint| {
        (
            present(&endpoint.url),
            endpoint
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), present_header_value(value)))
                .collect(),
        )
    })
    .collect(),
selected_rpc_headers: chain
    .selected_endpoint()
    .map(|endpoint| {
        endpoint
            .headers
            .iter()
            .map(|(name, value)| format!("{name}: {}", present_header_value(value)))
            .collect()
    })
    .unwrap_or_default(),
```

In `description()`, directly after the `Active RPC` writeln, add:

```rust
if !self.selected_rpc_headers.is_empty() {
    writeln!(
        output,
        "{}─ {}: {}",
        style("├").dim(),
        style("RPC Headers").cyan(),
        style(&self.selected_rpc_headers.join(", ")).green()
    )
    .expect("writing to a String cannot fail");
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: all PASS (existing listing tests confirm no regression for header-free chains).

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo fmt && cargo clippy --all-targets
git add src/listing.rs
git commit -m "feat: display RPC header names in list and show"
```

---

### Task 9: End-to-end config test, docs, changelog

**Files:**
- Modify: `tests/cli.rs` (one new integration test)
- Modify: `README.md`, `CHANGELOG.md`

**Interfaces:**
- Consumes: everything above; no new interfaces.

- [ ] **Step 1: Write the integration test**

Add to `tests/cli.rs`, using the file's existing helpers: `chainz(home)` builds an `assert_cmd::Command` with an isolated HOME/XDG dir, `one_shot_rpc(chain_id)` serves one canned `eth_chainId` response, `config_path(home)` locates the written config:

```rust
#[test]
fn add_with_header_persists_object_form_and_show_redacts_value() {
    let home = TempDir::new().unwrap();
    let rpc = one_shot_rpc(424242);

    chainz(home.path())
        .args([
            "add",
            "--name",
            "headerchain",
            "--chain-id",
            "424242",
            "--rpc-url",
            &rpc,
            "--header",
            "x-internal-service-secret: header-secret",
        ])
        .assert()
        .success();

    // Config stores the object form; header-free entries elsewhere stay strings.
    let raw = fs::read_to_string(config_path(home.path())).unwrap();
    assert!(raw.contains(r#""x-internal-service-secret": "header-secret""#), "{raw}");

    // Redacted show: header name visible, value hidden.
    chainz(home.path())
        .args(["show", "headerchain", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("x-internal-service-secret"))
        .stdout(predicate::str::contains("header-secret").not());

    // --show-secrets reveals the stored value.
    chainz(home.path())
        .args(["show", "headerchain", "--json", "--show-secrets"])
        .assert()
        .success()
        .stdout(predicate::str::contains("header-secret"));
}
```

Note: `one_shot_rpc` answers exactly one request — the `add` probe. The probe must SEND the header for this test to be meaningful only at the unit level (Task 5 covers that); here it seals flag→parse→probe→persist→display. `add` emits an argv warning for the literal header value — that's expected stderr, not a failure.

- [ ] **Step 2: Run it**

Run: `cargo test --test cli -- add_with_header_persists`
Expected: PASS (all functionality already landed; this test is the cross-layer seal).

- [ ] **Step 3: README + CHANGELOG**

README: add a `### Custom RPC headers` subsection under the existing RPC/usage docs:

````markdown
### Custom RPC headers

Some RPC endpoints (internal gateways, private proxies) authenticate with a
custom HTTP header instead of a URL-embedded key:

```bash
# Store the secret once
chainz var set GW_SECRET --stdin

# Attach the header to a specific RPC URL
chainz update ethereum \
  --rpc-url https://gateway.example.com/rpc/1 \
  --header "x-internal-service-secret: ${GW_SECRET}"
```

Headers are scoped to their URL — fallback RPCs on the same chain never
receive them. `chainz doctor` sends them when probing, and `exec`/`shell`
export them as `ETH_RPC_HEADERS`, which foundry's `cast` and `forge` pick up
automatically. Header values are redacted in `list`/`show` output unless
`--show-secrets` is passed.
````

Also add `x-internal-service-secret`-free mention of `--header` to the Features bullet list ("Multiple RPC support per chain" bullet is a natural neighbor).

CHANGELOG: add under an `## Unreleased` heading (create if absent, matching the file's existing format):

```markdown
- Per-RPC custom HTTP headers: `--header "name: value"` on `add`/`update`,
  sent in health probes and exported to tools as `ETH_RPC_HEADERS`
```

- [ ] **Step 4: Full verification**

```bash
cargo fmt && cargo clippy --all-targets && cargo test
```

Expected: everything green.

- [ ] **Step 5: Commit**

```bash
git add tests/cli.rs README.md CHANGELOG.md
git commit -m "test: end-to-end RPC header config coverage; document --header"
```
