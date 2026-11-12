use super::*;
use indexmap::IndexMap;

const DOC: &str = "# top comment\nversion = 2\ndefault = \"eth\"\n\n# eth is my main chain\n[chains.eth]\nid = 1 # mainnet\nrpc = \"a\"\nfailover = [\"b\"]\n[chains.eth.rpcs]\na = \"https://a.example\" # keep me\nb = \"https://b.example\"\n\n[addresses.router]\neth = \"0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf\"\n";

fn tx(config: &str) -> Tx {
    Tx::parse(
        config,
        Path::new("config.toml"),
        "",
        Path::new("secrets.toml"),
    )
    .unwrap()
}

fn text(tx: &Tx) -> String {
    tx.texts().0
}

#[test]
fn edits_preserve_comments_and_order() {
    let mut tx = tx(DOC);
    tx.set_active("eth", "b").unwrap();
    let text = text(&tx);
    assert!(text.contains("# top comment") && text.contains("# eth is my main chain"));
    assert!(text.contains("# keep me") && text.contains("# mainnet"));
    assert!(text.contains("rpc = \"b\""));
    assert!(!text.contains("failover"), "{text}");
    assert!(tx.set_active("eth", "ghost").is_err());
}

#[test]
fn insert_rename_remove_chain() {
    let mut tx = tx(DOC);
    let mut headers = IndexMap::new();
    headers.insert("x-secret".to_string(), "${GW}".to_string());
    let mut rpcs = IndexMap::new();
    rpcs.insert(
        "gw".to_string(),
        Endpoint {
            url: "https://gw.example".into(),
            headers,
        },
    );
    let base = Chain {
        id: 8453,
        aliases: vec!["Base".into()],
        signer: None,
        explorer: None,
        rpc: "gw".into(),
        failover: vec![],
        rpcs,
    };
    tx.insert_chain("base", &base, Attach::Nothing).unwrap();
    let out = text(&tx);
    assert!(
        out.contains(
            "[chains.base]\nid = 8453\naliases = [\"Base\"]\nrpc = \"gw\"\n\n[chains.base.rpcs]\n"
        ),
        "{out}"
    );
    assert!(
        out.contains("gw = { url = \"https://gw.example\", headers = { x-secret = \"${GW}\" } }"),
        "{out}"
    );
    // The checks `chain add` relies on.
    let err = tx.insert_chain("base", &base, Attach::Nothing).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    let err = tx
        .insert_chain("base2", &base, Attach::Nothing)
        .unwrap_err();
    assert!(
        err.to_string().contains("already configured as 'base'"),
        "{err}"
    );
    let mut ghost = base.clone();
    ghost.id = 99;
    ghost.signer = Some("ghost".into());
    let err = tx
        .insert_chain("other", &ghost, Attach::Nothing)
        .unwrap_err();
    assert!(
        err.to_string().contains("signer 'ghost' not found"),
        "{err}"
    );

    tx.rename_chain("eth", "mainnet").unwrap();
    let out = text(&tx);
    assert!(
        out.contains("[chains.mainnet]") && out.contains("default = \"mainnet\""),
        "{out}"
    );
    assert!(out.contains("mainnet = \"0x7E5F"), "{out}");
    assert!(
        out.find("[chains.mainnet]") < out.find("[chains.base]"),
        "{out}"
    );
    assert!(tx.rename_chain("mainnet", "base").is_err());
    assert!(tx.rename_chain("ghost", "x").is_err());
    assert!(tx.rename_chain("mainnet", "Bad Name").is_err());

    let removed = tx.remove_chain("mainnet").unwrap();
    assert_eq!(
        removed,
        RemovedChain {
            endpoints: 2,
            labels: vec!["router".into()],
            was_default: true,
        }
    );
    let out = text(&tx);
    assert!(!out.contains("default") && !out.contains("router"), "{out}");
}

