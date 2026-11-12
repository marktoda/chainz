//! The typed view of `config.toml` (version 2).
//!
//! These types are deserialized from the TOML document for reading and
//! validation, and serialized back by [`crate::tx::Tx`], which merges each
//! record into the `toml_edit` document so comments and ordering survive.
//! Every `Debug` impl redacts URLs and secret-bearing values.

use crate::address::Address;
use crate::redact;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize, Serializer};
use std::fmt;

/// The only config version this build reads and writes.
pub(crate) const CONFIG_VERSION: u32 = 2;

/// The whole config file.
#[derive(Clone, Default, Deserialize)]
pub struct Config {
    /// Must be `CONFIG_VERSION` (2).
    #[serde(default)]
    pub version: u32,
    /// Default chain name, set by `chainz use`.
    #[serde(default)]
    pub default: Option<String>,
    /// Named block explorers, attached to chains by name.
    #[serde(default)]
    pub explorers: IndexMap<String, Explorer>,
    /// Chains keyed by short name.
    #[serde(default)]
    pub chains: IndexMap<String, Chain>,
    /// Signers keyed by name.
    #[serde(default)]
    pub signers: IndexMap<String, Signer>,
    /// User labels: label name → chain name (or `"*"`) → address.
    #[serde(default)]
    pub addresses: IndexMap<String, IndexMap<String, String>>,
}

/// One chain, keyed by its short name in [`Config::chains`].
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Chain {
    /// EVM chain ID.
    pub id: u64,
    /// Extra names this chain answers to (case-insensitive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Signer used for `@wallet`, `ETH_FROM` and `--expose-key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer: Option<String>,
    /// Explorer name from [`Config::explorers`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explorer: Option<String>,
    /// The active endpoint, by name.
    pub rpc: String,
    /// Endpoints the gateway may retry reads on, in order. Nothing else.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failover: Vec<String>,
    /// Named endpoints.
    #[serde(default)]
    pub rpcs: IndexMap<String, Endpoint>,
}

/// Whether `name` is a legal HTTP header name (RFC 9110 token characters).
pub(crate) fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Why a header value cannot be exported via `ETH_RPC_HEADERS` (one line of
/// comma-separated `name: value` pairs). The gateway sends a comma fine;
/// a line break is never a valid header value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeaderValueProblem {
    /// `\r` or `\n`.
    LineBreak,
    /// `,`.
    Comma,
}

impl fmt::Display for HeaderValueProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            HeaderValueProblem::LineBreak => "a line break",
            HeaderValueProblem::Comma => "a comma",
        })
    }
}

/// Why `value` cannot be exported via `ETH_RPC_HEADERS`, if it cannot.
pub(crate) fn header_value_problem(value: &str) -> Option<HeaderValueProblem> {
    if value.contains(['\r', '\n']) {
        Some(HeaderValueProblem::LineBreak)
    } else if value.contains(',') {
        Some(HeaderValueProblem::Comma)
    } else {
        None
    }
}

/// A named RPC endpoint: a URL plus optional headers scoped to that URL.
/// Written as a plain string when it has no headers, else an inline table.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "EndpointWire")]
pub struct Endpoint {
    /// Raw URL; may contain `${NAME}` templates.
    pub url: String,
    /// Raw header values; may contain `${NAME}` templates.
    pub headers: IndexMap<String, String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum EndpointWire {
    Url(String),
    Detailed {
        url: String,
        #[serde(default)]
        headers: IndexMap<String, String>,
    },
}

impl Serialize for Endpoint {
    /// The plain URL string when there are no headers, else `{ url, headers }`.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Detailed<'a> {
            url: &'a str,
            headers: &'a IndexMap<String, String>,
        }
        if self.headers.is_empty() {
            serializer.serialize_str(&self.url)
        } else {
            Detailed {
                url: &self.url,
                headers: &self.headers,
            }
            .serialize(serializer)
        }
    }
}

