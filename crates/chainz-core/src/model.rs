//! The typed view of `config.toml` (version 2).
//!
//! These types are deserialized from the TOML document for reading and
//! validation. Writes never serialize them back: they edit the
//! `toml_edit` document (see [`crate::edit`]) so comments and ordering
//! survive. Every `Debug` impl redacts URLs and secret-bearing values.

use crate::address::Address;
use crate::redact::redact;
use indexmap::IndexMap;
use serde::Deserialize;
use std::fmt;

/// The only config version this build reads and writes.
pub const CONFIG_VERSION: u32 = 2;

/// The whole config file.
#[derive(Clone, Default, Deserialize)]
pub struct Config {
    /// Must be [`CONFIG_VERSION`].
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
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chain {
    /// EVM chain ID.
    pub id: u64,
    /// Extra names this chain answers to (case-insensitive).
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Signer used for `@wallet`, `ETH_FROM` and `--expose-key`.
    #[serde(default)]
    pub signer: Option<String>,
    /// Explorer name from [`Config::explorers`].
    #[serde(default)]
    pub explorer: Option<String>,
    /// The active endpoint, by name.
    pub rpc: String,
    /// Endpoints the gateway may retry reads on, in order. Nothing else.
    #[serde(default)]
    pub failover: Vec<String>,
    /// Named endpoints.
    #[serde(default)]
    pub rpcs: IndexMap<String, Endpoint>,
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
        let lower = self.url.trim_start().to_ascii_lowercase();
        lower.starts_with("ws://") || lower.starts_with("wss://")
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("url", &redact(&self.url))
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// A block explorer API, shared by every chain that names it.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Explorer {
    /// API URL; `${CHAIN_ID}` lets one definition serve many chains.
    pub api: String,
    /// API key, normally `${VAR}`.
    #[serde(default)]
    pub key: Option<String>,
    /// Optional browser URL (for links).
    #[serde(default)]
    pub url: Option<String>,
}

impl Explorer {
    /// Etherscan-family explorers are left to forge's built-in Etherscan v2
    /// handling: exec exports only `ETHERSCAN_API_KEY` for them.
    pub fn is_etherscan(&self) -> bool {
        url::Url::parse(&self.api.replace("${", "").replace('}', ""))
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            .is_some_and(|host| host.split('.').any(|label| label == "etherscan"))
    }
}

impl fmt::Debug for Explorer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Explorer")
            .field("api", &redact(&self.api))
            .field("key", &self.key.as_ref().map(|_| "[REDACTED]"))
            .field("url", &self.url)
            .finish()
    }
}

/// Where a signer's key material lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
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

    /// Parse the config spelling.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "keyring" => Backend::Keyring,
            "encrypted" => Backend::Encrypted,
            "keystore" => Backend::Keystore,
            "1password" => Backend::OnePassword,
            "plaintext" => Backend::Plaintext,
            _ => return None,
        })
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Keyring service used for new keyring signers (and by v1).
pub const KEYRING_SERVICE: &str = "chainz";

/// A key reference plus its cached public address. Holds no key material.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signer {
    /// Storage backend.
    pub backend: Backend,
    /// Cached address: `@wallet`, `ETH_FROM` and balances never unlock.
    #[serde(default)]
    pub address: Option<Address>,
    /// Keyring service (default [`KEYRING_SERVICE`]).
    #[serde(default)]
    pub service: Option<String>,
    /// Keyring username (default: the signer name).
    #[serde(default)]
    pub username: Option<String>,
    /// Keystore file path (`~/` allowed).
    #[serde(default)]
    pub path: Option<String>,
    /// 1Password vault.
    #[serde(default)]
    pub vault: Option<String>,
    /// 1Password item reference.
    #[serde(default)]
    pub item: Option<String>,
}

impl Signer {
    /// Keyring service and username for a keyring signer named `name`.
    pub fn keyring_entry<'a>(&'a self, name: &'a str) -> (&'a str, &'a str) {
        (
            self.service.as_deref().unwrap_or(KEYRING_SERVICE),
            self.username.as_deref().unwrap_or(name),
        )
    }
}

impl Config {
    /// An empty v2 config.
    pub fn empty() -> Self {
        Self {
            version: CONFIG_VERSION,
            ..Self::default()
        }
    }

    /// The chain named exactly `name` (primary name, case-sensitive key).
    pub fn chain(&self, name: &str) -> Option<&Chain> {
        self.chains.get(name)
    }

    /// The chain with ID `id`.
    pub fn chain_by_id(&self, id: u64) -> Option<(&str, &Chain)> {
        self.chains
            .iter()
            .find(|(_, chain)| chain.id == id)
            .map(|(name, chain)| (name.as_str(), chain))
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
    /// The active endpoint record, if the config is consistent.
    pub fn active(&self) -> Option<&Endpoint> {
        self.rpcs.get(&self.rpc)
    }

    /// Whether `query` equals one of this chain's aliases, ignoring ASCII
    /// case. The chain's own name (its key) is not checked.
    pub fn matches_alias(&self, query: &str) -> bool {
        self.aliases.iter().any(|a| a.eq_ignore_ascii_case(query))
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
        let config: Config = toml_edit::de::from_str(SAMPLE).unwrap();
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
        let config: Config = toml_edit::de::from_str(SAMPLE).unwrap();
        let debug = format!("{config:?}");
        for secret in ["literal-explorer-secret", "path-secret", "header-secret"] {
            assert!(!debug.contains(secret), "{debug}");
        }
        assert!(debug.contains("x-secret"));
    }

    #[test]
    fn unknown_chain_fields_are_rejected() {
        let err = toml_edit::de::from_str::<Config>(
            "version = 2\n[chains.eth]\nid = 1\nrcp = \"x\"\nrpc = \"x\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("rcp"), "{err}");
    }
}
