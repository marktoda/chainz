//! Foundry broadcast output (`broadcast/**/run-latest.json`) as address-book
//! labels: finding run files ([`find_runs`]), reading the contracts they
//! deployed ([`parse`]), and planning the labels to write ([`plan`]) for
//! `chainz addr import`.
//!
//! A run file records one `forge script --broadcast` run on one chain:
//!
//! ```json
//! { "chain": 8453,
//!   "transactions": [{ "hash": "0x…", "transactionType": "CREATE",
//!                      "contractName": "PoolManager", "contractAddress": "0x…",
//!                      "additionalContracts": [{ "transactionType": "CREATE2",
//!                          "contractName": "Hook", "address": "0x…", "initCode": "0x…" }] }],
//!   "receipts": [{ "transactionHash": "0x…", "status": "0x1" }] }
//! ```
//!
//! Multi-chain runs (`broadcast/multi/<script>-latest/run.json`) hold one
//! such object per chain under `deployments`.

use crate::address::Address;
use crate::model::Config;
use crate::names;
use crate::names::check_label_name;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One contract a run deployed, or tried to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployed {
    /// The contract name forge recorded (`UniversalRouter`), if any.
    pub contract: Option<String>,
    /// Where it was deployed, if the file says.
    pub address: Option<Address>,
    /// Why it cannot be labelled, whatever the config: the transaction
    /// failed, has no receipt, or the entry has no name or address.
    pub problem: Option<String>,
}

/// The deployments of one run on one chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// The run file.
    pub file: PathBuf,
    /// The chain ID it ran on.
    pub chain_id: u64,
    pub contracts: Vec<Deployed>,
}

/// `PascalCase` contract name → kebab-case label name:
/// `UniversalRouter` → `universal-router`, `ERC20Mock` → `erc20-mock`,
/// `V4Router` → `v4-router`, `WETHGateway` → `weth-gateway`. A word starts
/// at an uppercase letter that follows a lowercase letter or a digit, or
/// that ends an acronym (`HTTPServer` → `http-server`). Anything outside
/// `[A-Za-z0-9]` becomes one `-`; a `path:Name` artifact id keeps `Name`.
pub fn kebab(contract: &str) -> String {
    let name = contract.rsplit(':').next().unwrap_or(contract);
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_ascii_alphanumeric() {
            if !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
            continue;
        }
        if c.is_ascii_uppercase() && i > 0 && !out.is_empty() && !out.ends_with('-') {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(char::is_ascii_lowercase);
            if prev.is_ascii_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_ascii_uppercase() && next_lower)
            {
                out.push('-');
            }
        }
        out.push(c.to_ascii_lowercase());
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The run files under each of `paths`, in order, without repeats.
///
/// A file is taken as it is, whatever its name (a `run-<timestamp>.json`
/// or a dry run, when named). A directory is searched for its Foundry
/// broadcast output: a `broadcast` directory (or one inside it) is walked
/// for `run-latest.json` (and `multi/<script>-latest/run.json`), skipping
/// `dry-run` directories; any other directory is searched for projects,
/// and a project's `broadcast/` is used without looking further into the
/// project (so dependencies under `lib/` are not imported). Hidden
/// directories and `node_modules` are not searched.
pub fn find_runs(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for path in paths {
        let meta =
            std::fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
        let before = out.len();
        if meta.is_file() {
            out.push(path.clone());
        } else if path.components().any(|c| c.as_os_str() == "broadcast") {
            walk_broadcast(path, &mut out)?;
        } else {
            find_projects(path, &mut out)?;
        }
        if out.len() == before {
            bail!(
                "no Foundry broadcast output (broadcast/**/run-latest.json) under {}",
                path.display()
            );
        }
    }
    let mut seen = HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    Ok(out)
}

fn sorted_entries(dir: &Path) -> Result<Vec<(PathBuf, std::fs::FileType)>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let entry = entry?;
        entries.push((entry.path(), entry.file_type()?));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

fn find_projects(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let broadcast = dir.join("broadcast");
    if broadcast.is_dir() {
        return walk_broadcast(&broadcast, out);
    }
    for (path, kind) in sorted_entries(dir)? {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if kind.is_dir() && !name.starts_with('.') && name != "node_modules" {
            find_projects(&path, out)?;
        }
    }
    Ok(())
}

