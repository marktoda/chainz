//! End-to-end tests for dynamic shell completion: the `CHAINZ_COMPLETE=<shell>`
//! protocol (as the loaders drive it) and `chainz completions <shell>`.
//!
//! As in `cli.rs`, every invocation runs inside a sandbox (see `common`).

mod common;

use common::{Sandbox, TEST_ADDRESS, stderr};
use std::fs;

const ROUTER: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";

/// `eth` (1, alias `mainnet`) and `base` (8453), endpoints, signers,
/// labels, and a variable.
fn fixture() -> Sandbox {
    let sb = Sandbox::new();
    sb.write_config(&format!(
        r#"version = 2
default = "eth"

[signers.default]
backend = "plaintext"
address = "{TEST_ADDRESS}"

[signers.ledger]
backend = "keystore"
path = "/nonexistent"

[chains.eth]
id = 1
aliases = ["mainnet"]
signer = "default"
rpc = "alchemy"

[chains.eth.rpcs]
alchemy = "http://127.0.0.1:1/eth"
llama = "http://127.0.0.1:1/eth2"

[chains.base]
id = 8453
rpc = "public"

[chains.base.rpcs]
public = "http://127.0.0.1:1/base"
quicknode = "http://127.0.0.1:1/base2"

[addresses.router]
base = "{ROUTER}"
"#
    ));
    sb.write_secrets("[vars]\nALCHEMY_KEY = \"supersecret\"\nOTHER = \"x\"\n");
    sb
}

/// Complete the last of `words` the way the fish loader asks: candidate
/// values, without their help. Unless the word starts with `-`, the flags
/// clap offers alongside are dropped.
fn fish(sb: &Sandbox, words: &[&str]) -> Vec<String> {
    let flags = words.last().is_some_and(|w| w.starts_with('-'));
    fish_all(sb, words)
        .into_iter()
        .filter(|c| flags || !c.starts_with('-'))
        .collect()
}

/// Every candidate for the last of `words`, as the fish loader gets them.
fn fish_all(sb: &Sandbox, words: &[&str]) -> Vec<String> {
    let out = sb
        .process()
        .env("CHAINZ_COMPLETE", "fish")
        .arg("--")
        .arg("chainz")
        .args(words)
        .output()
        .unwrap();
    assert!(out.status.success(), "completion failed: {}", stderr(&out));
    assert!(
        out.stderr.is_empty(),
        "completion wrote to stderr: {}",
        stderr(&out)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.split('\t').next().unwrap().to_string())
        .collect()
}

