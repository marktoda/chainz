pub(crate) mod rpc;
pub(crate) mod wizard;

use crate::key::Key;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

pub const DEFAULT_KEY_NAME: &str = "default";

/// One RPC endpoint: a URL plus optional custom HTTP headers (for gateways
/// that authenticate via header rather than URL-embedded credential).
///
/// On the wire this is a plain JSON string when there are no headers — so
/// existing configs parse unchanged and header-free configs stay readable by
/// older chainz versions — and a `{url, headers}` object otherwise.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RpcEndpointWire", into = "RpcEndpointWire")]
pub struct RpcEndpoint {
    pub url: String,
    /// Raw (unexpanded) header values; may contain ${VAR} references.
    /// BTreeMap keeps serialization and env-export ordering deterministic.
    pub headers: BTreeMap<String, String>,
}

impl RpcEndpoint {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: BTreeMap::new(),
        }
    }

    pub fn with_headers(url: impl Into<String>, headers: BTreeMap<String, String>) -> Self {
        Self {
            url: url.into(),
            headers,
        }
    }
}

impl From<String> for RpcEndpoint {
    fn from(url: String) -> Self {
        Self::new(url)
    }
}

impl From<&str> for RpcEndpoint {
    fn from(url: &str) -> Self {
        Self::new(url)
    }
}

