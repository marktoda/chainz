//! Filesystem locations (XDG layout; see the design's "File layout").
//!
//! | Path | Holds |
//! | --- | --- |
//! | `$XDG_CONFIG_HOME/chainz/config.toml` | chains, endpoints, explorers, signer references, labels |
//! | `$XDG_CONFIG_HOME/chainz/secrets.toml` | variable values, plaintext/encrypted keys (0600) |
//! | `$XDG_STATE_HOME/chainz/` | endpoint health history, logs |
//! | `$XDG_RUNTIME_DIR/chainz/` (else the state dir) | gateway and fork registry |
//! | `$XDG_CACHE_HOME/chainz/` | chainlist |
//!
//! Every directory falls back to the XDG default under `$HOME` on all
//! platforms (v1 used `~/.config` on macOS too, and so does v2).

use std::io;
use std::path::{Path, PathBuf};

fn home() -> io::Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))
}

/// `$var` when it is set to an absolute path.
fn xdg_var(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|dir| !dir.is_empty() && Path::new(dir).is_absolute())
        .map(PathBuf::from)
}

fn xdg(var: &str, fallback: &str) -> io::Result<PathBuf> {
    match xdg_var(var) {
        Some(dir) => Ok(dir),
        None => Ok(home()?.join(fallback)),
    }
}

/// `$XDG_CONFIG_HOME/chainz`.
pub fn config_dir() -> io::Result<PathBuf> {
    Ok(xdg("XDG_CONFIG_HOME", ".config")?.join("chainz"))
}

/// `$XDG_STATE_HOME/chainz`.
pub fn state_dir() -> io::Result<PathBuf> {
    Ok(xdg("XDG_STATE_HOME", ".local/state")?.join("chainz"))
}

/// `$XDG_CACHE_HOME/chainz`.
pub fn cache_dir() -> io::Result<PathBuf> {
    Ok(xdg("XDG_CACHE_HOME", ".cache")?.join("chainz"))
}

/// `$XDG_RUNTIME_DIR/chainz`, falling back to the state dir when the
/// runtime dir is unset (macOS).
pub fn runtime_dir() -> io::Result<PathBuf> {
    match xdg_var("XDG_RUNTIME_DIR") {
        Some(dir) => Ok(dir.join("chainz")),
        None => state_dir(),
    }
}

/// The pre-0.3 v1 config location, `~/.chainz.json`.
pub(crate) fn legacy_home_config() -> io::Result<PathBuf> {
    Ok(home()?.join(".chainz.json"))
}

/// Expand a leading `~/` against `$HOME`.
pub fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home()
            .map(|h| h.join(rest))
            .unwrap_or_else(|_| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

/// Read a private file with [`chainz_sys::read_private`], printing its
/// permission warning to stderr. `Ok(None)` when the file does not exist.
///
/// Transitional (plan D2, stream S1): core should return the warning as a
/// notice for the bin to print instead.
pub fn read_private(path: &Path) -> io::Result<Option<String>> {
    Ok(chainz_sys::read_private(path)?.map(|read| {
        if let Some(warning) = read.warning {
            eprintln!("chainz: warning: {warning}");
        }
        read.text
    }))
}
