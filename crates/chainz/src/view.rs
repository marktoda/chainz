//! Credential-safe presentation of chains for `ls`, `chain show` and JSON.
//!
//! Redaction happens when a view is built, so renderers cannot leak raw
//! secrets. `--reveal` shows raw config values (templates unexpanded);
//! resolved secret values are only ever printed by `var get --reveal`.

use crate::ui;
use chainz_core::model::{Chain, Config, Endpoint};
use chainz_core::redact::{redact, summarize};
use console::{Alignment, pad_str, style};
use serde::Serialize;
use std::fmt::Write;

/// Hint shown when no chains are configured.
pub const EMPTY_HINT: &str =
    "No chains configured. Add one with `chainz chain add <chain-id|name>`.";

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

fn present_url(url: &str, reveal: bool) -> String {
    if reveal { url.to_string() } else { redact(url) }
}

impl EndpointView {
    pub fn new(name: &str, endpoint: &Endpoint, chain: &Chain, reveal: bool) -> Self {
        Self {
            name: name.to_string(),
            url: present_url(&endpoint.url, reveal),
            headers: endpoint
                .headers
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        if reveal {
                            v.clone()
                        } else {
                            "[REDACTED]".to_string()
                        },
                    )
                })
                .collect(),
            active: chain.rpc == name,
            failover: chain.failover.iter().any(|f| f == name),
            websocket: endpoint.is_websocket(),
        }
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
                .map(|e| present_url(&e.url, reveal))
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

/// The compact `ls` table.
pub fn compact(config: &Config, reveal: bool) -> String {
    if config.chains.is_empty() {
        return format!("{EMPTY_HINT}\n");
    }
    let rows: Vec<[String; 5]> = config
        .chains
        .iter()
        .map(|(name, chain)| {
            let marker = if config.default.as_deref() == Some(name) {
                "*"
            } else {
                " "
            };
            let url = chain
                .active()
                .map(|e| {
                    if reveal {
                        e.url.clone()
                    } else {
                        summarize(&e.url)
                    }
                })
                .unwrap_or_default();
            [
                format!("{marker}{name}"),
                chain.id.to_string(),
                format!("{} {}", chain.rpc, ui::dim(&url)),
                chain.signer.clone().unwrap_or_else(|| "-".into()),
                chain.explorer.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    let headers = [" NAME", "ID", "RPC", "SIGNER", "EXPLORER"];
    let mut widths = headers.map(console::measure_text_width);
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(console::measure_text_width(cell));
        }
    }
    let mut out = String::new();
    let line = |cells: &[String]| {
        cells
            .iter()
            .zip(widths)
            .map(|(c, w)| pad_str(c, w, Alignment::Left, None).into_owned())
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let header: Vec<String> = headers
        .iter()
        .map(|h| style(h).bold().to_string())
        .collect();
    writeln!(out, "{}", line(&header)).ok();
    for row in &rows {
        writeln!(out, "{}", line(row)).ok();
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
    writeln!(
        out,
        "{} {}{}",
        style("Chain").cyan().bold(),
        ui::emph(&view.name),
        aliases
    )
    .ok();
    writeln!(out, "  id        {}", view.id).ok();
    writeln!(
        out,
        "  default   {}",
        if view.default { "yes" } else { "no" }
    )
    .ok();
    writeln!(out, "  signer    {}", view.signer.as_deref().unwrap_or("-")).ok();
    writeln!(
        out,
        "  explorer  {}",
        view.explorer.as_deref().unwrap_or("-")
    )
    .ok();
    writeln!(
        out,
        "  failover  {}",
        if view.failover.is_empty() {
            "-".to_string()
        } else {
            view.failover.join(", ")
        }
    )
    .ok();
    for (k, v) in extra {
        writeln!(out, "  {k:<9} {v}").ok();
    }
    writeln!(out, "  {}", style("endpoints").bold()).ok();
    for e in &view.endpoints {
        let marker = if e.active {
            style("●").green().to_string()
        } else if e.failover {
            style("◐").yellow().to_string()
        } else {
            " ".to_string()
        };
        writeln!(out, "  {marker} {:<16} {}", e.name, e.url).ok();
        for (k, v) in &e.headers {
            writeln!(out, "      {}", ui::dim(&format!("{k}: {v}"))).ok();
        }
    }
    out
}
