# Terminal Experience Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make chainz's CLI fast and coherent: one style system, streaming RPC checks with a 4s deadline, download progress, and a first-class `chainz shell`.

**Architecture:** A new `src/ui.rs` module becomes the single output vocabulary (built on `console`, replacing `colored`). RPC probing in `chain/rpc.rs` becomes an event stream (`probe_urls` → mpsc receiver) consumed by live renderers in the wizard and doctor. Chainlist downloads stream with an indicatif progress bar. `chainz shell` reuses exec's chain resolution and env machinery.

**Tech Stack:** Rust (edition 2024), console 0.16, indicatif 0.18, tokio mpsc, dialoguer (unchanged), assert_cmd tests.

## Global Constraints

- `--json` output and non-TTY output must remain byte-identical to today (integration tests pin much of it; do not break them).
- No new prompt library; dialoguer stays (bumped 0.11 → 0.12 for console 0.16 alignment).
- **Raw-vs-expanded rule:** config storage and on-screen display always use raw URLs (with `${VAR}` intact); `expand_rpc_url` output exists only inside probe calls and exec-time resolution. Never store or print an expanded URL.
- Lazy-key rule: no command may touch key backends unless `@key`/`@wallet` is referenced (shell must not).
- All commits go through `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` green.
- RPC probe deadline: exactly 4 seconds (`CHECK_DEADLINE`), replacing the 10s worst case in wizard/doctor paths.
- GPG signing can time out: on `gpg: signing failed: Timeout`, wait ~8s and retry the commit once; if it fails again, leave changes staged and note it.

---

### Task 1: `ui` module and style dependencies

**Files:**
- Modify: `Cargo.toml` (add console, indicatif under `[dependencies]`)
- Create: `src/ui.rs`
- Modify: `src/lib.rs` (add `pub mod ui;`)

**Interfaces:**
- Produces: `ui::header(&str) -> String`, `ui::success(&str) -> String`, `ui::warn(&str) -> String`, `ui::fail(&str) -> String`, `ui::item(&str) -> String`, `ui::dim(&str) -> String`, `ui::emph(&str) -> String`. All return plain `String`s (styled only when stdout is a TTY — `console` handles detection).

- [ ] **Step 1: Add dependencies**

In `Cargo.toml` `[dependencies]` (keep alphabetical-ish placement near clap), and bump dialoguer so the whole stack shares console 0.16:

```toml
console = "0.16"
indicatif = "0.18"
dialoguer = { version = "0.12", features = ["fuzzy-select"] }
```

Run `cargo tree -d 2>/dev/null | grep -A2 console` — expected: no duplicate console versions. Fix any trivial dialoguer 0.11→0.12 API breaks surfaced by `cargo check` (the Select/FuzzySelect/Input/Confirm builder APIs are stable across this bump; if a method was renamed, follow the compiler suggestion).

- [ ] **Step 2: Write the failing test**

Create `src/ui.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_contain_glyph_and_message() {
        assert!(success("done").contains("✓"));
        assert!(success("done").contains("done"));
        assert!(warn("careful").contains("⚠"));
        assert!(fail("broken").contains("✗"));
        assert!(item("step").contains("▸"));
        assert!(header("Section").contains("Section"));
        assert!(header("Section").contains("═"));
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test ui:: 2>&1 | tail -5`
Expected: compile error — `success` etc. not found.

- [ ] **Step 4: Implement the module**

Prepend to `src/ui.rs`:

```rust
//! The single output vocabulary for chainz. Every user-facing styled line
//! goes through these helpers so glyphs and palette stay coherent.
//! `console` styles only when stderr/stdout is a TTY and honors NO_COLOR.

use console::style;

pub fn header(title: &str) -> String {
    format!(
        "\n{}\n{}",
        style(title).cyan().bold(),
        style("═".repeat(50)).dim()
    )
}

pub fn success(msg: &str) -> String {
    format!("{} {}", style("✓").green(), msg)
}

pub fn warn(msg: &str) -> String {
    format!("{} {}", style("⚠").yellow(), msg)
}

pub fn fail(msg: &str) -> String {
    format!("{} {}", style("✗").red(), msg)
}

pub fn item(msg: &str) -> String {
    format!("{} {}", style("▸").cyan(), msg)
}

pub fn dim(msg: &str) -> String {
    style(msg).dim().to_string()
}

pub fn emph(msg: &str) -> String {
    style(msg).yellow().to_string()
}
```

