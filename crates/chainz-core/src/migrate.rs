//! One-time migration from the v1 `config.json` (see the design's
//! "Migration from v1", steps 1–7).
//!
//! 1. Runs under the config write lock and re-checks that `config.toml` is
//!    still absent, so parallel `exec`s migrate exactly once.
//! 2. Chains get short names from [`crate::names::SHORT_NAMES`] (else a slug);
//!    the old name becomes an alias.
//! 3. Every endpoint is kept and named after its host, `-ws` for websockets,
//!    `-2`, `-3` on collision; headers stay inline; `failover` starts empty.
//! 4. Literal Infura `/v3/<key>` and Alchemy `/v2/<key>` keys move into
//!    variables; other inline credentials stay and are reported.
//! 5. Literal verifier keys move into variables; chains sharing an
//!    explorer URL shape and key share one named explorer; chains without a
//!    verifier get none.
//! 6. `key_name` becomes `signer`; key records keep backend and cached
//!    address; variables move to `secrets.toml`.
//! 7. Writes `secrets.toml`, then `config.toml` (fsync + rename each), then
//!    renames the v1 file to `config.v1.json.bak`.

use crate::address::Address;
use crate::edit::{self, NewChain};
use crate::model::{Backend, Endpoint, Explorer, Signer};
use crate::names::{self, CONTEXT_TOKENS};
use crate::paths;
use crate::secrets::{BUILTIN_VARS, SignerSecret};
use crate::store::{StorePaths, WriteLock};
use crate::template;
use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use toml_edit::DocumentMut;

/// v1 `config.json`.
#[derive(Deserialize)]
struct V1Config {
    #[serde(default)]
    chains: Vec<V1Chain>,
    #[serde(default)]
    variables: BTreeMap<String, String>,
    #[serde(default)]
    keys: BTreeMap<String, V1Key>,
    #[serde(default)]
    default_chain: Option<String>,
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

/// What a migration did. `Display` is the one-line summary.
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

/// Assigns variable names to secret values, reusing an existing variable
/// that already holds the same value.
struct VarTable {
    by_value: HashMap<String, String>,
    names: IndexMap<String, String>,
    created: Vec<String>,
}

impl VarTable {
    fn intern(&mut self, base: &str, secret: &str) -> String {
        if let Some(name) = self.by_value.get(secret) {
            return name.clone();
        }
        let name = (1..)
            .map(|n| {
                if n == 1 {
                    base.to_string()
                } else {
                    format!("{base}_{n}")
                }
            })
            .find(|n| !self.names.contains_key(n))
            .expect("a free name exists");
        self.names.insert(name.clone(), secret.to_string());
        self.by_value.insert(secret.to_string(), name.clone());
        self.created.push(name.clone());
        name
    }
}

/// Find a literal Infura or Alchemy key in `url`: returns the text before
/// and after the key, the base variable name, and the key.
pub fn extract_known_key(url: &str) -> Option<(&str, &str, &'static str, &str)> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let (marker, base) = if labels.contains(&"infura") {
        ("/v3/", "INFURA_KEY")
    } else if labels.contains(&"alchemy") {
        ("/v2/", "ALCHEMY_KEY")
    } else {
        return None;
    };
    let start = url.find(marker)? + marker.len();
    let rest = &url[start..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let key = &rest[..end];
    if key.len() < 8
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some((&url[..start], &rest[end..], base, key))
}

/// Heuristic: does `url` still embed a literal credential (userinfo, a
/// key-like query value, or a long mixed alphanumeric path segment)?
pub fn inline_credential(url: &str) -> Option<&'static str> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Some("user info");
    }
    for (name, value) in parsed.query_pairs() {
        let name = name.to_ascii_lowercase();
        if !template::has_templates(&value)
            && !value.is_empty()
            && ["key", "token", "secret", "auth", "password", "apikey"]
                .iter()
                .any(|k| name.contains(k))
        {
            return Some("query parameter");
        }
    }
    let decoded = parsed.path().replace("%7B", "{").replace("%7D", "}");
    for segment in decoded.split('/') {
        if segment.len() >= 16
            && !template::has_templates(segment)
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            && segment.bytes().any(|b| b.is_ascii_digit())
            && segment.bytes().any(|b| b.is_ascii_alphabetic())
        {
            return Some("path segment");
        }
    }
    None
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