/// Complete word `index` of `chainz words…` the way the bash loader asks.
fn bash(sb: &Sandbox, words: &[&str], index: usize) -> Vec<String> {
    let out = sb
        .process()
        .env("CHAINZ_COMPLETE", "bash")
        .env("_CLAP_COMPLETE_INDEX", index.to_string())
        .env("_CLAP_COMPLETE_COMP_TYPE", "9")
        .env("_CLAP_IFS", "\n")
        .arg("--")
        .arg("chainz")
        .args(words)
        .output()
        .unwrap();
    assert!(out.status.success(), "completion failed: {}", stderr(&out));
    assert!(
        out.stderr.is_empty(),
        "completion wrote to stderr: {}",
        stderr(&out)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect()
}

fn has(candidates: &[String], value: &str) -> bool {
    candidates.iter().any(|c| c == value)
}

#[track_caller]
fn assert_set(candidates: Vec<String>, expected: &[&str]) {
    let mut got = candidates;
    got.sort();
    let mut want: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    want.sort();
    assert_eq!(got, want);
}

// ------------------------------------------------------------------ chains

#[test]
fn chain_arguments_complete_names_and_aliases() {
    let sb = fixture();
    assert_set(fish(&sb, &["exec", ""]), &["eth", "base", "mainnet"]);
    assert_set(fish(&sb, &["x", "ma"]), &["mainnet"]);
    assert_set(fish(&sb, &["env", "b"]), &["base"]);
    assert_set(fish(&sb, &["rpc", "ls", "e"]), &["eth"]);
    assert_set(fish(&sb, &["chain", "rm", ""]), &["eth", "base", "mainnet"]);
    assert_set(fish(&sb, &["addr", "ls", "--chain", "b"]), &["base"]);
    assert_set(fish(&sb, &["label", "--chain=b"]), &["--chain=base"]);
    // `chain add` takes a chainlist ID or name, not a configured chain.
    assert!(fish(&sb, &["chain", "add", "b"]).is_empty());
}

#[test]
fn chain_arguments_include_live_forks_only() {
    let sb = fixture();
    fs::create_dir_all(sb.forks_dir()).unwrap();
    let record = |name: &str| {
        serde_json::json!({
            "format": 1, "name": name, "pid": std::process::id(), "anvil_pid": 1,
            "port": 8601, "parent": "base", "chain_id": 8453, "fork_block": null,
            "started_at": 0, "ready": true
        })
        .to_string()
    };
    // A fork is live while its lock is held: this test holds base-fork's.
    for name in ["base-fork", "dead-fork"] {
        fs::write(sb.forks_dir().join(format!("{name}.json")), record(name)).unwrap();
    }
    let lock = fs::File::create(sb.forks_dir().join("base-fork.lock")).unwrap();
    lock.try_lock().unwrap();
    assert_set(fish(&sb, &["exec", "b"]), &["base", "base-fork"]);
    // Configured-only arguments never offer forks; `fork stop` offers all.
    assert_set(fish(&sb, &["rpc", "ls", "b"]), &["base"]);
    assert_set(
        fish(&sb, &["fork", "stop", ""]),
        &["base-fork", "dead-fork"],
    );
}

// ------------------------------------------------------------- endpoints

#[test]
fn rpc_completes_the_typed_chain_endpoints() {
    let sb = fixture();
    assert_set(
        fish(&sb, &["exec", "base", "--rpc", ""]),
        &["public", "quicknode"],
    );
    assert_set(
        fish(&sb, &["exec", "mainnet", "--rpc", ""]),
        &["alchemy", "llama"],
    );
    assert_set(fish(&sb, &["env", "base", "--rpc", "q"]), &["quicknode"]);
    // No chain typed: the default chain's.
    assert_set(fish(&sb, &["exec", "--rpc", ""]), &["alchemy", "llama"]);
    // `chain set` and `rpc rm` take the chain positionally.
    assert_set(
        fish(&sb, &["chain", "set", "base", "--rpc", ""]),
        &["public", "quicknode"],
    );
    assert_set(
        fish(&sb, &["update", "base", "--failover", "p"]),
        &["public"],
    );
    assert_set(fish(&sb, &["rpc", "rm", "eth", "l"]), &["llama"]);
}

#[test]
fn completers_follow_the_arguments() {
    let sb = fixture();
    // Fork-accepting `--chain`s and `edit` get chains; `addr rm --chain`
    // also offers `*`.
    assert!(has(&fish(&sb, &["edit", "ba"]), "base"));
    assert!(has(&fish(&sb, &["label", "--chain", "ba"]), "base"));
    assert!(has(&fish(&sb, &["addr", "rm", "x", "--chain", ""]), "*"));
    assert!(has(
        &fish(&sb, &["chain", "set", "base", "--explorer", ""]),
        "etherscan"
    ));
    assert!(has(
        &fish(&sb, &["chain", "add", "--signer", ""]),
        "default"
    ));
}

#[test]
fn rpc_follows_chainz_chain_when_no_chain_is_typed() {
    let sb = fixture();
    let out = sb
        .process()
        .env("CHAINZ_COMPLETE", "fish")
        .env("CHAINZ_CHAIN", "base")
        .args(["--", "chainz", "exec", "--rpc", ""])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("public") && !text.contains("alchemy"),
        "{text}"
    );
}

