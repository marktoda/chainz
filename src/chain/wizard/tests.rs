use super::{manual_chain_entry, probe_summary, select_key, select_verifier, suggest_short_name};
use crate::chain::{ChainDefinition, rpc::ProbeResult};
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
