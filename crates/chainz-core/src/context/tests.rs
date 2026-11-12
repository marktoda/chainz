use super::*;
use crate::runtime::{GatewayAddr, ServeInfo};
use crate::store::{LoadMode, StorePaths};

const CONFIG: &str = r#"
version = 2
default = "eth"

[explorers.etherscan]
api = "https://api.etherscan.io/v2/api?chainid=${CHAIN_ID}"
key = "${CTX_TEST_ETHERSCAN}"

[explorers.blockscout]
api = "https://explorer.example/api"
key = "bs-literal"

[explorers.unset]
api = "https://unset.example/api"
key = "${CTX_TEST_UNSET_EXPLORER_KEY}"

[chains.eth]
id = 1
aliases = ["mainnet"]
signer = "default"
explorer = "etherscan"
rpc = "infura"
[chains.eth.rpcs]
infura = "https://mainnet.infura.io/v3/${CTX_TEST_INFURA}"
gw = { url = "https://gw.example/rpc/1", headers = { x-secret = "${CTX_TEST_GW}" } }
ws = "wss://eth.example"

[chains.celo]
id = 42220
signer = "deployer"
explorer = "blockscout"
rpc = "a"
rpcs.a = "https://celo.example"

[chains.bare]
id = 999
rpc = "a"
rpcs.a = "https://bare.example/${CTX_TEST_MISSING}"

[chains.nosigner]
id = 998
explorer = "unset"
rpc = "a"
rpcs.a = "https://nosigner.example"

[signers.default]
backend = "keyring"
address = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"

[signers.deployer]
backend = "keystore"
path = "/keys/deployer"
address = "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF"

[addresses.myrouter]
celo = "0x6813Eb9362372EEF6200f3b1dbC3f819671cBA69"
bare = "0x6813Eb9362372EEF6200f3b1dbC3f819671cBA69"
"#;

const SECRETS: &str = "[vars]\nCTX_TEST_ETHERSCAN = \"es-key\"\nCTX_TEST_INFURA = \"inf-key\"\nCTX_TEST_GW = \"gw-secret\"\n";

struct Fixture {
    _dir: tempfile::TempDir,
    chainz: Chainz,
    runtime: Runtime,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::in_dir(dir.path().join("config"));
    std::fs::create_dir_all(paths.dir()).unwrap();
    std::fs::write(paths.config(), CONFIG).unwrap();
    std::fs::write(paths.secrets(), SECRETS).unwrap();
    // A fixed, empty environment: the developer's shell (an enclosing
    // `chainz shell`, an exported variable) never changes these tests.
    let chainz = Chainz::open_at(paths, LoadMode::Strict)
        .unwrap()
        .with_env(crate::contract::Env::from_fn(|_| None));
    let runtime = Runtime::at(dir.path().join("run"));
    Fixture {
        _dir: dir,
        chainz,
        runtime,
    }
}

fn ctx(f: &Fixture, chain: &str, opts: &ExecOptions) -> Result<Context> {
    let target = resolve::resolve(f.chainz.config(), Some(&f.runtime), chain)?;
    build(&f.chainz, &f.runtime, &target, opts)
}

fn env_map(ctx: &Context) -> HashMap<&'static str, String> {
    ctx.exports()
        .unwrap()
        .set
        .into_iter()
        .map(|e| (e.name, e.value))
        .collect()
}

#[test]
fn etherscan_chain_env_table() {
    let f = fixture();
    let c = ctx(&f, "eth", &ExecOptions::default()).unwrap();
    let env = env_map(&c);
    assert_eq!(env["ETH_RPC_URL"], "https://mainnet.infura.io/v3/inf-key");
    assert_eq!(env["CHAIN_ID"], "1");
    assert_eq!(env["CHAINZ_CHAIN"], "eth");
    assert_eq!(env["CHAIN_NAME"], "eth");
    assert_eq!(env["ETHERSCAN_API_KEY"], "es-key");
    assert!(!env.contains_key("VERIFIER_URL") && !env.contains_key("VERIFIER_API_KEY"));
    assert_eq!(
        env["ETH_FROM"],
        "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"
    );
    assert_eq!(env["WALLET_ADDRESS"], env["ETH_FROM"]);
    assert!(!env.contains_key("ETH_KEYSTORE") && !env.contains_key("ETH_RPC_HEADERS"));
}

