//! Moving literal credentials out of `config.toml` into `secrets.toml`
//! variables. One set of rules serves v1 migration, `doctor --fix` and
//! `rpc add`.
//!
//! What moves:
//!
//! - a literal Infura `/v3/<key>` or Alchemy `/v2/<key>` key in an
//!   endpoint URL ([`extract_known_key`]);
//! - a literal header value that looks like a credential (16 or more
//!   characters), or that `ETH_RPC_HEADERS` cannot carry (a comma or line
//!   break), see [`header_needs_extraction`];
//! - a literal explorer key.
//!
//! Variable names: `INFURA_KEY` and `ALCHEMY_KEY` for URL keys,
//! `<ENDPOINT>_<HEADER>` for headers, `ETHERSCAN_API_KEY` for an
//! Etherscan explorer's key and `<EXPLORER>_API_KEY` for any other; `_2`,
//! `_3`… on collision. A value some variable already holds reuses that
//! variable.

use crate::contract::ETHERSCAN_API_KEY;
use crate::model::{Config, Endpoint, Explorer};
use crate::template;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::fmt;
use zeroize::Zeroize;

/// Variable names and values: the existing ones plus those created for
/// extracted secrets. `Debug` shows names only.
#[derive(Default)]
pub struct VarTable {
    by_value: HashMap<String, String>,
    vars: IndexMap<String, String>,
    created: Vec<String>,
}

impl VarTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// A table over existing variables (`secrets.toml`'s `[vars]`).
    pub fn with_vars<'a>(vars: impl IntoIterator<Item = (&'a String, &'a String)>) -> Self {
        let mut table = Self::new();
        for (name, value) in vars {
            table.insert(name, value);
        }
        table
    }

    /// Record an existing variable. The first variable holding a value is
    /// the one [`intern`](Self::intern) reuses for it.
    pub fn insert(&mut self, name: &str, value: &str) {
        self.by_value
            .entry(value.to_string())
            .or_insert_with(|| name.to_string());
        self.vars.insert(name.to_string(), value.to_string());
    }

    /// The variable holding `secret`: an existing one, else a new one named
    /// `base` (or `base_2`, `base_3`… if taken).
    pub fn intern(&mut self, base: &str, secret: &str) -> String {
        if let Some(name) = self.by_value.get(secret) {
            return name.clone();
        }
        let name = unique_var_name(base, |n| self.vars.contains_key(n));
        self.insert(&name, secret);
        self.created.push(name.clone());
        name
    }

    /// Whether a variable named `name` exists.
    pub fn contains(&self, name: &str) -> bool {
        self.vars.contains_key(name)
    }

    /// Every variable, existing and created, in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    /// Names of the variables [`intern`](Self::intern) created.
    pub fn created(&self) -> &[String] {
        &self.created
    }

    /// The variables [`intern`](Self::intern) created, with their values.
    pub fn created_vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.created
            .iter()
            .map(|n| (n.as_str(), self.vars[n].as_str()))
    }
}

impl Drop for VarTable {
    fn drop(&mut self) {
        for value in self.vars.values_mut() {
            value.zeroize();
        }
        for (mut value, _) in self.by_value.drain() {
            value.zeroize();
        }
    }
}

impl fmt::Debug for VarTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VarTable")
            .field("vars", &self.vars.keys().collect::<Vec<_>>())
            .field("created", &self.created)
            .finish()
    }
}

/// `base`, or `base_2`, `base_3`… — the first name `taken` rejects.
pub(crate) fn unique_var_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    crate::names::unique_with(base, '_', taken)
}

/// A variable name built from `parts`: uppercased, joined with `_`, then
/// [cleaned](template::clean_name).
pub(crate) fn var_base(parts: &[&str]) -> String {
    template::clean_name(&parts.join("_").to_ascii_uppercase())
}