fn signer_name(raw: &str, taken: &HashSet<String>) -> String {
    let mut base = names::slugify(raw);
    if CONTEXT_TOKENS.contains(&base.as_str()) || base == "key" {
        base.push_str("-signer");
    }
    names::unique_name(&base, |n| taken.contains(n))
}

/// Compute the migration of v1 JSON text without touching the filesystem.
pub fn plan(v1_json: &str) -> Result<Plan> {
    let v1: V1Config = serde_json::from_str(v1_json).map_err(|e| {
        anyhow!(
            "failed to parse the v1 config: {:?} error at line {}, column {}",
            e.classify(),
            e.line(),
            e.column()
        )
    })?;
    let mut summary = Summary::default();
    let mut vars = VarTable {
        by_value: HashMap::new(),
        names: IndexMap::new(),
        created: Vec::new(),
    };

    // Variables (step 6), sanitizing names v2 would reject.
    let mut var_renames: HashMap<String, String> = HashMap::new();
    for (name, value) in &v1.variables {
        let mut clean: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if clean.is_empty() || clean.starts_with(|c: char| c.is_ascii_digit()) {
            clean.insert(0, '_');
        }
        if BUILTIN_VARS.contains(&clean.as_str()) {
            clean.push_str("_VAR");
        }
        let clean = names::unique_name(&clean, |n| vars.names.contains_key(n));
        if clean != *name {
            summary.attention.push(format!(
                "variable '{name}' was renamed to '{clean}'; update any ${{{name}}} references"
            ));
            var_renames.insert(name.clone(), clean.clone());
        }
        vars.by_value
            .entry(value.clone())
            .or_insert_with(|| clean.clone());
        vars.names.insert(clean, value.clone());
        summary.variables += 1;
    }

    // Signers.
    let mut config = DocumentMut::new();
    config.decor_mut().set_prefix(
        "# chainz configuration (v2), migrated from the v1 config.json.\n# Secrets live in secrets.toml; reference them as ${NAME}.\n",
    );
    config["version"] = toml_edit::value(2);
    let mut secrets = DocumentMut::new();
    let mut signer_names: HashMap<String, String> = HashMap::new();
    let mut taken: HashSet<String> = HashSet::new();
    let mut signer_records: Vec<(String, Signer, Option<SignerSecret>)> = Vec::new();
    for (raw_name, key) in &v1.keys {
        let name = signer_name(raw_name, &taken);
        taken.insert(name.clone());
        if name != *raw_name {
            summary
                .attention
                .push(format!("key '{raw_name}' became signer '{name}'"));
        }
        signer_names.insert(raw_name.clone(), name.clone());
        let mut address = key
            .address
            .as_deref()
            .and_then(|a| a.parse::<Address>().ok());
        let mut signer = Signer {
            backend: Backend::Keyring,
            address: None,
            service: None,
            username: None,
            path: None,
            vault: None,
            item: None,
        };
        let mut secret = None;
        match &key.kind {
            V1KeyKind::Keyring { service, username } => {
                signer.backend = Backend::Keyring;
                if service != crate::model::KEYRING_SERVICE {
                    signer.service = Some(service.clone());
                }
                if *username != name {
                    signer.username = Some(username.clone());
                }
            }
            V1KeyKind::PrivateKey { value } => {
                signer.backend = Backend::Plaintext;
                address = address.or_else(|| Address::from_private_key(value).ok());
                let mut s = SignerSecret::default();
                s.private_key = Some(value.trim().to_string());
                secret = Some(s);
                summary.attention.push(format!(
                    "signer '{name}' is plaintext; move it with `chainz signer migrate {name}`"
                ));
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
                signer.backend = Backend::Encrypted;
                let mut s = SignerSecret::default();
                s.ciphertext = Some(value.clone());
                s.nonce = Some(nonce.clone());
                s.salt = Some(salt.clone());
                s.version = Some(version.unwrap_or(1));
                s.kdf_memory_kib = *kdf_memory_kib;
                s.kdf_iterations = *kdf_iterations;
                s.kdf_parallelism = *kdf_parallelism;
                secret = Some(s);
            }
            V1KeyKind::OnePassword { vault, item } => {
                signer.backend = Backend::OnePassword;
                signer.vault = Some(vault.clone());
                signer.item = Some(item.clone());
            }
        }
        if address.is_none() {
            summary.attention.push(format!(
                "signer '{name}' has no cached address; @wallet needs one (re-add it or run `chainz signer migrate {name}`)"
            ));
        }
        signer.address = address;
        signer_records.push((name, signer, secret));
        summary.signers += 1;
    }

    // Chains, endpoints, credentials, explorers.
    let mut chain_names: HashMap<String, String> = HashMap::new();
    let mut used_names: HashSet<String> = HashSet::new();
    let mut explorers: IndexMap<String, (String, Explorer)> = IndexMap::new(); // name -> (group key, explorer)
    let mut groups: HashMap<(String, Option<String>), String> = HashMap::new();
    let mut new_chains: Vec<(String, NewChain)> = Vec::new();

    for chain in &v1.chains {
        let base = names::short_name(chain.chain_id)
            .map(str::to_string)
            .unwrap_or_else(|| names::slugify(&chain.name));
        let name = names::unique_name(&base, |n| {
            used_names.contains(n) || n.parse::<u64>().is_ok()
        });
        used_names.insert(name.clone());
        chain_names.insert(chain.name.to_ascii_lowercase(), name.clone());
        for alias in &chain.aliases {
            chain_names.insert(alias.to_ascii_lowercase(), name.clone());
        }

        let mut aliases: Vec<String> = Vec::new();
        for alias in std::iter::once(&chain.name).chain(chain.aliases.iter()) {
            let alias = alias.trim();
            if alias.is_empty()
                || alias.eq_ignore_ascii_case(&name)
                || alias.parse::<u64>().is_ok()
                || aliases.iter().any(|a| a.eq_ignore_ascii_case(alias))
            {
                continue;
            }
            aliases.push(alias.to_string());
        }

        // Endpoints (step 3), keeping v1 order; the selected URL is added
        // if a legacy config listed it only in selected_rpc.
        let mut raw: Vec<(String, IndexMap<String, String>)> = chain
            .rpc_urls
            .iter()
            .map(|e| match e {
                V1Endpoint::Url(url) => (url.clone(), IndexMap::new()),
                V1Endpoint::Detailed { url, headers } => (
                    url.clone(),
                    headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                ),
            })
            .collect();
        if !chain.selected_rpc.is_empty() && !raw.iter().any(|(u, _)| *u == chain.selected_rpc) {
            raw.push((chain.selected_rpc.clone(), IndexMap::new()));
        }
        if raw.is_empty() {
            bail!(
                "v1 chain '{}' has no RPC URLs; fix config.json and retry",
                chain.name
            );
        }
        let mut rpcs: Vec<(String, Endpoint)> = Vec::new();
        let mut active = None;
        for (url, headers) in raw {
            let base = names::endpoint_name_for_url(&url);
            let ep_name = names::unique_name(&base, |n| rpcs.iter().any(|(e, _)| e == n));
            if url == chain.selected_rpc && active.is_none() {
                active = Some(ep_name.clone());
            }
            let mut new_url = url.clone();
            if let Some((before, after, var_base, key)) = extract_known_key(&url) {
                let var = vars.intern(var_base, key);
                new_url = format!("{before}${{{var}}}{after}");
            }
            for (old, new) in &var_renames {
                new_url = new_url.replace(&format!("${{{old}}}"), &format!("${{{new}}}"));
            }
            if let Some(kind) = inline_credential(&new_url) {
                summary.attention.push(format!(
                    "chain '{name}' endpoint '{ep_name}' embeds a credential ({kind}); move it into a variable"
                ));
            }
            let mut endpoint = Endpoint::new(new_url);
            for (k, mut v) in headers {
                for (old, new) in &var_renames {
                    v = v.replace(&format!("${{{old}}}"), &format!("${{{new}}}"));
                }
                if !template::has_templates(&v) {
                    summary.attention.push(format!(
                        "chain '{name}' endpoint '{ep_name}' header '{k}' holds a literal value; move it into a variable"
                    ));
                }
                endpoint.headers.insert(k, v);
            }
            rpcs.push((ep_name, endpoint));
            summary.endpoints += 1;
        }
        let rpc = active.unwrap_or_else(|| rpcs[0].0.clone());

        // Explorer (step 5).
        let explorer = match (&chain.verification_url, &chain.verification_api_key) {
            (None, None) => None,
            (url, key) => {
                let key_value = key.as_ref().filter(|k| !k.trim().is_empty());
                let Some(url) = url.as_ref().filter(|u| !u.trim().is_empty()) else {
                    if let Some(key) = key_value {
                        let var = vars.intern(
                            &format!(
                                "{}_VERIFIER_API_KEY",
                                name.to_ascii_uppercase().replace('-', "_")
                            ),
                            key,
                        );
                        summary.attention.push(format!(
                            "chain '{name}' had a verifier key but no URL; the key is kept as ${{{var}}} with no explorer attached"
                        ));
                    }
                    new_chains.push((
                        name.clone(),
                        NewChain {
                            id: chain.chain_id,
                            aliases,
                            signer: None,
                            explorer: None,
                            rpc,
                            failover: vec![],
                            rpcs,
                        },
                    ));
                    set_signer_ref(&mut new_chains, chain, &signer_names, &mut summary);
                    summary.chains += 1;
                    continue;
                };
                let api = normalize_explorer_api(url, chain.chain_id);
                let group_key = (api.clone(), key_value.cloned());
                match groups.get(&group_key) {
                    Some(existing) => Some(existing.clone()),
                    None => {
                        let host_label = names::endpoint_name_for_url(&api);
                        let host_label = host_label.trim_end_matches("-ws");
                        let exp_name = names::unique_name(
                            if host_label == "rpc" || host_label == "local" {
                                "explorer"
                            } else {
                                host_label
                            },
                            |n| explorers.contains_key(n),
                        );
                        let key_ref = key_value.map(|k| {
                            if template::has_templates(k) {
                                k.clone()
                            } else {
                                let base = if exp_name == "etherscan" {
                                    "ETHERSCAN_API_KEY".to_string()
                                } else {
                                    format!(
                                        "{}_API_KEY",
                                        exp_name.to_ascii_uppercase().replace('-', "_")
                                    )
                                };
                                format!("${{{}}}", vars.intern(&base, k))
                            }
                        });
                        explorers.insert(
                            exp_name.clone(),
                            (
                                String::new(),
                                Explorer {
                                    api,
                                    key: key_ref,
                                    url: None,
                                },
                            ),
                        );
                        groups.insert(group_key, exp_name.clone());
                        Some(exp_name)
                    }
                }
            }
        };
        new_chains.push((
            name.clone(),
            NewChain {
                id: chain.chain_id,
                aliases,
                signer: None,
                explorer,
                rpc,
                failover: vec![],
                rpcs,
            },
        ));
        set_signer_ref(&mut new_chains, chain, &signer_names, &mut summary);
        summary.chains += 1;
    }

    if let Some(default) = &v1.default_chain {
        match chain_names.get(&default.to_ascii_lowercase()) {
            Some(name) => config["default"] = toml_edit::value(name.as_str()),
            None => summary.attention.push(format!(
                "default chain '{default}' was not found and was dropped"
            )),
        }
    }
    for (name, (_, explorer)) in &explorers {
        edit::set_explorer(&mut config, name, explorer)?;
    }
    for (name, chain) in &new_chains {
        edit::insert_chain(&mut config, name, chain)?;
    }
    for (name, signer, secret) in &signer_records {
        edit::set_signer(&mut config, name, signer)?;
        if let Some(secret) = secret {
            edit::set_signer_secret(&mut secrets, name, secret)?;
        }
    }
    for (name, value) in &vars.names {
        edit::set_var(&mut secrets, name, value)?;
    }
    summary.extracted = vars.created;
    if !secrets.is_empty() {
        secrets
            .decor_mut()
            .set_prefix("# chainz secrets (mode 0600). Never share this file.\n");
    }

    let config_toml = config.to_string();
    let secrets_toml = secrets.to_string();
    // The result must load and validate like any hand-written config.
    let parsed = crate::store::parse_config(&config_toml, std::path::Path::new("config.toml"))?;
    let parsed_secrets = crate::secrets::Secrets::parse(&secrets_toml).map_err(|e| {
        anyhow!(
            "migrated secrets do not parse: {}",
            crate::store::redact_toml_error(&e)
        )
    })?;
    crate::validate::validate(&parsed, Some(&parsed_secrets))
        .context("migrated config is invalid")?;
    Ok(Plan {
        config_toml,
        secrets_toml,
        summary,
    })
}

