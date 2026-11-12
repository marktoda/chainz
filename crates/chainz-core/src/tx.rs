//! Typed, comment-preserving writes to `config.toml` and `secrets.toml`.
//!
//! A [`Tx`] holds both documents for one [`Chainz::transact`]. Each edit is
//! a method that picks its own document, so a caller can never write a
//! secret into `config.toml`. Records (chains, endpoints, explorers,
//! signers and their key material) are written by serializing the model
//! types and merging the result into the existing table: keys whose value
//! did not change are not touched, changed values are replaced in place
//! (keeping their comments), and new keys are appended. A hand-edited file
//! therefore keeps its comments, ordering and formatting.
//!
//! [`Chainz::transact`]: crate::store::Chainz::transact

use crate::address::Address;
use crate::extract::{self, Moved, VarTable};
use crate::labels::{ALL_CHAINS, scope_phrase};
use crate::model::{Chain, Config, Endpoint, Explorer, Signer};
use crate::names;
use crate::redact;
use crate::secrets::{Secrets, SignerSecret};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::path::Path;
use toml_edit::{DocumentMut, InlineTable, Item, Key, Table, TableLike, Value, value};

mod chain_edit;
pub use chain_edit::{Attach, Attached, ChainEdit, EditReport, RpcChoice};

/// The documents being edited inside [`Chainz::transact`](crate::store::Chainz::transact).
pub struct Tx {
    config: DocumentMut,
    secrets: DocumentMut,
}

/// What [`Tx::set_address`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetAddress {
    /// A new entry was written.
    Added,
    /// The entry already held this address; nothing changed.
    Unchanged,
    /// An existing entry was overwritten (only with [`Overwrite::Replace`]).
    Replaced(Address),
}

/// What [`Tx::set_address`] does with an entry that holds another address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overwrite {
    /// Refuse: the user removes the old entry first (`addr add`).
    Refuse,
    /// Replace it (`addr import`, which re-imports newer deployments).
    Replace,
}

/// The role of an endpoint added with [`Tx::add_endpoint_url`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointRole {
    /// Neither active nor in failover.
    Spare,
    /// Appended to the failover list.
    Failover,
    /// Made the active endpoint.
    Active,
}

/// What [`Tx::remove_signer`] does with chains that use the signer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Users {
    /// Leave them: validation then refuses the write if any chain uses it.
    Keep,
    /// Clear their `signer`.
    Unassign,
}

impl Tx {
    /// Parse both documents; errors name the file but never quote it.
    pub(crate) fn parse(
        config: &str,
        config_path: &Path,
        secrets: &str,
        secrets_path: &Path,
    ) -> Result<Self> {
        Ok(Self {
            config: parse_document(config, config_path)?,
            secrets: parse_document(secrets, secrets_path)?,
        })
    }

    /// The edited texts: `config.toml`, `secrets.toml`.
    pub(crate) fn texts(&self) -> (String, String) {
        (self.config.to_string(), self.secrets.to_string())
    }

    /// Put `header` (comment lines) before everything in `secrets.toml`.
    pub(crate) fn set_secrets_header(&mut self, header: &str) {
        self.secrets.decor_mut().set_prefix(header);
    }

    /// Whether `secrets.toml` holds nothing.
    pub(crate) fn secrets_is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// The typed, validated result, or why it may not be written:
    /// what every write ([`Chainz::transact`](crate::store::Chainz::transact),
    /// v1 migration) checks first. `config_path` names the file in errors.
    pub(crate) fn check(&self, config_path: &Path) -> Result<(Config, Secrets)> {
        let (config_text, secrets_text) = self.texts();
        let config = crate::store::files::parse_config(&config_text, config_path)?;
        let secrets = Secrets::parse(&secrets_text).map_err(|e| {
            anyhow!(
                "refusing to write invalid secrets: {}",
                redact::toml_error(&e)
            )
        })?;
        crate::validate::validate(&config, Some(&secrets))
            .context("refusing to write an invalid config")?;
        Ok((config, secrets))
    }

    /// The typed view of the config as edited so far.
    pub fn config(&self) -> Result<Config> {
        Config::parse(&self.config.to_string())
            .map_err(|e| anyhow!("edited config does not parse: {}", redact::toml_error(&e)))
    }

    /// The typed view of the secrets as edited so far.
    pub(crate) fn secrets(&self) -> Result<Secrets> {
        Secrets::parse(&self.secrets.to_string())
            .map_err(|e| anyhow!("edited secrets do not parse: {}", redact::toml_error(&e)))
    }