fn walk_broadcast(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let latest_dir = dir
        .file_name()
        .is_some_and(|n| n.to_string_lossy().ends_with("-latest"));
    for (path, kind) in sorted_entries(dir)? {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if kind.is_dir() {
            if name != "dry-run" && !name.starts_with('.') {
                walk_broadcast(&path, out)?;
            }
        } else if kind.is_file()
            && (name == "run-latest.json" || (latest_dir && name == "run.json"))
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Read a run file (see [`parse`]).
pub fn read(file: &Path) -> Result<Vec<Run>> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
    parse(&text, file).with_context(|| format!("{} is not a Foundry run file", file.display()))
}

/// The contracts a run file deployed: one [`Run`] per chain (one, or one
/// per entry of a multi-chain run's `deployments`). `file` is recorded in
/// each run, and its directory names the chain when `chain` is missing.
///
/// Deployments are `CREATE`/`CREATE2` transactions and every transaction's
/// `additionalContracts`. When the file has receipts, a transaction whose
/// receipt failed (or that has none) yields its contracts with a
/// [`Deployed::problem`].
pub fn parse(text: &str, file: &Path) -> Result<Vec<Run>> {
    let json: Value = serde_json::from_str(text)?;
    match json.get("deployments").and_then(Value::as_array) {
        Some(runs) => runs.iter().map(|run| parse_run(run, file)).collect(),
        None => Ok(vec![parse_run(&json, file)?]),
    }
}

fn parse_run(json: &Value, file: &Path) -> Result<Run> {
    let Some(transactions) = json.get("transactions").and_then(Value::as_array) else {
        bail!("no `transactions` array");
    };
    let chain_id = json
        .get("chain")
        .and_then(number)
        .or_else(|| {
            let dir = file.parent()?.file_name()?.to_str()?;
            dir.parse().ok()
        })
        .context("no `chain` ID")?;
    // Transaction hash → succeeded, from receipts that say.
    let receipts: HashMap<String, bool> = json
        .get("receipts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let hash = r.get("transactionHash")?.as_str()?.to_ascii_lowercase();
            let ok = r.get("status").is_none_or(|s| s.is_null() || succeeded(s));
            Some((hash, ok))
        })
        .collect();
    let mut contracts = Vec::new();
    for tx in transactions {
        let problem = if receipts.is_empty() {
            None
        } else {
            let hash = tx.get("hash").and_then(Value::as_str);
            match hash.and_then(|h| receipts.get(&h.to_ascii_lowercase())) {
                Some(true) => None,
                Some(false) => Some("transaction failed".to_string()),
                None => Some("no receipt for its transaction".to_string()),
            }
        };
        let kind = tx.get("transactionType").and_then(Value::as_str);
        if matches!(kind, Some("CREATE" | "CREATE2")) {
            contracts.push(deployed(tx, &["contractAddress"], problem.clone()));
        }
        for extra in tx
            .get("additionalContracts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            contracts.push(deployed(
                extra,
                &["address", "contractAddress"],
                problem.clone(),
            ));
        }
    }
    Ok(Run {
        file: file.to_path_buf(),
        chain_id,
        contracts,
    })
}

fn deployed(entry: &Value, address_keys: &[&str], problem: Option<String>) -> Deployed {
    let contract = entry
        .get("contractName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let raw = address_keys
        .iter()
        .find_map(|k| entry.get(*k).and_then(Value::as_str));
    let address = raw.and_then(|s| s.parse::<Address>().ok());
    let problem = problem.or_else(|| match (&contract, raw, address) {
        (_, None, _) => Some("no contract address".to_string()),
        (_, Some(raw), None) => Some(format!("bad contract address '{raw}'")),
        (None, _, _) => Some("no contract name".to_string()),
        _ => None,
    });
    Deployed {
        contract,
        address,
        problem,
    }
}

/// A JSON number or a decimal / `0x` hex string.
fn number(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => match s.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16).ok(),
            None => s.parse().ok(),
        },
        _ => None,
    }
}

/// A receipt status: `"0x1"`, `1` or `true`.
fn succeeded(status: &Value) -> bool {
    match status {
        Value::Bool(b) => *b,
        other => number(other) == Some(1),
    }
}

