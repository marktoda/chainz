//! Credential-safe presentation of chains for `ls`, `chain show` and JSON,
//! and the one plain-text [`table`] layout.
//!
//! Redaction happens when a view is built, so renderers cannot leak raw
//! secrets: `chainz_core::redact` masks only credential-shaped parts
//! (`${VAR}` templates, hosts and ordinary paths print as they are).
//! `--reveal` here shows raw config values (templates unexpanded). Resolved secret values are printed only where the user
//! asks for them: `var get --reveal`, `var ls --reveal`, `exec`'s
//! environment (`env`, `shell`, `exec --expose-key`), and `top --reveal`.

use crate::ui;
use chainz_core::model::{Chain, Config, Endpoint};
use chainz_core::{names, redact};
use console::{Alignment, pad_str, style};
use serde::Serialize;
use std::fmt::Write;

/// How to start from nothing: `chainz` with no chains, `init` off a
/// terminal, and the README (a test keeps them identical).
pub const QUICK_START: &str = "\
Quick start:
  chainz signer add deployer               # store a key (OS keyring by default)
  chainz chain add base                    # add a chain from chainlist; the signer attaches
  chainz exec base -- cast block-number    # run a tool with the chain's context
On a terminal, `chainz init` walks through the same steps.
";

/// What `chainz` and `ls` print when no chains are configured.
pub fn no_chains() -> String {
    format!("chainz: no chains configured yet.\n{QUICK_START}")
}

/// What `use` with no chain prints: the default chain, and how to change
/// it.
pub fn default_chain(default: Option<&str>) -> String {
    match default {
        Some(name) => format!(
            "default: {name}\nChange it with `chainz use <chain>`, or `chainz use --clear`\n"
        ),
        None => "default: none\nSet one with `chainz use <chain>`\n".to_string(),
    }
}

/// The marker for an endpoint's role: `●` active, `◐` failover.
pub fn marker(active: bool, failover: bool) -> &'static str {
    if active {
        "●"
    } else if failover {
        "◐"
    } else {
        " "
    }
}

/// What the role markers mean, under a listing that shows them.
pub const LEGEND: &str = "● active  ◐ failover";

/// `-` for an empty cell.
pub fn or_dash(value: &str) -> String {
    if value.is_empty() {
        "-".to_string()
    } else {
        value.to_string()
    }
}

#[derive(Serialize)]
pub struct EndpointView {
    pub name: String,
    pub url: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    pub active: bool,
    pub failover: bool,
    pub websocket: bool,
}

#[derive(Serialize)]
pub struct ChainView {
    pub name: String,
    pub id: u64,
    pub aliases: Vec<String>,
    pub rpc: String,
    pub rpc_url: String,
    pub failover: Vec<String>,
    pub signer: Option<String>,
    pub explorer: Option<String>,
    pub default: bool,
    pub endpoints: Vec<EndpointView>,
}

impl EndpointView {
    pub fn new(name: &str, endpoint: &Endpoint, chain: &Chain, reveal: bool) -> Self {
        Self {
            name: name.to_string(),
            url: redact::url(&endpoint.url, reveal),
            headers: endpoint
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), redact::header(v, reveal)))
                .collect(),
            active: chain.rpc == name,
            failover: chain.failover.iter().any(|f| f == name),
            websocket: endpoint.is_websocket(),
        }
    }

    /// The role marker (`●` active, `◐` failover).
    pub fn marker(&self) -> &'static str {
        marker(self.active, self.failover)
    }
}

impl ChainView {
    pub fn new(config: &Config, name: &str, chain: &Chain, reveal: bool) -> Self {
        Self {
            name: name.to_string(),
            id: chain.id,
            aliases: chain.aliases.clone(),
            rpc: chain.rpc.clone(),
            rpc_url: chain
                .active()
                .map(|e| redact::url(&e.url, reveal))
                .unwrap_or_default(),
            failover: chain.failover.clone(),
            signer: chain.signer.clone(),
            explorer: chain.explorer.clone(),
            default: config.default.as_deref() == Some(name),
            endpoints: chain
                .rpcs
                .iter()
                .map(|(n, e)| EndpointView::new(n, e, chain, reveal))
                .collect(),
        }
    }
}

/// All chains as views.
pub fn chains(config: &Config, reveal: bool) -> Vec<ChainView> {
    config
        .chains
        .iter()
        .map(|(name, chain)| ChainView::new(config, name, chain, reveal))
        .collect()
}

