//! One-time migration from the v1 `config.json` (see the design's
//! "Migration from v1", steps 1–7).
//!
//! 1. Runs under the config write lock and re-checks that `config.toml` is
//!    still absent, so parallel `exec`s migrate exactly once.
//! 2. Chains get short names from [`crate::names::SHORT_NAMES`] (else a slug);
//!    the old name becomes an alias. Chains sharing a chain ID are merged.
//!    An alias that is another chain's name or alias is dropped.
//! 3. Every endpoint is kept and named after its host, `-ws` for websockets,
//!    `-2`, `-3` on collision; headers stay inline; `failover` starts empty.
//! 4. Literal Infura `/v3/<key>` and Alchemy `/v2/<key>` keys and credential
//!    header values move into variables ([`crate::extract`]); other inline
//!    credentials stay and are reported.
//! 5. Literal verifier keys move into variables; chains sharing an
//!    explorer URL shape and key share one named explorer; chains without a
//!    verifier get none.
//! 6. `key_name` becomes `signer`; key records keep backend and cached
//!    address; variables move to `secrets.toml`, renamed if v2 would reject
//!    the name (valid names are kept first; a clash gets `_2`, `_3`…).
//! 7. Writes `secrets.toml`, then `config.toml` (fsync + rename each), then
//!    renames the v1 file to `config.v1.json.bak` (`.chainz.v1.json.bak`
//!    for the legacy `~/.chainz.json`).
//!
//! Migration never aborts over one bad entry, since every command migrates
//! automatically: whatever cannot be carried over is skipped and reported
//! in [`Summary::attention`], and stays in the v1 backup. Keys and secrets
//! are never dropped without such a note.

use crate::address::Address;
use crate::extract::{self, VarTable};
use crate::model::{Backend, Chain, Endpoint, Explorer, Signer};
use crate::names;
use crate::secrets::{BUILTIN_VARS, SignerSecret};
use crate::store::files::{StorePaths, WriteLock, new_config_text};
use crate::template;
use crate::tx::Tx;
use anyhow::{Context, Result, anyhow};
use indexmap::IndexMap;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// v1 `config.json`. Chains and keys that do not parse are skipped with a
/// note rather than failing the whole migration.
#[derive(Deserialize)]
struct V1Config {
    #[serde(default)]
    chains: Vec<Readable<V1Chain>>,
    #[serde(default)]
    variables: BTreeMap<String, String>,
    #[serde(default)]
    keys: BTreeMap<String, Readable<V1Key>>,
    #[serde(default)]
    default_chain: Option<String>,
}

/// An entry that parsed, or one that did not (unknown key type, missing
/// field).
#[derive(Deserialize)]
#[serde(untagged)]
enum Readable<T> {
    Ok(T),
    Unreadable(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
struct V1Chain {
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    chain_id: u64,
    #[serde(default)]
    rpc_urls: Vec<V1Endpoint>,
    #[serde(default)]
    selected_rpc: String,
    #[serde(default)]
    verification_api_key: Option<String>,
    #[serde(default)]
    verification_url: Option<String>,
    #[serde(default)]
    key_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum V1Endpoint {
    Url(String),
    Detailed {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

#[derive(Deserialize)]
struct V1Key {
    #[serde(default)]
    address: Option<String>,
    #[serde(flatten)]
    kind: V1KeyKind,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum V1KeyKind {
    PrivateKey {
        value: String,
    },
    EncryptedKey {
        value: String,
        nonce: String,
        salt: String,
        #[serde(default)]
        version: Option<u8>,
        #[serde(default)]
        kdf_memory_kib: Option<u32>,
        #[serde(default)]
        kdf_iterations: Option<u32>,
        #[serde(default)]
        kdf_parallelism: Option<u32>,
    },
    OnePassword {
        vault: String,
        item: String,
    },
    Keyring {
        service: String,
        username: String,
    },
}

impl Drop for V1KeyKind {
    fn drop(&mut self) {
        if let V1KeyKind::PrivateKey { value } = self {
            zeroize::Zeroize::zeroize(value);
        }
    }
}

/// What a migration did. `Display` is the one-line summary;
/// [`report`](Self::report) the whole report.
#[derive(Debug, Default, Clone)]
pub struct Summary {
    /// Chains migrated.
    pub chains: usize,
    /// Endpoints migrated.
    pub endpoints: usize,
    /// Signers migrated.
    pub signers: usize,
    /// v1 variables moved to secrets.toml.
    pub variables: usize,
    /// Variables created from extracted credentials.
    pub extracted: Vec<String>,
    /// Items that need the user's attention (never contain values).
    pub attention: Vec<String>,
    /// Where the v1 file was moved, once written.
    pub backup: Option<PathBuf>,
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "migrated {} chains, {} endpoints, {} signers, {} variables",
            self.chains, self.endpoints, self.signers, self.variables
        )?;
        if !self.extracted.is_empty() {
            write!(
                f,
                "; moved {} credentials into variables",
                self.extracted.len()
            )?;
        }
        match self.attention.len() {
            0 => Ok(()),
            1 => write!(f, "; 1 item needs attention"),
            n => write!(f, "; {n} items need attention"),
        }
    }
}

impl Summary {
    /// The report, one line each: the summary, what was written (`config`,
    /// the new `config.toml`), then every item that needs attention.
    pub fn report(&self, config: &Path) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.attention.len() + 2);
        lines.push(self.to_string());
        lines.push(match &self.backup {
            Some(backup) => format!(
                "wrote {} (v1 config kept as {})",
                config.display(),
                backup.display()
            ),
            None => format!("wrote {}", config.display()),
        });
        lines.extend(self.attention.iter().map(|item| format!("  - {item}")));
        lines
    }
}

/// A computed migration: the two files to write and the summary.
pub struct Plan {
    /// New `config.toml`.
    pub config_toml: String,
    /// New `secrets.toml`.
    pub secrets_toml: String,
    /// Summary.
    pub summary: Summary,
}

impl fmt::Debug for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Plan")
            .field("summary", &self.summary)
            .finish_non_exhaustive()
    }
}

