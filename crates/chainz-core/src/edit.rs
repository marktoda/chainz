//! Comment- and order-preserving edits to the `toml_edit` documents.
//!
//! Every config mutation goes through these functions inside
//! [`crate::store::Chainz::transact`], which validates the result before
//! anything is written. They change only the keys they name, so a hand
//! edited file keeps its comments, ordering and formatting.

use crate::model::{Endpoint, Explorer, Signer};
use crate::secrets::SignerSecret;
use anyhow::{Result, anyhow, bail};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TableLike, Value, value};

/// A chain to insert.
#[derive(Debug, Clone)]
pub struct NewChain {
    /// Chain ID.
    pub id: u64,
    /// Aliases.
    pub aliases: Vec<String>,
    /// Signer name.
    pub signer: Option<String>,
    /// Explorer name.
    pub explorer: Option<String>,
    /// Active endpoint name.
    pub rpc: String,
    /// Failover endpoint names.
    pub failover: Vec<String>,
    /// Endpoints in order.
    pub rpcs: Vec<(String, Endpoint)>,
}

fn implicit_table<'a>(doc: &'a mut DocumentMut, key: &str) -> Result<&'a mut Table> {
    let root = doc.as_table_mut();
    if !root.contains_key(key) {
        let mut table = Table::new();
        table.set_implicit(true);
        root.insert(key, Item::Table(table));
    }
    root[key]
        .as_table_mut()
        .ok_or_else(|| anyhow!("`{key}` must be a table"))
}

fn chain_table<'a>(doc: &'a mut DocumentMut, chain: &str) -> Result<&'a mut dyn TableLike> {
    doc.get_mut("chains")
        .and_then(|c| c.get_mut(chain))
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| anyhow!("chain '{chain}' not found"))
}

fn rpcs_table<'a>(doc: &'a mut DocumentMut, chain: &str) -> Result<&'a mut dyn TableLike> {
    let table = chain_table(doc, chain)?;
    if !table.contains_key("rpcs") {
        table.insert("rpcs", Item::Table(Table::new()));
    }
    table
        .get_mut("rpcs")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| anyhow!("chain '{chain}' rpcs must be a table"))
}

fn string_array(items: &[String]) -> Value {
    Value::Array(items.iter().map(String::as_str).collect::<Array>())
}

/// The TOML value of an endpoint: a string, or `{ url, headers }`.
pub fn endpoint_value(endpoint: &Endpoint) -> Value {
    if endpoint.headers.is_empty() {
        return Value::from(endpoint.url.as_str());
    }
    let mut headers = InlineTable::new();
    for (name, v) in &endpoint.headers {
        headers.insert(name, Value::from(v.as_str()));
    }
    let mut table = InlineTable::new();
    table.insert("url", Value::from(endpoint.url.as_str()));
    table.insert("headers", Value::InlineTable(headers));
    Value::InlineTable(table)
}

/// Set or clear `default`.
pub fn set_default(doc: &mut DocumentMut, chain: Option<&str>) {
    match chain {
        Some(chain) => doc["default"] = value(chain),
        None => {
            doc.remove("default");
        }
    }
}

/// Insert a new `[chains.NAME]` table (with `[chains.NAME.rpcs]`).
pub fn insert_chain(doc: &mut DocumentMut, name: &str, chain: &NewChain) -> Result<()> {
    let chains = implicit_table(doc, "chains")?;
    if chains.contains_key(name) {
        bail!("chain '{name}' already exists");
    }
    let mut t = Table::new();
    t.insert(
        "id",
        value(i64::try_from(chain.id).map_err(|_| anyhow!("chain ID too large"))?),
    );
    if !chain.aliases.is_empty() {
        t.insert("aliases", Item::Value(string_array(&chain.aliases)));
    }
    if let Some(signer) = &chain.signer {
        t.insert("signer", value(signer.as_str()));
    }
    if let Some(explorer) = &chain.explorer {
        t.insert("explorer", value(explorer.as_str()));
    }
    t.insert("rpc", value(chain.rpc.as_str()));
    if !chain.failover.is_empty() {
        t.insert("failover", Item::Value(string_array(&chain.failover)));
    }
    let mut rpcs = Table::new();
    for (ep_name, endpoint) in &chain.rpcs {
        rpcs.insert(ep_name, Item::Value(endpoint_value(endpoint)));
    }
    t.insert("rpcs", Item::Table(rpcs));
    chains.insert(name, Item::Table(t));
    Ok(())
}

