//! `rm`.

use crate::common::*;

#[test]
fn rm_by_name_and_id_requires_an_exact_match() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("base", 8453), ("arb", 42161)]);
    sb.cmd().args(["rm", "ba", "-y"]).assert().failure();
    sb.cmd().args(["rm", "base", "-y"]).assert().success();
    sb.cmd().args(["rm", "42161", "--yes"]).assert().success();
    sb.cmd().args(["rm", "nope", "-y"]).assert().failure();
    let text = sb.config();
    assert!(text.contains("[chains.eth]"));
    assert!(!text.contains("[chains.base]") && !text.contains("[chains.arb]"));
}

/// `rm` asks (off a terminal: needs --yes) and names what goes
/// with the chain: its labels and the default.
#[test]
fn rm_names_its_labels_and_the_default() {
    let sb = Sandbox::new();
    sb.seed(&[("eth", 1), ("arb", 42161)]);
    sb.ok(&["use", "arb"]);
    sb.ok(&["label", "add", "myrouter", TEST_ADDRESS_2, "--chain", "arb"]);
    sb.ok(&["label", "add", "pool", TEST_ADDRESS, "--chain", "arb"]);
    let out = sb.run(&["rm", "arb"]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(
        err.contains("removes chain 'arb' (1 endpoint; labels @myrouter, @pool; it is the default chain); pass --yes"),
        "{err}"
    );
    assert!(sb.config().contains("[chains.arb]"));
    let out = sb.ok(&["rm", "arb", "--yes"]);
    assert!(out.contains("Removed chain 'arb'"), "{out}");
    assert!(out.contains("dropped labels: @myrouter, @pool"), "{out}");
    assert!(out.contains("default cleared"), "{out}");
}
