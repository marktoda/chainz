//! End-to-end tests for `label import`: labels from Foundry broadcast
//! output. Broadcast trees are written by hand in the shape forge writes
//! them, inside the sandbox (see `common`).

mod common;

use common::{Sandbox, stderr, stdout};
use predicates::prelude::*;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const POOL: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";
const ROUTER: &str = "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF";
const HOOK: &str = "0x6813Eb9362372EEF6200f3b1dbC3f819671cBA69";
const OTHER: &str = "0x1efF47bc3a10a45D4B230B5d10E37751FE6AA718";
const FAILED: &str = "0xe1AB8145F7E55DC933d51a18c793F901A3A0b276";

fn hash(n: u8) -> String {
    format!("0x{}", format!("{n:02x}").repeat(32))
}

/// A forge `run-latest.json` for `chain`: `(contract, address, succeeded)`
/// CREATE transactions, plus `extra` additional contracts deployed by a
/// CALL (`(name, address)`, unnamed when the name is empty).
fn run_file(chain: u64, creates: &[(&str, &str, bool)], extra: &[(&str, &str)]) -> Value {
    let mut transactions = Vec::new();
    let mut receipts = Vec::new();
    for (i, (name, address, ok)) in creates.iter().enumerate() {
        let h = hash(i as u8 + 1);
        transactions.push(json!({
            "hash": h, "transactionType": if i % 2 == 0 { "CREATE" } else { "CREATE2" },
            "contractName": name, "contractAddress": address,
            "function": null, "arguments": null,
            "transaction": {"from": OTHER, "gas": "0x5208", "value": "0x0",
                            "input": "0x6080", "nonce": format!("{i:#x}"),
                            "chainId": format!("{chain:#x}")},
            "additionalContracts": [], "isFixedGasLimit": false
        }));
        receipts.push(json!({
            "status": if *ok { "0x1" } else { "0x0" }, "transactionHash": h,
            "contractAddress": address, "blockNumber": "0x10", "gasUsed": "0x5208"
        }));
    }
    if !extra.is_empty() {
        let h = hash(0xee);
        let additional: Vec<Value> = extra
            .iter()
            .map(|(name, address)| {
                let mut c = json!({"transactionType": "CREATE2", "address": address,
                                   "initCode": "0x6080"});
                if !name.is_empty() {
                    c["contractName"] = json!(name);
                }
                c
            })
            .collect();
        transactions.push(json!({
            "hash": h, "transactionType": "CALL", "contractName": null,
            "contractAddress": OTHER, "function": "deploy(bytes32)",
            "arguments": ["0x00"], "additionalContracts": additional,
            "isFixedGasLimit": false
        }));
        receipts.push(json!({"status": "0x1", "transactionHash": h}));
    }
    json!({
        "transactions": transactions, "receipts": receipts, "libraries": [],
        "pending": [], "returns": {}, "timestamp": 1_731_000_000u64,
        "chain": chain, "multi": false, "commit": "abc1234"
    })
}

/// Write `run` as `<root>/<rel>`.
fn put(root: &Path, rel: &str, run: &Value) -> PathBuf {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_string_pretty(run).unwrap()).unwrap();
    path
}

/// `eth` (1) and `base` (8453), and a Foundry project at `<root>/proj`
/// with two scripts on base, one on an unconfigured chain, a history file,
/// a dry run and a dependency's broadcast output.
fn project() -> (Sandbox, PathBuf) {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453)]);
    let proj = sb.path("proj");
    put(
        &proj,
        "broadcast/DeployPool.s.sol/8453/run-latest.json",
        &run_file(
            8453,
            &[("PoolManager", POOL, true), ("Broken", FAILED, false)],
            &[("MyHook", HOOK), ("", OTHER)],
        ),
    );
    put(
        &proj,
        "broadcast/DeployRouter.s.sol/8453/run-latest.json",
        &run_file(8453, &[("UniversalRouter", ROUTER, true)], &[]),
    );
    put(
        &proj,
        "broadcast/DeployRouter.s.sol/10/run-latest.json",
        &run_file(10, &[("UniversalRouter", ROUTER, true)], &[]),
    );
    let ignored = run_file(8453, &[("Ignored", OTHER, true)], &[]);
    put(
        &proj,
        "broadcast/DeployRouter.s.sol/8453/run-1730000000.json",
        &ignored,
    );
    put(
        &proj,
        "broadcast/DeployRouter.s.sol/8453/dry-run/run-latest.json",
        &ignored,
    );
    put(
        &proj,
        "lib/dep/broadcast/Dep.s.sol/8453/run-latest.json",
        &ignored,
    );
    (sb, proj)
}