Add `pub mod ui;` to `src/lib.rs` (alphabetical: after `pub mod opt;`... keep the existing sorted order — insert between `pub mod opt;` and `pub mod variables;`).

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test ui:: 2>&1 | tail -3`
Expected: `helpers_contain_glyph_and_message ... ok`

- [ ] **Step 6: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat: add ui module (console/indicatif style stack)"
```

---

### Task 2: Migrate all output to `ui`, drop `colored`

**Files:**
- Modify: `src/chain/mod.rs` (both `Display` impls)
- Modify: `src/chain/wizard.rs` (headers, ✓/✗ lines)
- Modify: `src/doctor.rs` (all styled lines)
- Modify: `src/init.rs` (banner)
- Modify: `Cargo.toml` (remove `colored`)

**Interfaces:**
- Consumes: every `ui::` helper from Task 1.
- Produces: no new interfaces; zero remaining `colored` usage (`grep -rn "colored" src/` returns nothing).

- [ ] **Step 1: Migrate `src/chain/mod.rs`**

Replace `use colored::*;` with `use crate::ui;` and `use console::style;`. In `Display for ChainDefinition`, the pattern for each line becomes (example for the name line; apply the same mechanical substitution to every line — `bright_blue()` → `style(..).cyan()`, `yellow()` → `ui::emph(..)`, `bright_green()` → `style(..).green()`, `bright_red()` → `style(..).red()`, `bright_black()` → `style(..).dim()`):

```rust
writeln!(
    f,
    "{}: {}{}",
    style("Chain").cyan().bold(),
    ui::emph(&self.name),
    if self.aliases.is_empty() {
        String::new()
    } else {
        ui::dim(&format!(" ({})", self.aliases.join(", ")))
    }
)?;
```

- [ ] **Step 2: Migrate `src/chain/wizard.rs`**

Replace section banners:

```rust
// before
println!("\n{}", "Chain Selection".bright_blue().bold());
println!("{}", "═".bright_black().repeat(50));
// after
println!("{}", crate::ui::header("Chain Selection"));
```

Replace status lines: `format!("{} {}", "✓".bright_green(), ...)` → `crate::ui::success(&format!("{}", ...))`, same for ✗ → `ui::fail`, ⋯ pending lines → `ui::dim(&format!("⋯ {}", ...))`. Remove `use colored::*;`.

- [ ] **Step 3: Migrate `src/doctor.rs` and `src/init.rs` the same way**

Every `"✓".bright_green()` composite becomes `ui::success(...)`, `"✗".bright_red()` → `ui::fail(...)`, `"⚠".bright_yellow()` → `ui::warn(...)`, section titles → `ui::header(...)`.

- [ ] **Step 4: Remove colored**

Delete the `colored = "3"` line from `Cargo.toml`. Run `grep -rn "colored" src/` — expected: no output.

- [ ] **Step 5: Full verify (output contracts)**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test 2>&1 | grep 'test result'`
Expected: all suites pass — the integration tests pin non-TTY output (plain text), which `console` produces identically.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "refactor: route all styled output through ui, drop colored"
```

---

### Task 3: Streaming RPC probes with 4s deadline; collapse the old RPC surface