#[test]
fn rename_drops_an_alias_equal_to_the_new_name() {
    let mut tx = tx(
        "version = 2\n[chains.eth]\nid = 1\naliases = [\"Mainnet\", \"ethereum\"]\nrpc = \"a\"\nrpcs.a = \"https://a.example\"\n",
    );
    tx.rename_chain("eth", "mainnet").unwrap();
    assert!(
        text(&tx).contains("aliases = [\"ethereum\"]"),
        "{}",
        text(&tx)
    );
}

#[test]
fn rename_refuses_a_label_scope_clash_instead_of_leaving_the_old_key() {
    let mut tx = tx(
        "version = 2\n[chains.eth]\nid = 1\nrpc = \"a\"\nrpcs.a = \"https://a.example\"\n\n[addresses.router]\neth = \"0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf\"\nmainnet = \"0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf\"\n",
    );
    let err = tx.rename_chain("eth", "mainnet").unwrap_err();
    assert!(
        err.to_string()
            .contains("label 'router' already has an entry for 'mainnet'"),
        "{err}"
    );
}

#[test]
fn rename_keeps_every_key_comment_and_format() {
    let input = "version = 2\n\n[chains.eth]\nid = 1\nrpc = \"a\"\nrpcs.a = \"https://a.example\"\n\n[chains.base]\nid = 8453\nrpc = \"a\"\nrpcs.a = \"https://a.example\"\n\n[addresses.router]\n# the mainnet deployment\neth   =   \"0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf\" # trailing\n# the base deployment\nbase = \"0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf\"\n";
    let mut tx = tx(input);
    tx.rename_chain("eth", "mainnet").unwrap();
    let out = text(&tx);
    assert!(
        out.contains(
            "# the mainnet deployment\nmainnet   =   \"0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf\" # trailing\n# the base deployment\nbase = "
        ),
        "{out}"
    );
    assert!(
        out.find("[chains.mainnet]") < out.find("[chains.base]"),
        "{out}"
    );
    // Renaming another key leaves the renamed one's comment in place.
    tx.rename_chain("base", "b").unwrap();
    let out = text(&tx);
    assert!(out.contains("# the base deployment\nb = "), "{out}");
    assert!(
        out.contains("# the mainnet deployment\nmainnet   ="),
        "{out}"
    );
}

#[test]
fn record_writes_touch_only_what_changed() {
    let input = "version = 2\n\n[chains.eth]\nid = 1 # mainnet\n# the active one\nrpc = \"a\"\nrpcs.a = { url = \"https://a.example\" } # inline, no headers\nrpcs.b = 'https://b.example'\n";
    let mut tx = tx(input);
    // An unrelated change leaves every other key exactly as written.
    tx.set_aliases("eth", &["ethereum".to_string()]).unwrap();
    let out = text(&tx);
    assert_eq!(
        out,
        input.replace(
            "rpcs.b = 'https://b.example'\n",
            "rpcs.b = 'https://b.example'\naliases = [\"ethereum\"]\n"
        ),
        "{out}"
    );
    // A changed value is replaced in place, keeping its comment.
    tx.set_active("eth", "b").unwrap();
    let out = text(&tx);
    assert!(out.contains("# the active one\nrpc = \"b\"\n"), "{out}");
    // New endpoints follow the layout of the table they join.
    tx.add_endpoint(
        "eth",
        "c",
        &Endpoint::new("https://c.example"),
        EndpointRole::Failover,
    )
    .unwrap();
    let out = text(&tx);
    assert!(out.contains("rpcs.c = \"https://c.example\""), "{out}");
    assert!(out.contains("failover = [\"c\"]"), "{out}");
    // Activating a failover endpoint takes it out of failover.
    tx.add_endpoint(
        "eth",
        "d",
        &Endpoint::new("https://d.example"),
        EndpointRole::Active,
    )
    .unwrap();
    tx.set_active("eth", "c").unwrap();
    let out = text(&tx);
    assert!(!out.contains("failover"), "{out}");
    assert!(out.contains("# inline, no headers"), "{out}");
    let err = tx
        .add_endpoint("eth", "c", &Endpoint::new("https://x"), EndpointRole::Spare)
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("already has an endpoint named 'c'")
    );
    tx.remove_endpoint("eth", "b").unwrap();
    assert!(!text(&tx).contains("b.example"));
    assert!(tx.remove_endpoint("eth", "b").is_err());
}