#[test]
fn non_etherscan_explorer_and_keystore() {
    let f = fixture();
    let env = env_map(&ctx(&f, "celo", &ExecOptions::default()).unwrap());
    assert_eq!(env["VERIFIER_URL"], "https://explorer.example/api");
    assert_eq!(env["VERIFIER_API_KEY"], "bs-literal");
    assert_eq!(env["ETHERSCAN_API_KEY"], "bs-literal");
    assert_eq!(env["ETH_KEYSTORE"], "/keys/deployer");
}

#[test]
fn rpc_override_exports_headers_and_explain_redacts() {
    let f = fixture();
    let opts = ExecOptions {
        rpc: Some("gw".into()),
        ..Default::default()
    };
    let c = ctx(&f, "eth", &opts).unwrap();
    let env = c.exports().unwrap().set;
    let headers = env.iter().find(|e| e.name == "ETH_RPC_HEADERS").unwrap();
    assert_eq!(headers.value, "x-secret: gw-secret");
    assert_eq!(headers.display(), "x-secret: [REDACTED]");
    let rpc = env.iter().find(|e| e.name == "ETH_RPC_URL").unwrap();
    assert!(!rpc.display().contains("inf-key"));
    assert!(
        ctx(
            &f,
            "eth",
            &ExecOptions {
                rpc: Some("nope".into()),
                ..Default::default()
            }
        )
        .is_err()
    );
}

#[test]
fn unresolved_variables_fail_closed_naming_the_variable() {
    let f = fixture();
    let err = format!(
        "{:#}",
        ctx(&f, "bare", &ExecOptions::default()).unwrap_err()
    );
    assert!(err.contains("CTX_TEST_MISSING"), "{err}");
}