/// The variable name for explorer `name`'s key: [`ETHERSCAN_API_KEY`] for
/// an Etherscan explorer, else `<NAME>_API_KEY`.
pub(crate) fn explorer_key_var(name: &str, explorer: &Explorer) -> String {
    if explorer.is_etherscan() {
        ETHERSCAN_API_KEY.to_string()
    } else {
        var_base(&[name, "API_KEY"])
    }
}

/// Whether `value` holds no variable reference (a malformed `${` counts as
/// literal text).
pub fn is_literal(value: &str) -> bool {
    template::references(value).map_or(true, |names| names.is_empty())
}

/// Find a literal Infura or Alchemy key in `url`: returns the text before
/// and after the key, the base variable name, and the key.
pub fn extract_known_key(url: &str) -> Option<(&str, &str, &'static str, &str)> {
    let (marker, base) = if crate::urls::host_has_label(url, "infura") {
        ("/v3/", "INFURA_KEY")
    } else if crate::urls::host_has_label(url, "alchemy") {
        ("/v2/", "ALCHEMY_KEY")
    } else {
        return None;
    };
    let start = url.find(marker)? + marker.len();
    let rest = &url[start..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let key = &rest[..end];
    if key.len() < 8
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some((&url[..start], &rest[end..], base, key))
}

/// Whether a header value should move into a variable: it is literal and
/// either long enough to be a credential (16+ characters) or holds a comma
/// or line break, which `config.toml` may not hold literally.
pub fn header_needs_extraction(value: &str) -> bool {
    !value.is_empty()
        && is_literal(value)
        && (value.len() >= 16 || crate::model::header_value_problem(value).is_some())
}

/// Where an extracted secret was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A key inside an endpoint URL.
    Url {
        /// Chain name.
        chain: String,
        /// Endpoint name.
        endpoint: String,
    },
    /// An endpoint header value.
    Header {
        /// Chain name.
        chain: String,
        /// Endpoint name.
        endpoint: String,
        /// Header name.
        header: String,
    },
    /// An explorer's API key.
    ExplorerKey {
        /// Explorer name.
        explorer: String,
    },
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Location::Url { chain, endpoint } => write!(f, "chains.{chain}.rpcs.{endpoint}"),
            Location::Header {
                chain,
                endpoint,
                header,
            } => write!(f, "chains.{chain}.rpcs.{endpoint} header '{header}'"),
            Location::ExplorerKey { explorer } => write!(f, "explorers.{explorer}.key"),
        }
    }
}

/// One secret moved into a variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    /// Where it was.
    pub location: Location,
    /// The variable that now holds it.
    pub var: String,
}

impl fmt::Display for Moved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} → ${{{}}}", self.location, self.var)
    }
}

/// Move the literal secrets of one endpoint (a known URL key, credential
/// header values) into variables, rewriting `endpoint` in place to
/// reference them. Returns what moved.
pub fn extract_endpoint(
    chain: &str,
    endpoint_name: &str,
    endpoint: &mut Endpoint,
    vars: &mut VarTable,
) -> Vec<Moved> {
    let mut moved = Vec::new();
    if let Some((before, after, base, key)) = extract_known_key(&endpoint.url) {
        let var = vars.intern(base, key);
        endpoint.url = format!("{before}${{{var}}}{after}");
        moved.push(Moved {
            location: Location::Url {
                chain: chain.to_string(),
                endpoint: endpoint_name.to_string(),
            },
            var,
        });
    }
    for (header, value) in endpoint.headers.iter_mut() {
        if !header_needs_extraction(value) {
            continue;
        }
        let var = vars.intern(&var_base(&[endpoint_name, header]), value);
        value.zeroize();
        *value = format!("${{{var}}}");
        moved.push(Moved {
            location: Location::Header {
                chain: chain.to_string(),
                endpoint: endpoint_name.to_string(),
                header: header.clone(),
            },
            var,
        });
    }
    moved
}