impl From<EndpointWire> for Endpoint {
    fn from(wire: EndpointWire) -> Self {
        match wire {
            EndpointWire::Url(url) => Endpoint::new(url),
            EndpointWire::Detailed { url, headers } => Endpoint { url, headers },
        }
    }
}

impl Endpoint {
    /// A header-free endpoint.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: IndexMap::new(),
        }
    }

    /// `ws://` or `wss://`: usable with `exec --direct`, never routed by the
    /// gateway.
    pub fn is_websocket(&self) -> bool {
        crate::urls::is_websocket(&self.url)
    }

    /// `http://` or `https://`: the only endpoints chainz can probe.
    pub fn is_http(&self) -> bool {
        crate::urls::is_http(&self.url)
    }

    /// Why `url` cannot be an endpoint URL, if it cannot: it must be
    /// non-empty, free of whitespace, well-formed `${NAME}` templates, and
    /// (with templates blanked) an `http(s)://` or `ws(s)://` URL with a
    /// host. Never echoes the URL.
    pub fn check_url(url: &str) -> anyhow::Result<()> {
        Self::check_url_text(url)?;
        let parsed = url::Url::parse(&crate::template::blanked(url))
            .map_err(|e| anyhow::anyhow!("the endpoint URL is not a valid URL ({e})"))?;
        if !matches!(parsed.scheme(), "http" | "https" | "ws" | "wss") || parsed.host().is_none() {
            anyhow::bail!("the endpoint URL must be an http(s):// or ws(s):// URL with a host");
        }
        Ok(())
    }
}

impl Endpoint {
    /// Whether two endpoint URLs are the same endpoint: equal up to a
    /// trailing `/`, once every `${NAME}` that `vars` (`secrets.toml`'s
    /// `[vars]`) holds is expanded on both sides. So a pasted URL whose key
    /// an earlier write moved into `${ALCHEMY_KEY}` is the endpoint holding
    /// that template, and re-adding it adds nothing. A reference `vars`
    /// doesn't hold compares as written. The one URL-identity rule:
    /// [`Chain::endpoint_at`] and every write go by it.
    pub fn same_url(a: &str, b: &str, vars: &crate::secrets::Vars) -> bool {
        let expanded = |url: &str| -> String {
            crate::template::expand(url, |name| {
                Ok::<_, crate::template::TemplateError>(Some(match vars.get(name) {
                    Some(value) => std::borrow::Cow::Borrowed(value.as_str()),
                    None => std::borrow::Cow::Owned(format!("${{{name}}}")),
                }))
            })
            .map_or_else(|_| url.to_string(), std::borrow::Cow::into_owned)
        };
        let (a, b) = (expanded(a), expanded(b));
        a.trim_end_matches('/') == b.trim_end_matches('/')
    }

