//! The config files themselves: where they live, the write lock, and how
//! their text is read and parsed. A leaf: v1 migration and [`Chainz`]
//! both build on it.
//!
//! [`Chainz`]: super::Chainz

use crate::model::{CONFIG_VERSION, Config};
use crate::paths;
use anyhow::{Context, Result, anyhow};
use chainz_os::{FileStamp, PermissionWarning};
use std::path::{Path, PathBuf};

/// Text of a brand-new config file: `header` (comment lines), then the
/// version.
pub(crate) fn new_config_text(header: &str) -> String {
    format!("{header}version = {CONFIG_VERSION}\n")
}

/// Text of a brand-new hand-editable config file.
pub(crate) fn new_config() -> String {
    new_config_text("# chainz configuration (v2). Hand-editable; secrets live in secrets.toml.\n")
}

/// Where the config files live.
#[derive(Debug, Clone)]
pub struct StorePaths {
    dir: PathBuf,
    config: PathBuf,
    secrets: PathBuf,
    lock: PathBuf,
    v1_config: PathBuf,
    v1_legacy: Option<PathBuf>,
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

    /// The same paths, with `legacy` as the pre-0.3 v1 file.
    #[cfg(test)]
    pub(crate) fn with_legacy(mut self, legacy: PathBuf) -> Self {
        self.v1_legacy = Some(legacy);
        self
    }

    /// The config directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `config.toml`.
    pub fn config(&self) -> &Path {
        &self.config
    }

    /// `secrets.toml`.
    pub fn secrets(&self) -> &Path {
        &self.secrets
    }

    /// v1 `config.json`.
    #[cfg(test)]
    pub(crate) fn v1_config(&self) -> &Path {
        &self.v1_config
    }

    /// The v1 file to migrate from, if one exists.
    pub fn v1_source(&self) -> Option<PathBuf> {
        if self.v1_config.exists() {
            return Some(self.v1_config.clone());
        }
        self.v1_legacy.clone().filter(|p| p.exists())
    }

    /// The files a config is read from, as they are now: compare two
    /// stamps to see that the config changed.
    pub fn stamp(&self) -> StoreStamp {
        StoreStamp {
            config: FileStamp::of(&self.config),
            secrets: FileStamp::of(&self.secrets),
        }
    }
}

/// The [`FileStamp`]s of every file a config is read from
/// (`config.toml` and `secrets.toml`), from [`StorePaths::stamp`]. Two
/// stamps differ whenever any of the files was written, created or
/// removed, even within the same second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreStamp {
    config: Option<FileStamp>,
    secrets: Option<FileStamp>,
}

/// The exclusive cross-process write lock on `config.lock`; released on
/// drop. Holding one is the proof that a locked operation may run.
pub struct WriteLock {
    file: std::fs::File,
    paths: StorePaths,
}

impl WriteLock {
    /// Block until the lock for `paths` is held.
    pub fn acquire(paths: &StorePaths) -> Result<Self> {
        let file = chainz_os::lock_file(&paths.lock)
            .with_context(|| format!("failed to lock {}", paths.lock.display()))?;
        Ok(Self {
            file,
            paths: paths.clone(),
        })
    }

    /// The paths this lock guards.
    pub fn paths(&self) -> &StorePaths {
        &self.paths
    }
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Write the config files that changed: `secrets.toml` first, so a
/// `config.toml` never names a variable that is not there yet. `None`
/// leaves a file as it is.
pub(crate) fn write_pair(
    paths: &StorePaths,
    config: Option<&str>,
    secrets: Option<&str>,
) -> Result<()> {
    for (path, text) in [(paths.secrets(), secrets), (paths.config(), config)] {
        if let Some(text) = text {
            chainz_os::write_private(path, text.as_bytes())
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
    }
    Ok(())
}

/// Parse config text into the typed view.
pub(crate) fn parse_config(text: &str, path: &Path) -> Result<Config> {
    Config::parse(text).map_err(|e| {
        anyhow!(
            "failed to parse {}: {} (fix it with `chainz edit`)",
            path.display(),
            crate::redact::toml_error(&e)
        )
    })
}

/// How a file is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// Tighten loose permissions to 0600 (and report it).
    Own,
    /// Read only: never change the file.
    Peek,
}

/// Read a private file: its text (`None` when it does not exist) and the
/// warning when its permissions were loose and got tightened.
pub(crate) fn read(
    path: &Path,
    access: Access,
) -> Result<(Option<String>, Option<PermissionWarning>)> {
    let read = match access {
        Access::Own => chainz_os::read_private(path).map(|read| match read {
            Some(read) => (Some(read.text), read.warning),
            None => (None, None),
        }),
        Access::Peek => match std::fs::read_to_string(path) {
            Ok(text) => Ok((Some(text), None)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((None, None)),
            Err(e) => Err(e),
        },
    };
    read.with_context(|| format!("failed to read {}", path.display()))
}