#[test]
fn signer_records_merge_and_remove_with_their_secret() {
    let mut tx = Tx::parse(
        "version = 2\n\n# my hot wallet\n[signers.hot]\nbackend = \"plaintext\" # for tests\n\n[chains.eth]\nid = 1\nsigner = \"hot\"\nrpc = \"a\"\nrpcs.a = \"https://a.example\"\n",
        Path::new("config.toml"),
        "[signers.hot]\nprivate_key = \"0x01\"\n",
        Path::new("secrets.toml"),
    )
    .unwrap();
    let signer: Signer = toml::from_str(
        "backend = \"plaintext\"\naddress = \"0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf\"\n",
    )
    .unwrap();
    tx.set_signer("hot", &signer).unwrap();
    let out = text(&tx);
    assert!(
        out.contains("# my hot wallet\n[signers.hot]\nbackend = \"plaintext\" # for tests\naddress = \"0x7E5F"),
        "{out}"
    );
    assert_eq!(tx.remove_signer("hot", Users::Unassign).unwrap(), ["eth"]);
    let (config, secrets) = tx.texts();
    assert!(!config.contains("hot"), "{config}");
    assert!(!secrets.contains("private_key"), "{secrets}");
    assert!(tx.remove_signer("hot", Users::Keep).is_err());
}

const A1: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";
const A2: &str = "0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF";
const LABELS: &str = "version = 2\n\n[chains.eth]\nid = 1\nrpc = \"a\"\nrpcs.a = \"https://a.example\"\n\n[chains.base]\nid = 8453\nrpc = \"a\"\nrpcs.a = \"https://a.example\"\n\n[signers.funder]\nbackend = \"keyring\"\n\n# my router\n[addresses.router]\neth = \"0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf\" # mainnet deployment\n";

fn addr(s: &str) -> Address {
    s.parse().unwrap()
}

fn valid(tx: &Tx) {
    crate::validate::validate(&tx.config().unwrap(), None).unwrap();
}

#[test]
fn set_address_per_chain_and_all_chains() {
    let mut tx = tx(LABELS);
    // Lowercase input is stored checksummed; the name is lowercased.
    let lower = addr(&A2.to_ascii_lowercase());
    assert_eq!(
        tx.set_address("Router", "base", &lower, Overwrite::Refuse)
            .unwrap(),
        SetAddress::Added
    );
    assert_eq!(
        tx.set_address("my-safe", ALL_CHAINS, &addr(A1), Overwrite::Refuse)
            .unwrap(),
        SetAddress::Added
    );
    let out = text(&tx);
    assert!(out.contains(&format!("base = \"{A2}\"")), "{out}");
    assert!(
        out.contains(&format!("[addresses.my-safe]\n\"*\" = \"{A1}\"")),
        "{out}"
    );
    assert!(
        out.contains("# my router") && out.contains("# mainnet deployment"),
        "{out}"
    );
    assert!(!out.contains("Router"), "{out}");
    valid(&tx);

    // A first label creates the table.
    let mut fresh = self::tx(&DOC[..DOC.find("[addresses").unwrap()]);
    fresh
        .set_address("x", "eth", &addr(A1), Overwrite::Refuse)
        .unwrap();
    assert!(
        text(&fresh).contains(&format!("[addresses.x]\neth = \"{A1}\"")),
        "{}",
        text(&fresh)
    );
}