    /// The part of [`Endpoint::check_url`] every stored URL must pass
    /// (validation applies it to the config): see [`check_stored_url`].
    pub(crate) fn check_url_text(url: &str) -> anyhow::Result<()> {
        check_stored_url(url, "the endpoint URL")
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("url", &redact::url(&self.url, false))
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// A block explorer API, shared by every chain that names it.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Explorer {
    /// API URL; `${CHAIN_ID}` lets one definition serve many chains.
    pub api: String,
    /// API key, normally `${VAR}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Optional browser URL (for links).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl Explorer {
    /// Etherscan-family explorers are left to forge's built-in Etherscan v2
    /// handling: exec exports only `ETHERSCAN_API_KEY` for them.
    pub(crate) fn is_etherscan(&self) -> bool {
        crate::urls::host_has_label(&crate::template::blanked(&self.api), "etherscan")
    }

    /// The built-in explorer's name: `--explorer etherscan` creates
    /// [`Explorer::etherscan`] when it is not configured.
    pub const ETHERSCAN: &'static str = "etherscan";

    /// The built-in `[explorers.etherscan]`: Etherscan's v2 multichain API
    /// (the same URL v1 migration writes) with the key in
    /// `${ETHERSCAN_API_KEY}`.
    pub fn etherscan() -> Self {
        Self {
            api: "https://api.etherscan.io/v2/api?chainid=${CHAIN_ID}".to_string(),
            key: Some(format!("${{{}}}", crate::contract::ETHERSCAN_API_KEY)),
            url: None,
        }
    }

    /// Whether Etherscan's v2 API covers `chain_id` ([`ETHERSCAN_CHAIN_IDS`]).
    /// The one rule for attaching the built-in explorer by default: `chain
    /// add` and `init` attach it only where it is covered, and `chain set
    /// --explorer etherscan` elsewhere warns ([`Explorer::etherscan_gap`]).
    pub fn etherscan_covers(chain_id: u64) -> bool {
        ETHERSCAN_CHAIN_IDS.binary_search(&chain_id).is_ok()
    }

    /// Why the built-in explorer does not fit `chain_id`, if it does not:
    /// `etherscan doesn't cover chain ID 31337`.
    pub fn etherscan_gap(chain_id: u64) -> Option<String> {
        (!Self::etherscan_covers(chain_id))
            .then(|| format!("{} doesn't cover chain ID {chain_id}", Self::ETHERSCAN))
    }
}

/// Chain IDs Etherscan's v2 multichain API serves, sorted. Source: the
/// v2 supported-chains list (<https://docs.etherscan.io/supported-chains>,
/// also served as <https://api.etherscan.io/v2/chainlist>). Local and
/// custom chains (anvil's 31337) and chains whose explorer is another
/// family (Blockscout: Ink, Mode, Soneium, Zora) are not on it. Update it
/// the way `SHORT_NAMES` and `tokens.toml` are updated: by hand, from that
/// list.
pub const ETHERSCAN_CHAIN_IDS: &[u64] = &[
    1,         // Ethereum
    10,        // OP Mainnet
    25,        // Cronos
    50,        // XDC
    51,        // XDC Apothem
    56,        // BNB Smart Chain
    97,        // BNB Smart Chain testnet
    100,       // Gnosis
    130,       // Unichain
    137,       // Polygon
    146,       // Sonic
    199,       // BitTorrent Chain
    204,       // opBNB
    252,       // Fraxtal
    300,       // zkSync Sepolia
    324,       // zkSync
    480,       // World Chain
    1101,      // Polygon zkEVM
    1284,      // Moonbeam
    1285,      // Moonriver
    1287,      // Moonbase Alpha
    1301,      // Unichain Sepolia
    1328,      // Sei testnet
    1329,      // Sei
    1923,      // Swellchain
    1924,      // Swellchain testnet
    2442,      // Polygon zkEVM Cardona
    2522,      // Fraxtal testnet
    2741,      // Abstract
    4801,      // World Chain Sepolia
    5000,      // Mantle
    5003,      // Mantle Sepolia
    5611,      // opBNB testnet
    8453,      // Base
    10143,     // Monad testnet
    11124,     // Abstract Sepolia
    17000,     // Holesky
    33111,     // ApeChain Curtis
    33139,     // ApeChain
    42161,     // Arbitrum One
    42170,     // Arbitrum Nova
    42220,     // Celo
    43113,     // Avalanche Fuji
    43114,     // Avalanche C-Chain
    44787,     // Celo Alfajores
    57054,     // Sonic Blaze
    59141,     // Linea Sepolia
    59144,     // Linea
    80002,     // Polygon Amoy
    80069,     // Berachain Bepolia
    80094,     // Berachain
    81457,     // Blast
    84532,     // Base Sepolia
    167000,    // Taiko
    167009,    // Taiko Hekla
    421614,    // Arbitrum Sepolia
    534351,    // Scroll Sepolia
    534352,    // Scroll
    560048,    // Hoodi
    11155111,  // Sepolia
    11155420,  // OP Sepolia
    168587773, // Blast Sepolia
];

impl Explorer {
    /// The rule every stored explorer `api` URL must pass (validation
    /// applies it to the config, v1 migration to what it imports): see
    /// [`check_stored_url`].
    pub(crate) fn check_api_text(api: &str) -> anyhow::Result<()> {
        check_stored_url(api, "the api URL")
    }
}

/// The one rule for every URL stored in the config (endpoint and explorer):
/// non-empty, no whitespace, well-formed `${NAME}` templates. `what` names
/// the field in the error.
fn check_stored_url(url: &str, what: &str) -> anyhow::Result<()> {
    if url.trim().is_empty() || url.chars().any(char::is_whitespace) {
        anyhow::bail!("{what} is empty or malformed");
    }
    crate::template::references(url)?;
    Ok(())
}

impl fmt::Debug for Explorer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Explorer")
            .field("api", &redact::url(&self.api, false))
            .field("key", &self.key.as_ref().map(|_| redact::REDACTED))
            .field("url", &self.url)
            .finish()
    }
}

