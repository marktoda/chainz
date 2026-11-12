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

pub(crate) mod files;

pub use files::{StorePaths, StoreStamp, WriteLock};

use crate::contract::Env;
use crate::migrate::{self, Summary};
use crate::model::{Config, Endpoint};
use crate::secrets::{Builtins, Secrets, VarResolver};
use crate::tx::Tx;
use crate::validate;
use anyhow::{Context, Result, anyhow, bail};
use chainz_os::PermissionWarning;
use files::{Access, new_config, parse_config};
use std::cell::{OnceCell, RefCell};
use std::fmt;
use std::path::{Path, PathBuf};

/// Something a load or write did that the user should hear about. Core
/// never prints: notices queue on the [`Chainz`] until the caller takes
/// them ([`Chainz::take_notices`]) or names a sink
/// ([`Chainz::set_notice_sink`]). `Display` is one or more lines.
#[derive(Debug)]
pub enum Notice {
    /// A private file has loose permissions that could not be tightened
    /// to 0600 (it was still read).
    Permissions(PermissionWarning),
    /// The v1 config was migrated (on first load, or inside a write).
    Migrated {
        /// What the migration did.
        summary: Summary,
        /// The `config.toml` it wrote.
        config: PathBuf,
    },
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Notice::Permissions(warning) => write!(f, "warning: {warning}"),
            Notice::Migrated { summary, config } => {
                f.write_str(&summary.report(Some(config)).join("\n"))
            }
        }
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
            .field("url", &crate::redact::url(&self.url, false))
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
    access: Access,
    secrets: OnceCell<Secrets>,
    notices: RefCell<Vec<Notice>>,
    sink: Option<fn(&Notice)>,
    env: Env,
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
    /// Parse only, and change nothing on disk: no v1 migration, no
    /// permission tightening (shell completion, see [`Chainz::peek`]).
    Peek,
}

impl Chainz {
    /// Load from the standard locations, migrating v1 first if needed.
    pub fn open() -> Result<Self> {
        Self::open_at(StorePaths::resolve()?, LoadMode::Strict)
    }

    /// Load from the standard locations without changing anything on
    /// disk (see [`LoadMode::Peek`]).
    pub fn peek() -> Result<Self> {
        Self::open_at(StorePaths::resolve()?, LoadMode::Peek)
    }

    /// Load from `paths`.
    pub fn open_at(paths: StorePaths, mode: LoadMode) -> Result<Self> {
        let access = match mode {
            LoadMode::Strict | LoadMode::Lenient => Access::Own,
            LoadMode::Peek => Access::Peek,
        };
        let mut notices = Vec::new();
        let (mut text, warning) = files::read(paths.config(), access)?;
        notices.extend(warning.map(Notice::Permissions));
        if text.is_none() && access == Access::Own && paths.v1_source().is_some() {
            let lock = WriteLock::acquire(&paths)?;
            if let Some(summary) = migrate::migrate_locked(&lock)? {
                notices.push(Notice::Migrated {
                    summary,
                    config: paths.config().to_path_buf(),
                });
            }
            text = files::read(paths.config(), access)?.0;
        }
        let exists = text.is_some();
        let config = match &text {
            Some(text) => parse_config(text, paths.config())?,
            None => Config::empty(),
        };
        if mode == LoadMode::Strict {
            validate::validate(&config, None).with_context(|| {
                format!(
                    "invalid config at {} (run `chainz doctor`)",
                    paths.config().display()
                )
            })?;
        }
        Ok(Self {
            paths,
            config,
            exists,
            access,
            secrets: OnceCell::new(),
            notices: RefCell::new(notices),
            sink: None,
            env: Env::process(),
        })
    }

    /// The environment templates and chain selection read (the process
    /// environment unless [`with_env`](Self::with_env) replaced it).
    pub fn env(&self) -> Env {
        self.env
    }

    /// The same store reading `env` instead of the process environment.
    pub fn with_env(mut self, env: Env) -> Self {
        self.env = env;
        self
    }

    /// Take the notices queued so far.
    pub fn take_notices(&self) -> Vec<Notice> {
        std::mem::take(&mut self.notices.borrow_mut())
    }