#[test]
fn set_address_refuses_to_overwrite_unless_told_to() {
    let mut tx = tx(LABELS);
    let before = text(&tx);
    // Same address: nothing to do.
    assert_eq!(
        tx.set_address("router", "eth", &addr(A1), Overwrite::Refuse)
            .unwrap(),
        SetAddress::Unchanged
    );
    assert_eq!(text(&tx), before);
    let err = tx
        .set_address("router", "eth", &addr(A2), Overwrite::Refuse)
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains(&format!("@router is already {A1} on eth"))
            && msg.contains("chainz addr rm router --chain 'eth'"),
        "{msg}"
    );
    assert_eq!(text(&tx), before);
    assert_eq!(
        tx.set_address("router", "eth", &addr(A2), Overwrite::Replace)
            .unwrap(),
        SetAddress::Replaced(addr(A1))
    );
    let out = text(&tx);
    assert!(
        out.contains(&format!("eth = \"{A2}\" # mainnet deployment")),
        "{out}"
    );
    // The "*" entry is a separate scope from a chain entry.
    tx.set_address("router", ALL_CHAINS, &addr(A1), Overwrite::Refuse)
        .unwrap();
    let err = tx
        .set_address("router", ALL_CHAINS, &addr(A2), Overwrite::Refuse)
        .unwrap_err();
    assert!(err.to_string().contains("on every chain"), "{err}");
}

#[test]
fn set_address_rejects_reserved_names_signers_and_unknown_chains() {
    let mut tx = tx(LABELS);
    let before = text(&tx);
    let set = |tx: &mut Tx, name: &str, scope: &str| {
        tx.set_address(name, scope, &addr(A1), Overwrite::Replace)
    };
    for name in names::context_tokens() {
        let err = set(&mut tx, name, "eth").unwrap_err();
        assert!(err.to_string().contains("context token"), "{name}: {err}");
    }
    for name in ["chainname", "verification_url", "key", "RPC", "Wallet"] {
        let err = set(&mut tx, name, "eth").unwrap_err();
        assert!(err.to_string().contains("context token"), "{name}: {err}");
    }
    for name in ["funder", "FUNDER"] {
        let err = set(&mut tx, name, ALL_CHAINS).unwrap_err();
        assert!(err.to_string().contains("is a signer"), "{name}: {err}");
    }
    for name in ["", "my router", "a.b", "a/b", "@x", "x:base", "é"] {
        let err = set(&mut tx, name, "eth").unwrap_err();
        assert!(err.to_string().contains("[a-z0-9_-]+"), "{name:?}: {err}");
    }
    for scope in ["ghost", "ETH", "1", ""] {
        let err = set(&mut tx, "x", scope).unwrap_err();
        assert!(err.to_string().contains("not found"), "{scope:?}: {err}");
    }
    assert_eq!(text(&tx), before);
    // Built-in token names may be shadowed.
    tx.set_address("usdc", "eth", &addr(A1), Overwrite::Refuse)
        .unwrap();
    valid(&tx);
}

#[test]
fn remove_address_one_entry_or_the_whole_label() {
    let mut tx = tx(LABELS);
    tx.set_address("router", "base", &addr(A2), Overwrite::Refuse)
        .unwrap();
    tx.set_address("router", ALL_CHAINS, &addr(A2), Overwrite::Refuse)
        .unwrap();
    tx.set_address("other", "base", &addr(A2), Overwrite::Refuse)
        .unwrap();

    assert!(tx.remove_address("ghost", None).is_err());
    let err = tx.remove_address("other", Some("eth")).unwrap_err();
    assert!(err.to_string().contains("no address on eth"), "{err}");

    assert_eq!(
        tx.remove_address("Router", Some("base")).unwrap(),
        vec![("base".to_string(), A2.to_string())]
    );
    let out = text(&tx);
    assert!(
        out.contains("[addresses.router]") && out.contains("eth = "),
        "{out}"
    );
    assert_eq!(
        tx.remove_address("router", None).unwrap(),
        vec![
            ("eth".to_string(), A1.to_string()),
            ("*".to_string(), A2.to_string())
        ]
    );
    // Removing the last entry removes the label's table.
    tx.remove_address("other", Some("base")).unwrap();
    let out = text(&tx);
    assert!(!out.contains("addresses"), "{out}");
    valid(&tx);
}

#[test]
fn variables_go_to_secrets_only() {
    let mut tx = tx(DOC);
    tx.set_var("K", "value").unwrap();
    assert!(tx.set_var("CHAIN_ID", "1").is_err());
    assert!(tx.set_var("1BAD", "1").is_err());
    let (config, secrets) = tx.texts();
    assert_eq!(config, DOC);
    assert_eq!(secrets, "[vars]\nK = \"value\"\n");
    assert!(tx.remove_var("K"));
    assert!(!tx.remove_var("K"));
}

