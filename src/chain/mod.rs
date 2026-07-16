pub mod rpc;
pub mod wizard;

use crate::key::Key;
use alloy::providers::DynProvider;
use colored::*;
use serde::{Deserialize, Serialize};
use std::fmt::Display;

pub use rpc::{resolve_rpc, resolve_rpcs};

pub const DEFAULT_KEY_NAME: &str = "default";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainDefinition {
    pub name: String,
    pub chain_id: u64,
    pub rpc_urls: Vec<String>,
    pub selected_rpc: String,
    pub verification_api_key: Option<String>,
    pub verification_url: Option<String>,
    pub key_name: String,
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

pub struct Rpc {
    pub rpc_url: String,
    pub provider: DynProvider,
}

impl Display for Rpc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.rpc_url)
    }
}

impl Display for ChainDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "{}: {}",
            "Chain".bright_blue().bold(),
            self.name.yellow()
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            "├".bright_black(),
            "ID".bright_blue(),
            self.chain_id.to_string().yellow()
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            "├".bright_black(),
            "Active RPC".bright_blue(),
            self.selected_rpc.bright_green()
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            "├".bright_black(),
            "Verification URL".bright_blue(),
            self.verification_url
                .as_deref()
                .map(|k| k.bright_green().to_string())
                .unwrap_or_else(|| "None".bright_red().to_string())
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            "├".bright_black(),
            "Verification Key".bright_blue(),
            self.verification_api_key
                .as_deref()
                .map(|k| k.bright_green().to_string())
                .unwrap_or_else(|| "None".bright_red().to_string())
        )?;
        write!(
            f,
            "{}─ {}: {}",
            "└".bright_black(),
            "Key Name".bright_blue(),
            self.key_name.bright_green(),
        )
    }
}

impl Display for ChainInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "{}: {}",
            "Chain".bright_blue().bold(),
            self.definition.name.yellow()
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            "├".bright_black(),
            "ID".bright_blue(),
            self.definition.chain_id.to_string().yellow()
        )?;
        writeln!(
            f,
            "{}─ {}: {}",
            "├".bright_black(),
            "RPC".bright_blue(),
            self.rpc_url.bright_green()
        )?;
        write!(
            f,
            "{}─ {}: {}",
            "└".bright_black(),
            "Wallet".bright_blue(),
            self.key
                .address()
                .map(|addr| addr.to_string().bright_green())
                .unwrap_or("None".bright_red())
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
