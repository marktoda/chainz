pub mod rpc;
pub mod wizard;

use crate::key::Key;
use crate::ui;
use console::style;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Display};

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
            .map(|url| crate::variables::redact_url(url))
            .collect();
        f.debug_struct("ChainDefinition")
            .field("name", &self.name)
            .field("aliases", &self.aliases)
            .field("chain_id", &self.chain_id)
            .field("rpc_urls", &rpc_urls)
            .field(
                "selected_rpc",
                &crate::variables::redact_url(&self.selected_rpc),
            )
            .field(
                "verification_api_key",
                &self.verification_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "verification_url",
                &self
                    .verification_url
                    .as_ref()
                    .map(|url| crate::variables::redact_url(url)),
            )
            .field("key_name", &self.key_name)
            .finish()
    }
}

impl ChainDefinition {
    /// All names this chain answers to: primary name first, then aliases.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str()).chain(self.aliases.iter().map(String::as_str))
    }

    pub fn matches_exact(&self, query: &str) -> bool {
        self.names().any(|n| n.eq_ignore_ascii_case(query))
    }

    pub fn matches_prefix(&self, query: &str) -> bool {
        let query = query.to_lowercase();
        self.names().any(|n| n.to_lowercase().starts_with(&query))
    }

    pub fn display_with_secrets(&self, show_secrets: bool) -> ChainDisplay<'_> {
        ChainDisplay {
            chain: self,
            show_secrets,
        }
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

pub struct ChainDisplay<'a> {
    chain: &'a ChainDefinition,
    show_secrets: bool,
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
    pub fn new(definition: ChainDefinition, rpc_url: String, key: Option<Key>) -> Self {
        Self {
            definition,
            rpc_url,
            key,
        }
    }

    pub fn with_key(mut self, key: Key) -> Self {
        self.key = Some(key);
        self
    }
}

impl Display for ChainDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.display_with_secrets(false).fmt(f)
    }
}

impl Display for ChainDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let chain = self.chain;
        writeln!(
            f,
            "{}: {}{}",
            style("Chain").cyan().bold(),
            ui::emph(&chain.name),
            if chain.aliases.is_empty() {
                String::new()
            } else {
                ui::dim(&format!(" ({})", chain.aliases.join(", ")))
            }
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            style("├").dim(),
            style("ID").cyan(),
            ui::emph(&chain.chain_id.to_string())
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            style("├").dim(),
            style("Active RPC").cyan(),
            style(if self.show_secrets {
                chain.selected_rpc.clone()
            } else {
                crate::variables::redact_url(&chain.selected_rpc)
            })
            .green()
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            style("├").dim(),
            style("Verification URL").cyan(),
            chain
                .verification_url
                .as_deref()
                .map(|url| {
                    style(if self.show_secrets {
                        url.to_string()
                    } else {
                        crate::variables::redact_url(url)
                    })
                    .green()
                    .to_string()
                })
                .unwrap_or_else(|| style("None").red().to_string())
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            style("├").dim(),
            style("Verification Key").cyan(),
            chain
                .verification_api_key
                .as_deref()
                .map(|key| {
                    style(if self.show_secrets { key } else { "Configured" })
                        .green()
                        .to_string()
                })
                .unwrap_or_else(|| style("None").red().to_string())
        )?;
        write!(
            f,
            "{}─ {}: {}",
            style("└").dim(),
            style("Key Name").cyan(),
            style(chain.key_name.as_deref().unwrap_or("None")).green(),
        )
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
    fn test_chain_definition_display_basic() {
        let def = make_chain_def(None, None);
        let output = format!("{}", def);

        assert!(output.contains("ethereum"), "should contain chain name");
        assert!(output.contains("1"), "should contain chain_id");
        assert!(
            output.contains("https://eth.llamarpc.com"),
            "should contain rpc url"
        );
        assert!(output.contains("default"), "should contain key name");
    }

    #[test]
    fn test_chain_definition_display_with_verification() {
        let def = make_chain_def(Some("https://api.etherscan.io"), Some("abc123"));
        let output = format!("{}", def);

        assert!(
            output.contains("https://api.etherscan.io"),
            "should contain verification url"
        );
        assert!(!output.contains("abc123"), "should redact verification key");
        assert!(output.contains("Configured"));
    }

    #[test]
    fn test_chain_definition_display_without_verification() {
        let def = make_chain_def(None, None);
        let output = format!("{}", def);

        assert!(
            output.contains("None"),
            "should show None when verification fields are absent"
        );
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