#[test]
fn chain_snippet_round_trips() {
    let mut tx = tx(DOC);
    let snippet = tx.chain_snippet("eth").unwrap();
    assert!(
        snippet.contains("[chains.eth]") && snippet.contains("# keep me"),
        "{snippet}"
    );
    let edited = snippet.replace("id = 1", "id = 1\naliases = [\"ethereum\"]");
    tx.replace_chain_snippet("eth", &edited).unwrap();
    assert!(text(&tx).contains("aliases = [\"ethereum\"]"));
    assert!(
        tx.replace_chain_snippet("eth", "[chains.other]\nid = 2\n")
            .is_err()
    );
    let err = tx
        .replace_chain_snippet("eth", "[chains.eth]\nx = \"super-secret\" trailing\n")
        .unwrap_err();
    assert!(!err.to_string().contains("super-secret"), "{err}");
}

#[test]
fn config_errors_do_not_echo_the_source_line() {
    let tx = tx(
        "version = 2\nchains.eth = { id = 1, rpc = 5, rpcs = { a = \"https://x.example/v3/super-secret-key\" } }\n",
    );
    let err = format!("{:#}", tx.config().unwrap_err());
    assert!(!err.contains("super-secret-key"), "{err}");
    assert!(err.contains("edited config does not parse"), "{err}");
}

const SIGNER: &str = "[signers.deployer]\nbackend = \"keystore\"\npath = \"/k\"\naddress = \"0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf\"\n";

#[test]
fn edit_chain_applies_one_patch() {
    let mut tx = tx(&format!("{DOC}{SIGNER}"));
    let edit = ChainEdit {
        signer: Some(Some("deployer".into())),
        rpc: Some("https://mainnet.infura.io/v3/0123456789abcdef0123456789abcdef".into()),
        headers: vec![("x-tag".into(), "chainz".into())],
        failover: Some(vec!["a".into()]),
        explorer: Some(Some("etherscan".into())),
        api_key: Some(zeroize::Zeroizing::new(" es-key\n".into())),
        add_aliases: vec!["Mainnet".into(), "eth".into()],
        name: Some("mainnet-eth".into()),
        ..ChainEdit::default()
    };
    let report = tx.edit_chain("eth", &edit).unwrap();
    assert_eq!(report.chain, "mainnet-eth");
    assert_eq!(report.added_endpoint.as_deref(), Some("infura"));
    assert_eq!(report.created_explorer.as_deref(), Some("etherscan"));
    assert_eq!(report.api_key_var.as_deref(), Some("ETHERSCAN_API_KEY"));
    assert_eq!(report.moved.len(), 1, "{:?}", report.moved);
    let config = tx.config().unwrap();
    let chain = &config.chains["mainnet-eth"];
    assert_eq!(chain.rpc, "infura");
    assert_eq!(
        chain.rpcs["infura"].url,
        "https://mainnet.infura.io/v3/${INFURA_KEY}"
    );
    assert_eq!(chain.rpcs["infura"].headers["x-tag"], "chainz");
    assert_eq!(chain.failover, ["a"]);
    assert_eq!(chain.signer.as_deref(), Some("deployer"));
    assert_eq!(chain.explorer.as_deref(), Some("etherscan"));
    assert_eq!(chain.aliases, ["Mainnet"]);
    assert_eq!(
        config.explorers["etherscan"].api,
        "https://api.etherscan.io/v2/api?chainid=${CHAIN_ID}"
    );
    let secrets = tx.secrets().unwrap();
    assert_eq!(secrets.vars["ETHERSCAN_API_KEY"], "es-key");
    assert!(secrets.vars["INFURA_KEY"].starts_with("0123"));
    let (config_text, _) = tx.texts();
    assert!(
        config_text.contains("# eth is my main chain"),
        "{config_text}"
    );
    assert!(config_text.contains("# keep me"), "{config_text}");
    assert_eq!(config.default.as_deref(), Some("mainnet-eth"));
}