fn normalize_explorer_api(url: &str, chain_id: u64) -> String {
    let needle = format!("chainid={chain_id}");
    match url.find(&needle) {
        Some(i) if !url[i + needle.len()..].starts_with(|c: char| c.is_ascii_digit()) => {
            format!(
                "{}chainid=${{CHAIN_ID}}{}",
                &url[..i],
                &url[i + needle.len()..]
            )
        }
        _ => url.to_string(),
    }
}

/// The v2 name for v1 key `raw`: a slug, with `-signer` appended if it
/// would shadow a token (`@wallet`, `@chainname`…), unique among `taken`.
fn signer_name(raw: &str, taken: &HashSet<String>) -> String {
    let mut base = names::slugify(raw);
    if names::is_reserved_token(&base) {
        base.push_str("-signer");
    }
    names::unique_name(&base, |n| taken.contains(n))
}

/// New names for the v1 variables v2 would reject (`A-B`, `CHAIN`). Names
/// that are already valid are claimed first, so a cleaned-up name never
/// takes one; a clash gets `_2`, `_3`….
fn var_renames(vars: &BTreeMap<String, String>) -> HashMap<String, String> {
    let keep = |n: &str| template::is_valid_name(n) && !BUILTIN_VARS.contains(&n);
    let mut taken: HashSet<String> = vars.keys().filter(|n| keep(n)).cloned().collect();
    let mut renames = HashMap::new();
    for name in vars.keys().filter(|n| !keep(n)) {
        let mut clean = template::clean_name(name);
        if BUILTIN_VARS.contains(&clean.as_str()) {
            clean.push_str("_VAR");
        }
        let new = extract::unique_var_name(&clean, |n| taken.contains(n));
        taken.insert(new.clone());
        renames.insert(name.clone(), new);
    }
    renames
}