/// Remove a chain, its default status, and its per-chain label entries.
pub fn remove_chain(doc: &mut DocumentMut, name: &str) -> Result<()> {
    let removed = doc
        .get_mut("chains")
        .and_then(Item::as_table_like_mut)
        .and_then(|c| c.remove(name));
    if removed.is_none() {
        bail!("chain '{name}' not found");
    }
    if doc.get("default").and_then(Item::as_str) == Some(name) {
        doc.remove("default");
    }
    if let Some(addresses) = doc.get_mut("addresses").and_then(Item::as_table_like_mut) {
        let labels: Vec<String> = addresses.iter().map(|(k, _)| k.to_string()).collect();
        for label in labels {
            if let Some(entry) = addresses.get_mut(&label).and_then(Item::as_table_like_mut) {
                entry.remove(name);
                if entry.is_empty() {
                    addresses.remove(&label);
                }
            }
        }
    }
    Ok(())
}

/// Rename a chain key in place, updating `default` and label scopes.
pub fn rename_chain(doc: &mut DocumentMut, old: &str, new: &str) -> Result<()> {
    rename_key(
        doc.get_mut("chains").and_then(Item::as_table_like_mut),
        old,
        new,
    )
    .map_err(|_| anyhow!("chain '{old}' not found"))?;
    if doc.get("default").and_then(Item::as_str) == Some(old) {
        doc["default"] = value(new);
    }
    if let Some(addresses) = doc.get_mut("addresses").and_then(Item::as_table_like_mut) {
        for (_, entry) in addresses.iter_mut() {
            if let Some(entry) = entry.as_table_like_mut()
                && entry.contains_key(old)
            {
                let _ = rename_key(Some(entry), old, new);
            }
        }
    }
    Ok(())
}

/// Rename `old` to `new` in a table, keeping its position.
fn rename_key(table: Option<&mut dyn TableLike>, old: &str, new: &str) -> Result<()> {
    let table = table.ok_or_else(|| anyhow!("missing table"))?;
    if !table.contains_key(old) {
        bail!("missing key");
    }
    if table.contains_key(new) {
        bail!("'{new}' already exists");
    }
    let entries: Vec<(String, Item)> = table
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    table.clear();
    for (k, v) in entries {
        table.insert(if k == old { new } else { &k }, v);
    }
    Ok(())
}

/// Replace a chain's aliases.
pub fn set_aliases(doc: &mut DocumentMut, chain: &str, aliases: &[String]) -> Result<()> {
    let table = chain_table(doc, chain)?;
    if aliases.is_empty() {
        table.remove("aliases");
    } else {
        table.insert("aliases", Item::Value(string_array(aliases)));
    }
    Ok(())
}

/// Set or clear a chain's string field (`signer`, `explorer`, `rpc`).
pub fn set_chain_field(
    doc: &mut DocumentMut,
    chain: &str,
    key: &str,
    v: Option<&str>,
) -> Result<()> {
    let table = chain_table(doc, chain)?;
    match v {
        Some(v) => {
            table.insert(key, value(v));
        }
        None => {
            table.remove(key);
        }
    }
    Ok(())
}

/// Make `endpoint` the active endpoint of `chain`, dropping it from failover.
pub fn set_active(doc: &mut DocumentMut, chain: &str, endpoint: &str) -> Result<()> {
    set_chain_field(doc, chain, "rpc", Some(endpoint))?;
    let failover = failover_of(doc, chain)?;
    if failover.iter().any(|f| f == endpoint) {
        let kept: Vec<String> = failover.into_iter().filter(|f| f != endpoint).collect();
        set_failover(doc, chain, &kept)?;
    }
    Ok(())
}