/// Where a signer's key material lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum Backend {
    /// OS keyring (Secret Service, macOS Keychain).
    #[serde(rename = "keyring")]
    Keyring,
    /// AES-256-GCM + Argon2id envelope in `secrets.toml`.
    #[serde(rename = "encrypted")]
    Encrypted,
    /// A Foundry keystore file, unlocked by Foundry via `ETH_KEYSTORE`.
    #[serde(rename = "keystore")]
    Keystore,
    /// A 1Password item read through `op read`.
    #[serde(rename = "1password")]
    OnePassword,
    /// Raw key in `secrets.toml`; explicit opt-in only.
    #[serde(rename = "plaintext")]
    Plaintext,
}

impl Backend {
    /// Whether the signer only references key material chainz does not
    /// hold (a keystore file, a 1Password item): chainz never writes,
    /// moves or deletes it.
    pub fn is_reference(self) -> bool {
        matches!(self, Backend::Keystore | Backend::OnePassword)
    }

    /// The config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Keyring => "keyring",
            Backend::Encrypted => "encrypted",
            Backend::Keystore => "keystore",
            Backend::OnePassword => "1password",
            Backend::Plaintext => "plaintext",
        }
    }
}

impl Backend {
    /// The optional [`Signer`] fields this backend takes (`backend` and
    /// `address` apply to all), and whether each is required. Every other
    /// optional field must be absent.
    pub(crate) fn fields(self) -> &'static [(&'static str, Need)] {
        match self {
            Backend::Keyring => &[("service", Need::Optional), ("username", Need::Optional)],
            Backend::Keystore => &[("path", Need::Required)],
            Backend::OnePassword => &[("vault", Need::Required), ("item", Need::Required)],
            Backend::Encrypted | Backend::Plaintext => &[],
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Keyring service used for new keyring signers (and by v1).
pub(crate) const KEYRING_SERVICE: &str = "chainz";

/// A key reference plus its cached public address. Holds no key material.
///
/// A flat record rather than an enum tagged by backend: which optional
/// fields a backend takes is the one table `Backend::fields`, checked by
/// validation, so a mistake gets a message naming the signer and field
/// (and `doctor` can still load the file to list every problem).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Signer {
    /// Storage backend.
    pub backend: Backend,
    /// Cached address: `@wallet`, `ETH_FROM` and balances never unlock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<Address>,
    /// Keyring service (default `chainz`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Keyring username (default: the signer name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Keystore file path (`~/` allowed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 1Password vault.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault: Option<String>,
    /// 1Password item reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
}

/// Whether a backend needs an optional [`Signer`] field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Need {
    /// The backend works without it.
    Optional,
    /// The backend needs a non-empty value.
    Required,
}

impl Signer {
    /// A signer with no optional fields: an encrypted or plaintext key in
    /// secrets.toml, or a keyring entry under the default service and the
    /// signer's own name.
    pub fn new(backend: Backend, address: Option<Address>) -> Self {
        Self {
            backend,
            address,
            service: None,
            username: None,
            path: None,
            vault: None,
            item: None,
        }
    }

    /// A keyring signer named `name`; `service` and `username` are kept
    /// only where they differ from the defaults.
    pub(crate) fn keyring(
        name: &str,
        service: Option<&str>,
        username: Option<&str>,
        address: Option<Address>,
    ) -> Self {
        Self {
            service: service
                .filter(|s| *s != KEYRING_SERVICE)
                .map(str::to_string),
            username: username.filter(|u| *u != name).map(str::to_string),
            ..Self::new(Backend::Keyring, address)
        }
    }

