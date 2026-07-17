//! Credential-safe presentation of configured chains.
//!
//! `ChainView` is the presentation seam shared by human detail output and the
//! stable JSON scripting contract. Endpoint redaction is applied when the view
//! is built, so downstream renderers cannot accidentally expose raw secrets.

use crate::{chain::ChainDefinition, endpoint, ui};
use console::{Alignment, pad_str, style};
use serde::Serialize;
use std::fmt::Write;

const EMPTY_HINT: &str = "No chains configured. Run 'chainz init' or 'chainz add' to get started.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SecretVisibility {
    Redacted,
    Revealed,
}

impl From<bool> for SecretVisibility {
    fn from(reveal: bool) -> Self {
        if reveal {
            Self::Revealed
        } else {
            Self::Redacted
        }
    }
}

/// A credential-safe projection of a chain for display or serialization.
#[derive(Serialize)]
struct ChainView<'a> {
    name: &'a str,
    aliases: &'a [String],
    chain_id: u64,
    selected_rpc: String,
    rpc_urls: Vec<String>,
    key_name: Option<&'a str>,
    verification_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verification_api_key: Option<&'a str>,
    is_default: bool,
    #[serde(skip)]
    verification_key_configured: bool,
}

impl<'a> ChainView<'a> {
    fn new(
        chain: &'a ChainDefinition,
        default: Option<&str>,
        visibility: SecretVisibility,
    ) -> Self {
        let reveal = visibility == SecretVisibility::Revealed;
        let present = |url: &str| {
            if reveal {
                url.to_string()
            } else {
                endpoint::redact(url)
            }
        };
        Self {
            name: &chain.name,
            aliases: &chain.aliases,
            chain_id: chain.chain_id,
            selected_rpc: present(&chain.selected_rpc),
            rpc_urls: chain.rpc_urls.iter().map(|url| present(url)).collect(),
            key_name: chain.key_name.as_deref(),
            verification_url: chain.verification_url.as_deref().map(present),
            verification_api_key: reveal
                .then_some(chain.verification_api_key.as_deref())
                .flatten(),
            is_default: default == Some(chain.name.as_str()),
            verification_key_configured: chain.verification_api_key.is_some(),
        }
    }

    /// Render the detailed `show` view, including explicit default status.
    fn show(&self) -> String {
        let mut output = self.description();
        writeln!(
            output,
            "Default: {}",
            if self.is_default { "Yes" } else { "No" }
        )
        .expect("writing to a String cannot fail");
        output
    }

    fn description(&self) -> String {
        let mut output = String::new();
        writeln!(
            output,
            "{}: {}{}",
            style("Chain").cyan().bold(),
            ui::emph(self.name),
            if self.aliases.is_empty() {
                String::new()
            } else {
                ui::dim(&format!(" ({})", self.aliases.join(", ")))
            }
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "{}─ {}: {}",
            style("├").dim(),
            style("ID").cyan(),
            ui::emph(&self.chain_id.to_string())
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "{}─ {}: {}",
            style("├").dim(),
            style("Active RPC").cyan(),
            style(&self.selected_rpc).green()
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "{}─ {}: {}",
            style("├").dim(),
            style("Verification URL").cyan(),
            self.verification_url
                .as_deref()
                .map(|url| style(url).green().to_string())
                .unwrap_or_else(|| style("None").red().to_string())
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "{}─ {}: {}",
            style("├").dim(),
            style("Verification Key").cyan(),
            if self.verification_key_configured {
                style(self.verification_api_key.unwrap_or("Configured"))
                    .green()
                    .to_string()
            } else {
                style("None").red().to_string()
            }
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "{}─ {}: {}",
            style("└").dim(),
            style("Key Name").cyan(),
            style(self.key_name.unwrap_or("None")).green(),
        )
        .expect("writing to a String cannot fail");
        output
    }
}

