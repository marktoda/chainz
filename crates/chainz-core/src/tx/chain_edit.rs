//! One patch to one chain: what `chain set` flags (or its menu) fill in,
//! applied by [`Tx::edit_chain`] inside one transaction; and the defaults a
//! new chain or a first signer attach.

use super::{EndpointRole, Tx};
use crate::extract::Moved;
use crate::model::{Chain, Endpoint, Explorer, is_header_name};
use crate::names;
use anyhow::{Result, anyhow, bail};
use zeroize::Zeroizing;

/// A patch to one chain. `None` (or empty) leaves a field alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChainEdit {
    /// `--signer NAME` (`Some(Some)`) or `--no-signer` (`Some(None)`).
    pub signer: Option<Option<String>>,
    /// `--rpc NAME|URL`: an endpoint name, or a URL. An existing name, or a
    /// URL an endpoint already has, activates that endpoint; a new URL is
    /// added as an endpoint (its literal secrets moved into variables) and
    /// activated. Checking a new URL's chain ID is the caller's job.
    pub rpc: Option<String>,
    /// `--header 'name: value'`…: set on the `--rpc` endpoint, else the
    /// active one.
    pub headers: Vec<(String, String)>,
    /// `--clear-headers`: drop that endpoint's headers first.
    pub clear_headers: bool,
    /// `--failover a,b` (`Some(names)`) or `--no-failover` (`Some(vec![])`).
    pub failover: Option<Vec<String>>,
    /// `--explorer NAME` or `--no-explorer`. `etherscan` creates the
    /// built-in [`Explorer::etherscan`] when it is not configured.
    pub explorer: Option<Option<String>>,
    /// `--api-key`: the key for the chain's explorer (after `explorer`
    /// applies), stored (trimmed) in `secrets.toml` and shared by every
    /// chain using that explorer.
    pub api_key: Option<Zeroizing<String>>,
    /// `--name NEW`: rename the chain (applied last).
    pub name: Option<String>,
    /// `--alias A`…: aliases to add.
    pub add_aliases: Vec<String>,
    /// `--rm-alias A`…: aliases to remove.
    pub remove_aliases: Vec<String>,
}

impl ChainEdit {
    /// Whether the patch changes nothing.
    pub fn is_empty(&self) -> bool {
        *self == ChainEdit::default()
    }

    /// What `--rpc` means for `chain` (named `name`): `None` when unset,
    /// an endpoint the chain has, or a new endpoint to add. An error when
    /// it is neither an endpoint nor a URL [`Tx::edit_chain`] would accept.
    /// The one owner of that rule: the edit, the caller's chain-ID probe
    /// and the `chain set` menu all go by it.
    pub fn rpc_choice(&self, name: &str, chain: &Chain) -> Result<Option<RpcChoice>> {
        let Some(rpc) = self.rpc.as_deref() else {
            return Ok(None);
        };
        if chain.rpcs.contains_key(rpc) {
            return Ok(Some(RpcChoice::Existing(rpc.to_string())));
        }
        if let Some(existing) = chain.endpoint_at(rpc) {
            return Ok(Some(RpcChoice::Existing(existing.to_string())));
        }
        if !rpc.contains("://") {
            bail!(
                "chain '{name}' has no endpoint '{rpc}' (endpoints: {})",
                chain
                    .rpcs
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let ep_name = chain.new_endpoint(name, rpc, None)?;
        let mut endpoint = Endpoint::new(rpc);
        for (header, value) in &self.headers {
            let header = header.trim();
            if !is_header_name(header) {
                bail!("invalid header name '{header}'");
            }
            endpoint
                .headers
                .insert(header.to_string(), value.trim().to_string());
        }
        Ok(Some(RpcChoice::New(ep_name, endpoint)))
    }
}

/// What `--rpc` means for a chain ([`ChainEdit::rpc_choice`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcChoice {
    /// An endpoint the chain has, named by its name or its URL (a trailing
    /// `/` ignored): that endpoint's name.
    Existing(String),
    /// A URL to add as a new endpoint: its name, and the endpoint with the
    /// edit's headers.
    New(String, Endpoint),
}

/// What [`Tx::edit_chain`] did, for the caller to report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditReport {
    /// One line per change, in the order applied (`signer: deployer`).
    pub changes: Vec<String>,
    /// The endpoint `--rpc URL` added, if it added one.
    pub added_endpoint: Option<String>,
    /// The explorer `--explorer etherscan` created, if it created one.
    pub created_explorer: Option<String>,
    /// The variable `--api-key` stored the key in.
    pub api_key_var: Option<String>,
    /// Literal secrets moved into variables.
    pub moved: Vec<Moved>,
    /// The chain's name after the edit.
    pub chain: String,
}