    /// A Foundry keystore file reference.
    pub fn keystore(path: String, address: Option<Address>) -> Self {
        Self {
            path: Some(path),
            ..Self::new(Backend::Keystore, address)
        }
    }

    /// A 1Password item reference.
    pub fn one_password(vault: String, item: String, address: Option<Address>) -> Self {
        Self {
            vault: Some(vault),
            item: Some(item),
            ..Self::new(Backend::OnePassword, address)
        }
    }

    /// Where the key material that chainz deletes along with this signer
    /// lives, if anywhere (keystore files and 1Password items are only
    /// referenced, and stay).
    pub fn key_material(&self) -> Option<&'static str> {
        match self.backend {
            Backend::Keyring => Some("its OS keyring entry"),
            Backend::Encrypted => Some("its encrypted key in secrets.toml"),
            Backend::Plaintext => Some("its key in secrets.toml"),
            Backend::Keystore | Backend::OnePassword => None,
        }
    }

    /// Every optional field by its config name, set or not.
    pub(crate) fn optional_fields(&self) -> [(&'static str, Option<&str>); 5] {
        [
            ("service", self.service.as_deref()),
            ("username", self.username.as_deref()),
            ("path", self.path.as_deref()),
            ("vault", self.vault.as_deref()),
            ("item", self.item.as_deref()),
        ]
    }

    /// Keyring service and username for a keyring signer named `name`.
    pub(crate) fn keyring_entry<'a>(&'a self, name: &'a str) -> (&'a str, &'a str) {
        (
            self.service.as_deref().unwrap_or(KEYRING_SERVICE),
            self.username.as_deref().unwrap_or(name),
        )
    }
}

/// A config record that holds `${VAR}` templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record<'a> {
    /// `chains.<chain>.rpcs.<endpoint>`
    Endpoint { chain: &'a str, endpoint: &'a str },
    /// `explorers.<explorer>`
    Explorer(&'a str),
}

impl fmt::Display for Record<'_> {
    /// The record's path in `config.toml`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Record::Endpoint { chain, endpoint } => write!(f, "chains.{chain}.rpcs.{endpoint}"),
            Record::Explorer(name) => write!(f, "explorers.{name}"),
        }
    }
}

impl Config {
    /// Parse `config.toml` text. Errors quote the source line; show them
    /// through the store, which redacts them.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// An empty v2 config.
    pub(crate) fn empty() -> Self {
        Self {
            version: CONFIG_VERSION,
            ..Self::default()
        }
    }

    /// Every value that may hold `${NAME}` templates, with the record it
    /// belongs to, in file order: endpoint URLs and header values, then
    /// explorer API URLs and keys.
    pub fn templated_fields(&self) -> impl Iterator<Item = (Record<'_>, &str)> {
        let endpoints = self.chains.iter().flat_map(|(chain, c)| {
            c.rpcs.iter().flat_map(move |(ep, e)| {
                std::iter::once(e.url.as_str())
                    .chain(e.headers.values().map(String::as_str))
                    .map(move |value| {
                        (
                            Record::Endpoint {
                                chain,
                                endpoint: ep,
                            },
                            value,
                        )
                    })
            })
        });
        let explorers = self.explorers.iter().flat_map(|(name, e)| {
            std::iter::once(e.api.as_str())
                .chain(e.key.as_deref())
                .map(move |value| (Record::Explorer(name), value))
        });
        endpoints.chain(explorers)
    }