/// Compute the migration of v1 JSON text without touching the filesystem.
pub(crate) fn plan(v1_json: &str) -> Result<Plan> {
    let v1: V1Config = serde_json::from_str(v1_json).map_err(|e| {
        anyhow!(
            "failed to parse the v1 config: {:?} error at line {}, column {}",
            e.classify(),
            e.line(),
            e.column()
        )
    })?;
    let mut summary = Summary::default();

    // Variables (step 6), renaming those v2 would reject. Every template
    // is rewritten in one pass, so renames never chain.
    let renames = var_renames(&v1.variables);
    let rename = |text: &str| template::rename_vars(text, &renames);
    let mut vars = VarTable::default();
    for (name, value) in &v1.variables {
        let new = renames.get(name).unwrap_or(name);
        if new != name {
            summary.attention.push(format!(
                "variable '{name}' was renamed to '{new}'; update any ${{{name}}} references outside chainz"
            ));
        }
        vars.insert(new, value);
        summary.variables += 1;
    }

    let (signer_records, signer_names) = migrate_keys(&v1.keys, &mut summary);
    let chains = migrate_chains(&v1.chains, &rename, &signer_names, &mut vars, &mut summary);

    let mut tx = Tx::parse(
        &new_config_text(
            "# chainz configuration (v2), migrated from the v1 config.json.\n# Secrets live in secrets.toml; reference them as ${NAME}.\n",
        ),
        Path::new("config.toml"),
        "",
        Path::new("secrets.toml"),
    )?;
    if let Some(default) = &v1.default_chain {
        match chains.by_v1_name.get(&default.to_ascii_lowercase()) {
            Some(name) => tx.set_default(Some(name)),
            None => summary.attention.push(format!(
                "default chain '{default}' was not found and was dropped"
            )),
        }
    }
    for (name, explorer) in &chains.explorers {
        tx.set_explorer(name, explorer)?;
    }
    for (name, chain) in &chains.list {
        tx.put_chain(name, chain)?;
    }
    for (name, signer, secret) in &signer_records {
        tx.set_signer(name, signer)?;
        if let Some(secret) = secret {
            tx.set_signer_secret(name, Some(secret))?;
        }
    }
    for (name, value) in vars.iter() {
        tx.set_var(name, value)?;
    }
    summary.extracted = vars.created().to_vec();
    if !tx.secrets_is_empty() {
        tx.set_secrets_header("# chainz secrets (mode 0600). Never share this file.\n");
    }

    // The result must load and validate like any hand-written config. Every
    // entry that could fail was skipped above, so a failure here is a bug.
    tx.check(Path::new("config.toml"))
        .context("migrated config is invalid (a chainz bug: please report it; the v1 config was left untouched)")?;
    let (config_toml, secrets_toml) = tx.texts();
    Ok(Plan {
        config_toml,
        secrets_toml,
        summary,
    })
}

/// A migrated signer: name, record, key material for secrets.toml.
type SignerRecord = (String, Signer, Option<SignerSecret>);

/// Migrate v1 keys to signers. Returns the records and the v1 → v2 name
/// map. A key that cannot be carried over is skipped with a note.
fn migrate_keys(
    keys: &BTreeMap<String, Readable<V1Key>>,
    summary: &mut Summary,
) -> (Vec<SignerRecord>, HashMap<String, String>) {
    let mut records = Vec::new();
    let mut names: HashMap<String, String> = HashMap::new();
    let mut taken: HashSet<String> = HashSet::new();
    for (raw_name, key) in keys {
        let Readable::Ok(key) = key else {
            summary.attention.push(format!(
                "key '{raw_name}' could not be read (unknown type or missing field) and was not migrated; it stays in the v1 backup"
            ));
            continue;
        };
        let name = signer_name(raw_name, &taken);
        let (signer, secret) = match migrate_key(&name, key, &mut summary.attention) {
            Ok(migrated) => migrated,
            Err(reason) => {
                summary.attention.push(format!(
                    "key '{raw_name}' was not migrated: {reason}; it stays in the v1 backup"
                ));
                continue;
            }
        };
        if name != *raw_name {
            summary
                .attention
                .push(format!("key '{raw_name}' became signer '{name}'"));
        }
        taken.insert(name.clone());
        names.insert(raw_name.clone(), name.clone());
        records.push((name, signer, secret));
        summary.signers += 1;
    }
    (records, names)
}