fn set_signer_ref(
    chains: &mut [(String, NewChain)],
    v1: &V1Chain,
    signer_names: &HashMap<String, String>,
    summary: &mut Summary,
) {
    let Some(key_name) = &v1.key_name else { return };
    let (name, chain) = chains.last_mut().expect("just pushed");
    match signer_names.get(key_name) {
        Some(signer) => chain.signer = Some(signer.clone()),
        None => summary.attention.push(format!(
            "chain '{name}' referenced missing key '{key_name}'; no signer attached"
        )),
    }
}

/// Backup name for a migrated v1 file.
fn backup_path(source: &std::path::Path) -> std::path::PathBuf {
    let dir = source.parent().unwrap_or(std::path::Path::new("."));
    if source.file_name().is_some_and(|n| n == "config.json") {
        dir.join("config.v1.json.bak")
    } else {
        dir.join(".chainz.v1.json.bak")
    }
}

/// Migrate while already holding the write lock. Returns `None` when there
/// was nothing to do (config.toml exists, or no v1 file).
pub fn migrate_locked(paths: &StorePaths) -> Result<Option<Summary>> {
    if paths.config.exists() {
        return Ok(None);
    }
    let Some(source) = paths.v1_source() else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&source)
        .with_context(|| format!("failed to read {}", source.display()))?;
    let plan = plan(&text).with_context(|| format!("failed to migrate {}", source.display()))?;
    paths::ensure_private_dir(&paths.dir)?;
    if !plan.secrets_toml.is_empty() {
        paths::write_atomically(&paths.secrets, plan.secrets_toml.as_bytes())
            .with_context(|| format!("failed to write {}", paths.secrets.display()))?;
    }
    paths::write_atomically(&paths.config, plan.config_toml.as_bytes())
        .with_context(|| format!("failed to write {}", paths.config.display()))?;
    let backup = backup_path(&source);
    std::fs::rename(&source, &backup).with_context(|| {
        format!(
            "failed to rename {} to {}",
            source.display(),
            backup.display()
        )
    })?;
    Ok(Some(plan.summary))
}

/// Migrate on first run: take the lock, re-check, migrate, and report on
/// stderr. Safe to call from many processes at once.
pub fn auto_migrate(paths: &StorePaths) -> Result<()> {
    let _lock = WriteLock::acquire(paths)?;
    if let Some(summary) = migrate_locked(paths)? {
        report(&summary, paths);
    }
    Ok(())
}

/// Print a migration summary to stderr.
pub fn report(summary: &Summary, paths: &StorePaths) {
    eprintln!("chainz: {summary}");
    eprintln!(
        "chainz: wrote {} (v1 config kept as config.v1.json.bak)",
        paths.config.display()
    );
    for item in &summary.attention {
        eprintln!("chainz:   - {item}");
    }
}

#[cfg(test)]
mod tests;
