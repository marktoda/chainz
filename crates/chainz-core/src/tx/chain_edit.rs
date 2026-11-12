//! One patch to one chain: what `update` flags (or its menu) fill in,
//! applied by [`Tx::edit_chain`] inside one transaction; and the defaults a
//! new chain or a first signer attach.

use super::{EndpointRole, Tx};
use crate::extract::Moved;
use crate::model::{Chain, Endpoint, Explorer, RpcChoice, is_header_name};
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
    /// `--add-rpc [NAME=]URL`…: endpoints to add without activating them
    /// (name, URL; see [`ChainEdit::parse_add_rpc`]). Their literal
    /// secrets move into variables; checking their chain ID is the
    /// caller's job ([`ChainEdit::added_endpoints`]).
    pub add_rpcs: Vec<(Option<String>, String)>,
    /// `--rm-rpc NAME`…: endpoints to remove (after `--rpc` applies, so
    /// the active one can be replaced and removed in one edit).
    pub remove_rpcs: Vec<String>,
    /// `--header 'name: value'`…: set on the `--rpc` endpoint, else the one
    /// `--add-rpc` endpoint, else the active one.
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

impl EditReport {
    /// Whether the edit changed nothing (a no-op: every value was already
    /// set).
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
            && self.added_endpoints.is_empty()
            && self.created_explorer.is_none()
            && self.moved.is_empty()
    }
}

impl ChainEdit {
    /// Whether the patch changes nothing.
    pub fn is_empty(&self) -> bool {
        *self == ChainEdit::default()
    }

    /// What `--rpc` means for `chain` (named `name`): `None` when unset,
    /// else [`Chain::rpc_choice`] (URLs compared with `vars`,
    /// `secrets.toml`'s `[vars]`), a new endpoint carrying the edit's
    /// headers. The edit, the caller's chain-ID probe and the `update`
    /// menu all go by it.
    pub fn rpc_choice(
        &self,
        name: &str,
        chain: &Chain,
        vars: &crate::secrets::Vars,
    ) -> Result<Option<RpcChoice>> {
        let Some(rpc) = self.rpc.as_deref() else {
            return Ok(None);
        };
        let (ep_name, mut endpoint) = match chain.rpc_choice(name, rpc, vars)? {
            RpcChoice::New(ep_name, endpoint) => (ep_name, endpoint),
            existing => return Ok(Some(existing)),
        };
        self.put_headers(&mut endpoint)?;
        Ok(Some(RpcChoice::New(ep_name, endpoint)))
    }

    /// `--add-rpc`'s argument: `NAME=URL`, or a bare URL (named from its
    /// host). Only text before the first `=` that holds no `:` or `/` is a
    /// name, so a URL with `=` in its query stays whole.
    pub fn parse_add_rpc(arg: &str) -> (Option<String>, String) {
        match arg.split_once('=') {
            Some((name, url)) if !name.is_empty() && !name.contains([':', '/']) => {
                (Some(name.trim().to_string()), url.trim().to_string())
            }
            _ => (None, arg.trim().to_string()),
        }
    }

    /// Whether the header flags go to an endpoint this edit adds by
    /// `--add-rpc` (there is no `--rpc` and exactly one `--add-rpc`). An
    /// error when they could mean several.
    fn headers_on_added(&self) -> Result<bool> {
        if self.rpc.is_some() || (self.headers.is_empty() && !self.clear_headers) {
            return Ok(false);
        }
        match self.add_rpcs.len() {
            0 => Ok(false),
            1 => Ok(true),
            _ => bail!(
                "--header with several --add-rpc is ambiguous; add the endpoint that needs headers on its own"
            ),
        }
    }

    /// Set the header flags on `endpoint`.
    fn put_headers(&self, endpoint: &mut Endpoint) -> Result<()> {
        for (header, value) in &self.headers {
            let header = header.trim();
            if !is_header_name(header) {
                bail!("invalid header name '{header}'");
            }
            endpoint
                .headers
                .insert(header.to_string(), value.trim().to_string());
        }
        Ok(())
    }