#[test]
fn edit_chain_rejects_what_it_cannot_apply() {
    let err = |edit: ChainEdit| tx(DOC).edit_chain("eth", &edit).unwrap_err().to_string();
    assert!(
        err(ChainEdit {
            rpc: Some("nope".into()),
            ..Default::default()
        })
        .contains("no endpoint 'nope'")
    );
    assert!(
        err(ChainEdit {
            signer: Some(Some("ghost".into())),
            ..Default::default()
        })
        .contains("chainz signer add ghost")
    );
    assert!(
        err(ChainEdit {
            failover: Some(vec!["a".into()]),
            ..Default::default()
        })
        .contains("active endpoint")
    );
    assert!(
        err(ChainEdit {
            explorer: Some(Some("blockscout".into())),
            ..Default::default()
        })
        .contains("built in")
    );
    assert!(
        err(ChainEdit {
            add_aliases: vec!["10".into()],
            ..Default::default()
        })
        .contains("number")
    );
    assert!(
        err(ChainEdit {
            remove_aliases: vec!["x".into()],
            ..Default::default()
        })
        .contains("no alias")
    );
    assert!(
        err(ChainEdit {
            api_key: Some(zeroize::Zeroizing::new("k".into())),
            ..Default::default()
        })
        .contains("no explorer")
    );
    // An existing URL activates its endpoint; nothing is added.
    let mut doc = tx(DOC);
    let edit = ChainEdit {
        rpc: Some("https://b.example".into()),
        ..Default::default()
    };
    let report = doc.edit_chain("eth", &edit).unwrap();
    assert_eq!(report.added_endpoint, None);
    let chain = &doc.config().unwrap().chains["eth"];
    assert_eq!((chain.rpc.as_str(), chain.failover.len()), ("b", 0));
}

#[test]
fn defaults_attach_the_only_signer_and_the_builtin_explorer() {
    let mut tx = tx(&format!(
        "{DOC}{SIGNER}[explorers.etherscan]\napi = \"https://api.etherscan.io/v2/api?chainid=${{CHAIN_ID}}\"\n"
    ));
    let base = Chain {
        id: 8453,
        aliases: vec![],
        signer: None,
        explorer: None,
        rpc: "a".into(),
        failover: vec![],
        rpcs: IndexMap::from([("a".to_string(), Endpoint::new("https://base.example"))]),
    };
    let attached = tx.insert_chain("base", &base, Attach::Defaults).unwrap();
    assert_eq!(attached.signer.as_deref(), Some("deployer"));
    assert_eq!(attached.explorer.as_deref(), Some("etherscan"));
    let mut op = base.clone();
    op.id = 10;
    let attached = tx.insert_chain("op", &op, Attach::ExplorerOnly).unwrap();
    assert_eq!(attached.signer, None);
    assert_eq!(attached.explorer.as_deref(), Some("etherscan"));
    let attached = tx.attach_signer_where_missing("deployer").unwrap();
    assert_eq!(attached, ["eth", "op"]);
    let config = tx.config().unwrap();
    assert!(
        config
            .chains
            .values()
            .all(|c| c.signer.as_deref() == Some("deployer"))
    );
    assert!(tx.attach_signer_where_missing("ghost").is_err());
}