**Files:**
- Modify: `src/chain/rpc.rs` (new stream API; delete `Rpc` plumbing)
- Modify: `src/chain/mod.rs` (delete `Rpc` struct + its `Display`; update `pub use`)
- Test: unit tests in `src/chain/rpc.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces (the complete public surface of `chain/rpc.rs` after this task):
  - `pub const CHECK_DEADLINE: Duration = Duration::from_secs(4);`
  - `pub async fn check_url(rpc_url: &str, expected_chain_id: u64) -> Result<()>` — single-URL validation, NO deadline (explicit validation paths keep the provider's own 10s connect timeout). `test_rpc` is folded into it.
  - `pub async fn probe(url: &str, expected_chain_id: u64) -> (bool, Duration)` — one health probe under `CHECK_DEADLINE`, with latency. The single definition used by sweeps.
  - `pub struct ProbeResult { pub index: usize, pub healthy: bool, pub latency: Duration }`
  - `pub fn probe_urls(urls: &[String], expected_chain_id: u64) -> tokio::sync::mpsc::Receiver<ProbeResult>` — yields exactly `urls.len()` results in completion order.
  - `pub fn rank_by_health(results: &[ProbeResult]) -> Vec<usize>` — healthy ascending by latency, then unhealthy in original index order.
  - `pub async fn check_urls(urls: &[String], expected_chain_id: u64) -> Vec<bool>` — collecting wrapper over `probe_urls` (used by doctor `--fix`).
- **Deletes** (collapse onto the new seam — verify with grep before finishing):
  - `Rpc` struct and `impl Display for Rpc` (chain/mod.rs) — its only job was pairing a URL with a provider nobody reads outside the check itself
  - `resolve_rpc`, `resolve_rpcs` (rpc.rs) and the `pub use rpc::{resolve_rpc, resolve_rpcs}` re-export (chain/mod.rs)
  - `test_rpc` (folded into `check_url`)
  - `create_provider` becomes private (`fn`, not `pub fn`)
  - Callers in wizard.rs are updated in Task 4; this task may leave wizard.rs temporarily calling shims — do Tasks 3+4 in one PR-worth of commits, compile green at each commit by updating wizard call sites in the same commit where their callee is deleted.

- [ ] **Step 1: Write the failing tests**

Append to `src/chain/rpc.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn result(index: usize, healthy: bool, ms: u64) -> ProbeResult {
        ProbeResult {
            index,
            healthy,
            latency: Duration::from_millis(ms),
        }
    }

    #[test]
    fn rank_healthy_fastest_first_then_unhealthy_in_order() {
        let results = vec![
            result(0, false, 4000),
            result(1, true, 150),
            result(2, true, 20),
            result(3, false, 4000),
        ];
        assert_eq!(rank_by_health(&results), vec![2, 1, 0, 3]);
    }

    #[tokio::test]
    async fn probe_urls_reports_every_url() {
        // connection-refused fails fast; no network needed
        let urls = vec![
            "http://localhost:1".to_string(),
            "http://localhost:2".to_string(),
        ];
        let mut rx = probe_urls(&urls, 1);
        let mut seen = Vec::new();
        while let Some(result) = rx.recv().await {
            assert!(!result.healthy);
            seen.push(result.index);
        }
        seen.sort();
        assert_eq!(seen, vec![0, 1]);
    }

    #[tokio::test]
    async fn probe_urls_empty_input_closes_immediately() {
        let mut rx = probe_urls(&[], 1);
        assert!(rx.recv().await.is_none());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test chain::rpc 2>&1 | tail -5`
Expected: compile error — `ProbeResult`/`probe_urls`/`rank_by_health` not found.

- [ ] **Step 3: Implement the new surface and delete the old one**

Rewrite `src/chain/rpc.rs` to exactly this shape (plus the existing `use` lines for alloy/anyhow):

```rust
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use anyhow::Result;
use std::time::{Duration, Instant};

/// Global deadline for a single RPC health probe in interactive sweeps.
pub const CHECK_DEADLINE: Duration = Duration::from_secs(4);

/// Test whether an (already-expanded) RPC URL serves the expected chain id.
/// No sweep deadline: explicit single-URL validation keeps the provider's
/// own 10s connect timeout. The single definition of "is this RPC healthy".
pub async fn check_url(rpc_url: &str, expected_chain_id: u64) -> Result<()> {
    let provider = create_provider(rpc_url).await?;
    let chain_id = provider
        .get_chain_id()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {}: {}", rpc_url, e))?;
    if chain_id != expected_chain_id {
        anyhow::bail!(
            "Chain ID mismatch on {}: expected {}, got {}",
            rpc_url,
            expected_chain_id,
            chain_id
        );
    }
    Ok(())
}

/// One health probe under CHECK_DEADLINE, with measured latency.
pub async fn probe(url: &str, expected_chain_id: u64) -> (bool, Duration) {
    let start = Instant::now();
    let healthy = tokio::time::timeout(CHECK_DEADLINE, check_url(url, expected_chain_id))
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);
    (healthy, start.elapsed())
}