impl fmt::Debug for RpcEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcEndpoint")
            .field("url", &crate::endpoint::redact(&self.url))
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Wire format: legacy plain string or {url, headers} object.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RpcEndpointWire {
    Url(String),
    Detailed {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

impl From<RpcEndpointWire> for RpcEndpoint {
    fn from(wire: RpcEndpointWire) -> Self {
        match wire {
            RpcEndpointWire::Url(url) => Self::new(url),
            RpcEndpointWire::Detailed { url, headers } => Self::with_headers(url, headers),
        }
    }
}

impl From<RpcEndpoint> for RpcEndpointWire {
    fn from(endpoint: RpcEndpoint) -> Self {
        if endpoint.headers.is_empty() {
            Self::Url(endpoint.url)
        } else {
            Self::Detailed {
                url: endpoint.url,
                headers: endpoint.headers,
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ChainDefinition {
    pub name: String,
    /// Alternate lookup names (e.g. the full chainlist name when the user
    /// picked a short one). Absent in configs written by older versions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub chain_id: u64,
    pub rpc_urls: Vec<RpcEndpoint>,
    pub selected_rpc: String,
    pub verification_api_key: Option<String>,
    pub verification_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_name: Option<String>,
}

impl fmt::Debug for ChainDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChainDefinition")
            .field("name", &self.name)
            .field("aliases", &self.aliases)
            .field("chain_id", &self.chain_id)
            .field("rpc_urls", &self.rpc_urls)
            .field("selected_rpc", &crate::endpoint::redact(&self.selected_rpc))
            .field(
                "verification_api_key",
                &self.verification_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "verification_url",
                &self
                    .verification_url
                    .as_ref()
                    .map(|url| crate::endpoint::redact(url)),
            )
            .field("key_name", &self.key_name)
            .finish()
    }
}

impl ChainDefinition {
    /// All names this chain answers to: primary name first, then aliases.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str()).chain(self.aliases.iter().map(String::as_str))
    }

    pub(crate) fn matches_exact(&self, query: &str) -> bool {
        self.names().any(|n| n.eq_ignore_ascii_case(query))
    }

    pub(crate) fn matches_prefix(&self, query: &str) -> bool {
        let query = query.to_lowercase();
        self.names().any(|n| n.to_lowercase().starts_with(&query))
    }

    /// The configured endpoint for `url`, if present.
    pub(crate) fn endpoint(&self, url: &str) -> Option<&RpcEndpoint> {
        self.rpc_urls.iter().find(|endpoint| endpoint.url == url)
    }

    /// The endpoint record backing `selected_rpc`. `None` only on configs that
    /// bypass validation (doctor's lenient load).
    pub(crate) fn selected_endpoint(&self) -> Option<&RpcEndpoint> {
        self.endpoint(&self.selected_rpc)
    }

    /// Select an RPC while preserving the config invariant that the selected
    /// endpoint is present in the chain's configured endpoint list. An entry
    /// that already exists keeps its headers.
    pub(crate) fn select_rpc(&mut self, rpc_url: String) {
        if self.endpoint(&rpc_url).is_none() {
            self.rpc_urls.push(RpcEndpoint::new(rpc_url.clone()));
        }
        self.selected_rpc = rpc_url;
    }

    /// Select an RPC and set its header set (replacing any previous headers).
    pub(crate) fn select_rpc_with_headers(
        &mut self,
        rpc_url: String,
        headers: BTreeMap<String, String>,
    ) {
        match self.rpc_urls.iter_mut().find(|e| e.url == rpc_url) {
            Some(endpoint) => endpoint.headers = headers,
            None => self
                .rpc_urls
                .push(RpcEndpoint::with_headers(rpc_url.clone(), headers)),
        }
        self.selected_rpc = rpc_url;
    }
}

/// A chain resolved for use: RPC URL expanded and key attached.
/// Deliberately holds no network state — commands that need the chain
/// (e.g. `exec`) only consume strings and the key.
pub struct ChainInstance {
    pub definition: ChainDefinition,
    pub rpc_url: String,
    /// Expanded custom headers of the selected endpoint (empty when none).
    pub headers: BTreeMap<String, String>,
    pub key: Option<Key>,
}

impl ChainInstance {
    pub fn with_key(mut self, key: Key) -> Self {
        self.key = Some(key);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_chain_def(
        verification_url: Option<&str>,
        verification_api_key: Option<&str>,
    ) -> ChainDefinition {
        ChainDefinition {
            name: "ethereum".to_string(),
            aliases: vec![],
            chain_id: 1,
            rpc_urls: vec!["https://eth.llamarpc.com".into()],
            selected_rpc: "https://eth.llamarpc.com".to_string(),
            verification_api_key: verification_api_key.map(String::from),
            verification_url: verification_url.map(String::from),
            key_name: Some("default".to_string()),
        }
    }

    #[test]
    fn debug_redacts_chain_credentials() {
        let chain = make_chain_def(
            Some("https://verify.example/api/key"),
            Some("verification-secret"),
        );
        let mut chain = chain;
        chain.selected_rpc = "https://user:password@rpc.example/v2/rpc-secret".into();
        chain.rpc_urls = vec![chain.selected_rpc.clone().into()];

        let output = format!("{chain:?}");
        for secret in ["verification-secret", "password", "rpc-secret"] {
            assert!(!output.contains(secret), "{output}");
        }
    }

    #[test]
    fn rpc_endpoint_deserializes_string_and_object_forms() {
        let plain: RpcEndpoint = serde_json::from_str(r#""https://rpc.example.com""#).unwrap();
        assert_eq!(plain, RpcEndpoint::new("https://rpc.example.com"));

        let detailed: RpcEndpoint = serde_json::from_str(
            r#"{"url":"https://gw.example.com/rpc/1","headers":{"x-secret":"${GW_SECRET}"}}"#,
        )
        .unwrap();
        assert_eq!(detailed.url, "https://gw.example.com/rpc/1");
        assert_eq!(detailed.headers["x-secret"], "${GW_SECRET}");

        // headers key optional in object form
        let bare: RpcEndpoint =
            serde_json::from_str(r#"{"url":"https://rpc.example.com"}"#).unwrap();
        assert!(bare.headers.is_empty());
    }

    #[test]
    fn rpc_endpoint_serializes_headerless_as_plain_string() {
        let plain = RpcEndpoint::new("https://rpc.example.com");
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            r#""https://rpc.example.com""#
        );

        let mut headers = std::collections::BTreeMap::new();
        headers.insert("x-secret".to_string(), "value".to_string());
        let detailed = RpcEndpoint::with_headers("https://gw.example.com", headers);
        let json = serde_json::to_string(&detailed).unwrap();
        assert!(json.contains(r#""url":"https://gw.example.com""#), "{json}");
        assert!(json.contains(r#""x-secret":"value""#), "{json}");

        // round-trip preserves both forms
        let back: RpcEndpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back, detailed);
    }

    #[test]
    fn rpc_endpoint_debug_redacts_url_and_header_values() {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("x-secret-name".to_string(), "header-secret".to_string());
        let endpoint = RpcEndpoint::with_headers(
            "https://user:password@private.rpc.example.com/path-secret",
            headers,
        );
        let output = format!("{endpoint:?}");
        for secret in ["password", "path-secret", "header-secret", "private"] {
            assert!(!output.contains(secret), "{output}");
        }
        assert!(output.contains("x-secret-name"), "{output}");
    }

    #[test]
    fn mixed_rpc_endpoint_list_round_trips_string_and_object_forms() {
        let json =
            r#"["https://a.example.com", {"url":"https://b.example.com","headers":{"x-s":"v"}}]"#;
        let endpoints: Vec<RpcEndpoint> = serde_json::from_str(json).unwrap();

        assert_eq!(endpoints.len(), 2);
        assert_eq!(endpoints[0], RpcEndpoint::new("https://a.example.com"));
        assert_eq!(endpoints[1].url, "https://b.example.com");
        assert_eq!(endpoints[1].headers["x-s"], "v");

        let reserialized = serde_json::to_string(&endpoints).unwrap();
        let reparsed: serde_json::Value = serde_json::from_str(&reserialized).unwrap();
        let array = reparsed.as_array().unwrap();
        assert_eq!(array[0], serde_json::json!("https://a.example.com"));
        assert_eq!(
            array[1],
            serde_json::json!({"url": "https://b.example.com", "headers": {"x-s": "v"}})
        );
    }

    #[test]
    fn select_rpc_preserves_headers_and_select_with_headers_replaces_them() {
        let mut chain = make_chain_def(None, None);
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("x-secret".to_string(), "v1".to_string());
        chain.select_rpc_with_headers("https://gw.example.com".to_string(), headers.clone());
        assert_eq!(chain.selected_rpc, "https://gw.example.com");
        assert_eq!(chain.selected_endpoint().unwrap().headers, headers);

        // re-selecting via plain select_rpc keeps the stored headers
        chain.select_rpc("https://eth.llamarpc.com".to_string());
        chain.select_rpc("https://gw.example.com".to_string());
        assert_eq!(chain.selected_endpoint().unwrap().headers, headers);

        // select_rpc_with_headers replaces the header set
        chain.select_rpc_with_headers(
            "https://gw.example.com".to_string(),
            std::collections::BTreeMap::new(),
        );
        assert!(chain.selected_endpoint().unwrap().headers.is_empty());
        // no duplicate entries were created
        assert_eq!(
            chain
                .rpc_urls
                .iter()
                .filter(|e| e.url == "https://gw.example.com")
                .count(),
            1
        );
    }
}