fn failover_of(doc: &mut DocumentMut, chain: &str) -> Result<Vec<String>> {
    Ok(chain_table(doc, chain)?
        .get("failover")
        .and_then(Item::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default())
}

/// Replace a chain's failover list.
pub fn set_failover(doc: &mut DocumentMut, chain: &str, failover: &[String]) -> Result<()> {
    let table = chain_table(doc, chain)?;
    if failover.is_empty() {
        table.remove("failover");
    } else {
        table.insert("failover", Item::Value(string_array(failover)));
    }
    Ok(())
}

/// Add an endpoint (optionally appending it to failover).
pub fn add_endpoint(
    doc: &mut DocumentMut,
    chain: &str,
    name: &str,
    endpoint: &Endpoint,
    failover: bool,
) -> Result<()> {
    let rpcs = rpcs_table(doc, chain)?;
    if rpcs.contains_key(name) {
        bail!("chain '{chain}' already has an endpoint named '{name}'");
    }
    rpcs.insert(name, Item::Value(endpoint_value(endpoint)));
    if failover {
        let mut list = failover_of(doc, chain)?;
        list.push(name.to_string());
        set_failover(doc, chain, &list)?;
    }
    Ok(())
}

/// Replace an existing endpoint's URL and headers.
pub fn set_endpoint(
    doc: &mut DocumentMut,
    chain: &str,
    name: &str,
    endpoint: &Endpoint,
) -> Result<()> {
    let rpcs = rpcs_table(doc, chain)?;
    let slot = rpcs
        .get_mut(name)
        .ok_or_else(|| anyhow!("chain '{chain}' has no endpoint '{name}'"))?;
    *slot = Item::Value(endpoint_value(endpoint));
    Ok(())
}

/// Remove an endpoint and its failover entry.
pub fn remove_endpoint(doc: &mut DocumentMut, chain: &str, name: &str) -> Result<()> {
    if rpcs_table(doc, chain)?.remove(name).is_none() {
        bail!("chain '{chain}' has no endpoint '{name}'");
    }
    let kept: Vec<String> = failover_of(doc, chain)?
        .into_iter()
        .filter(|f| f != name)
        .collect();
    set_failover(doc, chain, &kept)
}

/// Write `[signers.NAME]`, replacing any existing record.
pub fn set_signer(doc: &mut DocumentMut, name: &str, signer: &Signer) -> Result<()> {
    let signers = implicit_table(doc, "signers")?;
    let mut t = Table::new();
    t.insert("backend", value(signer.backend.as_str()));
    if let Some(address) = signer.address {
        t.insert("address", value(address.to_string()));
    }
    for (k, v) in [
        ("service", &signer.service),
        ("username", &signer.username),
        ("path", &signer.path),
        ("vault", &signer.vault),
        ("item", &signer.item),
    ] {
        if let Some(v) = v {
            t.insert(k, value(v.as_str()));
        }
    }
    match signers.get_mut(name).and_then(Item::as_table_mut) {
        // Keep the existing table's position and decor.
        Some(existing) => {
            Table::clear(existing);
            for (k, v) in t.iter() {
                existing.insert(k, v.clone());
            }
        }
        None => {
            signers.insert(name, Item::Table(t));
        }
    }
    Ok(())
}

/// Remove `[signers.NAME]`, detaching it from chains when `detach`.
pub fn remove_signer(doc: &mut DocumentMut, name: &str, detach: bool) -> Result<usize> {
    let removed = doc
        .get_mut("signers")
        .and_then(Item::as_table_like_mut)
        .and_then(|s| s.remove(name));
    if removed.is_none() {
        bail!("signer '{name}' not found");
    }
    let mut detached = 0;
    if detach && let Some(chains) = doc.get_mut("chains").and_then(Item::as_table_like_mut) {
        for (_, chain) in chains.iter_mut() {
            if let Some(chain) = chain.as_table_like_mut()
                && chain.get("signer").and_then(Item::as_str) == Some(name)
            {
                chain.remove("signer");
                detached += 1;
            }
        }
    }
    Ok(detached)
}

/// Write `[explorers.NAME]`.
pub fn set_explorer(doc: &mut DocumentMut, name: &str, explorer: &Explorer) -> Result<()> {
    let explorers = implicit_table(doc, "explorers")?;
    let mut t = Table::new();
    t.insert("api", value(explorer.api.as_str()));
    if let Some(key) = &explorer.key {
        t.insert("key", value(key.as_str()));
    }
    if let Some(url) = &explorer.url {
        t.insert("url", value(url.as_str()));
    }
    explorers.insert(name, Item::Table(t));
    Ok(())
}

/// Set `[explorers.NAME].key`.
pub fn set_explorer_key(doc: &mut DocumentMut, name: &str, key: &str) -> Result<()> {
    let table = doc
        .get_mut("explorers")
        .and_then(|e| e.get_mut(name))
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| anyhow!("explorer '{name}' not found"))?;
    table.insert("key", value(key));
    Ok(())
}