/// Move `explorer`'s literal key into a variable, rewriting the key in
/// place to reference it. Returns the variable's name, or `None` if the key
/// is missing, empty or already a template.
pub(crate) fn extract_explorer_key(
    name: &str,
    explorer: &mut Explorer,
    vars: &mut VarTable,
) -> Option<String> {
    let key = explorer.key.as_mut()?;
    if key.trim().is_empty() || !is_literal(key) {
        return None;
    }
    let var = vars.intern(&explorer_key_var(name, explorer), explorer.key.as_deref()?);
    if let Some(key) = explorer.key.as_mut() {
        key.zeroize();
        *key = format!("${{{var}}}");
    }
    Some(var)
}

/// Every literal secret in a config and where it goes, computed without
/// touching anything; [`Tx::extract_all_secrets`](crate::tx::Tx::extract_all_secrets)
/// makes the edits.
pub struct SecretPlan {
    /// Endpoints to rewrite: chain, endpoint name, the new endpoint.
    pub endpoints: Vec<(String, String, Endpoint)>,
    /// Explorer keys to rewrite: explorer name, the new `${VAR}` key.
    pub explorer_keys: Vec<(String, String)>,
    /// What moved where, in config order.
    pub moved: Vec<Moved>,
    vars: VarTable,
}

impl fmt::Debug for SecretPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretPlan")
            .field("moved", &self.moved)
            .field("new_vars", &self.vars.created())
            .finish_non_exhaustive()
    }
}

impl SecretPlan {
    /// Whether there is nothing to move.
    pub fn is_empty(&self) -> bool {
        self.moved.is_empty()
    }

    /// The new variables, with their values.
    pub fn new_vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars.created_vars()
    }

    /// The variable table, with the new variables.
    pub(crate) fn vars(&self) -> &VarTable {
        &self.vars
    }
}

