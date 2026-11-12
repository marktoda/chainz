//! `top`: flag wiring, URL redaction and what `--once` sends. The output
//! itself is pinned twice: from fixed states in chainz-tui's goldens, and
//! end to end against [`fixture`] in `golden.rs`.

use crate::common::*;
use chainz_testkit::abi;

/// Mainnet USDC, as [`abi::aggregate3_targets`] spells it.
const USDC: &str = "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";

/// `live` on `rpc` (through a `${VAR}` in the path), `dead` on a closed
/// port, and a keyring signer with a cached address.
pub fn fixture(rpc: &Mock) -> Sandbox {
    let sb = Sandbox::new();
    sb.write_config(&format!(
        r#"version = 2
default = "live"

[chains.live]
id = 1
aliases = ["mainnet"]
signer = "default"
rpc = "main"

[chains.live.rpcs]
main = "{rpc}/v3/${{RPC_KEY}}"
spare = "http://127.0.0.1:1/spare"
stream = "wss://live.example.org/ws/hunter2"

[chains.dead]
id = 8453
rpc = "local"

[chains.dead.rpcs]
local = "http://127.0.0.1:1"

[signers.default]
backend = "keyring"
address = "{TEST_ADDRESS}"
"#,
        rpc = rpc.url()
    ));
    sb.write_secrets("[vars]\nRPC_KEY = \"s3cr3t-key\"\n");
    sb
}

/// Chain 1 at head 1,000 with 12 s blocks, and a Multicall3 that reports
/// 1.5 ETH, 2.5 USDC and nothing of any other token.
pub fn chain() -> Mock {
    // One clock reading for every block, so the head and the look-back
    // block are always exactly 12 s apart (two readings could straddle a
    // second boundary and make the block time 12.005 s).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    mock(move |call| {
        Ok(match call.method.as_str() {
            "eth_getBlockByNumber" => {
                let number = match call.params[0].as_str().unwrap() {
                    "latest" => 1_000,
                    hex => u64::from_str_radix(&hex[2..], 16).unwrap(),
                };
                json!({
                    "number": format!("{number:#x}"),
                    "timestamp": format!("{:#x}", now - (1_000 - number) * 12),
                    "baseFeePerGas": "0x3b9aca00",
                })
            }
            "eth_call" => {
                let calldata = call.params[0]["data"].as_str().unwrap();
                let values: Vec<Option<u128>> = abi::aggregate3_targets(calldata)
                    .iter()
                    .enumerate()
                    .map(|(i, target)| match (i, target.as_str()) {
                        (0, _) => Some(1_500_000_000_000_000_000),
                        (_, USDC) => Some(2_500_000),
                        _ => Some(0),
                    })
                    .collect();
                json!(abi::aggregate3_result(&values))
            }
            _ => Value::Null,
        })
    })
}

/// `chainz top <args>` in the sandbox.
fn top(sb: &Sandbox, args: &[&str]) -> assert_cmd::Command {
    let mut cmd = sb.cmd();
    cmd.arg("top").args(args);
    cmd
}

/// The snapshot asks the endpoint for a head, one look-back block and one
/// multicall, through the expanded URL; the secret it expanded appears
/// nowhere in the output.
#[test]
fn once_sends_three_calls_and_keeps_the_secret_out_of_the_output() {
    let rpc = chain();
    let sb = fixture(&rpc);
    for args in [&["--once"][..], &["--once", "--json"]] {
        let out = top(&sb, args).output().unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        let text = stdout(&out);
        assert!(
            !text.contains("s3cr3t-key") && !text.contains("hunter2"),
            "{text}"
        );
    }
    let requests = rpc.requests();
    assert!(
        requests.iter().all(|r| r.path == "/v3/s3cr3t-key"),
        "{requests:?}"
    );
    assert_eq!(
        rpc.methods(),
        ["eth_getBlockByNumber", "eth_getBlockByNumber", "eth_call"].repeat(2)
    );
}

#[test]
fn reveal_controls_url_redaction() {
    let rpc = chain();
    let sandbox = fixture(&rpc);
    let url_of = |args: &[&str], index: usize| -> String {
        let output = top(&sandbox, args).output().unwrap();
        assert!(output.status.success());
        let snapshot: Value = serde_json::from_slice(&output.stdout).unwrap();
        snapshot["chains"][0]["endpoints"][index]["url"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let redacted = url_of(&["--once", "--json"], 2);
    assert_eq!(redacted, "wss://live.example.org/ws/[REDACTED]");
    // --reveal shows the config value: templates stay unexpanded.
    assert_eq!(
        url_of(&["--once", "--json", "--reveal"], 2),
        "wss://live.example.org/ws/hunter2"
    );
    let revealed = url_of(&["--once", "--json", "--reveal"], 0);
    assert_eq!(revealed, format!("{}/v3/${{RPC_KEY}}", rpc.url()));
}

#[test]
fn the_live_view_needs_a_terminal_and_json_needs_once() {
    let sandbox = Sandbox::new();
    sandbox.seed(&[("eth", 1)]);
    // Under the test harness stdout is a pipe: refuse, and say what to use.
    top(&sandbox, &[])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("needs a terminal"))
        .stderr(predicates::str::contains("chainz top --once --json"));
    // clap refuses `--json` without `--once` (a usage error).
    top(&sandbox, &["--json"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("--once"));
}