#[test]
fn gateway_seam_routes_through_serve_unless_direct() {
    let f = fixture();
    let lock = f.runtime.acquire_serve_lock().unwrap().unwrap();
    let addr = GatewayAddr {
        port: 18545,
        token: "t0k".into(),
    };
    let _published = lock.publish(&ServeInfo::new(&addr, 0)).unwrap();
    let opts = ExecOptions {
        rpc: Some("gw".into()),
        ..Default::default()
    };
    let c = ctx(&f, "eth", &opts).unwrap();
    assert!(c.rpc.via_gateway);
    let env = env_map(&c);
    assert_eq!(env["ETH_RPC_URL"], "http://127.0.0.1:18545/t0k/eth/gw");
    assert!(
        !env.contains_key("ETH_RPC_HEADERS"),
        "the gateway adds headers itself"
    );
    let direct = ctx(
        &f,
        "eth",
        &ExecOptions {
            direct: true,
            ..opts
        },
    )
    .unwrap();
    assert!(!direct.rpc.via_gateway);
    let ws = ctx(
        &f,
        "eth",
        &ExecOptions {
            rpc: Some("ws".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!ws.rpc.via_gateway, "websockets are never routed");
}

#[test]
fn forks_use_anvil_and_never_export_keystore() {
    let f = fixture();
    let claim = f.runtime.claim_fork("celo-fork").unwrap().unwrap();
    claim
        .register(&ForkRecord {
            format: ForkRecord::FORMAT,
            name: "celo-fork".into(),
            pid: 1,
            anvil_pid: 2,
            port: 8601,
            parent: "celo".into(),
            chain_id: 42220,
            fork_block: None,
            started_at: 0,
            ready: true,
        })
        .unwrap();
    let c = ctx(&f, "celo-fork", &ExecOptions::default()).unwrap();
    let env = env_map(&c);
    assert_eq!(env["ETH_RPC_URL"], "http://127.0.0.1:8601");
    assert_eq!(env["CHAINZ_CHAIN"], "celo-fork");
    assert!(!env.contains_key("ETH_KEYSTORE") && !env.contains_key("ETHERSCAN_API_KEY"));
    assert_eq!(
        env["ETH_FROM"],
        "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF"
    );
}

fn expand(f: &Fixture, chain: &str, args: &[&str]) -> Result<Vec<String>> {
    let opts = ExecOptions::default();
    let c = ctx(f, chain, &opts)?;
    let e = Expander::new(&f.chainz, &f.runtime, &c, opts.direct);
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    Ok(e.expand_args(&args)?.into_iter().map(|a| a.value).collect())
}

#[test]
fn tokens_expand_with_qualifiers_labels_and_signers() {
    let f = fixture();
    let out = expand(
        &f,
        "eth",
        &[
            "@chainid",
            "@chain",
            "@wallet",
            "@deployer",
            "--to=@usdc",
            "@usdc:celo",
            "@myrouter:celo",
            "@chainid:42220",
            "user@host",
            "@uniswap/v3-core",
            "@@wallet",
            "x@unknownthing",
        ],
    )
    .unwrap();
    assert_eq!(
        out,
        [
            "1",
            "eth",
            "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf",
            "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF",
            "--to=0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
            "0xcebA9300f2b948710d2653dD7B07f33A8B32118C",
            "0x6813Eb9362372EEF6200f3b1dbC3f819671cBA69",
            "42220",
            "user@host",
            "@uniswap/v3-core",
            "@wallet",
            "x@unknownthing",
        ]
    );
}

#[test]
fn token_errors() {
    let f = fixture();
    let err = expand(&f, "eth", &["@udsc"]).unwrap_err().to_string();
    assert!(err.contains("did you mean @usdc?"), "{err}");
    let err = expand(&f, "eth", &["@myrouter"]).unwrap_err().to_string();
    assert!(err.contains("no address on chain 'eth'"), "{err}");
    let err = expand(&f, "eth", &["--x=@key"]).unwrap_err().to_string();
    assert!(err.contains("--expose-key"), "{err}");
    // A chain whose context builds but has no signer.
    let err = expand(&f, "nosigner", &["@wallet"])
        .unwrap_err()
        .to_string();
    assert!(err.contains("chain 'nosigner' has no signer"), "{err}");
}

#[test]
fn qualified_tokens_build_only_what_they_use() {
    let f = fixture();
    // `bare`'s endpoint needs an unset variable: its labels, ID, name and
    // signer still resolve, only @rpc:bare fails.
    let out = expand(
        &f,
        "eth",
        &[
            "@myrouter:bare",
            "@chainid:bare",
            "@chain:bare",
            "@deployer:bare",
        ],
    )
    .unwrap();
    assert_eq!(
        out,
        [
            "0x6813Eb9362372EEF6200f3b1dbC3f819671cBA69",
            "999",
            "bare",
            "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF"
        ]
    );
    let err = format!("{:#}", expand(&f, "eth", &["@rpc:bare"]).unwrap_err());
    assert!(err.contains("CTX_TEST_MISSING"), "{err}");
    let err = format!("{:#}", expand(&f, "eth", &["@wallet:bare"]).unwrap_err());
    assert!(err.contains("chain 'bare' has no signer"), "{err}");
    // The other chain's RPC and explorer, when asked for.
    let out = expand(
        &f,
        "eth",
        &["@rpc:celo", "@explorer:celo", "@explorer_key:celo"],
    )
    .unwrap();
    assert_eq!(
        out,
        [
            "https://celo.example",
            "https://explorer.example/api",
            "bs-literal"
        ]
    );
}

#[test]
fn an_unset_explorer_key_fails_only_when_used() {
    let f = fixture();
    let c = ctx(&f, "nosigner", &ExecOptions::default()).unwrap();
    let explorer = c.explorer.as_ref().unwrap();
    assert!(explorer.key.is_none());
    assert!(
        explorer
            .key_error
            .as_deref()
            .is_some_and(|e| e.contains("CTX_TEST_UNSET_EXPLORER_KEY")),
        "{explorer:?}"
    );
    let env = env_map(&c);
    assert!(!env.contains_key("ETHERSCAN_API_KEY") && !env.contains_key("VERIFIER_API_KEY"));
    assert_eq!(env["VERIFIER_URL"], "https://unset.example/api");
    assert_eq!(
        expand(&f, "nosigner", &["@explorer", "@chainid"]).unwrap(),
        ["https://unset.example/api", "998"]
    );
    let err = format!(
        "{:#}",
        expand(&f, "nosigner", &["@explorer_key"]).unwrap_err()
    );
    assert!(err.contains("CTX_TEST_UNSET_EXPLORER_KEY"), "{err}");
    let err = format!(
        "{:#}",
        expand(&f, "eth", &["@explorer_key:nosigner"]).unwrap_err()
    );
    assert!(err.contains("CTX_TEST_UNSET_EXPLORER_KEY"), "{err}");
}

#[test]
fn env_var_debug_redacts_urls() {
    let var = EnvVar {
        name: "VERIFIER_URL",
        value: "https://explorer.example/api?apikey=verifier-secret".into(),
        shown: Shown::Url,
    };
    let debug = format!("{var:?}");
    assert!(!debug.contains("verifier-secret"), "{debug}");
    assert!(debug.starts_with("VERIFIER_URL=https://"), "{debug}");
    let headers = EnvVar {
        name: "ETH_RPC_HEADERS",
        value: "x-key: header-secret".into(),
        shown: Shown::Headers,
    };
    assert_eq!(format!("{headers:?}"), "ETH_RPC_HEADERS=x-key: [REDACTED]");
    let f = fixture();
    let env = ctx(&f, "celo", &ExecOptions::default())
        .unwrap()
        .exports()
        .unwrap()
        .set;
    let verifier = env.iter().find(|e| e.name == "VERIFIER_URL").unwrap();
    assert!(verifier.is_secret());
}

#[test]
fn deprecated_tokens_warn_and_secrets_are_redacted_for_explain() {
    let f = fixture();
    let opts = ExecOptions::default();
    let c = ctx(&f, "eth", &opts).unwrap();
    let e = Expander::new(&f.chainz, &f.runtime, &c, opts.direct);
    let args: Vec<String> = [
        "@chainname",
        "@verifier_api_key",
        "--rpc-url=@rpc",
        "@chainname",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let out = e.expand_args(&args).unwrap();
    assert_eq!(out[0].value, "eth");
    assert_eq!(out[1].value, "es-key");
    assert_eq!(out[1].display, "[REDACTED]");
    assert!(out[2].value.contains("inf-key") && !out[2].display.contains("inf-key"));
    // Each warning once, even when repeated non-adjacently.
    assert_eq!(
        e.warnings(),
        [
            "@chainname is deprecated; use @chain",
            "@verifier_api_key is deprecated; use @explorer_key"
        ]
    );
}

#[test]
fn exports_list_unset_contract_vars_and_warnings_once() {
    let f = fixture();
    let exports = ctx(&f, "nosigner", &ExecOptions::default())
        .unwrap()
        .exports()
        .unwrap();
    for name in CONTRACT_VARS {
        let set = exports.set.iter().any(|v| v.name() == name);
        assert_ne!(set, exports.unset.contains(&name), "{name}");
    }
    assert!(exports.unset.contains(&RAW_PRIVATE_KEY));
    assert_eq!(exports.warnings.len(), 1, "{:?}", exports.warnings);
    assert!(exports.warnings[0].starts_with("not exporting the explorer key: "));
    let mut command = std::process::Command::new("true");
    exports.apply(&mut command).unwrap();
    let removed: Vec<_> = command
        .get_envs()
        .filter(|(_, v)| v.is_none())
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    assert!(removed.iter().any(|k| k == "ETH_KEYSTORE"), "{removed:?}");
}

#[test]
fn expose_key_needs_a_signer_and_no_fork_and_an_unlocked_key() {
    let f = fixture();
    let expose = ExecOptions {
        expose_key: true,
        ..Default::default()
    };
    let err = ctx(&f, "nosigner", &expose).unwrap_err().to_string();
    assert!(
        err.contains("`chainz chain set nosigner --signer NAME`"),
        "{err}"
    );
    let claim = f.runtime.claim_fork("eth-fork").unwrap().unwrap();
    claim
        .register(&ForkRecord {
            format: ForkRecord::FORMAT,
            name: "eth-fork".into(),
            pid: 1,
            anvil_pid: 2,
            port: 8602,
            parent: "eth".into(),
            chain_id: 1,
            fork_block: None,
            started_at: 0,
            ready: true,
        })
        .unwrap();
    let err = ctx(&f, "eth-fork", &expose).unwrap_err().to_string();
    assert!(err.contains("refused on a fork"), "{err}");

    // Listed (redacted) for --explain, but never applied while locked.
    let c = ctx(&f, "eth", &expose).unwrap();
    let exports = c.exports().unwrap();
    let raw = exports
        .set
        .iter()
        .find(|v| v.name() == RAW_PRIVATE_KEY)
        .unwrap();
    assert_eq!(raw.display(), "[REDACTED]");
    let err = exports
        .apply(&mut std::process::Command::new("true"))
        .unwrap_err();
    assert!(err.to_string().contains("not unlocked"), "{err}");
}

#[test]
fn explain_is_a_safe_view() {
    let f = fixture();
    let c = ctx(&f, "eth", &ExecOptions::default()).unwrap();
    let view = c.explain();
    assert_eq!((view.chain, view.chain_id), ("eth", 1));
    assert!(!view.rpc.contains("inf-key"), "{}", view.rpc);
    assert!(view.signer.is_some_and(|(name, _, _)| !name.is_empty()));
}