/// Set a variable in `secrets.toml`.
pub fn set_var(secrets: &mut DocumentMut, name: &str, v: &str) -> Result<()> {
    if !crate::template::is_valid_name(name) {
        bail!("variable name '{name}' must match [A-Za-z_][A-Za-z0-9_]*");
    }
    if crate::secrets::BUILTIN_VARS.contains(&name) {
        bail!("'{name}' is a built-in and cannot be set");
    }
    let vars = implicit_table(secrets, "vars")?;
    vars.set_implicit(false);
    vars.insert(name, value(v));
    Ok(())
}

/// Remove a variable; returns whether it existed.
pub fn remove_var(secrets: &mut DocumentMut, name: &str) -> bool {
    secrets
        .get_mut("vars")
        .and_then(Item::as_table_like_mut)
        .and_then(|v| v.remove(name))
        .is_some()
}

/// Write `[signers.NAME]` key material in `secrets.toml`.
pub fn set_signer_secret(
    secrets: &mut DocumentMut,
    name: &str,
    secret: &SignerSecret,
) -> Result<()> {
    let signers = implicit_table(secrets, "signers")?;
    let mut t = Table::new();
    for (k, v) in [
        ("private_key", &secret.private_key),
        ("ciphertext", &secret.ciphertext),
        ("nonce", &secret.nonce),
        ("salt", &secret.salt),
    ] {
        if let Some(v) = v {
            t.insert(k, value(v.as_str()));
        }
    }
    if let Some(v) = secret.version {
        t.insert("version", value(i64::from(v)));
    }
    for (k, v) in [
        ("kdf_memory_kib", secret.kdf_memory_kib),
        ("kdf_iterations", secret.kdf_iterations),
        ("kdf_parallelism", secret.kdf_parallelism),
    ] {
        if let Some(v) = v {
            t.insert(k, value(i64::from(v)));
        }
    }
    signers.insert(name, Item::Table(t));
    Ok(())
}

/// Remove a signer's key material from `secrets.toml`.
pub fn remove_signer_secret(secrets: &mut DocumentMut, name: &str) -> bool {
    secrets
        .get_mut("signers")
        .and_then(Item::as_table_like_mut)
        .and_then(|s| s.remove(name))
        .is_some()
}

/// One chain's tables as a standalone document, for `chain edit`.
pub fn chain_snippet(doc: &DocumentMut, chain: &str) -> Result<String> {
    let item = doc
        .get("chains")
        .and_then(|c| c.get(chain))
        .ok_or_else(|| anyhow!("chain '{chain}' not found"))?
        .clone();
    let mut out = DocumentMut::new();
    let mut chains = Table::new();
    chains.set_implicit(true);
    chains.insert(chain, item);
    out.insert("chains", Item::Table(chains));
    Ok(out.to_string())
}