/// Plan moving every literal secret in `config` into variables, reusing
/// `existing` variables (`secrets.toml`'s `[vars]`) that hold the same
/// value. Used by `doctor --fix`; `rpc add` can call [`extract_endpoint`]
/// directly for the one endpoint it writes.
pub fn plan_secret_extraction(config: &Config, existing: &IndexMap<String, String>) -> SecretPlan {
    let mut vars = VarTable::with_vars(existing);
    let mut endpoints = Vec::new();
    let mut explorer_keys = Vec::new();
    let mut moved = Vec::new();
    for (chain_name, chain) in &config.chains {
        for (endpoint_name, endpoint) in &chain.rpcs {
            let mut new = endpoint.clone();
            let found = extract_endpoint(chain_name, endpoint_name, &mut new, &mut vars);
            if !found.is_empty() {
                moved.extend(found);
                endpoints.push((chain_name.clone(), endpoint_name.clone(), new));
            }
        }
    }
    for (name, explorer) in &config.explorers {
        let mut new = explorer.clone();
        if let Some(var) = extract_explorer_key(name, &mut new, &mut vars)
            && let Some(key) = new.key.take()
        {
            moved.push(Moved {
                location: Location::ExplorerKey {
                    explorer: name.clone(),
                },
                var,
            });
            explorer_keys.push((name.clone(), key));
        }
    }
    SecretPlan {
        endpoints,
        explorer_keys,
        moved,
        vars,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::Secrets;

    const CONFIG: &str = r#"
version = 2

[explorers.etherscan]
api = "https://api.etherscan.io/v2/api?chainid=${CHAIN_ID}"
key = "literal-etherscan-key"

[explorers.blockscout]
api = "https://blockscout.example/api"
key = "${ALREADY}"

[chains.eth]
id = 1
explorer = "etherscan"
rpc = "infura"
[chains.eth.rpcs]
infura = "https://mainnet.infura.io/v3/abcdef0123456789"
alchemy = "https://eth-mainnet.g.alchemy.com/v2/alchemykey0001"
gw = { url = "https://gw.example/rpc", headers = { x-api-key = "header-secret-0123456", accept = "json", x-list = "a,b" } }

[chains.base]
id = 8453
rpc = "infura"
[chains.base.rpcs]
infura = "https://base-mainnet.infura.io/v3/abcdef0123456789"
plain = "https://mainnet.base.org"
"#;

    #[test]
    fn plans_every_literal_secret_and_applies_cleanly() {
        let config: Config = crate::model::Config::parse(CONFIG).unwrap();
        let mut existing = IndexMap::new();
        existing.insert("ALREADY".to_string(), "x".to_string());
        existing.insert("MY_ALCHEMY".to_string(), "alchemykey0001".to_string());
        let plan = plan_secret_extraction(&config, &existing);
        let moved: Vec<String> = plan.moved.iter().map(ToString::to_string).collect();
        assert_eq!(
            moved,
            [
                "chains.eth.rpcs.infura → ${INFURA_KEY}",
                "chains.eth.rpcs.alchemy → ${MY_ALCHEMY}",
                "chains.eth.rpcs.gw header 'x-api-key' → ${GW_X_API_KEY}",
                "chains.eth.rpcs.gw header 'x-list' → ${GW_X_LIST}",
                "chains.base.rpcs.infura → ${INFURA_KEY}",
                "explorers.etherscan.key → ${ETHERSCAN_API_KEY}",
            ]
        );
        assert_eq!(
            plan.new_vars().map(|(n, _)| n).collect::<Vec<_>>(),
            [
                "INFURA_KEY",
                "GW_X_API_KEY",
                "GW_X_LIST",
                "ETHERSCAN_API_KEY"
            ]
        );
        assert!(!format!("{plan:?}").contains("literal-etherscan-key"));

        let mut tx = crate::tx::Tx::parse(
            CONFIG,
            std::path::Path::new("config.toml"),
            "[vars]\nALREADY = \"x\"\nMY_ALCHEMY = \"alchemykey0001\"\n",
            std::path::Path::new("secrets.toml"),
        )
        .unwrap();
        assert_eq!(tx.extract_all_secrets().unwrap(), plan.moved);
        let (text, secrets) = tx.texts();
        for secret in [
            "abcdef0123456789",
            "alchemykey0001",
            "header-secret-0123456",
            "literal-etherscan-key",
            "a,b",
        ] {
            assert!(!text.contains(secret), "{text}");
        }
        assert!(text.contains("accept = \"json\""), "{text}");
        let config: Config = crate::model::Config::parse(&text).unwrap();
        let secrets = Secrets::parse(&secrets).unwrap();
        crate::validate::validate(&config, Some(&secrets)).unwrap();
        assert_eq!(secrets.vars["INFURA_KEY"], "abcdef0123456789");
        assert_eq!(secrets.vars["GW_X_LIST"], "a,b");

        // Nothing left to do.
        assert!(plan_secret_extraction(&config, &secrets.vars).is_empty());
    }

    #[test]
    fn names_and_literals() {
        assert_eq!(var_base(&["uni-gw", "x-api-key"]), "UNI_GW_X_API_KEY");
        assert_eq!(var_base(&["1rpc", "auth"]), "_1RPC_AUTH");
        let etherscan = Explorer::etherscan();
        let mut other = Explorer::etherscan();
        other.api = "https://blockscout.example/api".into();
        assert_eq!(
            explorer_key_var("etherscan", &etherscan),
            "ETHERSCAN_API_KEY"
        );
        assert_eq!(explorer_key_var("scan", &etherscan), "ETHERSCAN_API_KEY");
        assert_eq!(explorer_key_var("blockscout", &other), "BLOCKSCOUT_API_KEY");
        assert_eq!(unique_var_name("K", |n| n == "K" || n == "K_2"), "K_3");
        assert!(is_literal("abc") && is_literal("a${b") && !is_literal("${A}"));
        assert!(header_needs_extraction("Bearer 0123456789abcdef"));
        assert!(header_needs_extraction("a,b"));
        assert!(!header_needs_extraction("json"));
        assert!(!header_needs_extraction("Bearer ${TOKEN_THAT_IS_LONG}"));
    }
}