// --------------------------------------------------- signers, labels, vars

#[test]
fn signers_labels_and_vars_complete() {
    let sb = fixture();
    assert_set(
        fish(&sb, &["exec", "base", "--signer", ""]),
        &["default", "ledger"],
    );
    assert_set(fish(&sb, &["signer", "rm", "l"]), &["ledger"]);
    assert_set(fish(&sb, &["key", "rm", "d"]), &["default"]);
    assert_set(fish(&sb, &["addr", "rm", ""]), &["router"]);
    assert_set(fish(&sb, &["var", "get", ""]), &["ALCHEMY_KEY", "OTHER"]);
    assert_set(fish(&sb, &["var", "rm", "A"]), &["ALCHEMY_KEY"]);
}

#[test]
fn completion_output_never_contains_secret_values() {
    let sb = fixture();
    let out = sb
        .process()
        .env("CHAINZ_COMPLETE", "fish")
        .args(["--", "chainz", "var", "get", ""])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains("supersecret"));
}

// -------------------------------------------------------------- @tokens

#[test]
fn exec_tokens_complete_after_the_separator() {
    let sb = fixture();
    let got = fish(&sb, &["exec", "base", "--", "cast", "call", "@"]);
    for token in [
        "@rpc",
        "@chainid",
        "@chain",
        "@wallet",
        "@explorer",
        "@explorer_key",
        "@@",
        "@default",
        "@ledger",
        "@router",
        "@usdc",
    ] {
        assert!(has(&got, token), "missing {token} in {got:?}");
    }
    // The prefix filters; labels come from the typed chain.
    assert_set(
        fish(&sb, &["exec", "base", "--", "cast", "@ro"]),
        &["@router"],
    );
    assert!(fish(&sb, &["exec", "eth", "--", "cast", "@ro"]).is_empty());
    // No chain typed: the default chain (eth) applies.
    assert!(fish(&sb, &["exec", "--", "cast", "@ro"]).is_empty());
    assert_set(fish(&sb, &["exec", "--", "@wal"]), &["@wallet"]);
}

#[test]
fn label_colon_completes_chains_that_have_it() {
    let sb = fixture();
    assert_set(
        fish(&sb, &["exec", "base", "--", "cast", "@usdc:"]),
        &["@usdc:eth", "@usdc:base"],
    );
    assert_set(
        fish(&sb, &["exec", "base", "--", "@router:"]),
        &["@router:base"],
    );
    assert_set(
        fish(&sb, &["exec", "eth", "--", "@usdc:b"]),
        &["@usdc:base"],
    );
}

#[test]
fn bash_split_at_colon_completes_the_chain_part() {
    let sb = fixture();
    // Bash breaks `@usdc:ba` into `@usdc`, `:`, `ba` and replaces only `ba`.
    let words = ["exec", "base", "--", "cast", "@usdc", ":", "ba"];
    assert_set(bash(&sb, &words, 7), &["base"]);
    let words = ["exec", "base", "--", "cast", "@usdc", ":"];
    assert_set(bash(&sb, &words, 6), &[":eth", ":base"]);
}

#[test]
fn exec_words_without_at_complete_files() {
    let sb = fixture();
    fs::write(sb.root().join("script.sh"), "").unwrap();
    let out = sb
        .process()
        .current_dir(sb.root())
        .env("CHAINZ_COMPLETE", "fish")
        .args(["--", "chainz", "exec", "base", "--", "bash", "scr"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "script.sh");
}

#[test]
fn bash_and_zsh_protocols_use_the_cursor_index() {
    let sb = fixture();
    // Cursor on the chain, with words after it.
    assert_set(
        bash(&sb, &["exec", "ba", "--", "cast", "block-number"], 2),
        &["base"],
    );
    let out = sb
        .process()
        .env("CHAINZ_COMPLETE", "zsh")
        .env("_CLAP_COMPLETE_INDEX", "4")
        .env("_CLAP_IFS", "\n")
        .args(["--", "chainz", "exec", "base", "--rpc", "q"])
        .output()
        .unwrap();
    assert!(out.status.success());
    // zsh lines are `value:help`.
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().split(':').next(),
        Some("quicknode")
    );
}