/// How [`plan`] names and filters.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Prepended to every name (lowercased).
    pub prefix: String,
    /// Only these contracts, by contract name or kebab name (any case).
    /// Empty means every contract.
    pub only: Vec<String>,
    /// Overwrite a label that holds a different address on the chain.
    pub force: bool,
}

/// What importing one deployment does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A new label entry.
    Add,
    /// The label already holds this address on the chain.
    Unchanged,
    /// The label held this other address (with `force`).
    Overwrite(Address),
    /// Not imported, and why.
    Skip(String),
}

impl Action {
    /// `added`, `unchanged`, `overwritten` or `skipped`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Action::Add => "added",
            Action::Unchanged => "unchanged",
            Action::Overwrite(_) => "overwritten",
            Action::Skip(_) => "skipped",
        }
    }

    /// Whether this changes the config.
    pub fn writes(&self) -> bool {
        matches!(self, Action::Add | Action::Overwrite(_))
    }
}

/// One planned label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub file: PathBuf,
    pub chain_id: u64,
    /// The configured chain the label goes on (the first with the ID), if any.
    pub chain: Option<String>,
    pub contract: Option<String>,
    /// The label name, once one could be made.
    pub name: Option<String>,
    pub address: Option<Address>,
    pub action: Action,
}

/// Plan the labels for `runs` against `config`. Each deployment becomes
/// one entry, in order, except repeats of a (chain, name, address) already
/// planned, which are dropped. Labels go on the configured chain with the
/// run's ID (never a fork). When one import names different addresses
/// alike on a chain, the later ones get `-2`, `-3`, ….
pub fn plan(config: &Config, runs: &[Run], options: &Options) -> Vec<Planned> {
    let prefix = options.prefix.to_ascii_lowercase();
    let only: Vec<String> = options
        .only
        .iter()
        .map(|o| o.trim().to_ascii_lowercase())
        .filter(|o| !o.is_empty())
        .collect();
    // (chain ID, base name) → the addresses given it and their final names.
    let mut given: HashMap<(u64, String), Vec<(Address, String)>> = HashMap::new();
    // Final names taken per chain ID in this import.
    let mut taken: HashMap<u64, HashSet<String>> = HashMap::new();
    let mut out = Vec::new();
    for run in runs {
        let chain = config.chain_by_id(run.chain_id).map(|(n, _)| n.to_string());
        for deployed in &run.contracts {
            let base = deployed.contract.as_deref().map(kebab);
            if !only.is_empty() {
                let wanted = deployed
                    .contract
                    .as_deref()
                    .zip(base.as_deref())
                    .is_some_and(|(contract, base)| {
                        only.iter()
                            .any(|o| *o == contract.to_ascii_lowercase() || o == base)
                    });
                if !wanted {
                    continue;
                }
            }
            let mut entry = Planned {
                file: run.file.clone(),
                chain_id: run.chain_id,
                chain: chain.clone(),
                contract: deployed.contract.clone(),
                name: base.as_ref().map(|b| format!("{prefix}{b}")),
                address: deployed.address,
                action: Action::Add,
            };
            let skip = |mut entry: Planned, why: String| {
                entry.action = Action::Skip(why);
                entry
            };
            if let Some(problem) = &deployed.problem {
                out.push(skip(entry, problem.clone()));
                continue;
            }
            let (Some(name), Some(address)) = (entry.name.clone(), deployed.address) else {
                unreachable!("a deployment without a problem has a name and an address");
            };
            let Some(chain) = &chain else {
                let why = format!("chain {} is not configured", run.chain_id);
                out.push(skip(entry, why));
                continue;
            };
            if let Err(e) = check_label_name(&name, config.signers.keys().map(String::as_str)) {
                out.push(skip(entry, e.to_string()));
                continue;
            }
            let names = given.entry((run.chain_id, name.clone())).or_default();
            if names.iter().any(|(a, _)| *a == address) {
                continue; // the same label again (another run of the script)
            }
            let on_chain = taken.entry(run.chain_id).or_default();
            let name = names::unique_name(&name, |n| on_chain.contains(n));
            on_chain.insert(name.clone());
            names.push((address, name.clone()));
            entry.name = Some(name.clone());
            let existing = config.addresses.get(&name).and_then(|e| e.get(chain));
            entry.action = match existing {
                None => Action::Add,
                Some(held) => match held.parse::<Address>() {
                    Ok(held) if held == address => Action::Unchanged,
                    Ok(held) if options.force => Action::Overwrite(held),
                    Ok(held) => Action::Skip(format!(
                        "@{name} is already {held} on {chain} (--force replaces it)"
                    )),
                    Err(_) => Action::Skip(format!(
                        "@{name} holds an invalid address on {chain}; fix it with `chainz config edit`"
                    )),
                },
            };
            out.push(entry);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const A: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";
    const B: &str = "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF";
    const C: &str = "0x6813Eb9362372EEF6200f3b1dbC3f819671cBA69";
    const D: &str = "0x1efF47bc3a10a45D4B230B5d10E37751FE6AA718";
    const H1: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";
    const H2: &str = "0x2222222222222222222222222222222222222222222222222222222222222222";
    const H3: &str = "0x3333333333333333333333333333333333333333333333333333333333333333";

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    #[test]
    fn kebab_splits_words_digits_and_acronyms() {
        for (input, want) in [
            ("UniversalRouter", "universal-router"),
            ("ERC20Mock", "erc20-mock"),
            ("V4Router", "v4-router"),
            ("MockERC20", "mock-erc20"),
            ("WETHGateway", "weth-gateway"),
            ("PoolManager", "pool-manager"),
            ("UniswapV3Factory", "uniswap-v3-factory"),
            ("ERC1967Proxy", "erc1967-proxy"),
            ("Permit2", "permit2"),
            ("L2Bridge", "l2-bridge"),
            ("WETH9", "weth9"),
            ("Counter", "counter"),
            ("counter", "counter"),
            ("Foo_Bar", "foo-bar"),
            ("src/Hook.sol:MyHook", "my-hook"),
            ("__X__", "x"),
        ] {
            assert_eq!(kebab(input), want, "{input}");
        }
    }

    fn run_json() -> Value {
        json!({
            "transactions": [
                {
                    "hash": H1, "transactionType": "CREATE",
                    "contractName": "PoolManager", "contractAddress": A,
                    "function": null, "arguments": ["0x0"],
                    "transaction": {"from": D, "gas": "0x1", "value": "0x0", "input": "0x", "nonce": "0x0", "chainId": "0x2105"},
                    "additionalContracts": [],
                    "isFixedGasLimit": false
                },
                {
                    "hash": H2, "transactionType": "CALL",
                    "contractName": "Deployer", "contractAddress": D,
                    "function": "deploy()",
                    "additionalContracts": [
                        {"transactionType": "CREATE2", "contractName": "MyHook", "address": B, "initCode": "0x00"},
                        {"transactionType": "CREATE", "address": C, "initCode": "0x00"}
                    ]
                },
                {
                    "hash": H3, "transactionType": "CREATE2",
                    "contractName": "Broken", "contractAddress": C,
                    "additionalContracts": []
                }
            ],
            "receipts": [
                {"transactionHash": H1, "status": "0x1", "contractAddress": A},
                {"transactionHash": H2, "status": "0x1"},
                {"transactionHash": H3, "status": "0x0"}
            ],
            "libraries": [], "pending": [], "returns": {},
            "timestamp": 1_700_000_000u64, "chain": 8453, "commit": "abc1234"
        })
    }

    fn parse_one(v: &Value) -> Run {
        let mut runs = parse(
            &v.to_string(),
            Path::new("broadcast/D.s.sol/8453/run-latest.json"),
        )
        .unwrap();
        assert_eq!(runs.len(), 1);
        runs.remove(0)
    }

    fn found(name: Option<&str>, address: &str, problem: Option<&str>) -> Deployed {
        Deployed {
            contract: name.map(str::to_string),
            address: Some(addr(address)),
            problem: problem.map(str::to_string),
        }
    }

    #[test]
    fn parses_creates_additional_contracts_and_failed_receipts() {
        let run = parse_one(&run_json());
        assert_eq!(run.chain_id, 8453);
        assert_eq!(
            run.contracts,
            vec![
                found(Some("PoolManager"), A, None),
                found(Some("MyHook"), B, None),
                found(None, C, Some("no contract name")),
                found(Some("Broken"), C, Some("transaction failed")),
            ]
        );
    }

    #[test]
    fn receipts_decide_only_when_present() {
        // No receipts at all (a dry run): every deployment counts.
        let mut v = run_json();
        v["receipts"] = json!([]);
        let run = parse_one(&v);
        assert!(
            run.contracts
                .iter()
                .filter(|c| c.contract.is_some())
                .all(|c| c.problem.is_none())
        );
        // Receipts, but none for a transaction: pending, not labelled.
        let mut v = run_json();
        v["receipts"].as_array_mut().unwrap().remove(0);
        let run = parse_one(&v);
        assert_eq!(
            run.contracts[0].problem.as_deref(),
            Some("no receipt for its transaction")
        );
        // Numeric and boolean statuses; a receipt without one succeeded.
        let mut v = run_json();
        v["receipts"] = json!([
            {"transactionHash": H1, "status": 1},
            {"transactionHash": H2},
            {"transactionHash": H3, "status": false}
        ]);
        let run = parse_one(&v);
        let problems: Vec<_> = run.contracts.iter().map(|c| c.problem.as_deref()).collect();
        assert_eq!(
            problems,
            vec![
                None,
                None,
                Some("no contract name"),
                Some("transaction failed")
            ]
        );
    }

    #[test]
    fn parse_takes_the_chain_from_the_directory_and_reads_multi_runs() {
        let mut v = run_json();
        v.as_object_mut().unwrap().remove("chain");
        assert_eq!(parse_one(&v).chain_id, 8453);
        assert!(parse(&v.to_string(), Path::new("run-latest.json")).is_err());
        assert!(parse("{}", Path::new("x.json")).is_err());
        assert!(parse("not json", Path::new("x.json")).is_err());

        let mut eth = run_json();
        eth["chain"] = json!(1);
        let multi = json!({"deployments": [run_json(), eth], "timestamp": 1});
        let runs = parse(&multi.to_string(), Path::new("run.json")).unwrap();
        assert_eq!(
            runs.iter().map(|r| r.chain_id).collect::<Vec<_>>(),
            [8453, 1]
        );
    }

    #[test]
    fn missing_and_bad_addresses_are_problems() {
        let v = json!({"chain": 1, "transactions": [
            {"transactionType": "CREATE", "contractName": "A", "contractAddress": null},
            {"transactionType": "CREATE", "contractName": "B", "contractAddress": "0x12"},
        ]});
        let run = parse_one(&v);
        assert_eq!(
            run.contracts[0].problem.as_deref(),
            Some("no contract address")
        );
        assert_eq!(
            run.contracts[1].problem.as_deref(),
            Some("bad contract address '0x12'")
        );
    }

    fn config() -> Config {
        crate::model::Config::parse(&format!(
            r#"
version = 2
[chains.base]
id = 8453
rpc = "a"
rpcs.a = "https://a"
[signers.deployer]
backend = "keyring"
address = "{D}"
[addresses.pool-manager]
base = "{A}"
[addresses.my-hook]
base = "{C}"
"#
        ))
        .unwrap()
    }

    fn run(chain_id: u64, contracts: &[(&str, &str)]) -> Run {
        Run {
            file: PathBuf::from("run-latest.json"),
            chain_id,
            contracts: contracts
                .iter()
                .map(|(n, a)| found(Some(n), a, None))
                .collect(),
        }
    }

    fn summary(planned: &[Planned]) -> Vec<(String, String)> {
        planned
            .iter()
            .map(|p| {
                let what = match &p.action {
                    Action::Skip(why) => format!("skipped: {why}"),
                    other => other.as_str().to_string(),
                };
                (p.name.clone().unwrap_or_default(), what)
            })
            .collect()
    }

    #[test]
    fn plan_names_dedupes_and_checks_collisions() {
        let config = config();
        let runs = [
            run(
                8453,
                &[
                    ("PoolManager", A),
                    ("MyHook", B),
                    ("Router", B),
                    ("Router", D),
                ],
            ),
            // A re-run repeats a label; another chain is not configured.
            run(8453, &[("Router", B), ("Wallet", C), ("Deployer", C)]),
            run(1, &[("Router", A)]),
        ];
        let planned = plan(&config, &runs, &Options::default());
        assert_eq!(
            summary(&planned),
            [
                ("pool-manager", "unchanged".to_string()),
                (
                    "my-hook",
                    format!("skipped: @my-hook is already {C} on base (--force replaces it)")
                ),
                ("router", "added".to_string()),
                ("router-2", "added".to_string()),
                (
                    "wallet",
                    "skipped: 'wallet' is a context token (@wallet) and cannot be used as a label"
                        .to_string()
                ),
                (
                    "deployer",
                    "skipped: 'deployer' is a signer, and signers are labels already (@deployer is its address)".to_string()
                ),
                ("router", "skipped: chain 1 is not configured".to_string()),
            ]
            .map(|(n, a)| (n.to_string(), a))
        );
        assert_eq!(planned[0].chain.as_deref(), Some("base"));
        assert_eq!(planned[3].address, Some(addr(D)));

        let forced = plan(
            &config,
            &runs[..1],
            &Options {
                force: true,
                ..Options::default()
            },
        );
        assert_eq!(forced[1].action, Action::Overwrite(addr(C)));
        assert!(forced[1].action.writes() && !forced[0].action.writes());
    }

    #[test]
    fn plan_prefixes_and_filters() {
        let config = config();
        let runs = [run(
            8453,
            &[("PoolManager", A), ("MyHook", B), ("V4Router", C)],
        )];
        let planned = plan(
            &config,
            &runs,
            &Options {
                prefix: "V4-".to_string(),
                only: vec!["poolmanager".to_string(), " v4-router".to_string()],
                force: false,
            },
        );
        assert_eq!(
            summary(&planned),
            [("v4-pool-manager", "added"), ("v4-v4-router", "added")]
                .map(|(n, a)| (n.to_string(), a.to_string()))
        );
        // Unnamed deployments never match a filter.
        let mut unnamed = run(8453, &[]);
        unnamed
            .contracts
            .push(found(None, A, Some("no contract name")));
        let only = Options {
            only: vec!["x".to_string()],
            ..Options::default()
        };
        assert!(plan(&config, &[unnamed.clone()], &only).is_empty());
        assert_eq!(
            plan(&config, &[unnamed], &Options::default())[0].action,
            Action::Skip("no contract name".to_string())
        );
    }

    #[test]
    fn find_runs_walks_broadcast_dirs_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let touch = |rel: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{}").unwrap();
            path
        };
        let a = touch("proj/broadcast/Deploy.s.sol/1/run-latest.json");
        touch("proj/broadcast/Deploy.s.sol/1/run-1700000000.json");
        touch("proj/broadcast/Deploy.s.sol/1/dry-run/run-latest.json");
        let b = touch("proj/broadcast/Hook.s.sol/8453/run-latest.json");
        let m = touch("proj/broadcast/multi/All.s.sol-latest/run.json");
        touch("proj/broadcast/multi/All.s.sol-1700000000/run.json");
        touch("proj/lib/dep/broadcast/X.s.sol/1/run-latest.json");
        let c = touch("other/sub/broadcast/Y.s.sol/10/run-latest.json");
        touch("other/node_modules/z/broadcast/Z.s.sol/1/run-latest.json");
        touch("other/.git/broadcast/Z.s.sol/1/run-latest.json");
        let dry = touch("proj/broadcast/Deploy.s.sol/1/dry-run/run-latest.json");

        let all = |p: &[PathBuf]| find_runs(p).unwrap();
        // Directories in name order: `other` before `proj`.
        assert_eq!(
            all(&[root.to_path_buf()]),
            [c, a.clone(), b.clone(), m.clone()]
        );
        assert_eq!(all(&[root.join("proj")]), [a.clone(), b.clone(), m.clone()]);
        assert_eq!(
            all(&[root.join("proj/broadcast")]),
            [a.clone(), b.clone(), m]
        );
        assert_eq!(
            all(&[root.join("proj/broadcast/Hook.s.sol")]),
            std::slice::from_ref(&b)
        );
        // Files are taken as named, once each.
        assert_eq!(all(&[dry.clone(), b.clone(), dry.clone()]), [dry, b]);
        std::fs::create_dir(root.join("empty")).unwrap();
        let err = find_runs(&[root.join("empty")]).unwrap_err();
        assert!(
            err.to_string().contains("no Foundry broadcast output"),
            "{err}"
        );
        assert!(find_runs(&[root.join("missing")]).is_err());
    }
}