/// Replace a chain's tables with an edited snippet from [`chain_snippet`].
pub fn replace_chain_from_snippet(doc: &mut DocumentMut, chain: &str, snippet: &str) -> Result<()> {
    let edited: DocumentMut = snippet
        .parse()
        .map_err(|e: toml_edit::TomlError| anyhow!("{}", e.message()))?;
    let keys: Vec<&str> = edited.iter().map(|(k, _)| k).collect();
    if keys != ["chains"] {
        bail!("the edited text must contain only [chains.{chain}]");
    }
    let chains = edited["chains"]
        .as_table_like()
        .ok_or_else(|| anyhow!("`chains` must be a table"))?;
    let names: Vec<&str> = chains.iter().map(|(k, _)| k).collect();
    if names != [chain] {
        bail!(
            "the edited text must define exactly one chain, '{chain}' (rename with `chainz chain rename`)"
        );
    }
    let item = chains.get(chain).cloned().expect("checked above");
    let slot = doc
        .get_mut("chains")
        .and_then(|c| c.get_mut(chain))
        .ok_or_else(|| anyhow!("chain '{chain}' not found"))?;
    *slot = item;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    const DOC: &str = "# top comment\nversion = 2\ndefault = \"eth\"\n\n# eth is my main chain\n[chains.eth]\nid = 1 # mainnet\nrpc = \"a\"\nfailover = [\"b\"]\n[chains.eth.rpcs]\na = \"https://a.example\" # keep me\nb = \"https://b.example\"\n\n[addresses.router]\neth = \"0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf\"\n";

    #[test]
    fn edits_preserve_comments_and_order() {
        let mut doc: DocumentMut = DOC.parse().unwrap();
        set_active(&mut doc, "eth", "b").unwrap();
        let text = doc.to_string();
        assert!(text.contains("# top comment") && text.contains("# eth is my main chain"));
        assert!(text.contains("# keep me") && text.contains("# mainnet"));
        assert!(text.contains("rpc = \"b\""));
        assert!(!text.contains("failover"), "{text}");
    }

    #[test]
    fn insert_rename_remove_chain() {
        let mut doc: DocumentMut = DOC.parse().unwrap();
        let mut headers = IndexMap::new();
        headers.insert("x-secret".to_string(), "${GW}".to_string());
        insert_chain(
            &mut doc,
            "base",
            &NewChain {
                id: 8453,
                aliases: vec!["Base".into()],
                signer: None,
                explorer: None,
                rpc: "gw".into(),
                failover: vec![],
                rpcs: vec![(
                    "gw".into(),
                    Endpoint {
                        url: "https://gw.example".into(),
                        headers,
                    },
                )],
            },
        )
        .unwrap();
        let text = doc.to_string();
        assert!(text.contains("[chains.base]"), "{text}");
        assert!(
            text.contains(
                "gw = { url = \"https://gw.example\", headers = { x-secret = \"${GW}\" } }"
            ),
            "{text}"
        );

        rename_chain(&mut doc, "eth", "mainnet").unwrap();
        let text = doc.to_string();
        assert!(
            text.contains("[chains.mainnet]") && text.contains("default = \"mainnet\""),
            "{text}"
        );
        assert!(text.contains("mainnet = \"0x7E5F"), "{text}");
        assert!(
            text.find("[chains.mainnet]") < text.find("[chains.base]"),
            "{text}"
        );

        remove_chain(&mut doc, "mainnet").unwrap();
        let text = doc.to_string();
        assert!(
            !text.contains("default") && !text.contains("router"),
            "{text}"
        );
    }

    #[test]
    fn chain_snippet_round_trips() {
        let mut doc: DocumentMut = DOC.parse().unwrap();
        let snippet = chain_snippet(&doc, "eth").unwrap();
        assert!(
            snippet.contains("[chains.eth]") && snippet.contains("# keep me"),
            "{snippet}"
        );
        let edited = snippet.replace("id = 1", "id = 1\naliases = [\"ethereum\"]");
        replace_chain_from_snippet(&mut doc, "eth", &edited).unwrap();
        assert!(doc.to_string().contains("aliases = [\"ethereum\"]"));
        assert!(replace_chain_from_snippet(&mut doc, "eth", "[chains.other]\nid = 2\n").is_err());
    }
}
