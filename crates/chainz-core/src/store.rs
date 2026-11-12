//! Loading and transactional writes of `config.toml` and `secrets.toml`.
//!
//! - **Reads take no lock.** Each file is only ever replaced by an atomic
//!   rename, so a reader always sees a whole file. `secrets.toml` is read
//!   lazily, only when a `${VAR}` misses the environment or key material is
//!   needed.
//! - **Writes lock.** [`Chainz::transact`] takes an exclusive `flock` on
//!   `config.lock`, re-reads both files, applies the edit through a
//!   [`Tx`], validates, then writes `secrets.toml` first and `config.toml`
//!   second, each fsync'd and atomically renamed.
//! - **v1 migration** runs under the same lock with a re-check, the first
//!   time a v1 `config.json` exists and `config.toml` does not.

use crate::migrate;
use crate::model::{CONFIG_VERSION, Config, Endpoint};
use crate::paths;
use crate::secrets::{Builtins, Secrets, VarResolver};
use crate::tx::Tx;
use crate::validate;
use anyhow::{Context, Result, anyhow, bail};
use std::cell::OnceCell;
use std::path::{Path, PathBuf};

/// Text of a brand-new config file: `header` (comment lines), then the
/// version.
pub(crate) fn new_config_text(header: &str) -> String {
    format!("{header}version = {CONFIG_VERSION}\n")
}

/// Text of a brand-new hand-editable config file.
fn new_config() -> String {
    new_config_text("# chainz configuration (v2). Hand-editable; secrets live in secrets.toml.\n")
}

/// Where the config files live.
#[derive(Debug, Clone)]
pub struct StorePaths {
    /// The config directory.
    pub dir: PathBuf,
    /// `config.toml`.
    pub config: PathBuf,
    /// `secrets.toml`.
    pub secrets: PathBuf,
    /// `config.lock`, the cross-process write lock.
    pub lock: PathBuf,
    /// v1 `config.json`.
    pub v1_config: PathBuf,
    /// v1 pre-0.3 `~/.chainz.json`.
    pub v1_legacy: Option<PathBuf>,
}

impl StorePaths {
    /// The standard XDG locations.
    pub fn resolve() -> Result<Self> {
        let dir = paths::config_dir().context("unable to find the config directory")?;
        let mut paths = Self::in_dir(dir);
        paths.v1_legacy = paths::legacy_home_config().ok();
        Ok(paths)
    }

    /// Paths rooted at `dir` (no legacy home file).
    pub fn in_dir(dir: PathBuf) -> Self {
        Self {
            config: dir.join("config.toml"),
            secrets: dir.join("secrets.toml"),
            lock: dir.join("config.lock"),
            v1_config: dir.join("config.json"),
            v1_legacy: None,
            dir,
        }
    }

    /// The v1 file to migrate from, if one exists.
    pub fn v1_source(&self) -> Option<PathBuf> {
        if self.v1_config.exists() {
            return Some(self.v1_config.clone());
        }
        self.v1_legacy.clone().filter(|p| p.exists())
    }
}

/// The exclusive cross-process write lock; released on drop.
pub struct WriteLock(std::fs::File);