    /// The records that reference `${name}`, each once, in file order.
    pub fn var_uses(&self, name: &str) -> Vec<Record<'_>> {
        let mut out: Vec<Record<'_>> = Vec::new();
        for (record, value) in self.templated_fields() {
            if out.last() != Some(&record)
                && crate::template::references(value).is_ok_and(|r| r.contains(&name))
            {
                out.push(record);
            }
        }
        out
    }

    /// The chain with ID `id`.
    pub fn chain_by_id(&self, id: u64) -> Option<(&str, &Chain)> {
        self.chains
            .iter()
            .find(|(_, chain)| chain.id == id)
            .map(|(name, chain)| (name.as_str(), chain))
    }

    /// What to call chain `id` in output: its configured name, else its
    /// well-known short name, else `chain <id>`.
    pub fn chain_display(&self, id: u64) -> String {
        match self.chain_by_id(id) {
            Some((name, _)) => name.to_string(),
            None => {
                crate::names::short_name(id).map_or_else(|| format!("chain {id}"), str::to_string)
            }
        }
    }

    /// Why chain `name` with ID `id` (and signer `signer`) cannot be
    /// added, if it cannot: an invalid or taken name, a configured ID, or
    /// a missing signer.
    pub fn check_new_chain(&self, name: &str, id: u64, signer: Option<&str>) -> anyhow::Result<()> {
        crate::names::check_chain_name(name)?;
        if self.chains.contains_key(name) {
            anyhow::bail!("chain '{name}' already exists");
        }
        if let Some((existing, _)) = self.chain_by_id(id) {
            anyhow::bail!("chain ID {id} is already configured as '{existing}'");
        }
        match signer {
            Some(signer) => self.check_signer(signer),
            None => Ok(()),
        }
    }

    /// Why a chain cannot use signer `signer`, if it cannot: it is not
    /// configured.
    pub fn check_signer(&self, signer: &str) -> anyhow::Result<()> {
        if !self.signers.contains_key(signer) {
            anyhow::bail!("signer '{signer}' not found; add it with `chainz signer add {signer}`");
        }
        Ok(())
    }

    /// Chain names that use signer `name`.
    pub fn chains_using_signer(&self, name: &str) -> Vec<&str> {
        self.chains
            .iter()
            .filter(|(_, c)| c.signer.as_deref() == Some(name))
            .map(|(n, _)| n.as_str())
            .collect()
    }
}

impl Chain {
    /// What changed from `old` to `new` (one chain, before and after a
    /// write), one line per change, in a fixed order: chain ID, aliases,
    /// endpoints, active endpoint, failover, signer, explorer. Empty when
    /// nothing did. The one way writes describe a chain change (`chain
    /// set`, `edit CHAIN`). Never shows a URL or header value.
    pub fn changes(old: &Chain, new: &Chain) -> Vec<String> {
        let mut out = Vec::new();
        if old.id != new.id {
            out.push(format!(
                "chain ID: {} → {} (its endpoints were not re-checked)",
                old.id, new.id
            ));
        }
        let has = |list: &[String], a: &str| list.iter().any(|x| x.eq_ignore_ascii_case(a));
        for alias in old.aliases.iter().filter(|a| !has(&new.aliases, a)) {
            out.push(format!("alias removed: {alias}"));
        }
        for alias in new.aliases.iter().filter(|a| !has(&old.aliases, a)) {
            out.push(format!("alias added: {alias}"));
        }
        for name in old.rpcs.keys().filter(|n| !new.rpcs.contains_key(*n)) {
            out.push(format!("removed endpoint '{name}'"));
        }
        for (name, endpoint) in &new.rpcs {
            let Some(before) = old.rpcs.get(name) else {
                out.push(format!("added endpoint '{name}'"));
                continue;
            };
            if before.url != endpoint.url {
                out.push(format!("endpoint '{name}': URL changed"));
            }
            for header in before.headers.keys() {
                if !endpoint.headers.contains_key(header) {
                    out.push(format!("header {header} removed from {name}"));
                }
            }
            for (header, value) in &endpoint.headers {
                if before.headers.get(header) != Some(value) {
                    out.push(format!("header {header} set on {name}"));
                }
            }
        }
        if old.rpc != new.rpc {
            out.push(format!("active endpoint: {} → {}", old.rpc, new.rpc));
        }
        let list = |l: &[String]| match l.is_empty() {
            true => "none".to_string(),
            false => l.join(", "),
        };
        if old.failover != new.failover {
            out.push(format!(
                "failover: {} → {}",
                list(&old.failover),
                list(&new.failover)
            ));
        }
        let or_none = |v: &Option<String>| v.clone().unwrap_or_else(|| "none".to_string());
        if old.signer != new.signer {
            out.push(format!(
                "signer: {} → {}",
                or_none(&old.signer),
                or_none(&new.signer)
            ));
        }
        if old.explorer != new.explorer {
            out.push(format!(
                "explorer: {} → {}",
                or_none(&old.explorer),
                or_none(&new.explorer)
            ));
        }
        out
    }