/// What [`Tx::insert_chain`] attaches to a new chain that names none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attach {
    /// The only signer, if there is exactly one, and `[explorers.etherscan]`,
    /// if it is configured.
    Defaults,
    /// The built-in explorer only (`--no-signer`).
    ExplorerOnly,
    /// Nothing.
    Nothing,
}

/// What [`Tx::insert_chain`] attached by default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attached {
    /// The signer attached, if any.
    pub signer: Option<String>,
    /// The explorer attached, if any.
    pub explorer: Option<String>,
}

const ETHERSCAN: &str = Explorer::ETHERSCAN;

impl Tx {
    /// Insert a new chain (name, ID and signer checked), first filling in
    /// what `attach` asks for where `chain` names nothing.
    pub fn insert_chain(&mut self, name: &str, chain: &Chain, attach: Attach) -> Result<Attached> {
        let config = self.config()?;
        config.check_new_chain(name, chain.id, chain.signer.as_deref())?;
        let mut chain = chain.clone();
        let mut attached = Attached::default();
        if attach == Attach::Defaults
            && chain.signer.is_none()
            && let [only] = config.signers.keys().collect::<Vec<_>>()[..]
        {
            chain.signer = Some(only.clone());
            attached.signer = Some(only.clone());
        }
        if attach != Attach::Nothing
            && chain.explorer.is_none()
            && config.explorers.contains_key(ETHERSCAN)
        {
            chain.explorer = Some(ETHERSCAN.to_string());
            attached.explorer = Some(ETHERSCAN.to_string());
        }
        self.put_chain(name, &chain)?;
        Ok(attached)
    }

    /// Attach signer `signer` to every chain that has none (a first
    /// signer). Returns those chains, in config order.
    pub fn attach_signer_where_missing(&mut self, signer: &str) -> Result<Vec<String>> {
        let config = self.config()?;
        if !config.signers.contains_key(signer) {
            bail!("signer '{signer}' not found");
        }
        let bare: Vec<String> = config
            .chains
            .iter()
            .filter(|(_, c)| c.signer.is_none())
            .map(|(n, _)| n.clone())
            .collect();
        for name in &bare {
            self.update_chain(name, |c| {
                c.signer = Some(signer.to_string());
                Ok(())
            })?;
        }
        Ok(bare)
    }

    /// Apply `edit` to chain `name`: endpoint and headers first, then
    /// failover, explorer and key, signer, aliases, and the rename last.
    /// Validation runs on commit, as for every transaction.
    pub fn edit_chain(&mut self, name: &str, edit: &ChainEdit) -> Result<EditReport> {
        let config = self.config()?;
        let chain = config
            .chains
            .get(name)
            .ok_or_else(|| anyhow!("chain '{name}' not found"))?
            .clone();
        let mut report = EditReport {
            chain: name.to_string(),
            ..EditReport::default()
        };
        self.edit_endpoint(name, &chain, edit, &mut report)?;

        if let Some(failover) = &edit.failover {
            let chain = &self.config()?.chains[name];
            for f in failover {
                chain.check_failover(name, f)?;
            }
            self.set_failover(name, failover)?;
            report.changes.push(match failover.is_empty() {
                true => "failover: none".to_string(),
                false => format!("failover: {}", failover.join(", ")),
            });
        }

        self.edit_explorer(name, edit, &mut report)?;

        if let Some(signer) = &edit.signer {
            if let Some(signer) = signer {
                config.check_signer(signer)?;
            }
            self.update_chain(name, |c| {
                c.signer = signer.clone();
                Ok(())
            })?;
            report
                .changes
                .push(format!("signer: {}", signer.as_deref().unwrap_or("none")));
        }

        if !edit.add_aliases.is_empty() || !edit.remove_aliases.is_empty() {
            let mut aliases = chain.aliases.clone();
            for alias in &edit.remove_aliases {
                let before = aliases.len();
                aliases.retain(|a| !a.eq_ignore_ascii_case(alias));
                if aliases.len() == before {
                    bail!("chain '{name}' has no alias '{alias}'");
                }
                report.changes.push(format!("alias removed: {alias}"));
            }
            for alias in &edit.add_aliases {
                let alias = alias.trim();
                names::check_alias(alias)?;
                if alias.eq_ignore_ascii_case(name)
                    || aliases.iter().any(|a| a.eq_ignore_ascii_case(alias))
                {
                    continue;
                }
                aliases.push(alias.to_string());
                report.changes.push(format!("alias added: {alias}"));
            }
            self.set_aliases(name, &aliases)?;
        }

        if let Some(new) = &edit.name
            && new != name
        {
            self.rename_chain(name, new)?;
            report.changes.push(format!("renamed: {name} → {new}"));
            report.chain = new.clone();
        }
        Ok(report)
    }

