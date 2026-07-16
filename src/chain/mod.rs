pub mod rpc;
pub mod wizard;

use crate::key::Key;
use crate::ui;
use console::style;
use serde::{Deserialize, Serialize};
use std::fmt::Display;

pub const DEFAULT_KEY_NAME: &str = "default";

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    pub key_name: String,
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
}

/// A chain resolved for use: RPC URL expanded and key attached.
/// Deliberately holds no network state — commands that need the chain
/// (e.g. `exec`) only consume strings and the key.
pub struct ChainInstance {
    pub definition: ChainDefinition,
    pub rpc_url: String,
    pub key: Key,
}

impl ChainInstance {
    pub fn new(definition: ChainDefinition, rpc_url: String, key: Key) -> Self {
        Self {
            definition,
            rpc_url,
            key,
        }
    }

    pub fn with_key(mut self, key: Key) -> Self {
        self.key = key;
        self
    }
}

impl Display for ChainDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "{}: {}{}",
            style("Chain").cyan().bold(),
            ui::emph(&self.name),
            if self.aliases.is_empty() {
                String::new()
            } else {
                ui::dim(&format!(" ({})", self.aliases.join(", ")))
            }
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            style("├").dim(),
            style("ID").cyan(),
            ui::emph(&self.chain_id.to_string())
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            style("├").dim(),
            style("Active RPC").cyan(),
            style(&self.selected_rpc).green()
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            style("├").dim(),
            style("Verification URL").cyan(),
            self.verification_url
                .as_deref()
                .map(|k| style(k).green().to_string())
                .unwrap_or_else(|| style("None").red().to_string())
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            style("├").dim(),
            style("Verification Key").cyan(),
            self.verification_api_key
                .as_deref()
                .map(|k| style(k).green().to_string())
                .unwrap_or_else(|| style("None").red().to_string())
        )?;
        write!(
            f,
            "{}─ {}: {}",
            style("└").dim(),
            style("Key Name").cyan(),
            style(&self.key_name).green(),
        )
    }
}

impl Display for ChainInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "{}: {}",
            style("Chain").cyan().bold(),
            ui::emph(&self.definition.name)
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            style("├").dim(),
            style("ID").cyan(),
            ui::emph(&self.definition.chain_id.to_string())
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            style("├").dim(),
            style("RPC").cyan(),
            style(&self.rpc_url).green()
        )?;
        write!(
            f,
            "{}─ {}: {}",
            style("└").dim(),
            style("Wallet").cyan(),
            self.key
                .address()
                .map(|addr| style(addr.to_string()).green().to_string())
                .unwrap_or_else(|_| style("None").red().to_string())
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
            key_name: "default".to_string(),
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
        assert!(
            output.contains("abc123"),
            "should contain verification api key"
        );
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
}
