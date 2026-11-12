//! Naming rules: chain short names, endpoint names, label names, and the
//! built-in short-name table used by `chain add` and v1 migration.

/// Context tokens. Labels may not use these names.
pub const CONTEXT_TOKENS: [&str; 6] = [
    "rpc",
    "chainid",
    "chain",
    "wallet",
    "explorer",
    "explorer_key",
];

/// v1 token names kept as deprecated aliases for one release.
pub const DEPRECATED_TOKENS: [(&str, &str); 3] = [
    ("chainname", "chain"),
    ("verification_url", "explorer"),
    ("verifier_api_key", "explorer_key"),
];

/// Names a fork may not take (they are `chainz fork` subcommands).
pub const RESERVED_FORK_NAMES: [&str; 3] = ["ls", "list", "stop"];

/// Short names for well-known chain IDs (first match wins in reverse
/// lookups). Unknown IDs fall back to [`slugify`] of the chainlist name.
pub const SHORT_NAMES: &[(u64, &str)] = &[
    (1, "eth"),
    (10, "op"),
    (56, "bsc"),
    (100, "gnosis"),
    (130, "uni"),
    (137, "polygon"),
    (196, "xlayer"),
    (250, "fantom"),
    (324, "zksync"),
    (480, "world"),
    (1101, "polygon-zkevm"),
    (1301, "uni-sepolia"),
    (1868, "soneium"),
    (4326, "megaeth"),
    (4663, "robinhood"),
    (5000, "mantle"),
    (5042, "arc"),
    (8453, "base"),
    (17000, "holesky"),
    (34443, "mode"),
    (42161, "arb"),
    (42220, "celo"),
    (43114, "avax"),
    (57073, "ink"),
    (59144, "linea"),
    (81457, "blast"),
    (84532, "base-sepolia"),
    (421614, "arb-sepolia"),
    (534352, "scroll"),
    (560048, "hoodi"),
    (7777777, "zora"),
    (11155111, "sepolia"),
    (11155420, "op-sepolia"),
];

/// The built-in short name for a chain ID.
pub fn short_name(chain_id: u64) -> Option<&'static str> {
    SHORT_NAMES
        .iter()
        .find(|(id, _)| *id == chain_id)
        .map(|(_, name)| *name)
}

/// Whether `name` is a valid chain or endpoint name:
/// `[a-z0-9][a-z0-9_-]*`, and not all digits (digits mean a chain ID).
pub fn is_valid_chain_name(name: &str) -> bool {
    is_name(name) && !name.bytes().all(|b| b.is_ascii_digit())
}

/// Why `name` cannot be a chain name, if it cannot (see
/// [`is_valid_chain_name`]).
pub fn check_chain_name(name: &str) -> anyhow::Result<()> {
    if !is_valid_chain_name(name) {
        anyhow::bail!("chain name '{name}' must match [a-z0-9][a-z0-9_-]* and not be all digits");
    }
    Ok(())
}

/// Whether `name` is a valid endpoint name: `[a-z0-9][a-z0-9_-]*`.
pub fn is_valid_endpoint_name(name: &str) -> bool {
    is_name(name)
}

/// Whether `name` is a valid label name: `[a-z0-9_-]+`, lowercase only
/// (the token grammar matches names case-insensitively, so stored names
/// are lowercase).
pub fn is_valid_label_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Whether `name` is taken by the token grammar itself: a context token,
/// a deprecated v1 token, or the removed `@key`. Labels and signers may
/// not use these names, since they would be shadowed by (or shadow) the
/// token.
pub fn is_reserved_token(name: &str) -> bool {
    CONTEXT_TOKENS.contains(&name)
        || DEPRECATED_TOKENS.iter().any(|(n, _)| *n == name)
        || name == "key"
}

/// Why `name` cannot be a signer name, if it cannot: signer names match
/// `[a-z0-9][a-z0-9_-]*` and are not [reserved token names](is_reserved_token),
/// because every signer is also the label `@<name>`.
pub fn check_signer_name(name: &str) -> anyhow::Result<()> {
    if !is_name(name) {
        anyhow::bail!("signer name '{name}' must match [a-z0-9][a-z0-9_-]*");
    }
    if is_reserved_token(name) {
        anyhow::bail!("signer name '{name}' is reserved for the token @{name}; pick another name");
    }
    Ok(())
}

/// Why `name` cannot be a label, if it cannot. `signers` are the
/// configured signer names (signers are labels automatically). The one
/// definition of the label-name rules, shared by validation.
pub fn check_label_name<'a>(
    name: &str,
    signers: impl IntoIterator<Item = &'a str>,
) -> anyhow::Result<()> {
    if !is_valid_label_name(name) {
        anyhow::bail!("label name '{name}' must match [a-z0-9_-]+");
    }
    if is_reserved_token(name) {
        anyhow::bail!("'{name}' is a context token (@{name}) and cannot be used as a label");
    }
    if signers.into_iter().any(|s| s.eq_ignore_ascii_case(name)) {
        anyhow::bail!(
            "'{name}' is a signer, and signers are labels already (@{name} is its address)"
        );
    }
    Ok(())
}