/// The compact `ls` table: the active endpoint by name, its host only
/// when the name doesn't already say it (the full URL with `--reveal`),
/// and the endpoint count.
pub fn compact(config: &Config, reveal: bool) -> String {
    if config.chains.is_empty() {
        return no_chains();
    }
    let rows: Vec<[String; 7]> = config
        .chains
        .iter()
        .map(|(name, chain)| {
            let marker = if config.default.as_deref() == Some(name) {
                "*"
            } else {
                " "
            };
            // The host is a terse label (stricter than `redact::url`);
            // `chain show` and `rpc ls` show the URL.
            let host = match chain.active() {
                Some(e) if reveal => format!(" {}", ui::dim(&e.url)),
                Some(e) if !names::named_after_url(&chain.rpc, &e.url) => {
                    format!(" {}", ui::dim(&redact::summarize(&e.url)))
                }
                _ => String::new(),
            };
            [
                format!("{marker}{name}"),
                chain.id.to_string(),
                or_dash(&chain.aliases.join(", ")),
                format!("{}{host}", chain.rpc),
                chain.rpcs.len().to_string(),
                or_dash(chain.signer.as_deref().unwrap_or_default()),
                or_dash(chain.explorer.as_deref().unwrap_or_default()),
            ]
        })
        .collect();
    table(
        &[" NAME", "ID", "ALIASES", "RPC", "EPS", "SIGNER", "EXPLORER"],
        &rows,
    )
}

/// Left-aligned columns two spaces apart, a bold header row first.
/// Widths are display widths, so styled and wide cells line up; trailing
/// spaces are trimmed.
pub fn table<R: AsRef<[String]>>(headers: &[&str], rows: &[R]) -> String {
    table_lines(headers, rows)
        .into_iter()
        .map(|line| line + "\n")
        .collect()
}

/// [`table`] as lines without their newline: the header, then one per row,
/// for a caller that puts lines between them (sections, sub-lines).
pub fn table_lines<R: AsRef<[String]>>(headers: &[&str], rows: &[R]) -> Vec<String> {
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|h| console::measure_text_width(h))
        .collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row.as_ref()) {
            *width = (*width).max(console::measure_text_width(cell));
        }
    }
    let line = |cells: &mut dyn Iterator<Item = String>| {
        let mut text = cells
            .zip(&widths)
            .map(|(cell, width)| pad_str(&cell, *width, Alignment::Left, None).into_owned())
            .collect::<Vec<_>>()
            .join("  ");
        text.truncate(text.trim_end().len());
        text
    };
    let mut out = vec![line(
        &mut headers.iter().map(|h| style(h).bold().to_string()),
    )];
    for row in rows {
        out.push(line(&mut row.as_ref().iter().cloned()));
    }
    out
}

/// The detailed `chain show` view.
pub fn show(view: &ChainView, extra: &[(String, String)]) -> String {
    let mut out = String::new();
    let aliases = if view.aliases.is_empty() {
        String::new()
    } else {
        ui::dim(&format!(" ({})", view.aliases.join(", ")))
    };
    let _ = writeln!(
        out,
        "{} {}{}",
        style("Chain").cyan().bold(),
        ui::emph(&view.name),
        aliases
    );
    let _ = writeln!(out, "  id        {}", view.id);
    let _ = writeln!(
        out,
        "  default   {}",
        if view.default { "yes" } else { "no" }
    );
    let _ = writeln!(out, "  signer    {}", view.signer.as_deref().unwrap_or("-"));
    let _ = writeln!(
        out,
        "  explorer  {}",
        view.explorer.as_deref().unwrap_or("-")
    );
    let _ = writeln!(
        out,
        "  failover  {}",
        if view.failover.is_empty() {
            "-".to_string()
        } else {
            view.failover.join(", ")
        }
    );
    for (k, v) in extra {
        let _ = writeln!(out, "  {k:<9} {v}");
    }
    let _ = writeln!(out, "  {}", style("endpoints").bold());
    let rows: Vec<[String; 3]> = view
        .endpoints
        .iter()
        .map(|e| {
            let marker = match (e.active, e.failover) {
                (true, _) => style(e.marker()).green().to_string(),
                (false, true) => style(e.marker()).yellow().to_string(),
                _ => e.marker().to_string(),
            };
            [marker, e.name.clone(), e.url.clone()]
        })
        .collect();
    // No header row: the `endpoints` title names the list.
    let lines = table_lines(&["", "", ""], &rows);
    for (e, line) in view.endpoints.iter().zip(&lines[1..]) {
        let _ = writeln!(out, "  {line}");
        for (k, v) in &e.headers {
            let _ = writeln!(out, "      {}", ui::dim(&format!("{k}: {v}")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_align_on_display_width() {
        console::set_colors_enabled(false);
        let rows = [
            vec![
                "base-fork".to_string(),
                "0x0123…4567".to_string(),
                "5s".into(),
            ],
            // Wide characters take two cells.
            vec!["日本".to_string(), "-".to_string(), "1s".into()],
            vec!["x".to_string(), "-".to_string(), String::new()],
        ];
        assert_eq!(
            table(&["NAME", "ADDRESS", "UP"], &rows),
            "NAME       ADDRESS      UP\nbase-fork  0x0123…4567  5s\n日本       -            1s\nx          -\n"
        );
    }
}