    // ------------------------------------------------------------ chains

    /// Set or clear `default`.
    pub fn set_default(&mut self, chain: Option<&str>) {
        match chain {
            Some(chain) => self.config["default"] = value(chain),
            None => {
                self.config.remove("default");
            }
        }
    }

    /// [`insert_chain`](Self::insert_chain) without the checks, for a
    /// migration that adds signers after the chains naming them (the
    /// result is validated as a whole).
    pub(crate) fn put_chain(&mut self, name: &str, chain: &Chain) -> Result<()> {
        let chains = implicit_table(&mut self.config, "chains")?;
        put_record(chains, name, None, chain, &["rpcs"])
    }

    /// Change chain `name` through its typed record: `edit` mutates a copy,
    /// and only the keys it changed are rewritten.
    pub(crate) fn update_chain<T>(
        &mut self,
        name: &str,
        edit: impl FnOnce(&mut Chain) -> Result<T>,
    ) -> Result<T> {
        let config = self.config()?;
        let old = config
            .chains
            .get(name)
            .ok_or_else(|| anyhow!("chain '{name}' not found"))?;
        let mut new = old.clone();
        let out = edit(&mut new)?;
        let chains = self
            .config
            .get_mut("chains")
            .and_then(Item::as_table_mut)
            .ok_or_else(|| anyhow!("`chains` must be a table"))?;
        put_record(chains, name, Some(old), &new, &["rpcs"])?;
        Ok(out)
    }