/// The token-name alphabet, matched case-insensitively.
pub fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn is_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

/// Lowercase slug: runs of anything outside `[a-z0-9]` become one `-`.
pub fn slugify(input: &str) -> String {
    let mut out = String::new();
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() || out.bytes().all(|b| b.is_ascii_digit()) {
        out.insert_str(0, "chain-");
    }
    out
}

/// Endpoint name derived from a URL's host: the registrable label
/// (`mainnet.infura.io` → `infura`, `eth.llamarpc.com` → `llamarpc`),
/// `local` for localhost and IPs, plus `-ws` for websocket URLs.
pub fn endpoint_name_for_url(url: &str) -> String {
    let base = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .map(|host| {
            let bare = host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string();
            if bare == "localhost" || bare.parse::<std::net::IpAddr>().is_ok() {
                "local".to_string()
            } else {
                let labels: Vec<&str> = bare.split('.').filter(|l| !l.is_empty()).collect();
                let label = match labels.len() {
                    0 => "rpc",
                    1 => labels[0],
                    n => labels[n - 2],
                };
                slugify(label)
            }
        })
        .unwrap_or_else(|| "rpc".to_string());
    let lower = url.trim_start().to_ascii_lowercase();
    if lower.starts_with("ws://") || lower.starts_with("wss://") {
        format!("{base}-ws")
    } else {
        base
    }
}

/// `base`, or `base-2`, `base-3`… — the first name `taken` rejects.
pub fn unique_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken(candidate))
        .expect("an unused suffix exists")
}

/// The closest name to `query` by edit distance, if reasonably close.
pub fn suggest<'a>(query: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let query = query.to_ascii_lowercase();
    candidates
        .into_iter()
        .map(|c| (levenshtein(&query, &c.to_ascii_lowercase()), c))
        .filter(|(d, c)| *d <= 2.max(c.len() / 3) && *d < c.len().max(1))
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_names_follow_the_host() {
        assert_eq!(
            endpoint_name_for_url("https://mainnet.infura.io/v3/x"),
            "infura"
        );
        assert_eq!(
            endpoint_name_for_url("wss://eth.llamarpc.com"),
            "llamarpc-ws"
        );
        assert_eq!(endpoint_name_for_url("http://127.0.0.1:8545"), "local");
        assert_eq!(
            endpoint_name_for_url("https://cloudflare-eth.com"),
            "cloudflare-eth"
        );
        assert_eq!(endpoint_name_for_url("not a url"), "rpc");
    }

    #[test]
    fn names_and_slugs() {
        assert!(is_valid_chain_name("base-sepolia"));
        assert!(!is_valid_chain_name("8453"));
        assert!(check_chain_name("base").is_ok());
        let err = check_chain_name("8453").unwrap_err().to_string();
        assert!(err.contains("not be all digits"), "{err}");
        assert!(!is_valid_chain_name("Base"));
        assert_eq!(slugify("OP Mainnet"), "op-mainnet");
        assert_eq!(slugify("zkSync  Era!"), "zksync-era");
        assert_eq!(slugify("1234"), "chain-1234");
        assert_eq!(
            unique_name("infura", |n| n == "infura" || n == "infura-2"),
            "infura-3"
        );
        assert_eq!(short_name(42161), Some("arb"));
    }

    #[test]
    fn label_and_signer_names() {
        assert!(is_valid_label_name("my-safe_2"));
        assert!(is_valid_label_name("_x"));
        assert!(!is_valid_label_name("MySafe"));
        assert!(!is_valid_label_name(""));
        check_signer_name("deployer").unwrap();
        check_signer_name("2nd-key").unwrap();
        for bad in ["", "-x", "_x", "Deployer", "a b"] {
            let err = check_signer_name(bad).unwrap_err().to_string();
            assert!(err.contains("[a-z0-9][a-z0-9_-]*"), "{bad:?}: {err}");
        }
        for reserved in [
            "wallet",
            "rpc",
            "chainname",
            "verification_url",
            "verifier_api_key",
            "key",
        ] {
            let err = check_signer_name(reserved).unwrap_err().to_string();
            assert!(err.contains("reserved"), "{reserved}: {err}");
        }
    }

    #[test]
    fn suggestions_are_close_matches_only() {
        assert_eq!(suggest("udsc", ["usdc", "weth", "wallet"]), Some("usdc"));
        assert_eq!(suggest("zzzzzz", ["usdc", "weth"]), None);
    }
}
