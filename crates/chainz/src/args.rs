//! Parsing argument values the handlers share.

use anyhow::{Result, anyhow, bail};
use chainz_core::Address;

/// Parse an address argument; the error names `what` it was for.
pub fn parse_address(input: &str, what: &str) -> Result<Address> {
    input
        .parse()
        .map_err(|e| anyhow!("{what} '{input}' is not an address: {e}"))
}

/// Parse `--header 'name: value'` flags, in order.
pub fn parse_headers(raw: &[String]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(raw.len());
    for h in raw {
        let (name, value) = h
            .split_once(':')
            .ok_or_else(|| anyhow!("header must be 'name: value' (got a value without ':')"))?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() {
            bail!("header name cannot be empty");
        }
        out.push((name, value.trim().to_string()));
    }
    Ok(out)
}
