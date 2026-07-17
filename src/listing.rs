//! Human-readable chain listing formats.
//!
//! The interface returns complete text so command dispatch and tests use the
//! same seam; JSON rendering remains a separate scripting contract.

use crate::{chain::ChainDefinition, ui};
use console::{Alignment, pad_str};
use std::fmt::Write;

const EMPTY_HINT: &str = "No chains configured. Run 'chainz init' or 'chainz add' to get started.";

pub fn compact(chains: &[ChainDefinition], default: Option<&str>) -> String {
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
        let rpc = ui::redact_url(&chain.selected_rpc);
        let key = chain.key_name.as_deref().unwrap_or("—");
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
            pad_str(&rpc, 30, Alignment::Left, Some("…")),
            console::truncate_str(key, 16, "…")
        )
        .expect("writing to a String cannot fail");
    }
    if default.is_some() {
        output.push_str("* default chain\n");
    }
    output
}

pub fn verbose(chains: &[ChainDefinition], default: Option<&str>) -> String {
    if chains.is_empty() {
        return format!("{EMPTY_HINT}\n");
    }

    let mut output = String::new();
    for (index, chain) in chains.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        writeln!(output, "{}", chain).expect("writing to a String cannot fail");
        if default == Some(chain.name.as_str()) {
            output.push_str("Default: Yes\n");
        }
    }
    output
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
            verification_api_key: None,
            verification_url: None,
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
}
