pub(crate) mod rpc;
pub(crate) mod wizard;

use crate::key::Key;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const DEFAULT_KEY_NAME: &str = "default";

#[derive(Clone, Serialize, Deserialize)]
pub struct ChainDefinition {
    pub name: String,
    /// Alternate lookup names (e.g. the full chainlist name when the user
    /// picked a short one). Absent in configs written by older versions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub chain_id: u64,
    pub rpc_urls: Vec<String>,
    pub selected_rpc: String,
    pub verification_api_key: Option<String>,
    pub verification_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_name: Option<String>,
}

impl fmt::Debug for ChainDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rpc_urls: Vec<_> = self
            .rpc_urls
            .iter()
            .map(|url| crate::endpoint::redact(url))
            .collect();
        f.debug_struct("ChainDefinition")
            .field("name", &self.name)
            .field("aliases", &self.aliases)
            .field("chain_id", &self.chain_id)
            .field("rpc_urls", &rpc_urls)
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

    /// Select an RPC while preserving the config invariant that the selected
    /// endpoint is present in the chain's configured endpoint list.
    pub(crate) fn select_rpc(&mut self, rpc_url: String) {
        if !self.rpc_urls.contains(&rpc_url) {
            self.rpc_urls.push(rpc_url.clone());
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
            rpc_urls: vec!["https://eth.llamarpc.com".to_string()],
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
        chain.rpc_urls = vec![chain.selected_rpc.clone()];

        let output = format!("{chain:?}");
        for secret in ["verification-secret", "password", "rpc-secret"] {
            assert!(!output.contains(secret), "{output}");
        }
    }
}