    /// Hand every notice to `sink`: the queued ones now, later ones as they
    /// happen.
    pub fn set_notice_sink(&mut self, sink: fn(&Notice)) {
        for notice in self.take_notices() {
            sink(&notice);
        }
        self.sink = Some(sink);
    }

    fn notify(&self, notice: Notice) {
        match self.sink {
            Some(sink) => sink(&notice),
            None => self.notices.borrow_mut().push(notice),
        }
    }

    /// Read a private file, reporting tightened permissions.
    fn read(&self, path: &Path) -> Result<Option<String>> {
        let (text, warning) = files::read(path, self.access)?;
        if let Some(warning) = warning {
            self.notify(Notice::Permissions(warning));
        }
        Ok(text)
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
        let path = self.paths.secrets();
        let loaded = match self.read(path)? {
            None => Secrets::default(),
            Some(text) => Secrets::parse(&text).map_err(|e| {
                anyhow!(
                    "failed to parse {}: {}",
                    path.display(),
                    crate::redact::toml_error(&e)
                )
            })?,
        };
        Ok(self.secrets.get_or_init(|| loaded))
    }

    /// Expand the URL and header values of `endpoint`, named `endpoint_name`
    /// on chain `chain_name` (ID `chain_id`): the built-ins `${CHAIN}` and
    /// `${CHAIN_ID}`, then variables as `VarResolver` looks them up
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
            self.env,
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
        let lock = WriteLock::acquire(&self.paths)?;
        if let Some(summary) = migrate::migrate_locked(&lock)? {
            self.notify(Notice::Migrated {
                summary,
                config: self.paths.config().to_path_buf(),
            });
        }
        let config_text = self.read(self.paths.config())?;
        let secrets_text = self.read(self.paths.secrets())?;
        let config_orig = config_text.clone().unwrap_or_else(new_config);
        let secrets_orig = secrets_text.unwrap_or_default();
        let mut tx = Tx::parse(
            &config_orig,
            self.paths.config(),
            &secrets_orig,
            self.paths.secrets(),
        )?;
        let out = edit(&mut tx)?;
        let (config, secrets) = tx.check(self.paths.config())?;
        let (new_config, new_secrets) = tx.texts();
        // The first write materializes config.toml even when the edit
        // left the new-file text as it was.
        let config_changed = new_config != config_orig || config_text.is_none();
        files::write_pair(
            &self.paths,
            config_changed.then_some(new_config.as_str()),
            (new_secrets != secrets_orig).then_some(new_secrets.as_str()),
        )?;
        drop(lock);
        self.config = config;
        self.exists = true;
        self.secrets = OnceCell::from(secrets);
        Ok(out)
    }

    /// Run `edit` like [`transact`](Self::transact) on the files as they
    /// are now, and check the result, but write nothing: what a
    /// confirmation prompt or a dry run shows.
    pub fn preview<T>(&self, edit: impl FnOnce(&mut Tx) -> Result<T>) -> Result<T> {
        let config_text = self.read(self.paths.config())?.unwrap_or_else(new_config);
        let secrets_text = self.read(self.paths.secrets())?.unwrap_or_default();
        let mut tx = Tx::parse(
            &config_text,
            self.paths.config(),
            &secrets_text,
            self.paths.secrets(),
        )?;
        let out = edit(&mut tx)?;
        tx.check(self.paths.config())?;
        Ok(out)
    }

    /// The text of `config.toml` as it is on disk now (a new file's text
    /// if there is none yet), to edit by hand and hand back to
    /// [`replace_text`](Self::replace_text).
    pub fn config_text(&self) -> Result<String> {
        Ok(self.read(self.paths.config())?.unwrap_or_else(new_config))
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
        let tx = Tx::parse(&config, self.paths.config(), "", self.paths.secrets())?;
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
        assert!(paths.secrets().exists() && paths.config().exists());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(paths.secrets())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let reloaded = Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
        assert_eq!(reloaded.secrets().unwrap().vars["K"], "value");

        // An invalid edit writes nothing.
        let before = std::fs::read_to_string(paths.config()).unwrap();
        let err = chainz
            .transact(|tx| {
                tx.set_default(Some("ghost"));
                Ok(())
            })
            .unwrap_err();
        assert!(format!("{err:#}").contains("ghost"), "{err:#}");
        assert_eq!(std::fs::read_to_string(paths.config()).unwrap(), before);
    }

    #[test]
    fn the_stamp_changes_with_either_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::in_dir(dir.path().join("chainz"));
        let empty = paths.stamp();
        assert_eq!(empty, paths.stamp());
        let mut chainz = Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
        chainz.transact(|tx| tx.set_var("K", "one")).unwrap();
        let written = paths.stamp();
        assert_ne!(written, empty);
        // A secrets-only edit is a change too.
        chainz.transact(|tx| tx.set_var("K", "two")).unwrap();
        assert_ne!(paths.stamp(), written);
    }

    #[test]
    fn expand_endpoint_expands_url_and_headers() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::in_dir(dir.path().to_path_buf());
        std::fs::write(
            paths.config(),
            "version = 2\n[chains.eth]\nid = 1\nrpc = \"gw\"\nrpcs.gw = { url = \"https://gw.example/${CHAIN}/${CHAIN_ID}\", headers = { x-key = \"${STORE_TEST_GW}\" } }\nrpcs.bad = \"https://x.example/${STORE_TEST_UNSET}\"\n",
        )
        .unwrap();
        std::fs::write(paths.secrets(), "[vars]\nSTORE_TEST_GW = \"gw-secret\"\n").unwrap();
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
        std::fs::write(paths.config(), new_config()).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(paths.config(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let chainz = Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
        // Tightened quietly (only a failed chmod is worth a notice).
        assert!(chainz.take_notices().is_empty());
        let mode = std::fs::metadata(paths.config())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn preview_checks_but_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::in_dir(dir.path().to_path_buf());
        let chainz = Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
        assert_eq!(chainz.preview(|tx| tx.set_var("K", "v")).ok(), Some(()));
        assert!(!paths.config().exists() && !paths.secrets().exists());
        let err = chainz
            .preview(|tx| {
                tx.set_default(Some("ghost"));
                Ok(())
            })
            .unwrap_err();
        assert!(format!("{err:#}").contains("ghost"), "{err:#}");
    }

    #[test]
    fn hand_edits_are_refused_when_the_file_changed_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::in_dir(dir.path().to_path_buf());
        let mut chainz = Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
        let original = chainz.config_text().unwrap();
        chainz.transact(|tx| tx.set_var("K", "v")).unwrap();
        let edited = format!("{original}# mine\n");
        chainz.replace_text(&original, &edited).unwrap();
        assert_eq!(std::fs::read_to_string(paths.config()).unwrap(), edited);
        let err = chainz.replace_text(&original, &original).unwrap_err();
        assert!(err.to_string().contains("changed while you were editing"));
    }

    #[test]
    fn transaction_parse_errors_do_not_echo_the_source_line() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::in_dir(dir.path().to_path_buf());
        std::fs::write(paths.config(), new_config()).unwrap();
        let mut chainz = Chainz::open_at(paths.clone(), LoadMode::Strict).unwrap();
        // Broken by hand after loading: transact re-reads and re-parses.
        std::fs::write(
            paths.config(),
            "version = 2\nx = \"https://rpc.example/v3/super-secret-key\" trailing\n",
        )
        .unwrap();
        let err = format!("{:#}", chainz.transact(|_| Ok(())).unwrap_err());
        assert!(!err.contains("super-secret-key"), "{err}");
        assert!(err.contains("config.toml"), "{err}");

        std::fs::write(paths.config(), new_config()).unwrap();
        std::fs::write(paths.secrets(), "K = \"super-secret-var\" trailing\n").unwrap();
        let err = format!("{:#}", chainz.transact(|_| Ok(())).unwrap_err());
        assert!(!err.contains("super-secret-var"), "{err}");
    }

    #[test]
    fn secrets_parse_errors_do_not_echo_values() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StorePaths::in_dir(dir.path().to_path_buf());
        std::fs::write(paths.config(), new_config()).unwrap();
        std::fs::write(
            paths.secrets(),
            "[vars]\nK = \"super-secret-value\" trailing\n",
        )
        .unwrap();
        let chainz = Chainz::open_at(paths, LoadMode::Strict).unwrap();
        let err = format!("{:#}", chainz.secrets().unwrap_err());
        assert!(!err.contains("super-secret-value"), "{err}");
    }
}