    /// Remove a chain, its default status, and its per-chain label entries.
    pub fn remove_chain(&mut self, name: &str) -> Result<()> {
        let doc = &mut self.config;
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
    /// An alias equal to the new name is dropped (the name answers already).
    pub(crate) fn rename_chain(&mut self, old: &str, new: &str) -> Result<()> {
        names::check_chain_name(new)?;
        let doc = &mut self.config;
        let chains = doc.get_mut("chains").and_then(Item::as_table_like_mut);
        if !chains.as_ref().is_some_and(|c| c.contains_key(old)) {
            bail!("chain '{old}' not found");
        }
        rename_key(chains, old, new).map_err(|_| anyhow!("chain '{new}' already exists"))?;
        if doc.get("default").and_then(Item::as_str) == Some(old) {
            doc["default"] = value(new);
        }
        if let Some(addresses) = doc.get_mut("addresses").and_then(Item::as_table_like_mut) {
            for (label, entry) in addresses.iter_mut() {
                if let Some(entry) = entry.as_table_like_mut()
                    && entry.contains_key(old)
                {
                    rename_key(Some(entry), old, new)
                        .map_err(|_| anyhow!("label '{label}' already has an entry for '{new}'"))?;
                }
            }
        }
        self.update_chain(new, |chain| {
            chain.aliases.retain(|a| !a.eq_ignore_ascii_case(new));
            Ok(())
        })
    }

    /// Replace a chain's aliases.
    pub(crate) fn set_aliases(&mut self, chain: &str, aliases: &[String]) -> Result<()> {
        self.update_chain(chain, |c| {
            c.aliases = aliases.to_vec();
            Ok(())
        })
    }

    /// Make `endpoint` the active endpoint of `chain`, dropping it from
    /// failover.
    pub fn set_active(&mut self, chain: &str, endpoint: &str) -> Result<()> {
        self.update_chain(chain, |c| {
            if !c.rpcs.contains_key(endpoint) {
                bail!("chain '{chain}' has no endpoint '{endpoint}'");
            }
            activate(c, endpoint);
            Ok(())
        })
    }

    /// Replace a chain's failover list.
    pub fn set_failover(&mut self, chain: &str, failover: &[String]) -> Result<()> {
        self.update_chain(chain, |c| {
            c.failover = failover.to_vec();
            Ok(())
        })
    }

    /// Add an endpoint in `role`.
    pub(crate) fn add_endpoint(
        &mut self,
        chain: &str,
        name: &str,
        endpoint: &Endpoint,
        role: EndpointRole,
    ) -> Result<()> {
        self.update_chain(chain, |c| {
            if c.rpcs.contains_key(name) {
                bail!("chain '{chain}' already has an endpoint named '{name}'");
            }
            c.rpcs.insert(name.to_string(), endpoint.clone());
            match role {
                EndpointRole::Spare => {}
                EndpointRole::Failover => c.failover.push(name.to_string()),
                EndpointRole::Active => activate(c, name),
            }
            Ok(())
        })
    }

    /// Add an endpoint from a user's URL to chain `chain`: the one way
    /// `rpc add`, `rpc discover`, `chain add` and `chain set --rpc URL` do
    /// it. The URL and name are checked by [`Chain::new_endpoint`] (which
    /// also names it when `name` is `None`, and refuses a URL the chain
    /// has, by [`Endpoint::same_url`] over `secrets.toml`'s variables), and
    /// literal secrets in the URL and headers move into variables. Returns
    /// the endpoint's name and what moved.
    pub fn add_endpoint_url(
        &mut self,
        chain: &str,
        name: Option<&str>,
        endpoint: &Endpoint,
        role: EndpointRole,
    ) -> Result<(String, Vec<Moved>)> {
        let secrets = self.secrets()?;
        let name = self
            .config()?
            .chains
            .get(chain)
            .ok_or_else(|| anyhow!("chain '{chain}' not found"))?
            .new_endpoint(chain, &endpoint.url, name, &secrets.vars)?;
        let mut endpoints = [(name.clone(), endpoint.clone())];
        let moved = self.extract_secrets(chain, &mut endpoints)?;
        let [(_, endpoint)] = &endpoints;
        self.add_endpoint(chain, &name, endpoint, role)?;
        Ok((name, moved))
    }

    /// Replace an existing endpoint's URL and headers.
    pub(crate) fn set_endpoint(
        &mut self,
        chain: &str,
        name: &str,
        endpoint: &Endpoint,
    ) -> Result<()> {
        self.update_chain(chain, |c| {
            let slot = c
                .rpcs
                .get_mut(name)
                .ok_or_else(|| anyhow!("chain '{chain}' has no endpoint '{name}'"))?;
            *slot = endpoint.clone();
            Ok(())
        })
    }

    /// Remove an endpoint and its failover entry.
    pub fn remove_endpoint(&mut self, chain: &str, name: &str) -> Result<()> {
        self.update_chain(chain, |c| {
            if c.rpcs.shift_remove(name).is_none() {
                bail!("chain '{chain}' has no endpoint '{name}'");
            }
            c.failover.retain(|f| f != name);
            Ok(())
        })
    }

    /// One chain's tables as a standalone document, for editing by hand.
    pub(crate) fn chain_snippet(&self, chain: &str) -> Result<String> {
        let item = self
            .config
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

    /// Replace a chain's tables with an edited [`chain_snippet`](Self::chain_snippet).
    pub(crate) fn replace_chain_snippet(&mut self, chain: &str, snippet: &str) -> Result<()> {
        let edited = parse_document(snippet, Path::new("the edited text"))?;
        let keys: Vec<&str> = edited.iter().map(|(k, _)| k).collect();
        if keys != ["chains"] {
            bail!("the edited text must contain only [chains.{chain}]");
        }
        let Some((name, item)) = edited["chains"]
            .as_table_like()
            .filter(|chains| chains.len() == 1)
            .and_then(|chains| chains.iter().next())
            .filter(|(name, _)| *name == chain)
        else {
            bail!(
                "the edited text must define exactly one chain, '{chain}' (rename with `chainz chain set --name`)"
            );
        };
        let slot = self
            .config
            .get_mut("chains")
            .and_then(|c| c.get_mut(name))
            .ok_or_else(|| anyhow!("chain '{chain}' not found"))?;
        *slot = item.clone();
        Ok(())
    }

    /// Replace the whole of `config.toml` with `text`.
    pub(crate) fn replace_config_text(&mut self, text: &str) -> Result<()> {
        self.config = parse_document(text, Path::new("config.toml"))?;
        Ok(())
    }

    // ----------------------------------------------------------- signers

    /// Write `[signers.NAME]`, merging into an existing record.
    pub(crate) fn set_signer(&mut self, name: &str, signer: &Signer) -> Result<()> {
        let config = self.config()?;
        let old = config.signers.get(name);
        let signers = implicit_table(&mut self.config, "signers")?;
        put_record(signers, name, old, signer, &[])
    }

    /// Remove `[signers.NAME]` and its key material in `secrets.toml`.
    /// Returns how many chains it was unassigned from.
    pub fn remove_signer(&mut self, name: &str, users: Users) -> Result<usize> {
        let removed = self
            .config
            .get_mut("signers")
            .and_then(Item::as_table_like_mut)
            .and_then(|s| s.remove(name));
        if removed.is_none() {
            bail!("signer '{name}' not found");
        }
        self.set_signer_secret(name, None)?;
        let mut unassigned = 0;
        if users == Users::Unassign {
            let config = self.config()?;
            let chains: Vec<String> = config
                .chains_using_signer(name)
                .into_iter()
                .map(String::from)
                .collect();
            for chain in &chains {
                self.update_chain(chain, |c| {
                    c.signer = None;
                    Ok(())
                })?;
                unassigned += 1;
            }
        }
        Ok(unassigned)
    }

    /// Write (or, with `None`, remove) `[signers.NAME]` key material in
    /// `secrets.toml`.
    pub(crate) fn set_signer_secret(
        &mut self,
        name: &str,
        secret: Option<&SignerSecret>,
    ) -> Result<()> {
        let Some(secret) = secret else {
            if let Some(signers) = self
                .secrets
                .get_mut("signers")
                .and_then(Item::as_table_like_mut)
            {
                signers.remove(name);
            }
            return Ok(());
        };
        let secrets = self.secrets()?;
        let old = secrets.signers.get(name);
        let signers = implicit_table(&mut self.secrets, "signers")?;
        put_record(signers, name, old, secret, &[])
    }

    // --------------------------------------------------------- explorers

    /// Write `[explorers.NAME]`, merging into an existing record.
    pub(crate) fn set_explorer(&mut self, name: &str, explorer: &Explorer) -> Result<()> {
        let config = self.config()?;
        let old = config.explorers.get(name);
        let explorers = implicit_table(&mut self.config, "explorers")?;
        put_record(explorers, name, old, explorer, &[])
    }

    /// Set `[explorers.NAME].key`.
    pub(crate) fn set_explorer_key(&mut self, name: &str, key: &str) -> Result<()> {
        let mut explorer = self
            .config()?
            .explorers
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("explorer '{name}' not found"))?;
        explorer.key = Some(key.to_string());
        self.set_explorer(name, &explorer)
    }

    // ------------------------------------------------------------ labels

    /// Write `[addresses.NAME]` `scope = "0x…"` (checksummed). `scope` is a
    /// configured chain's name or [`ALL_CHAINS`]. The name is lowercased
    /// and must be a valid, unreserved label name.
    pub fn set_address(
        &mut self,
        name: &str,
        scope: &str,
        address: &Address,
        overwrite: Overwrite,
    ) -> Result<SetAddress> {
        let name = name.to_ascii_lowercase();
        let config = self.config()?;
        names::check_label_name(&name, config.signers.keys().map(String::as_str))?;
        if scope != ALL_CHAINS && !config.chains.contains_key(scope) {
            bail!("chain '{scope}' not found");
        }
        let previous = config
            .addresses
            .get(&name)
            .and_then(|entry| entry.get(scope))
            .map(|stored| stored.parse::<Address>().ok());
        let outcome = match previous {
            None => SetAddress::Added,
            // Rewritten below all the same, which normalizes it to the checksum.
            Some(Some(old)) if old == *address => SetAddress::Unchanged,
            Some(old) if overwrite == Overwrite::Refuse => {
                let on = scope_phrase(scope);
                let held = old.map_or_else(|| "an invalid value".to_string(), |a| a.to_checksum());
                bail!(
                    "@{name} is already {held} {on}; to change it, first run `chainz addr rm {name} --chain '{scope}'`"
                );
            }
            Some(old) => old.map_or(SetAddress::Added, SetAddress::Replaced),
        };
        let addresses = implicit_table(&mut self.config, "addresses")?;
        if !addresses.contains_key(&name) {
            addresses.insert(&name, Item::Table(Table::new()));
        }
        let entry = addresses
            .get_mut(&name)
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| anyhow!("`addresses.{name}` must be a table"))?;
        put_value(entry, scope, Value::from(address.to_checksum()));
        Ok(outcome)
    }