/// One v1 key as a signer record (and key material), or why it cannot be.
fn migrate_key(
    name: &str,
    key: &V1Key,
    attention: &mut Vec<String>,
) -> Result<(Signer, Option<SignerSecret>), String> {
    let mut address = key
        .address
        .as_deref()
        .and_then(|a| a.parse::<Address>().ok());
    let mut secret = None;
    let mut signer = match &key.kind {
        V1KeyKind::Keyring { service, username } => {
            Signer::keyring(name, Some(service), Some(username), None)
        }
        V1KeyKind::PrivateKey { value } => {
            let (derived, mismatch) = crate::address::check_cached_address(value, address)
                .map_err(|_| "it holds an invalid private key".to_string())?;
            if mismatch {
                attention.push(format!(
                    "signer '{name}' had a cached address that does not match its key; the key's own address is used"
                ));
            }
            address = Some(derived);
            let mut s = SignerSecret::default();
            s.private_key = Some(value.trim().to_string());
            secret = Some(s);
            attention.push(format!(
                "signer '{name}' is plaintext; move it with `chainz signer migrate {name}`"
            ));
            Signer::new(Backend::Plaintext, None)
        }
        V1KeyKind::EncryptedKey {
            value,
            nonce,
            salt,
            version,
            kdf_memory_kib,
            kdf_iterations,
            kdf_parallelism,
        } => {
            let mut s = SignerSecret::default();
            s.ciphertext = Some(value.clone());
            s.nonce = Some(nonce.clone());
            s.salt = Some(salt.clone());
            s.version = Some(version.unwrap_or(1));
            s.kdf_memory_kib = *kdf_memory_kib;
            s.kdf_iterations = *kdf_iterations;
            s.kdf_parallelism = *kdf_parallelism;
            crate::envelope::Envelope::read(&s)
                .map_err(|e| format!("its encrypted key cannot be read ({e})"))?;
            secret = Some(s);
            Signer::new(Backend::Encrypted, None)
        }
        V1KeyKind::OnePassword { vault, item } => {
            if vault.trim().is_empty() || item.trim().is_empty() {
                return Err("its 1Password vault or item is empty".to_string());
            }
            Signer::one_password(vault.clone(), item.clone(), None)
        }
    };
    if address.is_none() {
        let why = if key.address.is_some() {
            "an invalid cached address"
        } else {
            "no cached address"
        };
        attention.push(format!(
            "signer '{name}' has {why}; @wallet needs one (re-add it or run `chainz signer migrate {name}`)"
        ));
    }
    signer.address = address;
    Ok((signer, secret))
}

/// One v2 chain assembled from one or more v1 entries (v1 allowed several
/// entries with the same chain ID; v2 does not, so they are merged).
#[derive(Default)]
struct ChainPlan {
    id: u64,
    name: String,
    /// The v1 names (`true`) and aliases (`false`) of every merged entry.
    v1_names: Vec<(String, bool)>,
    rpcs: Vec<(String, Endpoint)>,
    rpc: Option<String>,
    explorer: Option<String>,
    signer: Option<String>,
}

/// The migrated chains and explorers.
struct Chains {
    list: Vec<(String, Chain)>,
    explorers: IndexMap<String, Explorer>,
    /// Lowercase v1 name (or, failing that, alias) → v2 chain name.
    by_v1_name: HashMap<String, String>,
}