    /// The endpoints this edit adds to `chain` (named `name`), named, with
    /// their role: a new `--rpc` URL (active), then each `--add-rpc`
    /// (spare), the header flags on the one they apply to. URLs are
    /// checked as [`Chain::new_endpoint`] does, against the chain and each
    /// other (compared with `vars`). [`Tx::edit_chain`] adds exactly these;
    /// the caller probes them first.
    pub fn added_endpoints(
        &self,
        name: &str,
        chain: &Chain,
        vars: &crate::secrets::Vars,
    ) -> Result<Vec<(String, Endpoint, EndpointRole)>> {
        let mut out = Vec::new();
        let mut chain = chain.clone();
        if let Some(RpcChoice::New(ep_name, endpoint)) = self.rpc_choice(name, &chain, vars)? {
            chain.rpcs.insert(ep_name.clone(), endpoint.clone());
            out.push((ep_name, endpoint, EndpointRole::Active));
        }
        let headers_here = self.headers_on_added()?;
        for (ep_name, url) in &self.add_rpcs {
            let ep_name = chain.new_endpoint(name, url, ep_name.as_deref(), vars)?;
            let mut endpoint = Endpoint::new(url.as_str());
            if headers_here {
                self.put_headers(&mut endpoint)?;
            }
            chain.rpcs.insert(ep_name.clone(), endpoint.clone());
            out.push((ep_name, endpoint, EndpointRole::Spare));
        }
        Ok(out)
    }
}

/// What [`Tx::edit_chain`] did, for the caller to report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditReport {
    /// One line per change ([`Chain::changes`], then the explorer key and
    /// the rename): `signer: deployer → ops`.
    pub changes: Vec<String>,
    /// The endpoints `--rpc URL` and `--add-rpc` added, in that order.
    pub added_endpoints: Vec<String>,
    /// The explorer `--explorer etherscan` created, if it created one.
    pub created_explorer: Option<String>,
    /// The variable `--api-key` stored the key in.
    pub api_key_var: Option<String>,
    /// Literal secrets moved into variables.
    pub moved: Vec<Moved>,
    /// What the caller should warn about: the edit applied, but may not do
    /// what was meant (`etherscan doesn't cover chain ID 31337`).
    pub warnings: Vec<String>,
    /// The chain's name after the edit.
    pub chain: String,
}

/// What [`Tx::insert_chain`] attaches to a new chain that names none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attach {
    /// The only signer, if there is exactly one, and `[explorers.etherscan]`,
    /// if it is configured and [covers](Explorer::etherscan_covers) the chain.
    Defaults,
    /// The built-in explorer only, on the same terms (`--no-signer`).
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
    /// Why `[explorers.etherscan]`, though configured, was not attached
    /// ([`Explorer::etherscan_gap`]).
    pub explorer_skipped: Option<String>,
    /// Why no signer was attached though some exist: `2 signers to choose
    /// from: a, b`.
    pub signer_skipped: Option<String>,
}

/// The chains with no explorer, split by whether the built-in `etherscan`
/// [covers](Explorer::etherscan_covers) them: what
/// [`Tx::attach_explorer_where_missing`] attaches it to, and skips.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MissingExplorers {
    /// Chains `etherscan` covers, in config order.
    pub covered: Vec<String>,
    /// Chains it does not, in config order.
    pub skipped: Vec<String>,
}

impl MissingExplorers {
    /// The chains in `config` with no explorer.
    pub fn of(config: &crate::model::Config) -> Self {
        let mut missing = Self::default();
        for (name, chain) in config.chains.iter().filter(|(_, c)| c.explorer.is_none()) {
            match Explorer::etherscan_covers(chain.id) {
                true => missing.covered.push(name.clone()),
                false => missing.skipped.push(name.clone()),
            }
        }
        missing
    }
}