pub struct ProbeResult {
    pub index: usize,
    pub healthy: bool,
    pub latency: Duration,
}

/// Probe URLs concurrently, yielding each result as it lands (completion
/// order). The receiver yields exactly `urls.len()` results, then closes.
pub fn probe_urls(
    urls: &[String],
    expected_chain_id: u64,
) -> tokio::sync::mpsc::Receiver<ProbeResult> {
    let (tx, rx) = tokio::sync::mpsc::channel(urls.len().max(1));
    for (index, url) in urls.iter().cloned().enumerate() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let (healthy, latency) = probe(&url, expected_chain_id).await;
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

/// Picker ordering: healthy probes fastest-first, then unhealthy ones in
/// their original order.
pub fn rank_by_health(results: &[ProbeResult]) -> Vec<usize> {
    let mut healthy: Vec<&ProbeResult> = results.iter().filter(|r| r.healthy).collect();
    healthy.sort_by_key(|r| r.latency);
    let mut unhealthy: Vec<&ProbeResult> = results.iter().filter(|r| !r.healthy).collect();
    unhealthy.sort_by_key(|r| r.index);
    healthy
        .into_iter()
        .chain(unhealthy)
        .map(|r| r.index)
        .collect()
}

/// Collecting wrapper: one health flag per input URL, in input order.
pub async fn check_urls(urls: &[String], expected_chain_id: u64) -> Vec<bool> {
    let mut results = vec![false; urls.len()];
    let mut rx = probe_urls(urls, expected_chain_id);
    while let Some(result) = rx.recv().await {
        results[result.index] = result.healthy;
    }
    results
}

async fn create_provider(rpc_url: &str) -> Result<DynProvider> {
    let provider = tokio::time::timeout(
        Duration::from_secs(10),
        ProviderBuilder::new().connect(rpc_url),
    )
    .await
    .map_err(|_| anyhow::anyhow!("RPC connection timed out: {}", rpc_url))??;
    Ok(provider.erased())
}
```

Note what is GONE from this file: `Rpc`, `resolve_rpc`, `resolve_rpcs`, `test_rpc`; `create_provider` is now private. In `src/chain/mod.rs`: delete the `Rpc` struct, its `Display` impl, and the `pub use rpc::{resolve_rpc, resolve_rpcs};` line. Task 4 updates the wizard call sites — to keep every commit green, land Task 3 + Task 4 wizard updates in the same commit if the intermediate state doesn't compile.

Verify the collapse: `grep -rn "resolve_rpc\|test_rpc\|Rpc {" src/` — expected: no hits outside comments after Task 4.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test chain::rpc 2>&1 | tail -4` then `cargo test 2>&1 | grep 'test result'`
Expected: new tests pass; full suite stays green.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat: streaming RPC probes with 4s deadline and health ranking"
```

---

### Task 4: Live-updating RPC test in the wizard; raw-URL boundary

**Files:**
- Modify: `src/chain/wizard.rs` (`select_rpc`, `select_manual_rpc`, `UpdateArgs::handle`, `AddArgs::handle_interactive`, `AddArgs::handle_non_interactive`)

**Interfaces:**
- Consumes: `probe_urls`, `rank_by_health`, `ProbeResult`, `check_url` (Task 3); `ui::success/fail/dim/emph` (Task 1); `GlobalVariables::expand_rpc_url`.
- Produces: `pub async fn select_rpc(chain_name: &str, chain_id: u64, urls: Vec<String>, globals: &GlobalVariables) -> Result<String>` — takes and **returns raw URLs**; expansion happens only for probing. `select_manual_rpc(chain_id: u64, globals: &GlobalVariables) -> Result<String>` likewise returns the raw URL the user typed.
- Boundary being enforced: raw URLs on screen and in config (no API keys displayed or persisted); expanded URLs exist only inside probe calls. This *fixes* the old flow, which displayed and stored expanded URLs.

- [ ] **Step 1: Rewrite `select_rpc`**

```rust
use crate::ui;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

/// Pick an RPC for a chain. `urls` are raw (may contain ${VAR}); they are
/// expanded only for probing. Displays and returns raw URLs so secrets are
/// never shown on screen or written to config.
pub async fn select_rpc(
    chain_name: &str,
    chain_id: u64,
    urls: Vec<String>,
    globals: &GlobalVariables,
) -> Result<String> {
    let expanded: Vec<String> = urls.iter().map(|u| globals.expand_rpc_url(u)).collect();

    // Live per-RPC status lines; hidden automatically when not a TTY
    let multi = MultiProgress::new();
    let bars: Vec<ProgressBar> = urls
        .iter()
        .map(|url| {
            let bar = multi.add(ProgressBar::new_spinner());
            bar.set_style(ProgressStyle::with_template("{spinner} {msg}").unwrap());
            bar.enable_steady_tick(std::time::Duration::from_millis(120));
            bar.set_message(url.clone());
            bar
        })
        .collect();

    let mut results = Vec::with_capacity(urls.len());
    let mut rx = probe_urls(&expanded, chain_id);
    while let Some(result) = rx.recv().await {
        let bar = &bars[result.index];
        if result.healthy {
            bar.finish_with_message(ui::success(&format!(
                "{}  {}ms",
                urls[result.index],
                result.latency.as_millis()
            )));
        } else {
            bar.finish_with_message(ui::fail(&format!(
                "{}  {}",
                urls[result.index],
                ui::dim("unreachable")
            )));
        }
        results.push(result);
    }

    // Healthy-first, fastest-first picker over RAW urls
    let order = rank_by_health(&results);
    let mut items: Vec<String> = order
        .iter()
        .map(|&i| {
            let r = results.iter().find(|r| r.index == i).unwrap();
            if r.healthy {
                format!("{} ({}ms)", urls[i], r.latency.as_millis())
            } else {
                format!("{} (unreachable)", urls[i])
            }
        })
        .collect();
    items.push("Enter RPC URL manually...".to_string());

    let selection = fuzzy_select(
        &format!("Select an RPC URL for {}", ui::emph(chain_name)),
        &items,
        0,
    )?;

    if selection == items.len() - 1 {
        select_manual_rpc(chain_id, globals).await
    } else {
        Ok(urls[order[selection]].clone())
    }
}
```

- [ ] **Step 2: Rewrite `select_manual_rpc` (returns raw String, uses check_url)**

```rust
async fn select_manual_rpc(chain_id: u64, globals: &GlobalVariables) -> Result<String> {
    loop {
        let rpc_url: String = text_input("Enter RPC URL", None)?;
        println!("Testing RPC...");
        if check_url(&globals.expand_rpc_url(&rpc_url), chain_id)
            .await
            .is_ok()
        {
            println!("{}", ui::success("RPC working"));
            return Ok(rpc_url);
        }
        println!("{}", ui::fail("RPC failed. Try again? (ESC to exit)"));
    }
}
```

- [ ] **Step 3: Update the three call sites**

`UpdateArgs::handle` (RPC branch) — no more `resolve_rpcs`:

```rust
let available_rpcs = chainlist_entry
    .map(|c| c.rpc)
    .unwrap_or_else(|_| chain.rpc_urls.clone());
let new_rpc = select_rpc(
    &chain.name,
    chain.chain_id,
    available_rpcs,
    &chainz.config.globals,
)
.await?;
chain.selected_rpc = new_rpc;
```

`AddArgs::handle_interactive` — RPC selection branch:

```rust
select_rpc(
    &name,
    selected_chain.chain_id,
    selected_chain.rpc.clone(),
    &chainz.config.globals,
)
.await?
```

and its explicit `--rpc-url` branch plus `handle_non_interactive`'s test both become (the id expression is `selected_chain.chain_id` in the interactive branch and the local `chain_id` in `handle_non_interactive`):

```rust
println!("Testing RPC...");
check_url(
    &chainz.config.globals.expand_rpc_url(rpc_url),
    selected_chain.chain_id, // `chain_id` in handle_non_interactive
)
.await?;
println!("{}", crate::ui::success("RPC working"));
```

Update the wizard's imports: `use super::rpc::{check_url, probe_urls, rank_by_health};` — `Rpc`, `resolve_rpc`, `resolve_rpcs`, `test_rpc`, `create_provider` no longer exist.

- [ ] **Step 4: Verify the collapse and the suite**

Run: `grep -rn "resolve_rpc\|test_rpc\|Rpc" src/ | grep -v ProbeResult | grep -v rpc_url | grep -v check` — expected: no hits.
Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test 2>&1 | grep 'test result'`
Expected: all green (ranking logic covered by Task 3; wizard path is interactive).

- [ ] **Step 5: Manual smoke (needs a TTY)**

Run: `cargo run -- add`, observe: spinners resolve individually, picker opens ≤4s after the slowest probe, healthy RPCs first with latencies, raw `${VAR}` URLs shown unexpanded.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat: live latency-ranked RPC picker; raw-URL display/storage boundary"
```

---

### Task 5: Doctor uses the probe stream

**Files:**
- Modify: `src/doctor.rs` (`check_rpc_health`)

**Interfaces:**
- Consumes: `probe` (Task 3 — chains have different expected ids, so one `probe` per chain, all spawned concurrently as today), `ui` helpers.
- Produces: `check_rpc_health` keeps its signature; per-chain output line gains latency and shows the **raw** URL: `✓ ethereum (https://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}) 89ms`.

- [ ] **Step 1: Rework the sweep**

The per-chain task body becomes a single `probe` call (drop the local `healthy` helper — `probe` owns the deadline and latency). Note: expand for the probe, display the raw URL (Global Constraints raw-vs-expanded rule — the old code printed expanded URLs, leaking keys to screen):

```rust
let checks: Vec<_> = chains
    .iter()
    .map(|c| {
        let expanded = chainz.config.globals.expand_rpc_url(&c.selected_rpc);
        let raw = c.selected_rpc.clone();
        let chain_id = c.chain_id;
        let name = c.name.clone();
        tokio::spawn(async move {
            let (healthy, latency) = crate::chain::rpc::probe(&expanded, chain_id).await;
            (name, healthy, raw, latency)
        })
    })
    .collect();

let mut failed = Vec::new();
for handle in checks {
    let Ok((name, is_healthy, raw_url, latency)) = handle.await else {
        continue;
    };
    if is_healthy {
        println!(
            "  {}",
            ui::success(&format!("{} ({}) {}ms", name, raw_url, latency.as_millis()))
        );
    } else {
        report.failures += 1;
        println!("  {}", ui::fail(&format!("{} ({})", name, raw_url)));
        failed.push(name);
    }
}
```

- [ ] **Step 2: Fix the raw-URL leak in `fix_rpcs`**

In `fix_rpcs`, the success line currently prints `expanded[i]`; change it to the raw candidate so switched-to URLs with `${VAR}` never render expanded:

```rust
println!(
    "  {}",
    ui::success(&format!("{}: switched to {}", name, candidates[i]))
);
```

- [ ] **Step 3: Verify (integration contract)**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test 2>&1 | grep 'test result'`
Expected: green — `doctor_reports_dead_rpc_and_exits_nonzero` still matches (the ✗ line still contains the chain name; failure summary unchanged). If a test asserts on text that changed, fix the assertion to the new line shape — the *contract* (name present, nonzero exit) is what matters.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat: doctor health sweep shows latency, honors 4s probe deadline"
```

---

### Task 6: Chainlist download progress bar

**Files:**
- Modify: `src/chainlist.rs` (`fetch_from_network`)

**Interfaces:**
- Consumes: `indicatif::{ProgressBar, ProgressStyle}`.
- Produces: `fetch_from_network` keeps its `-> Result<String>` signature; body is streamed with a byte progress bar (auto-hidden when not a TTY), spinner fallback when content-length is unknown.

- [ ] **Step 1: Implement streaming download**

```rust
async fn fetch_from_network() -> Result<String> {
    use indicatif::{ProgressBar, ProgressStyle};

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut response = client
        .get(CHAINLIST_URL)
        .send()
        .await?
        .error_for_status()?;

    let bar = match response.content_length() {
        Some(len) => {
            let bar = ProgressBar::new(len);
            bar.set_style(
                ProgressStyle::with_template(
                    "downloading chainlist {bytes}/{total_bytes} [{bar:30}] {eta}",
                )
                .unwrap(),
            );
            bar
        }
        None => {
            let bar = ProgressBar::new_spinner();
            bar.set_message("downloading chainlist…");
            bar
        }
    };

    let mut body = Vec::with_capacity(response.content_length().unwrap_or(0) as usize);
    while let Some(chunk) = response.chunk().await? {
        body.extend_from_slice(&chunk);
        bar.inc(chunk.len() as u64);
    }
    bar.finish_and_clear();
    Ok(String::from_utf8(body)?)
}
```

Note the overall timeout rises 10s → 30s: the old 10s covered header-arrival for a small request; a multi-MB streamed body on slow links needs headroom, and progress is now visible so waiting is honest.

- [ ] **Step 2: Verify**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test 2>&1 | grep 'test result'`
Expected: green (chainlist deserialization tests unaffected).

Manual smoke: `cargo run -- add --refresh` on a TTY shows the byte bar.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat: progress bar for chainlist download"
```

---

### Task 7: `chainz shell`

**Files:**
- Modify: `src/opt.rs` (new `Shell` command)
- Modify: `src/main.rs` (handler)
- Test: `tests/cli.rs`

**Interfaces:**
- Consumes: `ChainVariables::new(&chain, &[])` (no args → lazy rule keeps key backends untouched), exec's chain resolution pattern, `ui::item/dim`.
- Produces: `chainz shell [name_or_id]` — spawns `$SHELL` (fallback `sh`) with chain env + `CHAINZ_CHAIN`, PS1 prefixed `(⛓ <chain>) `, exit code passthrough.

- [ ] **Step 1: Write the failing integration tests**

Append to `tests/cli.rs`:

```rust
#[test]
fn shell_sets_env_and_passes_exit_code() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);

    // A fake "shell" that proves env vars arrive and exit codes pass through
    let fake_shell = home.path().join("fakeshell.sh");
    fs::write(
        &fake_shell,
        "#!/bin/sh\necho chain=$CHAINZ_CHAIN rpc=$ETH_RPC_URL key=${RAW_PRIVATE_KEY:-unset}\nexit 3\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_shell, fs::Permissions::from_mode(0o755)).unwrap();
    }

    chainz(home.path())
        .env("SHELL", &fake_shell)
        .args(["shell", "testchain"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains(
            "chain=testchain rpc=http://localhost:1 key=unset",
        ));
}

#[test]
fn shell_uses_default_chain_when_omitted() {
    let home = TempDir::new().unwrap();
    seed_config(home.path(), &[("testchain", 31337)]);
    chainz(home.path())
        .args(["use", "testchain"])
        .assert()
        .success();

    let fake_shell = home.path().join("fakeshell.sh");
    fs::write(&fake_shell, "#!/bin/sh\necho in=$CHAINZ_CHAIN\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_shell, fs::Permissions::from_mode(0o755)).unwrap();
    }

    chainz(home.path())
        .env("SHELL", &fake_shell)
        .arg("shell")
        .assert()
        .success()
        .stdout(predicate::str::contains("in=testchain"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test cli shell 2>&1 | tail -5`
Expected: FAIL — `unrecognized subcommand 'shell'`.

- [ ] **Step 3: Add the command**

`src/opt.rs`, after `Exec` in `Command`:

```rust
/// Open a subshell with the chain's environment loaded
///
/// Sets ETH_RPC_URL, CHAIN_ID, CHAIN_NAME, VERIFIER_* and CHAINZ_CHAIN,
/// and prefixes PS1 with the chain name for bash-like shells.
/// Key material is NOT loaded into the environment.
///
/// Example: chainz shell base
Shell {
    /// Chain name or ID (default chain or interactive picker if omitted)
    name_or_id: Option<String>,
},
```

`src/main.rs`, new arm next to `Exec` (before it, order cosmetic):

```rust
opt::Command::Shell { name_or_id } => {
    let name_or_id = match name_or_id.or_else(|| chainz.config.default_chain.clone()) {
        Some(id) => id,
        None => select_chain(&chainz)?,
    };
    let chain = chainz.get_chain(&name_or_id)?;
    // Empty command args → lazy rule: key backends are never touched
    let variables = ChainVariables::new(&chain, &[])?;
    let chain_name = chain.definition.name.clone();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());

    eprintln!(
        "{}",
        ui::item(&format!("entering {} shell — ctrl-d to exit", chain_name))
    );
    let ps1 = format!(
        "(⛓ {}) {}",
        chain_name,
        std::env::var("PS1").unwrap_or_default()
    );
    let status = ProcessCommand::new(&shell)
        .envs(variables.as_map())
        .env("CHAINZ_CHAIN", &chain_name)
        .env("PS1", ps1)
        .status()?;
    eprintln!("{}", ui::dim(&format!("left {} shell", chain_name)));
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}
```

Add `use chainz::ui;` to main.rs imports.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test cli shell 2>&1 | tail -4` then full `cargo test 2>&1 | grep 'test result'`
Expected: both new tests pass; suite green.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add -A && git commit -m "feat: chainz shell — subshell with chain env and prompt indicator"
```

---

### Task 8: README + final verification

**Files:**
- Modify: `README.md`

**Interfaces:**
- Consumes: everything above.
- Produces: documented `shell` command, prompt snippet, probe-deadline note.

- [ ] **Step 1: Document**

In README Quick Start, replace the `chainz exec ethereum -- bash` subshell example with:

```bash
# Open a subshell with chain environment (prompt shows the chain)
chainz shell ethereum
```

Add under Usage, after "Executing Commands":

```markdown
### Chain Shells

`chainz shell [chain]` opens your `$SHELL` with the chain's environment
(`ETH_RPC_URL`, `CHAIN_ID`, `CHAIN_NAME`, `VERIFIER_*`, `CHAINZ_CHAIN`) —
private keys are never injected. Bash prompts get a `(⛓ ethereum)` prefix
automatically; for zsh/starship, add e.g.:

    # starship.toml
    [env_var.CHAINZ_CHAIN]
    format = "(⛓ $env_value) "
```

In the RPC section, note: interactive RPC tests and `doctor` probes time out after 4 seconds per endpoint; results stream in live and pickers list healthy endpoints fastest-first.

- [ ] **Step 2: Full verification**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test 2>&1 | grep 'test result'`
Expected: everything green.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "docs: shell command, prompt snippet, probe deadline"
```