fn import_json(sb: &Sandbox, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["label", "import", "--json"];
    all.extend_from_slice(args);
    let rows: Value = serde_json::from_str(&sb.ok(&all)).unwrap();
    rows.as_array().unwrap().clone()
}

/// `(name, action)` per row.
fn actions(rows: &[Value]) -> Vec<(String, String)> {
    rows.iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap_or("-").to_string(),
                r["action"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn pairs(want: &[(&str, &str)]) -> Vec<(String, String)> {
    want.iter()
        .map(|(n, a)| (n.to_string(), a.to_string()))
        .collect()
}

#[test]
fn import_walks_a_project_and_skips_what_it_cannot_label() {
    let (sb, proj) = project();
    let out = sb.run(&["label", "import", proj.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0].split_whitespace().collect::<Vec<_>>(),
        ["CHAIN", "NAME", "ADDRESS", "RESULT"]
    );
    for want in [
        format!("base   @pool-manager      {POOL}  added"),
        format!("base   @broken            {FAILED}  skipped: transaction failed"),
        format!("base   @my-hook           {HOOK}  added"),
        format!("base   -                  {OTHER}  skipped: no contract name"),
        format!("base   @universal-router  {ROUTER}  added"),
        format!("op     @universal-router  {ROUTER}  skipped: chain 10 is not configured"),
    ] {
        assert!(text.contains(&want), "missing {want:?} in\n{text}");
    }
    assert!(text.ends_with("\nAdded 3 labels; 3 skipped.\n"), "{text}");
    // History files, dry runs and dependencies are not read.
    assert!(!text.contains("ignored"), "{text}");

    let config = sb.config();
    for want in [
        format!("[addresses.pool-manager]\nbase = \"{POOL}\""),
        format!("[addresses.my-hook]\nbase = \"{HOOK}\""),
        format!("[addresses.universal-router]\nbase = \"{ROUTER}\""),
    ] {
        assert!(config.contains(&want), "missing {want:?} in\n{config}");
    }
    assert!(!config.contains("broken") && !config.contains("ignored"));
    // The labels work as tokens.
    sb.cmd()
        .args(["exec", "base", "--", "echo", "@universal-router"])
        .assert()
        .success()
        .stdout(format!("{ROUTER}\n"));

    // Again: nothing changes.
    let before = sb.config();
    let rows = import_json(&sb, &[proj.join("broadcast").to_str().unwrap()]);
    assert_eq!(
        actions(&rows),
        pairs(&[
            ("pool-manager", "unchanged"),
            ("broken", "skipped"),
            ("my-hook", "unchanged"),
            ("-", "skipped"),
            ("universal-router", "skipped"),
            ("universal-router", "unchanged"),
        ])
    );
    assert_eq!(sb.config(), before);
}

#[test]
fn import_json_reports_each_entry() {
    let (sb, proj) = project();
    let file = proj.join("broadcast/DeployPool.s.sol/8453/run-latest.json");
    let rows = import_json(&sb, &[file.to_str().unwrap(), "--dry-run"]);
    assert_eq!(
        rows[0],
        json!({"chain": "base", "chain_id": 8453, "name": "pool-manager", "address": POOL,
               "action": "added", "contract": "PoolManager", "file": file})
    );
    assert_eq!(
        rows[1],
        json!({"chain": "base", "chain_id": 8453, "name": "broken", "address": FAILED,
               "action": "skipped", "reason": "transaction failed", "contract": "Broken",
               "file": file})
    );
    assert_eq!(rows[3]["name"], Value::Null);
    assert_eq!(rows[3]["contract"], Value::Null);
    assert_eq!(rows[3]["reason"], "no contract name");
    // The unconfigured chain is reported by its well-known name and ID.
    let other = proj.join("broadcast/DeployRouter.s.sol/10/run-latest.json");
    let rows = import_json(&sb, &[other.to_str().unwrap()]);
    assert_eq!(
        (&rows[0]["chain"], &rows[0]["chain_id"], &rows[0]["reason"]),
        (
            &json!("op"),
            &json!(10),
            &json!("chain 10 is not configured")
        )
    );
}

#[test]
fn import_dry_run_writes_nothing() {
    let (sb, proj) = project();
    let before = sb.config();
    let text = sb.ok(&["label", "import", "--dry-run", proj.to_str().unwrap()]);
    assert!(
        text.contains(&format!("@pool-manager      {POOL}  add\n")),
        "{text}"
    );
    assert!(
        text.ends_with("\nDry run: would add 3; 3 skipped. Nothing written.\n"),
        "{text}"
    );
    assert_eq!(sb.config(), before);
}

#[test]
fn import_collisions_need_force() {
    let (sb, proj) = project();
    sb.ok(&["label", "add", "pool-manager", OTHER, "--chain", "base"]);
    sb.ok(&["label", "add", "my-hook", HOOK, "--chain", "base"]);
    let file = proj.join("broadcast/DeployPool.s.sol/8453/run-latest.json");
    let file = file.to_str().unwrap();

    let rows = import_json(&sb, &[file]);
    assert_eq!(
        actions(&rows)[..3],
        pairs(&[
            ("pool-manager", "skipped"),
            ("broken", "skipped"),
            ("my-hook", "unchanged"),
        ])
    );
    assert_eq!(
        rows[0]["reason"],
        format!("@pool-manager is already {OTHER} on base (--force replaces it)")
    );
    assert!(
        sb.config()
            .contains(&format!("[addresses.pool-manager]\nbase = \"{OTHER}\""))
    );

    // A dry run with --force says what it would replace.
    let text = sb.ok(&["label", "import", file, "--force", "--dry-run"]);
    assert!(text.contains(&format!("overwrite (was {OTHER})")), "{text}");
    assert!(
        text.contains("Dry run: would add 0, overwrite 1; 1 unchanged, 2 skipped."),
        "{text}"
    );

    let text = sb.ok(&["label", "import", file, "--force"]);
    assert!(
        text.contains(&format!("overwritten (was {OTHER})")),
        "{text}"
    );
    assert!(
        text.ends_with("\nAdded 0 labels, overwrote 1; 1 unchanged, 2 skipped.\n"),
        "{text}"
    );
    assert!(
        sb.config()
            .contains(&format!("[addresses.pool-manager]\nbase = \"{POOL}\""))
    );
    let rows = import_json(&sb, &[file]);
    assert_eq!(rows[0]["action"], "unchanged");
}

#[test]
fn import_suffixes_names_repeated_within_one_import() {
    let sb = Sandbox::new();
    sb.seed(&[("base", 8453)]);
    let root = sb.path("runs");
    // Two scripts deploy a `Router` each; a third redeploys the first one's.
    put(
        &root,
        "broadcast/A.s.sol/8453/run-latest.json",
        &run_file(8453, &[("Router", ROUTER, true)], &[]),
    );
    put(
        &root,
        "broadcast/B.s.sol/8453/run-latest.json",
        &run_file(8453, &[("Router", POOL, true), ("Router", HOOK, true)], &[]),
    );
    put(
        &root,
        "broadcast/C.s.sol/8453/run-latest.json",
        &run_file(8453, &[("Router", ROUTER, true)], &[]),
    );
    let rows = import_json(&sb, &[root.to_str().unwrap()]);
    let got: Vec<(&str, &str)> = rows
        .iter()
        .map(|r| (r["name"].as_str().unwrap(), r["address"].as_str().unwrap()))
        .collect();
    assert_eq!(
        got,
        [("router", ROUTER), ("router-2", POOL), ("router-3", HOOK)]
    );
    let config = sb.config();
    assert!(
        config.contains(&format!("[addresses.router-3]\nbase = \"{HOOK}\"")),
        "{config}"
    );
}

#[test]
fn import_prefix_and_only() {
    let (sb, proj) = project();
    let rows = import_json(
        &sb,
        &[
            proj.to_str().unwrap(),
            "--prefix",
            "v4-",
            "--only",
            "PoolManager,my-hook",
        ],
    );
    assert_eq!(
        actions(&rows),
        pairs(&[("v4-pool-manager", "added"), ("v4-my-hook", "added")])
    );
    let config = sb.config();
    assert!(config.contains("[addresses.v4-pool-manager]"), "{config}");
    assert!(!config.contains("[addresses.pool-manager]"), "{config}");

    // Nothing matches: nothing to do.
    sb.cmd()
        .args(["label", "import", proj.to_str().unwrap(), "--only", "nope"])
        .assert()
        .success()
        .stdout("No contracts to import in 3 run files.\n");
    // A prefix that makes a bad name skips with the reason.
    let rows = import_json(
        &sb,
        &[proj.to_str().unwrap(), "--prefix", "a.b", "--dry-run"],
    );
    assert!(
        rows[0]["reason"].as_str().unwrap().contains("[a-z0-9_-]+"),
        "{:?}",
        rows[0]
    );
}

#[test]
fn import_errors_on_paths_without_broadcast_output() {
    let sb = Sandbox::new();
    sb.seed(&[("base", 8453)]);
    fs::create_dir_all(sb.path("empty/src")).unwrap();
    sb.cmd()
        .args(["label", "import", sb.path("empty").to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no Foundry broadcast output"));
    sb.cmd()
        .args(["label", "import", sb.path("missing").to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot read"));
    fs::write(sb.path("bad.json"), "{\"chain\": 1}").unwrap();
    sb.cmd()
        .args(["label", "import", sb.path("bad.json").to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not a Foundry run file"));
    sb.cmd()
        .args(["label", "import"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("<PATH>"));
}