fn migrate_chains(
    v1_chains: &[Readable<V1Chain>],
    rename: &dyn Fn(&str) -> String,
    signer_names: &HashMap<String, String>,
    vars: &mut VarTable,
    summary: &mut Summary,
) -> Chains {
    let mut chains: Vec<&V1Chain> = Vec::with_capacity(v1_chains.len());
    for chain in v1_chains {
        match chain {
            Readable::Ok(chain) => chains.push(chain),
            Readable::Unreadable(_) => summary.attention.push(
                "a v1 chain entry could not be read (missing name or chain_id) and was not migrated; it stays in the v1 backup".to_string(),
            ),
        }
    }

    // One plan per chain ID.
    let mut plans: Vec<ChainPlan> = Vec::new();
    let mut owner: Vec<usize> = Vec::with_capacity(chains.len());
    let mut by_id: HashMap<u64, usize> = HashMap::new();
    for chain in &chains {
        let i = *by_id.entry(chain.chain_id).or_insert_with(|| {
            plans.push(ChainPlan {
                id: chain.chain_id,
                ..ChainPlan::default()
            });
            plans.len() - 1
        });
        owner.push(i);
    }

    // Names (step 2): built-in short names first, so a slug never takes one.
    let mut used: HashSet<String> = HashSet::new();
    for plan in &mut plans {
        if let Some(short) = names::short_name(plan.id)
            && used.insert(short.to_string())
        {
            plan.name = short.to_string();
        }
    }
    for (chain, &i) in chains.iter().zip(&owner) {
        if plans[i].name.is_empty() {
            let base = names::short_name(chain.chain_id)
                .map(str::to_string)
                .unwrap_or_else(|| names::slugify(&chain.name));
            let name =
                names::unique_name(&base, |n| used.contains(n) || names::is_chain_id_like(n));
            used.insert(name.clone());
            plans[i].name = name;
        }
    }

    let mut explorers: IndexMap<String, Explorer> = IndexMap::new();
    let mut groups: HashMap<(String, Option<String>), String> = HashMap::new();
    for (chain, &i) in chains.iter().zip(&owner) {
        let name = plans[i].name.clone();
        if let Some((first, _)) = plans[i].v1_names.first() {
            summary.attention.push(format!(
                "v1 chains '{first}' and '{}' share chain ID {}; they were merged into chain '{name}'",
                chain.name, chain.chain_id
            ));
        }
        plans[i].v1_names.push((chain.name.clone(), true));
        plans[i]
            .v1_names
            .extend(chain.aliases.iter().map(|a| (a.clone(), false)));

        migrate_endpoints(chain, &name, &mut plans[i], rename, vars, summary);

        let explorer = migrate_explorer(
            chain,
            &name,
            rename,
            vars,
            &mut explorers,
            &mut groups,
            summary,
        );
        let plan = &mut plans[i];
        match (&plan.explorer, explorer) {
            (None, explorer) => plan.explorer = explorer,
            (Some(kept), Some(other)) if *kept != other => summary.attention.push(format!(
                "chain '{name}': v1 entry '{}' used another verifier, which was not attached (explorer '{other}' exists for it)",
                chain.name
            )),
            _ => {}
        }

        if let Some(key_name) = &chain.key_name {
            match (signer_names.get(key_name), &plan.signer) {
                (None, _) => summary.attention.push(format!(
                    "chain '{name}' referenced key '{key_name}', which was not migrated; no signer attached"
                )),
                (Some(signer), None) => plan.signer = Some(signer.clone()),
                (Some(signer), Some(kept)) if signer != kept => {
                    summary.attention.push(format!(
                        "chain '{name}': v1 entry '{}' used key '{key_name}'; signer '{kept}' was kept",
                        chain.name
                    ))
                }
                _ => {}
            }
        }
    }

    // A chain needs an endpoint.
    plans.retain(|plan| {
        if plan.rpcs.is_empty() {
            summary.attention.push(format!(
                "chain '{}' (chain ID {}) has no usable RPC URL and was not migrated; it stays in the v1 backup",
                plan.name, plan.id
            ));
        }
        !plan.rpcs.is_empty()
    });

    // v1 names resolve first, then v1 aliases (for `default_chain`).
    let mut by_v1_name: HashMap<String, String> = HashMap::new();
    for want_name in [true, false] {
        for plan in &plans {
            for (v1_name, is_name) in &plan.v1_names {
                if *is_name == want_name {
                    by_v1_name
                        .entry(v1_name.to_ascii_lowercase())
                        .or_insert_with(|| plan.name.clone());
                }
            }
        }
    }

    // Aliases: an alias another chain already answers to is dropped.
    let mut answers: HashMap<String, String> = plans
        .iter()
        .map(|p| (p.name.clone(), p.name.clone()))
        .collect();
    let mut list = Vec::with_capacity(plans.len());
    for plan in plans {
        let mut aliases: Vec<String> = Vec::new();
        for (raw, _) in &plan.v1_names {
            let alias = raw.trim();
            if alias.is_empty()
                || alias.eq_ignore_ascii_case(&plan.name)
                || names::is_chain_id_like(alias)
                || aliases.iter().any(|a| a.eq_ignore_ascii_case(alias))
            {
                continue;
            }
            if alias.chars().any(char::is_control) {
                summary.attention.push(format!(
                    "an alias of chain '{}' contains a control character and was dropped",
                    plan.name
                ));
                continue;
            }
            let key = alias.to_ascii_lowercase();
            if let Some(other) = answers.get(&key)
                && *other != plan.name
            {
                summary.attention.push(format!(
                    "alias '{alias}' of chain '{}' was dropped: it is already the name or an alias of chain '{other}'",
                    plan.name
                ));
                continue;
            }
            answers.insert(key, plan.name.clone());
            aliases.push(alias.to_string());
        }
        let rpc = plan.rpc.unwrap_or_else(|| plan.rpcs[0].0.clone());
        summary.chains += 1;
        list.push((
            plan.name,
            Chain {
                id: plan.id,
                aliases,
                signer: plan.signer,
                explorer: plan.explorer,
                rpc,
                failover: vec![],
                rpcs: plan.rpcs.into_iter().collect(),
            },
        ));
    }
    Chains {
        list,
        explorers,
        by_v1_name,
    }
}