pub(crate) fn compact(chains: &[ChainDefinition], default: Option<&str>) -> String {
    if chains.is_empty() {
        return format!("{EMPTY_HINT}\n");
    }

    let name_width = chains
        .iter()
        .map(|chain| console::measure_text_width(&chain.name))
        .max()
        .unwrap_or(5)
        .clamp(5, 24);
    let id_width = chains
        .iter()
        .map(|chain| chain.chain_id.to_string().len())
        .max()
        .unwrap_or(2)
        .clamp(2, 12);
    let mut output = String::new();
    writeln!(
        output,
        "  {}  {}  {}  KEY",
        pad_str("CHAIN", name_width, Alignment::Left, Some("…")),
        pad_str("ID", id_width, Alignment::Right, Some("…")),
        pad_str("RPC", 30, Alignment::Left, Some("…"))
    )
    .expect("writing to a String cannot fail");
    for chain in chains {
        let marker = if default == Some(chain.name.as_str()) {
            "*"
        } else {
            " "
        };
        writeln!(
            output,
            "{} {}  {}  {}  {}",
            marker,
            pad_str(&chain.name, name_width, Alignment::Left, Some("…")),
            pad_str(
                &chain.chain_id.to_string(),
                id_width,
                Alignment::Right,
                Some("…")
            ),
            pad_str(
                &endpoint::summarize(&chain.selected_rpc),
                30,
                Alignment::Left,
                Some("…")
            ),
            console::truncate_str(chain.key_name.as_deref().unwrap_or("—"), 16, "…")
        )
        .expect("writing to a String cannot fail");
    }
    if default.is_some() {
        output.push_str("* default chain\n");
    }
    output
}

pub(crate) fn verbose(
    chains: &[ChainDefinition],
    default: Option<&str>,
    visibility: SecretVisibility,
) -> String {
    if chains.is_empty() {
        return format!("{EMPTY_HINT}\n");
    }

    let mut output = String::new();
    for (index, chain) in chains.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        let view = ChainView::new(chain, default, visibility);
        output.push_str(&view.description());
        if view.is_default {
            output.push_str("Default: Yes\n");
        }
    }
    output
}

pub(crate) fn json(
    chains: &[ChainDefinition],
    default: Option<&str>,
    visibility: SecretVisibility,
) -> serde_json::Result<String> {
    let views: Vec<_> = chains
        .iter()
        .map(|chain| ChainView::new(chain, default, visibility))
        .collect();
    serde_json::to_string_pretty(&views)
}

pub(crate) fn show(
    chain: &ChainDefinition,
    default: Option<&str>,
    visibility: SecretVisibility,
) -> String {
    ChainView::new(chain, default, visibility).show()
}

pub(crate) fn show_json(
    chain: &ChainDefinition,
    default: Option<&str>,
    visibility: SecretVisibility,
) -> serde_json::Result<String> {
    serde_json::to_string_pretty(&ChainView::new(chain, default, visibility))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(name: &str, id: u64, key: Option<&str>) -> ChainDefinition {
        ChainDefinition {
            name: name.to_string(),
            aliases: vec![],
            chain_id: id,
            rpc_urls: vec!["https://provider.example/v2/secret".to_string()],
            selected_rpc: "https://provider.example/v2/secret".to_string(),
            verification_api_key: Some("verification-secret".to_string()),
            verification_url: Some("https://verify.example/api/secret".to_string()),
            key_name: key.map(str::to_string),
        }
    }

    #[test]
    fn compact_marks_default_and_hides_endpoint_details() {
        let output = compact(
            &[
                chain("ethereum", 1, Some("default")),
                chain("base", 8453, None),
            ],
            Some("ethereum"),
        );

        assert!(output.contains("* ethereum"));
        assert!(output.contains("—"));
        assert!(!output.contains("secret"));
        assert_eq!(output.lines().count(), 4);
    }

    #[test]
    fn redacted_view_is_safe_across_human_and_json_renderers() {
        let chain = chain("ethereum", 1, Some("default"));
        for output in [
            show(&chain, Some("ethereum"), SecretVisibility::Redacted),
            show_json(&chain, Some("ethereum"), SecretVisibility::Redacted).unwrap(),
        ] {
            assert!(!output.contains("verification-secret"), "{output}");
            assert!(!output.contains("/secret"), "{output}");
        }
        let output = show(&chain, Some("ethereum"), SecretVisibility::Redacted);
        assert!(output.contains("Configured"));
        assert!(output.contains("Default: Yes"));
    }

    #[test]
    fn revealed_view_preserves_explicit_show_secrets_behavior() {
        let chain = chain("ethereum", 1, None);
        let json = show_json(&chain, None, SecretVisibility::Revealed).unwrap();
        assert!(json.contains("verification-secret"));
        assert!(json.contains("/v2/secret"));
    }

    #[test]
    fn detail_view_preserves_basic_information_and_none_states() {
        let mut chain = chain("ethereum", 1, Some("default"));
        chain.verification_url = None;
        chain.verification_api_key = None;
        let output = show(&chain, None, SecretVisibility::Redacted);

        for expected in ["ethereum", "1", "provider.example", "default", "None"] {
            assert!(output.contains(expected), "{output}");
        }
    }
}