// ------------------------------------------------- no config, no effects

#[test]
fn no_config_completes_nothing_and_never_migrates() {
    let sb = Sandbox::new();
    // A v1 config that a normal command would auto-migrate.
    sb.write("config.json", r#"{"chains": [], "default_chain": null}"#);
    for words in [
        &["exec", ""][..],
        &["exec", "base", "--rpc", ""],
        &["exec", "base", "--", "@us"],
        &["exec", "base", "--", "@usdc:"],
        &["signer", "rm", ""],
        &["var", "get", ""],
        &["addr", "rm", ""],
    ] {
        assert!(fish(&sb, words).is_empty(), "{words:?}");
    }
    assert!(!sb.config_path().exists(), "completion must not migrate v1");
    assert!(!sb.dir().join("config.lock").exists());
    assert!(
        fs::read_dir(sb.path("state")).unwrap().next().is_none(),
        "completion must not write state"
    );
}

#[test]
fn broken_config_completes_nothing_silently() {
    let sb = Sandbox::new();
    sb.write_config("this is = = not toml");
    assert!(fish(&sb, &["exec", ""]).is_empty());
    assert!(fish(&sb, &["exec", "base", "--", "@"]).is_empty());
}

#[test]
fn subcommands_and_flags_still_complete() {
    let sb = fixture();
    let got = fish(&sb, &["rp"]);
    assert_set(got, &["rpc"]);
    assert!(has(&fish_all(&sb, &["exec", "base", "--"]), "--rpc"));
    assert!(has(&fish_all(&sb, &["exec", "base", ""]), "--rpc"));
}

// ------------------------------------------------------------- loaders

#[test]
fn completions_print_a_dynamic_loader() {
    let sb = Sandbox::new();
    for (shell, marker) in [
        ("zsh", "compdef _clap_dynamic_completer_chainz chainz"),
        (
            "bash",
            "complete -o nospace -o bashdefault -o nosort -F _clap_complete_chainz chainz",
        ),
        ("fish", "complete --keep-order --exclusive --command chainz"),
        ("elvish", "set edit:completion:arg-completer[chainz]"),
        (
            "powershell",
            "Register-ArgumentCompleter -Native -CommandName chainz",
        ),
    ] {
        let out = sb.run(&["completions", shell]);
        assert!(out.status.success(), "{shell}: {}", stderr(&out));
        let script = String::from_utf8(out.stdout).unwrap();
        assert!(script.contains(marker), "{shell} loader:\n{script}");
        assert!(
            script.contains("CHAINZ_COMPLETE"),
            "{shell} loader:\n{script}"
        );
    }
}

#[test]
fn complete_env_without_words_prints_the_loader() {
    let sb = Sandbox::new();
    let out = sb.process().env("CHAINZ_COMPLETE", "zsh").output().unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .starts_with("#compdef chainz")
    );
}

#[test]
fn unknown_shell_fails_silently() {
    let sb = fixture();
    let out = sb
        .process()
        .env("CHAINZ_COMPLETE", "tcsh")
        .args(["--", "chainz", "exec", ""])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stderr.is_empty() && out.stdout.is_empty());
}

#[test]
fn complete_zero_is_not_completion_mode() {
    let sb = fixture();
    let out = sb
        .process()
        .env("CHAINZ_COMPLETE", "0")
        .args(["exec", "eth", "--", "true"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn a_generic_complete_variable_is_ignored() {
    // Other tools use `COMPLETE`; a stray one must not hijack `exec`.
    let sb = fixture();
    let out = sb
        .process()
        .env("COMPLETE", "zsh")
        .args(["exec", "eth", "--", "true"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
}