/// Endpoints (steps 3 and 4), in v1 order; the selected URL is added if a
/// legacy config listed it only in `selected_rpc`. Appends to `plan`.
fn migrate_endpoints(
    chain: &V1Chain,
    name: &str,
    plan: &mut ChainPlan,
    rename: &dyn Fn(&str) -> String,
    vars: &mut VarTable,
    summary: &mut Summary,
) {
    let mut raw: Vec<(&str, Vec<(&str, &str)>)> = chain
        .rpc_urls
        .iter()
        .map(|e| match e {
            V1Endpoint::Url(url) => (url.as_str(), Vec::new()),
            V1Endpoint::Detailed { url, headers } => (
                url.as_str(),
                headers
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect(),
            ),
        })
        .collect();
    if !chain.selected_rpc.is_empty() && !raw.iter().any(|(u, _)| *u == chain.selected_rpc) {
        raw.push((&chain.selected_rpc, Vec::new()));
    }
    for (url, headers) in raw {
        let selected = url == chain.selected_rpc;
        let new_url = rename(url);
        if Endpoint::check_url_text(&new_url).is_err() {
            summary.attention.push(format!(
                "chain '{name}' has an empty or malformed endpoint URL; it was skipped and stays in the v1 backup"
            ));
            continue;
        }
        let ep_name = names::unique_name(&names::endpoint_name_for_url(&new_url), |n| {
            plan.rpcs.iter().any(|(e, _)| e == n)
        });
        let mut endpoint = Endpoint::new(new_url);
        let mut unexportable = Vec::new();
        for (header, value) in headers {
            let mut value = rename(value);
            let at = format!("chain '{name}' endpoint '{ep_name}' header '{header}'");
            if !crate::model::is_header_name(header) {
                let kept = if value.is_empty() || !extract::is_literal(&value) {
                    String::new()
                } else {
                    let base = extract::var_base(&[&ep_name, "HEADER"]);
                    format!("; its value is kept as ${{{}}}", vars.intern(&base, &value))
                };
                summary
                    .attention
                    .push(format!("{at} has an invalid name and was dropped{kept}"));
                continue;
            }
            if template::references(&value).is_err() {
                // Not a usable template: keep the whole value as a secret.
                let base = extract::var_base(&[&ep_name, header]);
                let var = vars.intern(&base, &value);
                summary.attention.push(format!(
                    "{at} had a malformed ${{…}}; its whole value moved to ${{{var}}}"
                ));
                value = format!("${{{var}}}");
            }
            if !extract::is_literal(&value) && value.contains(['\r', '\n']) {
                summary.attention.push(format!(
                    "{at} contains a line break and was dropped; it stays in the v1 backup"
                ));
                continue;
            }
            if crate::model::header_value_problem(&value).is_some() {
                unexportable.push(at);
            }
            endpoint.headers.insert(header.to_string(), value);
        }
        extract::extract_endpoint(name, &ep_name, &mut endpoint, vars);
        for at in unexportable {
            summary.attention.push(format!(
                "{at} contains a comma or line break, so `exec --direct` cannot export it via ETH_RPC_HEADERS (the gateway still sends it)"
            ));
        }
        if let Some(kind) = extract::manual_credential(&endpoint.url) {
            summary.attention.push(format!(
                "chain '{name}' endpoint '{ep_name}' embeds a credential ({kind}); move it into a variable"
            ));
        }
        for (header, value) in &endpoint.headers {
            if extract::is_literal(value) {
                summary.attention.push(format!(
                    "chain '{name}' endpoint '{ep_name}' header '{header}' holds a literal value; move it into a variable"
                ));
            }
        }
        // A merged duplicate chain often repeats an endpoint.
        let ep_name = match plan.rpcs.iter().find(|(_, e)| *e == endpoint) {
            Some((existing, _)) => existing.clone(),
            None => {
                plan.rpcs.push((ep_name.clone(), endpoint));
                summary.endpoints += 1;
                ep_name
            }
        };
        if selected && plan.rpc.is_none() {
            plan.rpc = Some(ep_name);
        }
    }
}