impl WriteLock {
    /// Block until the lock at `paths.lock` is held.
    pub fn acquire(paths: &StorePaths) -> Result<Self> {
        chainz_sys::create_private_dir(&paths.dir)
            .with_context(|| format!("failed to create {}", paths.dir.display()))?;
        let file = chainz_sys::open_private(&paths.lock)
            .with_context(|| format!("failed to open {}", paths.lock.display()))?;
        file.lock()
            .with_context(|| format!("failed to lock {}", paths.lock.display()))?;
        Ok(Self(file))
    }
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Parse config text into the typed view.
pub fn parse_config(text: &str, path: &Path) -> Result<Config> {
    Config::parse(text).map_err(|e| {
        anyhow!(
            "failed to parse {}: {} (fix it with `chainz config edit`)",
            path.display(),
            redact_toml_error(&e)
        )
    })
}

fn read_secrets(path: &Path) -> Result<Secrets> {
    match paths::read_private(path).with_context(|| format!("failed to read {}", path.display()))? {
        None => Ok(Secrets::default()),
        Some(text) => Secrets::parse(&text).map_err(|e| {
            anyhow!(
                "failed to parse {}: {}",
                path.display(),
                redact_toml_error(&e)
            )
        }),
    }
}

/// TOML parse errors quote the offending line, which may hold a secret
/// (an inline credential, a variable value): keep only message and position.
pub fn redact_toml_error(error: &toml::de::Error) -> String {
    redact_parse_error(error.message(), error.span())
}

/// [`redact_toml_error`] for any TOML error's message and span (the
/// `toml_edit` parser and deserializer errors quote the source line too).
pub(crate) fn redact_parse_error(message: &str, span: Option<std::ops::Range<usize>>) -> String {
    match span {
        Some(span) => format!("{message} (at byte {})", span.start),
        None => message.to_string(),
    }
}

/// An endpoint with every `${VAR}` expanded: what is actually sent.
/// `Debug` shows the redacted URL and header names only.
#[derive(Clone, PartialEq, Eq)]
pub struct ExpandedEndpoint {
    /// The URL, secrets expanded.
    pub url: String,
    /// Header names and expanded values, in config order.
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for ExpandedEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExpandedEndpoint")
            .field("url", &crate::redact::redact(&self.url))
            .field(
                "headers",
                &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// A loaded configuration: the typed config plus lazily loaded secrets.
pub struct Chainz {
    paths: StorePaths,
    config: Config,
    exists: bool,
    secrets: OnceCell<Secrets>,
}

impl std::fmt::Debug for Chainz {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chainz")
            .field("paths", &self.paths)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// How strictly to load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadMode {
    /// Validate invariants (every command except doctor).
    Strict,
    /// Parse only, so `doctor` can report invariant violations.
    Lenient,
}

impl Chainz {
    /// Load from the standard locations, migrating v1 first if needed.
    pub fn open() -> Result<Self> {
        Self::open_at(StorePaths::resolve()?, LoadMode::Strict)
    }

    /// Load from `paths`.
    pub fn open_at(paths: StorePaths, mode: LoadMode) -> Result<Self> {
        let text = match paths::read_private(&paths.config)
            .with_context(|| format!("failed to read {}", paths.config.display()))?
        {
            Some(text) => Some(text),
            None if paths.v1_source().is_some() => {
                migrate::auto_migrate(&paths)?;
                paths::read_private(&paths.config)?
            }
            None => None,
        };
        let exists = text.is_some();
        let config = match &text {
            Some(text) => parse_config(text, &paths.config)?,
            None => Config::empty(),
        };
        if mode == LoadMode::Strict {
            validate::validate(&config, None).with_context(|| {
                format!(
                    "invalid config at {} (run `chainz doctor`)",
                    paths.config.display()
                )
            })?;
        }
        Ok(Self {
            paths,
            config,
            exists,
            secrets: OnceCell::new(),
        })
    }

    /// The typed config.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// File locations.
    pub fn paths(&self) -> &StorePaths {
        &self.paths
    }

    /// Whether `config.toml` exists.
    pub fn exists(&self) -> bool {
        self.exists
    }

    /// `secrets.toml`, read on first use.
    pub fn secrets(&self) -> Result<&Secrets> {
        if let Some(secrets) = self.secrets.get() {
            return Ok(secrets);
        }
        let loaded = read_secrets(&self.paths.secrets)?;
        Ok(self.secrets.get_or_init(|| loaded))
    }

    /// Expand the URL and header values of `endpoint`, named `endpoint_name`
    /// on chain `chain_name` (ID `chain_id`): the built-ins `${CHAIN}` and
    /// `${CHAIN_ID}`, then variables as [`VarResolver`] looks them up
    /// (`secrets.toml` is read only if needed). The chain need not be
    /// configured yet (`chain add` checks an endpoint before writing it).
    /// Errors name the chain, endpoint, header and variable, never a value.
    pub fn expand_endpoint(
        &self,
        chain_name: &str,
        chain_id: u64,
        endpoint_name: &str,
        endpoint: &Endpoint,
    ) -> Result<ExpandedEndpoint> {
        let loader = || self.secrets();
        let vars = VarResolver::new(
            Builtins {
                chain_id: Some(chain_id),
                chain: Some(chain_name),
            },
            &loader,
        );
        let url = vars
            .expand(&endpoint.url)
            .with_context(|| format!("chain '{chain_name}' endpoint '{endpoint_name}' URL"))?
            .into_owned();
        let mut headers = Vec::with_capacity(endpoint.headers.len());
        for (name, value) in &endpoint.headers {
            let value = vars
                .expand(value)
                .with_context(|| {
                    format!("chain '{chain_name}' endpoint '{endpoint_name}' header '{name}'")
                })?
                .into_owned();
            headers.push((name.clone(), value));
        }
        Ok(ExpandedEndpoint { url, headers })
    }

    /// Run one locked read-modify-write transaction. `edit` mutates the
    /// documents; the result is validated and written (secrets first) only
    /// if it changed. On any error nothing is written.
    pub fn transact<T>(&mut self, edit: impl FnOnce(&mut Tx) -> Result<T>) -> Result<T> {
        let _lock = WriteLock::acquire(&self.paths)?;
        if !self.paths.config.exists() && self.paths.v1_source().is_some() {
            migrate::migrate_locked(&self.paths)?;
        }
        let config_text = paths::read_private(&self.paths.config)?;
        let secrets_text = paths::read_private(&self.paths.secrets)?;
        let config_orig = config_text.clone().unwrap_or_else(new_config);
        let secrets_orig = secrets_text.clone().unwrap_or_default();
        let mut tx = Tx::parse(
            &config_orig,
            &self.paths.config,
            &secrets_orig,
            &self.paths.secrets,
        )?;
        let out = edit(&mut tx)?;
        let (new_config, new_secrets) = tx.texts();
        let config = parse_config(&new_config, &self.paths.config)?;
        let secrets = Secrets::parse(&new_secrets).map_err(|e| {
            anyhow!(
                "refusing to write invalid secrets: {}",
                redact_toml_error(&e)
            )
        })?;
        validate::validate(&config, Some(&secrets))
            .context("refusing to write an invalid config")?;
        if new_secrets != secrets_orig {
            chainz_sys::write_private(&self.paths.secrets, new_secrets.as_bytes())
                .with_context(|| format!("failed to write {}", self.paths.secrets.display()))?;
        }
        if new_config != config_orig || config_text.is_none() {
            chainz_sys::write_private(&self.paths.config, new_config.as_bytes())
                .with_context(|| format!("failed to write {}", self.paths.config.display()))?;
        }
        self.config = config;
        self.exists = true;
        self.secrets = OnceCell::from(secrets);
        Ok(out)
    }

    /// The text of `config.toml` as it is on disk now (a new file's text
    /// if there is none yet), to edit by hand and hand back to
    /// [`replace_text`](Self::replace_text).
    pub fn config_text(&self) -> Result<String> {
        Ok(paths::read_private(&self.paths.config)
            .with_context(|| format!("failed to read {}", self.paths.config.display()))?
            .unwrap_or_else(new_config))
    }

    /// Replace `config.toml` with `new`, an edited copy of `expected` (from
    /// [`config_text`](Self::config_text)); refused if the file changed
    /// meanwhile.
    pub fn replace_text(&mut self, expected: &str, new: &str) -> Result<()> {
        self.transact(|tx| {
            unchanged_since(tx, expected)?;
            tx.replace_config_text(new)
        })
    }

    /// Chain `name`'s tables as a standalone document to edit by hand,
    /// with the config text it was cut from (for
    /// [`replace_chain_snippet`](Self::replace_chain_snippet)).
    pub fn chain_snippet(&self, name: &str) -> Result<ChainSnippet> {
        let config = self.config_text()?;
        let tx = Tx::parse(&config, &self.paths.config, "", &self.paths.secrets)?;
        Ok(ChainSnippet {
            chain: name.to_string(),
            text: tx.chain_snippet(name)?,
            config,
        })
    }

    /// Replace a chain's tables with an edited [`ChainSnippet`]; refused
    /// if `config.toml` changed since the snippet was cut.
    pub fn replace_chain_snippet(&mut self, snippet: &ChainSnippet, edited: &str) -> Result<()> {
        self.transact(|tx| {
            unchanged_since(tx, &snippet.config)?;
            tx.replace_chain_snippet(&snippet.chain, edited)
        })
    }
}

/// One chain's tables cut from `config.toml` by [`Chainz::chain_snippet`].
#[derive(Debug, Clone)]
pub struct ChainSnippet {
    chain: String,
    config: String,
    /// The snippet to edit.
    pub text: String,
}

fn unchanged_since(tx: &Tx, expected: &str) -> Result<()> {
    if tx.texts().0 != expected {
        bail!("config.toml changed while you were editing; run the command again");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transactions_write_secrets_then_config_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::in_dir(dir.path().join("chainz"));
        let mut chainz = Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
        assert!(!chainz.exists());
        chainz.transact(|tx| tx.set_var("K", "value")).unwrap();
        assert!(paths.secrets.exists() && paths.config.exists());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&paths.secrets)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let reloaded = Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
        assert_eq!(reloaded.secrets().unwrap().vars["K"], "value");

        // An invalid edit writes nothing.
        let before = std::fs::read_to_string(&paths.config).unwrap();
        let err = chainz
            .transact(|tx| {
                tx.set_default(Some("ghost"));
                Ok(())
            })
            .unwrap_err();
        assert!(format!("{err:#}").contains("ghost"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&paths.config).unwrap(), before);
    }

    #[test]
    fn expand_endpoint_expands_url_and_headers() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::in_dir(dir.path().to_path_buf());
        std::fs::write(
            &paths.config,
            "version = 2\n[chains.eth]\nid = 1\nrpc = \"gw\"\nrpcs.gw = { url = \"https://gw.example/${CHAIN}/${CHAIN_ID}\", headers = { x-key = \"${STORE_TEST_GW}\" } }\nrpcs.bad = \"https://x.example/${STORE_TEST_UNSET}\"\n",
        )
        .unwrap();
        std::fs::write(&paths.secrets, "[vars]\nSTORE_TEST_GW = \"gw-secret\"\n").unwrap();
        let chainz = Chainz::open_at(paths, LoadMode::Strict).unwrap();
        let chain = &chainz.config().chains["eth"];
        let gw = chainz
            .expand_endpoint("eth", chain.id, "gw", &chain.rpcs["gw"])
            .unwrap();
        assert_eq!(gw.url, "https://gw.example/eth/1");
        assert_eq!(gw.headers, [("x-key".to_string(), "gw-secret".to_string())]);
        assert!(!format!("{gw:?}").contains("gw-secret"));
        let err = chainz
            .expand_endpoint("eth", chain.id, "bad", &chain.rpcs["bad"])
            .unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains("endpoint 'bad' URL") && err.contains("STORE_TEST_UNSET"),
            "{err}"
        );
    }