    /// `--rpc` and the header flags.
    fn edit_endpoint(
        &mut self,
        name: &str,
        chain: &Chain,
        edit: &ChainEdit,
        report: &mut EditReport,
    ) -> Result<()> {
        // A new endpoint is added, with the header flags, and activated.
        // Else the header flags apply to the `--rpc` endpoint, or the
        // active one.
        let ep_name = match edit.rpc_choice(name, chain)? {
            None => chain.rpc.clone(),
            Some(RpcChoice::Existing(ep_name)) => ep_name,
            Some(RpcChoice::New(ep_name, endpoint)) => {
                for (header, _) in &endpoint.headers {
                    report
                        .changes
                        .push(format!("header {header} set on {ep_name}"));
                }
                let (ep_name, moved) =
                    self.add_endpoint_url(name, Some(&ep_name), &endpoint, EndpointRole::Active)?;
                report.moved = moved;
                report.added_endpoint = Some(ep_name.clone());
                report.changes.push(format!("active endpoint: {ep_name}"));
                return Ok(());
            }
        };
        let Some(mut endpoint) = chain.rpcs.get(&ep_name).cloned() else {
            bail!("chain '{name}' has no active endpoint '{ep_name}'");
        };
        let mut changed = false;
        if edit.clear_headers && !endpoint.headers.is_empty() {
            endpoint.headers.clear();
            changed = true;
            report.changes.push(format!("headers cleared on {ep_name}"));
        }
        for (header, value) in &edit.headers {
            let header = header.trim();
            if !is_header_name(header) {
                bail!("invalid header name '{header}'");
            }
            endpoint
                .headers
                .insert(header.to_string(), value.trim().to_string());
            changed = true;
            report
                .changes
                .push(format!("header {header} set on {ep_name}"));
        }
        if changed {
            let mut endpoints = [(ep_name.clone(), endpoint)];
            report.moved = self.extract_secrets(name, &mut endpoints)?;
            let [(_, endpoint)] = endpoints;
            self.set_endpoint(name, &ep_name, &endpoint)?;
        }
        if edit.rpc.is_some() && chain.rpc != ep_name {
            self.set_active(name, &ep_name)?;
            report.changes.push(format!("active endpoint: {ep_name}"));
        }
        Ok(())
    }

    /// `--explorer` and `--api-key`.
    fn edit_explorer(
        &mut self,
        name: &str,
        edit: &ChainEdit,
        report: &mut EditReport,
    ) -> Result<()> {
        if let Some(explorer) = &edit.explorer {
            if let Some(explorer) = explorer
                && !self.config()?.explorers.contains_key(explorer)
            {
                if explorer != ETHERSCAN {
                    bail!(
                        "explorer '{explorer}' not found; `{ETHERSCAN}` is built in, others go in [explorers.<name>] (`chainz edit`)"
                    );
                }
                self.set_explorer(ETHERSCAN, &Explorer::etherscan())?;
                report.created_explorer = Some(ETHERSCAN.to_string());
            }
            self.update_chain(name, |c| {
                c.explorer = explorer.clone();
                Ok(())
            })?;
            report.changes.push(format!(
                "explorer: {}",
                explorer.as_deref().unwrap_or("none")
            ));
        }
        if let Some(key) = &edit.api_key {
            let key = key.trim();
            if key.is_empty() {
                bail!("the API key is empty");
            }
            let config = self.config()?;
            let explorer_name = config.chains[name].explorer.clone().ok_or_else(|| {
                anyhow!("chain '{name}' has no explorer to set a key for; pass --explorer")
            })?;
            let mut explorer = config.explorers[&explorer_name].clone();
            let var = crate::extract::explorer_key_var(&explorer_name, &explorer);
            self.set_var(&var, key)?;
            let reference = format!("${{{var}}}");
            if explorer.key.as_deref() != Some(&reference) {
                explorer.key = Some(reference);
                self.set_explorer(&explorer_name, &explorer)?;
            }
            report.changes.push(format!(
                "explorer key: stored in ${{{var}}} (used by every chain on {explorer_name})"
            ));
            report.api_key_var = Some(var);
        }
        Ok(())
    }
}