    /// The active endpoint record, if the config is consistent.
    pub fn active(&self) -> Option<&Endpoint> {
        self.rpcs.get(&self.rpc)
    }

    /// Whether `query` equals one of this chain's aliases, ignoring ASCII
    /// case. The chain's own name (its key) is not checked.
    pub(crate) fn matches_alias(&self, query: &str) -> bool {
        self.aliases.iter().any(|a| a.eq_ignore_ascii_case(query))
    }

    /// Why endpoint `endpoint` cannot be in the failover list of this chain
    /// (named `name`), if it cannot: it is not one of the chain's
    /// endpoints, it is a websocket (the gateway routes HTTP only), or it
    /// is the active endpoint.
    pub fn check_failover(&self, name: &str, endpoint: &str) -> anyhow::Result<()> {
        match self.rpcs.get(endpoint) {
            None => {
                anyhow::bail!("chain '{name}' failover endpoint '{endpoint}' is not in its rpcs")
            }
            Some(e) if e.is_websocket() => anyhow::bail!(
                "chain '{name}' failover endpoint '{endpoint}' is a websocket; the gateway routes HTTP only"
            ),
            Some(_) if *endpoint == self.rpc => {
                anyhow::bail!("chain '{name}' lists its active endpoint '{endpoint}' as failover")
            }
            Some(_) => Ok(()),
        }
    }

    /// The endpoint at `url`, if this chain has one
    /// ([`Endpoint::same_url`] with `vars`, `secrets.toml`'s `[vars]`).
    pub fn endpoint_at(&self, url: &str, vars: &crate::secrets::Vars) -> Option<&str> {
        self.rpcs
            .iter()
            .find(|(_, e)| Endpoint::same_url(&e.url, url, vars))
            .map(|(name, _)| name.as_str())
    }