    #[test]
    fn loose_permissions_are_tightened_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::in_dir(dir.path().to_path_buf());
        std::fs::write(&paths.config, new_config()).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&paths.config, std::fs::Permissions::from_mode(0o644)).unwrap();
        Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
        let mode = std::fs::metadata(&paths.config)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn transaction_parse_errors_do_not_echo_the_source_line() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::in_dir(dir.path().to_path_buf());
        std::fs::write(&paths.config, new_config()).unwrap();
        let mut chainz = Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
        // Broken by hand after loading: transact re-reads and re-parses.
        std::fs::write(
            &paths.config,
            "version = 2\nx = \"https://rpc.example/v3/super-secret-key\" trailing\n",
        )
        .unwrap();
        let err = format!("{:#}", chainz.transact(|_| Ok(())).unwrap_err());
        assert!(!err.contains("super-secret-key"), "{err}");
        assert!(err.contains("config.toml"), "{err}");

        std::fs::write(&paths.config, new_config()).unwrap();
        std::fs::write(&paths.secrets, "K = \"super-secret-var\" trailing\n").unwrap();
        let err = format!("{:#}", chainz.transact(|_| Ok(())).unwrap_err());
        assert!(!err.contains("super-secret-var"), "{err}");
    }

    #[test]
    fn secrets_parse_errors_do_not_echo_values() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::in_dir(dir.path().to_path_buf());
        std::fs::write(&paths.config, new_config()).unwrap();
        std::fs::write(
            &paths.secrets,
            "[vars]\nK = \"super-secret-value\" trailing\n",
        )
        .unwrap();
        let chainz = Chainz::open_at(paths, LoadMode::Strict).unwrap();
        let err = format!("{:#}", chainz.secrets().unwrap_err());
        assert!(!err.contains("super-secret-value"), "{err}");
    }
}
