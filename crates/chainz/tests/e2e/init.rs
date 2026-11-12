//! The first run: the quick start (`chainz` and `init` off a terminal),
//! then the first chain and signer.

use crate::common::*;

#[test]
fn first_run_attaches_the_signer_and_the_explorer() {
    let sb = Sandbox::new();
    // No chains: the quick start, signer first; `init` off a terminal too.
    let text = sb.ok(&[]);
    assert!(text.contains("Quick start:"), "{text}");
    let signer = text.find("signer add").unwrap();
    assert!(signer < text.find("chainz add").unwrap(), "{text}");
    assert_eq!(
        sb.ok(&["init"]),
        text.replacen("chainz: no chains configured yet.\n", "", 1)
    );

    // A chain with no signer, then a first signer: it attaches.
    let rpc = Mock::chain_id(8453);
    let url = rpc.url();
    sb.ok(&["add", "8453", "--url", &url]);
    assert!(!sb.config().contains("signer ="), "{}", sb.config());
    let out = sb
        .cmd()
        .args([
            "signer",
            "add",
            "deployer",
            "--stdin",
            "--backend",
            "plaintext",
        ])
        .write_stdin(TEST_KEY)
        .assert()
        .success();
    let text = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(text.contains("Attached 'deployer' to base"), "{text}");
    assert!(sb.config().contains("signer = \"deployer\""));

    // The next chain takes the only signer, and etherscan once it exists.
    sb.cmd()
        .args(["update", "base", "--explorer", "etherscan"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created [explorers.etherscan]"));
    let rpc = Mock::chain_id(10);
    let url = rpc.url();
    let text = sb.ok(&["add", "10", "--url", &url]);
    assert!(text.contains("signer: deployer"), "{text}");
    assert!(text.contains("explorer: etherscan"), "{text}");
    let rpc = Mock::chain_id(42161);
    let url = rpc.url();
    let text = sb.ok(&["add", "42161", "--url", &url, "--no-signer"]);
    assert!(!text.contains("signer:"), "{text}");
    let show = sb.ok(&["ls", "arb", "--json"]);
    assert!(show.contains("\"signer\": null"), "{show}");

    // A second signer attaches nowhere.
    let text = sb
        .cmd()
        .args(["signer", "add", "ops", "--stdin", "--backend", "plaintext"])
        .write_stdin(TEST_KEY.replace("ac09", "bc09"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(!String::from_utf8_lossy(&text).contains("Attached"));
}