    /// Check a new endpoint at `url` for this chain (`chain` is its name,
    /// for messages) and return its name: `name`, or the URL's
    /// [endpoint name](crate::names::endpoint_name_for_url) made unique
    /// among the chain's endpoints. The URL must pass
    /// [`Endpoint::check_url`] and be new to the chain (by
    /// [`endpoint_at`](Self::endpoint_at) with `vars`); a given name must
    /// be free.
    pub fn new_endpoint(
        &self,
        chain: &str,
        url: &str,
        name: Option<&str>,
        vars: &crate::secrets::Vars,
    ) -> anyhow::Result<String> {
        Endpoint::check_url(url)?;
        if let Some(existing) = self.endpoint_at(url, vars) {
            anyhow::bail!(
                "chain '{chain}' already has this URL as endpoint '{existing}'; activate it with `chainz chain set {chain} --rpc {existing}`"
            );
        }
        match name {
            Some(name) if self.rpcs.contains_key(name) => {
                anyhow::bail!("chain '{chain}' already has an endpoint named '{name}'")
            }
            Some(name) => Ok(name.to_string()),
            None => Ok(crate::names::unique_name(
                &crate::names::endpoint_name_for_url(url),
                |n| self.rpcs.contains_key(n),
            )),
        }
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("version", &self.version)
            .field("default", &self.default)
            .field("explorers", &self.explorers)
            .field("chains", &self.chains)
            .field("signers", &self.signers)
            .field("addresses", &self.addresses.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl fmt::Debug for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chain")
            .field("id", &self.id)
            .field("aliases", &self.aliases)
            .field("signer", &self.signer)
            .field("explorer", &self.explorer)
            .field("rpc", &self.rpc)
            .field("failover", &self.failover)
            .field("rpcs", &self.rpcs)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etherscan_covers_its_listed_chains_only() {
        assert!(
            ETHERSCAN_CHAIN_IDS.windows(2).all(|w| w[0] < w[1]),
            "sorted, no repeats"
        );
        for id in [1, 10, 8453, 42161, 11155111] {
            assert!(Explorer::etherscan_covers(id), "{id}");
            assert_eq!(Explorer::etherscan_gap(id), None);
        }
        for id in [31337, 999_999, 7777777, 57073] {
            assert!(!Explorer::etherscan_covers(id), "{id}");
        }
        assert_eq!(
            Explorer::etherscan_gap(31337).as_deref(),
            Some("etherscan doesn't cover chain ID 31337")
        );
    }

    const SAMPLE: &str = r#"
version = 2
default = "eth"

[explorers.etherscan]
api = "https://api.etherscan.io/v2/api?chainid=${CHAIN_ID}"
key = "literal-explorer-secret"

[chains.eth]
id = 1
aliases = ["ethereum", "mainnet"]
signer = "default"
explorer = "etherscan"
rpc = "infura"
failover = ["llama"]
[chains.eth.rpcs]
infura = "https://mainnet.infura.io/v3/path-secret"
uni-gw = { url = "https://gw.example.org/rpc/1", headers = { x-secret = "header-secret" } }
llama = "https://eth.llamarpc.com"

[chains.base]
id = 8453
rpc = "infura"
rpcs.infura = "https://base-mainnet.infura.io/v3/${INFURA_KEY}"

[signers.default]
backend = "keyring"
address = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"

[addresses.my-safe]
"*" = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"
"#;

    #[test]
    fn parses_the_design_example() {
        let config: Config = Config::parse(SAMPLE).unwrap();
        assert_eq!(config.version, 2);
        let eth = &config.chains["eth"];
        assert_eq!(eth.rpcs.len(), 3);
        assert_eq!(eth.rpcs["uni-gw"].headers["x-secret"], "header-secret");
        assert_eq!(config.chains["base"].rpcs["infura"].headers.len(), 0);
        assert_eq!(config.signers["default"].backend, Backend::Keyring);
        assert!(config.explorers["etherscan"].is_etherscan());
        assert_eq!(config.chains.keys().collect::<Vec<_>>(), ["eth", "base"]);
    }

    #[test]
    fn debug_never_shows_secrets() {
        let config: Config = Config::parse(SAMPLE).unwrap();
        let debug = format!("{config:?}");
        for secret in ["literal-explorer-secret", "path-secret", "header-secret"] {
            assert!(!debug.contains(secret), "{debug}");
        }
        assert!(debug.contains("x-secret"));
    }

    #[test]
    fn unknown_chain_fields_are_rejected() {
        let err = Config::parse("version = 2\n[chains.eth]\nid = 1\nrcp = \"x\"\nrpc = \"x\"\n")
            .unwrap_err();
        assert!(err.to_string().contains("rcp"), "{err}");
    }

    #[test]
    fn only_backends_holding_key_material_delete_it() {
        for backend in [Backend::Keyring, Backend::Encrypted, Backend::Plaintext] {
            assert!(
                Signer::new(backend, None).key_material().is_some(),
                "{backend}"
            );
        }
        for backend in [Backend::Keystore, Backend::OnePassword] {
            assert!(
                Signer::new(backend, None).key_material().is_none(),
                "{backend}"
            );
        }
    }

    #[test]
    fn keyring_signers_keep_only_non_default_entries() {
        let plain = Signer::keyring("hot", Some(KEYRING_SERVICE), Some("hot"), None);
        assert!(plain.service.is_none() && plain.username.is_none());
        let custom = Signer::keyring("hot", Some("v1"), Some("me"), None);
        assert_eq!(custom.keyring_entry("hot"), ("v1", "me"));
    }
}
