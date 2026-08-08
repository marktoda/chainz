use super::{
    manual_chain_entry, merge_refreshed_rpcs, probe_summary, select_key, select_verifier,
    suggest_short_name,
};
use crate::chain::{ChainDefinition, RpcEndpoint, rpc::ProbeResult};
use crate::config::Chainz;
use crate::opt::UpdateArgs;
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

fn make_chain(rpc_urls: Vec<RpcEndpoint>) -> ChainDefinition {
    ChainDefinition {
        name: "ethereum".into(),
        aliases: vec![],
        chain_id: 1,
        rpc_urls,
        selected_rpc: "https://eth.llamarpc.com".into(),
        verification_api_key: None,
        verification_url: None,
        key_name: None,
    }
}

#[test]
fn merge_refreshed_rpcs_preserves_headers_and_appends_dropped_gateways() {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("x-secret".to_string(), "${GW_SECRET}".to_string());
    let chain = make_chain(vec![
        "https://eth.llamarpc.com".into(),
        RpcEndpoint::with_headers("https://private-gateway.example.com/rpc", headers.clone()),
    ]);

    let refreshed = vec![
        "https://eth.llamarpc.com".to_string(),
        "https://new-public-rpc.example.com".to_string(),
    ];
    let candidates = merge_refreshed_rpcs(&chain, refreshed);

    // Refreshed URLs come first, in refreshed order, with known headers preserved.
    assert_eq!(candidates.len(), 3);
    assert_eq!(candidates[0].url, "https://eth.llamarpc.com");
    assert!(candidates[0].headers.is_empty());
    assert_eq!(candidates[1].url, "https://new-public-rpc.example.com");
    assert!(candidates[1].headers.is_empty());

    // The headered private gateway dropped from chainlist survives, appended
    // at the end with its headers intact.
    assert_eq!(candidates[2].url, "https://private-gateway.example.com/rpc");
    assert_eq!(candidates[2].headers, headers);
}

#[test]
fn merge_refreshed_rpcs_drops_headerless_urls_missing_from_refresh() {
    let chain = make_chain(vec!["https://stale-public-rpc.example.com".into()]);
    let candidates = merge_refreshed_rpcs(&chain, vec!["https://new.example.com".to_string()]);

    // A plain (headerless) URL that fell off the refreshed list is simply
    // gone — no credential to lose, so no special-casing needed.
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].url, "https://new.example.com");
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
        let actual = select_verifier(&mut prompt).unwrap();
        assert_eq!(actual.0.as_deref(), expected.0);
        assert_eq!(actual.1.as_deref(), expected.1);
    }
}

#[tokio::test]
async fn scripted_prompt_drives_manual_entry_and_update_menu() {
    let mut entry_prompt =
        ScriptedPrompt::new([Answer::Text("local".into()), Answer::Text("31337".into())]);
    let entry = manual_chain_entry(&mut entry_prompt, None, None)
        .await
        .unwrap();
    assert_eq!(entry.name, "local");
    assert_eq!(entry.chain_id, 31_337);

    let mut chain = ChainDefinition {
        name: entry.name,
        aliases: vec![],
        chain_id: entry.chain_id,
        rpc_urls: vec!["http://127.0.0.1:8545".into()],
        selected_rpc: "http://127.0.0.1:8545".into(),
        verification_api_key: None,
        verification_url: None,
        key_name: None,
    };
    let args = UpdateArgs {
        name_or_id: None,
        refresh: false,
        name: None,
        rpc_url: None,
        headers: vec![],
        clear_headers: false,
        key: None,
        no_key: false,
        verification_url: None,
        verification_api_key: None,
        verification_api_key_stdin: false,
        clear_verification: false,
    };
    let mut update_prompt = ScriptedPrompt::new([
        Answer::Select(3),
        Answer::Text("local-renamed".into()),
        Answer::Select(4),
    ]);
    let mut chainz = Chainz::new();
    args.edit_interactively(&mut update_prompt, &mut chainz, &mut chain)
        .await
        .unwrap();
    assert_eq!(chain.name, "local-renamed");
}

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
    for bad in [
        "no-colon-secret",
        ": value-secret",
        "name-only:",
        "name-only:   ",
    ] {
        let error = super::parse_rpc_headers(&[bad.to_string()]).unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(!diagnostic.contains("secret"), "{diagnostic}");
    }
    // duplicate names rejected
    let error =
        super::parse_rpc_headers(&["x-dup: a".to_string(), "x-dup: b".to_string()]).unwrap_err();
    assert!(format!("{error:#}").contains("x-dup"));
}

#[test]
fn scripted_prompt_drives_staged_key_selection() {
    const PRIVATE_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    let mut chainz = Chainz::new();
    let mut prompt = ScriptedPrompt::new([
        Answer::Select(1),
        Answer::Text("deployer".into()),
        Answer::Secret(PRIVATE_KEY.into()),
    ]);

    let selected = select_key(&mut prompt, &mut chainz).unwrap();

    assert_eq!(selected.as_deref(), Some("deployer"));
    assert!(chainz.get_key("deployer").is_ok());
}
