//! Naming rules: chain short names, endpoint names, label names, and the
//! built-in short-name table used by `chain add` and v1 migration.

/// What a name in the token grammar's own vocabulary means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Token {
    /// A context token (`@rpc`, `@wallet`…).
    Context,
    /// A v1 token kept for one release as an alias of this context token.
    Deprecated(&'static str),
    /// A removed token; the hint says what replaces it.
    Removed(&'static str),
}

/// The token grammar's own names: context tokens, deprecated v1 tokens and
/// removed ones. Labels and signers may not use any of them. The one table
/// expansion, naming rules and completion read.
pub(crate) const TOKENS: [(&str, Token); 10] = [
    ("rpc", Token::Context),
    ("chainid", Token::Context),
    ("chain", Token::Context),
    ("wallet", Token::Context),
    ("explorer", Token::Context),
    ("explorer_key", Token::Context),
    ("chainname", Token::Deprecated("chain")),
    ("verification_url", Token::Deprecated("explorer")),
    ("verifier_api_key", Token::Deprecated("explorer_key")),
    (
        "key",
        Token::Removed("pass --expose-key and read $RAW_PRIVATE_KEY instead"),
    ),
];

/// What `name` means in the token grammar, if it is one of its names.
pub(crate) fn token(name: &str) -> Option<Token> {
    TOKENS.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

/// The context token names, in table order.
pub fn context_tokens() -> impl Iterator<Item = &'static str> {
    TOKENS
        .iter()
        .filter(|(_, t)| *t == Token::Context)
        .map(|(n, _)| *n)
}

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

/// The chain ID of a built-in short name (case-insensitive): what `chain
/// add NAME --url …` resolves offline.
pub fn chain_id_for_short_name(name: &str) -> Option<u64> {
    SHORT_NAMES
        .iter()
        .find(|(_, short)| short.eq_ignore_ascii_case(name))
        .map(|(id, _)| *id)
}

/// Why `alias` cannot be a chain alias, if it cannot: a number would
/// mean a chain ID.
pub fn check_alias(alias: &str) -> anyhow::Result<()> {
    if is_chain_id_like(alias) {
        anyhow::bail!("alias '{alias}' is a number; numbers mean chain IDs");
    }
    Ok(())
}

/// Whether `text` reads as a chain ID: all digits. Numbers always mean
/// chain IDs, so no chain name or alias may be one.
pub(crate) fn is_chain_id_like(text: &str) -> bool {
    text.parse::<u64>().is_ok() || (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether `name` is a valid chain name: `[a-z0-9][a-z0-9_-]*`, and not
/// [all digits](is_chain_id_like).
pub(crate) fn is_valid_chain_name(name: &str) -> bool {
    is_name(name) && !is_chain_id_like(name)
}

/// The pattern chain, fork, endpoint, explorer and signer names match.
const NAME_PATTERN: &str = "[a-z0-9][a-z0-9_-]*";

/// Why `name` cannot be a chain name, if it cannot: it must match
/// `[a-z0-9][a-z0-9_-]*` and not be all digits (numbers mean chain IDs).
pub fn check_chain_name(name: &str) -> anyhow::Result<()> {
    check_chain_like("chain", name)
}

/// [`check_chain_name`] for a `kind` of name that follows the chain-name
/// rules (a fork's).
pub(crate) fn check_chain_like(kind: &str, name: &str) -> anyhow::Result<()> {
    if !is_valid_chain_name(name) {
        anyhow::bail!("{kind} name '{name}' must match {NAME_PATTERN} and not be all digits");
    }
    Ok(())
}

/// Why `name` cannot be an endpoint or explorer name (`kind`), if it
/// cannot: it must match `[a-z0-9][a-z0-9_-]*`.
pub(crate) fn check_name(kind: &str, name: &str) -> anyhow::Result<()> {
    if !is_name(name) {
        anyhow::bail!("{kind} name '{name}' must match {NAME_PATTERN}");
    }
    Ok(())
}

/// Whether `name` is a valid label name: `[a-z0-9_-]+`, lowercase only
/// (the token grammar matches names case-insensitively, so stored names
/// are lowercase).
fn is_valid_label_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Whether `name` is taken by the token grammar itself (see [`TOKENS`]).
/// Labels and signers may not use these names, since they would be
/// shadowed by (or shadow) the token.
pub(crate) fn is_reserved_token(name: &str) -> bool {
    token(name).is_some()
}

/// Why `name` cannot be a signer name, if it cannot: signer names match
/// `[a-z0-9][a-z0-9_-]*` and are not reserved token names,
/// because every signer is also the label `@<name>`.
pub fn check_signer_name(name: &str) -> anyhow::Result<()> {
    check_name("signer", name)?;
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
    match token(name) {
        Some(Token::Context) => {
            anyhow::bail!("'{name}' is a context token (@{name}) and cannot be used as a label")
        }
        Some(Token::Deprecated(_) | Token::Removed(_)) => anyhow::bail!(
            "'{name}' is reserved (@{name} was a context token) and cannot be used as a label"
        ),
        None => {}
    }
    if signers.into_iter().any(|s| s.eq_ignore_ascii_case(name)) {
        anyhow::bail!(
            "'{name}' is a signer, and signers are labels already (@{name} is its address)"
        );
    }
    Ok(())
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
    if out.is_empty() || is_chain_id_like(&out) {
        out.insert_str(0, "chain-");
    }
    out
}

/// What an endpoint's URL names it after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UrlName {
    /// The host's registrable label, slugified (`mainnet.infura.io` → `infura`).
    Host(String),
    /// `localhost` or an IP address.
    Local,
    /// No usable host (unparseable, or a template in the host).
    Unknown,
}

impl UrlName {
    /// The name of `url`'s host.
    pub fn of(url: &str) -> Self {
        match crate::urls::host(url) {
            Some(host) if crate::urls::is_local_host(&host) => UrlName::Local,
            Some(host) => UrlName::Host(slugify(crate::urls::domain_label(&host).unwrap_or("rpc"))),
            None => UrlName::Unknown,
        }
    }

    /// The host name, if there is a real one.
    pub fn host(&self) -> Option<&str> {
        match self {
            UrlName::Host(name) => Some(name),
            UrlName::Local | UrlName::Unknown => None,
        }
    }
}

/// Endpoint name derived from a URL: its host's registrable label,
/// slugified (`mainnet.infura.io` → `infura`), `local` for
/// localhost and IPs, `rpc` without a usable host, plus `-ws` for
/// websocket URLs.
pub fn endpoint_name_for_url(url: &str) -> String {
    let base = match UrlName::of(url) {
        UrlName::Host(name) => name,
        UrlName::Local => "local".to_string(),
        UrlName::Unknown => "rpc".to_string(),
    };
    if crate::urls::is_websocket(url) {
        format!("{base}-ws")
    } else {
        base
    }
}

/// Whether endpoint `name` is the one [`endpoint_name_for_url`] gives
/// `url`, or that name made unique (`alchemy-2`): the name already says
/// where the URL points, so a compact listing can leave the host out.
pub fn named_after_url(name: &str, url: &str) -> bool {
    let base = endpoint_name_for_url(url);
    name.strip_prefix(base.as_str()).is_some_and(|rest| {
        rest.is_empty()
            || rest
                .strip_prefix('-')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// The provider an endpoint URL points at, for grouping endpoints that
/// share an API key and rate limit: the host's registrable domain
/// (`eth-mainnet.g.alchemy.com` → `alchemy.com`), or `host:port` for
/// localhost, IPs and single-label hosts (each local node is its own
/// provider). `None` without a usable host.
pub fn provider_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = crate::urls::bare(parsed.host_str()?).to_ascii_lowercase();
    match crate::urls::registrable_domain(&host) {
        Some(domain) if !crate::urls::is_local_host(&host) => Some(domain.to_string()),
        _ => Some(format!(
            "{host}:{}",
            parsed.port_or_known_default().unwrap_or(0)
        )),
    }
}

/// `base`, or `base{sep}2`, `base{sep}3`… — the first name `taken`
/// rejects. Names use `-`, variables `_`.
pub(crate) fn unique_with(base: &str, sep: char, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    let mut n = 2u64;
    loop {
        let candidate = format!("{base}{sep}{n}");
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// `base`, or `base-2`, `base-3`… — the first name `taken` rejects.
pub fn unique_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    unique_with(base, '-', taken)
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
        assert!(named_after_url("infura", "https://mainnet.infura.io/v3/x"));
        assert!(named_after_url(
            "alchemy-3",
            "https://eth-mainnet.g.alchemy.com/v2/x"
        ));
        assert!(named_after_url("local", "http://127.0.0.1:8545"));
        assert!(!named_after_url("public", "https://mainnet.base.org"));
        assert!(!named_after_url(
            "infura-x",
            "https://mainnet.infura.io/v3/x"
        ));
        assert!(!named_after_url(
            "alchemy-",
            "https://eth-mainnet.g.alchemy.com/v2/x"
        ));
        assert_eq!(
            provider_of("https://eth-mainnet.g.alchemy.com/v2/x").as_deref(),
            Some("alchemy.com")
        );
        assert_eq!(
            provider_of("http://127.0.0.1:8545").as_deref(),
            Some("127.0.0.1:8545")
        );
        assert_eq!(provider_of("not a url"), None);
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
