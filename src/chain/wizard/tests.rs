use super::{probe_summary, select_verifier_with, suggest_short_name};
use crate::chain::rpc::ProbeResult;
use crate::prompt::testing::{Answer, ScriptedPrompt};
use std::time::Duration;

#[test]
fn short_name_suggestions() {
    assert_eq!(suggest_short_name("Ethereum Mainnet"), "ethereum");
    assert_eq!(suggest_short_name("OP Mainnet"), "op");
    assert_eq!(suggest_short_name("Avalanche C-Chain"), "avalanche");
    assert_eq!(suggest_short_name("zora"), "zora");
}

#[test]
fn probe_summary_collapses_endpoint_results() {
    let results = vec![
        ProbeResult {
            index: 0,
            healthy: true,
            latency: Duration::from_millis(20),
        },
        ProbeResult {
            index: 1,
            healthy: false,
            latency: Duration::from_millis(40),
        },
    ];
    assert_eq!(probe_summary(&results), "1 of 2 RPCs healthy");
}

#[test]
fn verifier_prompt_covers_set_partial_and_clear_states() {
    let cases = [
        (
            "https://verify.example",
            "token",
            (Some("https://verify.example"), Some("token")),
        ),
        (
            "https://verify.example",
            "",
            (Some("https://verify.example"), None),
        ),
        ("", "token", (None, Some("token"))),
        ("", "", (None, None)),
    ];
    for (url, key, expected) in cases {
        let mut prompt =
            ScriptedPrompt::new([Answer::Text(url.into()), Answer::Secret(key.into())]);
        let actual = select_verifier_with(&mut prompt).unwrap();
        assert_eq!(actual.0.as_deref(), expected.0);
        assert_eq!(actual.1.as_deref(), expected.1);
    }
}