    /// Apply a label write an import planned (it already decided the
    /// write may replace what the label held).
    pub fn write_address(&mut self, write: &crate::broadcast::AddressWrite) -> Result<SetAddress> {
        self.set_address(
            &write.name,
            &write.chain,
            &write.address,
            Overwrite::Replace,
        )
    }

    /// Remove a label: one entry (`scope` is a chain name or
    /// [`ALL_CHAINS`]) or, with `None`, the whole label. Returns the
    /// removed scopes with their stored addresses, in file order.
    pub fn remove_address(
        &mut self,
        name: &str,
        scope: Option<&str>,
    ) -> Result<Vec<(String, String)>> {
        let name = name.to_ascii_lowercase();
        let config = self.config()?;
        let stored = config
            .addresses
            .get(&name)
            .ok_or_else(|| anyhow!("label '{name}' not found"))?;
        let removed: Vec<(String, String)> = match scope {
            None => stored.iter().map(|(s, a)| (s.clone(), a.clone())).collect(),
            Some(scope) => match stored.get(scope) {
                Some(address) => vec![(scope.to_string(), address.clone())],
                None => bail!("label '{name}' has no address {}", scope_phrase(scope)),
            },
        };
        let addresses = self
            .config
            .get_mut("addresses")
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| anyhow!("label '{name}' not found"))?;
        let emptied = match scope {
            None => true,
            Some(scope) => {
                let entry = addresses
                    .get_mut(&name)
                    .and_then(Item::as_table_like_mut)
                    .ok_or_else(|| anyhow!("`addresses.{name}` must be a table"))?;
                entry.remove(scope);
                entry.is_empty()
            }
        };
        if emptied {
            addresses.remove(&name);
        }
        Ok(removed)
    }

    // -------------------------------------------------------- variables

    /// Set a variable in `secrets.toml`.
    pub fn set_var(&mut self, name: &str, v: &str) -> Result<()> {
        crate::secrets::check_var_name(name)?;
        let vars = implicit_table(&mut self.secrets, "vars")?;
        vars.set_implicit(false);
        vars.insert(name, value(v));
        Ok(())
    }

    /// Remove a variable; returns whether it existed.
    pub fn remove_var(&mut self, name: &str) -> bool {
        self.secrets
            .get_mut("vars")
            .and_then(Item::as_table_like_mut)
            .and_then(|v| v.remove(name))
            .is_some()
    }

    /// Write the variables `vars` created (extracted secrets).
    pub(crate) fn put_created_vars(&mut self, vars: &VarTable) -> Result<()> {
        for (name, value) in vars.created_vars() {
            self.set_var(name, value)?;
        }
        Ok(())
    }

    /// Move the literal secrets of `endpoints` (about to be written to
    /// chain `chain`) into `secrets.toml` variables, rewriting the
    /// endpoints to reference them. Returns what moved.
    pub(crate) fn extract_secrets(
        &mut self,
        chain: &str,
        endpoints: &mut [(String, Endpoint)],
    ) -> Result<Vec<Moved>> {
        let mut vars = VarTable::with_vars(&self.secrets()?.vars);
        let mut moved = Vec::new();
        for (name, endpoint) in endpoints.iter_mut() {
            moved.extend(extract::extract_endpoint(chain, name, endpoint, &mut vars));
        }
        self.put_created_vars(&vars)?;
        Ok(moved)
    }

    /// Move every literal secret in the config into variables (see
    /// [`extract::plan_secret_extraction`]). Returns what moved.
    pub fn extract_all_secrets(&mut self) -> Result<Vec<Moved>> {
        let plan = extract::plan_secret_extraction(&self.config()?, &self.secrets()?.vars);
        self.put_created_vars(plan.vars())?;
        for (chain, name, endpoint) in &plan.endpoints {
            self.set_endpoint(chain, name, endpoint)?;
        }
        for (explorer, key) in &plan.explorer_keys {
            self.set_explorer_key(explorer, key)?;
        }
        Ok(plan.moved)
    }
}