/// The explorer (step 5) for one v1 chain: chains sharing an API URL shape
/// and key share one. Returns its name, or `None` without a usable URL.
fn migrate_explorer(
    chain: &V1Chain,
    name: &str,
    rename: &dyn Fn(&str) -> String,
    vars: &mut VarTable,
    explorers: &mut IndexMap<String, Explorer>,
    groups: &mut HashMap<(String, Option<String>), String>,
    summary: &mut Summary,
) -> Option<String> {
    let key = chain
        .verification_api_key
        .as_deref()
        .map(rename)
        .filter(|k| !k.trim().is_empty());
    let mut api = chain
        .verification_url
        .as_deref()
        .filter(|u| !u.trim().is_empty())
        .map(|u| normalize_explorer_api(&rename(u), chain.chain_id));
    if api
        .as_deref()
        .is_some_and(|a| Explorer::check_api_text(a).is_err())
    {
        summary.attention.push(format!(
            "chain '{name}' has a malformed verifier URL; no explorer was attached (it stays in the v1 backup)"
        ));
        api = None;
    }
    let Some(api) = api else {
        if let Some(key) = key {
            let kept = if extract::is_literal(&key) {
                let var = vars.intern(&extract::var_base(&[name, "VERIFIER_API_KEY"]), &key);
                format!("the key is kept as ${{{var}}} with")
            } else {
                "it has".to_string()
            };
            summary.attention.push(format!(
                "chain '{name}' had a verifier key but no usable URL; {kept} no explorer attached"
            ));
        }
        return None;
    };
    let group = (api, key);
    if let Some(existing) = groups.get(&group) {
        return Some(existing.clone());
    }
    let host = names::UrlName::of(&group.0);
    let exp_name = names::unique_name(
        match host.host() {
            Some(host) if host != "rpc" && host != "local" => host,
            _ => "explorer",
        },
        |n| explorers.contains_key(n),
    );
    let mut explorer = Explorer {
        api: group.0.clone(),
        key: group.1.clone(),
        url: None,
    };
    extract::extract_explorer_key(&exp_name, &mut explorer, vars);
    explorers.insert(exp_name.clone(), explorer);
    groups.insert(group, exp_name.clone());
    Some(exp_name)
}

/// Backup name for a migrated v1 file.
fn backup_path(source: &Path) -> PathBuf {
    let dir = source.parent().unwrap_or(Path::new("."));
    if source.file_name().is_some_and(|n| n == "config.json") {
        dir.join("config.v1.json.bak")
    } else {
        dir.join(".chainz.v1.json.bak")
    }
}

/// Whether a v1 config is waiting to be migrated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// `config.toml` exists: migrated, or never v1.
    Done,
    /// No `config.toml` and no v1 file.
    NoV1,
    /// The v1 file to migrate.
    Pending(PathBuf),
}

/// Where `paths` stand (see [`Status`]).
pub fn status(paths: &StorePaths) -> Status {
    if paths.config().exists() {
        return Status::Done;
    }
    match paths.v1_source() {
        Some(source) => Status::Pending(source),
        None => Status::NoV1,
    }
}

/// [`plan`] for the v1 file at `source`.
fn plan_file(source: &Path) -> Result<Plan> {
    let text = std::fs::read_to_string(source)
        .with_context(|| format!("failed to read {}", source.display()))?;
    plan(&text).with_context(|| format!("failed to migrate {}", source.display()))
}

/// Migrate under `lock`. Returns `None` when there was nothing to do
/// (config.toml exists, or no v1 file).
pub fn migrate_locked(lock: &WriteLock) -> Result<Option<Summary>> {
    let paths = lock.paths();
    let Status::Pending(source) = status(paths) else {
        return Ok(None);
    };
    let mut plan = plan_file(&source)?;
    chainz_os::create_private_dir(paths.dir())?;
    let secrets = (!plan.secrets_toml.is_empty()).then_some(plan.secrets_toml.as_str());
    crate::store::files::write_pair(paths, Some(&plan.config_toml), secrets)?;
    let backup = backup_path(&source);
    std::fs::rename(&source, &backup).with_context(|| {
        format!(
            "failed to rename {} to {}",
            source.display(),
            backup.display()
        )
    })?;
    plan.summary.backup = Some(backup);
    Ok(Some(plan.summary))
}

#[cfg(test)]
mod tests;