/// What [`Tx::attach_explorer_where_missing`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExplorersAttached {
    /// The chains (and the ones skipped), as before the attach.
    pub chains: MissingExplorers,
    /// The variable holding the key (`secrets.toml`).
    pub api_key_var: Option<String>,
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
        if attach == Attach::Defaults && chain.signer.is_none() {
            let signers: Vec<&str> = config.signers.keys().map(String::as_str).collect();
            match signers[..] {
                [] => {}
                [only] => {
                    chain.signer = Some(only.to_string());
                    attached.signer = Some(only.to_string());
                }
                _ => {
                    attached.signer_skipped = Some(format!(
                        "{} signers to choose from: {}",
                        signers.len(),
                        signers.join(", ")
                    ));
                }
            }
        }
        if attach != Attach::Nothing
            && chain.explorer.is_none()
            && config.explorers.contains_key(ETHERSCAN)
        {
            match Explorer::etherscan_gap(chain.id) {
                None => {
                    chain.explorer = Some(ETHERSCAN.to_string());
                    attached.explorer = Some(ETHERSCAN.to_string());
                }
                Some(gap) => attached.explorer_skipped = Some(gap),
            }
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

    /// Attach the built-in `etherscan` explorer (created if missing) to
    /// every chain with none that it covers, storing `key` once, in a
    /// variable. Chains it does not cover are skipped.
    pub fn attach_explorer_where_missing(
        &mut self,
        key: Zeroizing<String>,
    ) -> Result<ExplorersAttached> {
        let chains = MissingExplorers::of(&self.config()?);
        let mut key = Some(key);
        let mut api_key_var = None;
        for name in &chains.covered {
            let edit = ChainEdit {
                explorer: Some(Some(ETHERSCAN.to_string())),
                api_key: key.take(),
                ..ChainEdit::default()
            };
            let report = self.edit_chain(name, &edit)?;
            api_key_var = api_key_var.or(report.api_key_var);
        }
        Ok(ExplorersAttached {
            chains,
            api_key_var,
        })
    }

    /// Apply `edit` to chain `name`: endpoint and headers first, then
    /// failover, explorer and key, signer, aliases, and the rename last.
    /// Validation runs on commit, as for every transaction. The report's
    /// changes are [`Chain::changes`] from before to after (so a no-op
    /// edit reports none), then the explorer key and the rename.
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
        }

        let key_line = self.edit_explorer(name, edit, &mut report)?;

        if let Some(signer) = &edit.signer {
            if let Some(signer) = signer {
                config.check_signer(signer)?;
            }
            self.update_chain(name, |c| {
                c.signer = signer.clone();
                Ok(())
            })?;
        }

        if !edit.add_aliases.is_empty() || !edit.remove_aliases.is_empty() {
            let mut aliases = chain.aliases.clone();
            for alias in &edit.remove_aliases {
                let before = aliases.len();
                aliases.retain(|a| !a.eq_ignore_ascii_case(alias));
                if aliases.len() == before {
                    bail!("chain '{name}' has no alias '{alias}'");
                }
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
            }
            self.set_aliases(name, &aliases)?;
        }

        let after = self.config()?.chains[name].clone();
        report.changes = Chain::changes(&chain, &after);
        report.changes.extend(key_line);
        if let Some(new) = &edit.name
            && new != name
        {
            self.rename_chain(name, new)?;
            report.changes.push(format!("renamed: {name} → {new}"));
            report.chain = new.clone();
        }
        Ok(report)
    }

    /// `--rpc`, `--add-rpc`, the header flags and `--rm-rpc`.
    fn edit_endpoint(
        &mut self,
        name: &str,
        chain: &Chain,
        edit: &ChainEdit,
        report: &mut EditReport,
    ) -> Result<()> {
        // New endpoints are added (a new `--rpc` URL activated), the header
        // flags on the one they name. Else the header flags apply to the
        // `--rpc` endpoint, or the active one.
        let secrets = self.secrets()?;
        for (ep_name, endpoint, role) in edit.added_endpoints(name, chain, &secrets.vars)? {
            let (ep_name, moved) = self.add_endpoint_url(name, Some(&ep_name), &endpoint, role)?;
            report.moved.extend(moved);
            report.added_endpoints.push(ep_name);
        }
        match edit.rpc_choice(name, chain, &secrets.vars)? {
            // Added and activated above, with the header flags.
            Some(RpcChoice::New(..)) => {}
            Some(RpcChoice::Existing(ep_name)) => {
                self.edit_headers(name, chain, &ep_name, edit, report)?;
                if chain.rpc != ep_name {
                    self.set_active(name, &ep_name)?;
                }
            }
            // On the `--add-rpc` endpoint, added above.
            None if edit.headers_on_added()? => {}
            None => self.edit_headers(name, chain, &chain.rpc, edit, report)?,
        }
        for ep_name in &edit.remove_rpcs {
            if self.config()?.chains[name].rpc == *ep_name {
                bail!(
                    "'{ep_name}' is the active endpoint of '{name}'; activate another in the same command with --rpc NAME"
                );
            }
            self.remove_endpoint(name, ep_name)?;
        }
        Ok(())
    }

    /// The header flags on existing endpoint `ep_name`.
    fn edit_headers(
        &mut self,
        name: &str,
        chain: &Chain,
        ep_name: &str,
        edit: &ChainEdit,
        report: &mut EditReport,
    ) -> Result<()> {
        let Some(mut endpoint) = chain.rpcs.get(ep_name).cloned() else {
            bail!("chain '{name}' has no active endpoint '{ep_name}'");
        };
        let mut changed = false;
        if edit.clear_headers && !endpoint.headers.is_empty() {
            endpoint.headers.clear();
            changed = true;
        }
        if !edit.headers.is_empty() {
            edit.put_headers(&mut endpoint)?;
            changed = true;
        }
        if changed {
            let mut endpoints = [(ep_name.to_string(), endpoint)];
            report
                .moved
                .extend(self.extract_secrets(name, &mut endpoints)?);
            let [(_, endpoint)] = endpoints;
            self.set_endpoint(name, ep_name, &endpoint)?;
        }
        Ok(())
    }

    /// `--explorer` and `--api-key`. Returns the line saying where the key
    /// went, when it changed anything.
    fn edit_explorer(
        &mut self,
        name: &str,
        edit: &ChainEdit,
        report: &mut EditReport,
    ) -> Result<Option<String>> {
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
            if explorer.as_deref() == Some(ETHERSCAN)
                && let Some(gap) = Explorer::etherscan_gap(self.config()?.chains[name].id)
            {
                report
                    .warnings
                    .push(format!("{gap}; its API will reject this chain"));
            }
            self.update_chain(name, |c| {
                c.explorer = explorer.clone();
                Ok(())
            })?;
        }
        let Some(key) = &edit.api_key else {
            return Ok(None);
        };
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
        let held = self.secrets()?.vars.get(&var).map(|v| v == key);
        self.set_var(&var, key)?;
        let reference = format!("${{{var}}}");
        let referenced = explorer.key.as_deref() == Some(&reference);
        if !referenced {
            explorer.key = Some(reference);
            self.set_explorer(&explorer_name, &explorer)?;
        }
        report.api_key_var = Some(var.clone());
        let users = config
            .chains
            .iter()
            .filter(|(_, c)| c.explorer.as_deref() == Some(&explorer_name))
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Ok(match held {
            Some(true) if referenced => None,
            Some(false) => Some(format!(
                "explorer key: replaced the value of ${{{var}}} (used by {users})"
            )),
            _ => Some(format!(
                "explorer key: stored in ${{{var}}} (used by every chain on {explorer_name})"
            )),
        })
    }
}