#[test]
fn endpoints_from_urls_are_checked_named_and_matched_one_way() {
    let mut doc = tx(DOC);
    // Named after the host, unique among the chain's endpoints.
    let (name, moved) = doc
        .add_endpoint_url(
            "eth",
            None,
            &Endpoint::new("https://mainnet.infura.io/v3/0123456789abcdef0123456789abcdef"),
            EndpointRole::Failover,
        )
        .unwrap();
    assert_eq!((name.as_str(), moved.len()), ("infura", 1));
    let (name, _) = doc
        .add_endpoint_url(
            "eth",
            None,
            &Endpoint::new("https://other.infura.io"),
            EndpointRole::Spare,
        )
        .unwrap();
    assert_eq!(name, "infura-2");
    let chain = doc.config().unwrap().chains["eth"].clone();
    assert_eq!(chain.failover, ["b", "infura"]);

    let err = |url: &str, name: Option<&str>| {
        tx(DOC)
            .add_endpoint_url("eth", name, &Endpoint::new(url), EndpointRole::Spare)
            .unwrap_err()
            .to_string()
    };
    // A URL the chain has, up to a trailing slash, is refused by name.
    assert!(err("https://b.example/", None).contains("as endpoint 'b'"));
    assert!(err("ftp://x.example", None).contains("http(s)://"));
    assert!(err("https://c.example", Some("a")).contains("named 'a'"));
    // `chain set --rpc` sees the same match: it activates `b`.
    let no_vars = IndexMap::new();
    assert_eq!(chain.endpoint_at("https://b.example/", &no_vars), Some("b"));
    let edit = ChainEdit {
        rpc: Some("https://new.example".into()),
        ..Default::default()
    };
    let Some(RpcChoice::New(name, endpoint)) = edit.rpc_choice("eth", &chain, &no_vars).unwrap()
    else {
        panic!("a new URL is a new endpoint");
    };
    assert_eq!(
        (name.as_str(), endpoint.url.as_str()),
        ("new", "https://new.example")
    );
    let by = |rpc: &str| {
        let edit = ChainEdit {
            rpc: Some(rpc.into()),
            ..Default::default()
        };
        edit.rpc_choice("eth", &chain, &no_vars)
    };
    let existing = Some(RpcChoice::Existing("b".to_string()));
    assert_eq!(by("b").unwrap(), existing);
    assert_eq!(by("https://b.example/").unwrap(), existing);
    assert!(
        by("nope")
            .unwrap_err()
            .to_string()
            .contains("no endpoint 'nope'")
    );
}

/// Re-adding a URL whose key an earlier write moved into a variable is the
/// same endpoint (it used to add `alchemy-2`, `alchemy-3`…), for `rpc add`
/// and `chain set --rpc` alike.
#[test]
fn a_url_whose_key_moved_is_still_the_same_endpoint() {
    let url = "https://base-mainnet.g.alchemy.com/v2/abcdef0123456789abcdef0123455678";
    let mut doc = tx(DOC);
    let (name, moved) = doc
        .add_endpoint_url("eth", None, &Endpoint::new(url), EndpointRole::Spare)
        .unwrap();
    assert_eq!((name.as_str(), moved.len()), ("alchemy", 1));
    let stored = doc.config().unwrap().chains["eth"].rpcs["alchemy"]
        .url
        .clone();
    assert!(stored.ends_with("/v2/${ALCHEMY_KEY}"), "{stored}");

    // The pasted URL again, with or without a trailing slash: refused.
    for again in [url.to_string(), format!("{url}/")] {
        let err = doc
            .add_endpoint_url("eth", None, &Endpoint::new(again), EndpointRole::Spare)
            .unwrap_err();
        assert!(err.to_string().contains("as endpoint 'alchemy'"), "{err}");
    }
    // `chain set --rpc URL` activates it instead of adding one.
    let edit = ChainEdit {
        rpc: Some(url.into()),
        ..Default::default()
    };
    let report = doc.edit_chain("eth", &edit).unwrap();
    assert_eq!(report.added_endpoint, None);
    let chain = doc.config().unwrap().chains["eth"].clone();
    assert_eq!(chain.rpc, "alchemy");
    assert_eq!(chain.rpcs.len(), 3);

    // The rule: references the variables hold are expanded on both sides;
    // others compare as written.
    let mut vars = IndexMap::new();
    vars.insert("K".to_string(), "secret".to_string());
    assert!(Endpoint::same_url(
        "https://x/${K}",
        "https://x/secret/",
        &vars
    ));
    assert!(Endpoint::same_url(
        "https://x/${U}",
        "https://x/${U}",
        &vars
    ));
    assert!(!Endpoint::same_url(
        "https://x/${U}",
        "https://x/secret",
        &vars
    ));
    assert!(!Endpoint::same_url(
        "https://x/${K}",
        "https://x/other",
        &vars
    ));
}