/// Make `endpoint` active on `chain`, dropping it from failover.
fn activate(chain: &mut Chain, endpoint: &str) {
    chain.rpc = endpoint.to_string();
    chain.failover.retain(|f| f != endpoint);
}

fn parse_document(text: &str, path: &Path) -> Result<DocumentMut> {
    text.parse().map_err(|e: toml_edit::TomlError| {
        anyhow!(
            "failed to parse {}: {}",
            path.display(),
            redact::parse_error(e.message(), e.span())
        )
    })
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

/// Rename `old` to `new` in a table, keeping its position and every key's
/// formatting and comments.
fn rename_key(table: Option<&mut dyn TableLike>, old: &str, new: &str) -> Result<()> {
    let table = table.ok_or_else(|| anyhow!("missing table"))?;
    if !table.contains_key(old) {
        bail!("missing key");
    }
    if table.contains_key(new) {
        bail!("'{new}' already exists");
    }
    let names: Vec<String> = table.iter().map(|(k, _)| k.to_string()).collect();
    let entries: Vec<(Key, Item)> = names
        .iter()
        .filter_map(|name| table.get_key_value(name))
        .map(|(key, item)| {
            let key = if key.get() == old {
                Key::new(new)
                    .with_leaf_decor(key.leaf_decor().clone())
                    .with_dotted_decor(key.dotted_decor().clone())
            } else {
                key.clone()
            };
            (key, item.clone())
        })
        .collect();
    table.clear();
    for (key, item) in entries {
        table.entry_format(&key).or_insert(item);
    }
    Ok(())
}

/// `record` as an inline table of its serialized fields.
fn fields_of<T: Serialize>(record: &T) -> Result<InlineTable> {
    match record.serialize(toml_edit::ser::ValueSerializer::new()) {
        Ok(Value::InlineTable(table)) => Ok(table),
        Ok(_) => bail!("internal error: a config record is not a table"),
        Err(e) => bail!("internal error: failed to serialize a config record: {e}"),
    }
}

/// Write `new` as `parent[name]`. With `old` (the record as the document
/// holds it now) the existing table is merged: see [`merge`]. Keys listed
/// in `subtables` become `[parent.name.key]` tables when written fresh.
fn put_record<T: Serialize>(
    parent: &mut Table,
    name: &str,
    old: Option<&T>,
    new: &T,
    subtables: &[&str],
) -> Result<()> {
    let new = fields_of(new)?;
    match (old, parent.get_mut(name)) {
        (Some(old), Some(item)) if item.is_table_like() => {
            merge(item, &fields_of(old)?, &new, subtables);
        }
        _ => {
            parent.insert(name, Item::Table(layout(&new, subtables)));
        }
    }
    Ok(())
}

/// A fresh table holding `fields`, with `subtables` as nested tables.
fn layout(fields: &InlineTable, subtables: &[&str]) -> Table {
    let mut table = Table::new();
    for (key, v) in fields.iter() {
        match v {
            Value::InlineTable(sub) if subtables.contains(&key) => {
                table.insert(key, Item::Table(sub.clone().into_table()));
            }
            _ => {
                table.insert(key, Item::Value(bare(v)));
            }
        }
    }
    table
}

/// Merge into `item` (a table holding `old`) the record `new`: keys `new`
/// drops are removed, values that changed are replaced in place, keys
/// that are new are appended, and everything else is left exactly as
/// written.
fn merge(item: &mut Item, old: &InlineTable, new: &InlineTable, subtables: &[&str]) {
    let is_table = item.is_table();
    let Some(existing) = item.as_table_like_mut() else {
        return;
    };
    for (key, _) in old.iter() {
        if !new.contains_key(key) {
            existing.remove(key);
        }
    }
    for (key, new_value) in new.iter() {
        let old_value = old.get(key);
        if old_value.is_some_and(|o| same_value(o, new_value)) {
            continue;
        }
        if let (Some(Value::InlineTable(old_sub)), Value::InlineTable(new_sub)) =
            (old_value, new_value)
            && subtables.contains(&key)
            && let Some(slot) = existing.get_mut(key).filter(|s| s.is_table_like())
        {
            merge(slot, old_sub, new_sub, &[]);
            continue;
        }
        match existing.get_mut(key) {
            Some(Item::Value(slot)) => replace_value(slot, new_value),
            _ => {
                let fresh = match new_value {
                    Value::InlineTable(sub) if is_table && subtables.contains(&key) => {
                        Item::Table(sub.clone().into_table())
                    }
                    _ => Item::Value(bare(new_value)),
                };
                existing.insert(key, fresh);
            }
        }
    }
}

/// Set `key` to `v` in `table`, replacing an existing value in place so
/// its comments survive.
fn put_value(table: &mut dyn TableLike, key: &str, v: Value) {
    match table.get_mut(key) {
        Some(Item::Value(slot)) => replace_value(slot, &v),
        _ => {
            table.insert(key, Item::Value(v));
        }
    }
}

fn replace_value(slot: &mut Value, new: &Value) {
    let decor = slot.decor().clone();
    *slot = new.clone();
    *slot.decor_mut() = decor;
}

/// `v` without the decor it had inside an inline table.
fn bare(v: &Value) -> Value {
    let mut v = v.clone();
    v.decor_mut().clear();
    v
}

/// Whether two values are equal, ignoring formatting.
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.value() == b.value(),
        (Value::Integer(a), Value::Integer(b)) => a.value() == b.value(),
        (Value::Boolean(a), Value::Boolean(b)) => a.value() == b.value(),
        (Value::Float(a), Value::Float(b)) => a.value() == b.value(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(a, b)| same_value(a, b))
        }
        (Value::InlineTable(a), Value::InlineTable(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|((ka, va), (kb, vb))| ka == kb && same_value(va, vb))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
