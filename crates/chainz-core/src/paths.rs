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

fn xdg(var: &str, fallback: &str) -> io::Result<PathBuf> {
    match std::env::var_os(var) {
        Some(dir) if !dir.is_empty() && Path::new(&dir).is_absolute() => Ok(PathBuf::from(dir)),
        _ => Ok(home()?.join(fallback)),
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
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() && Path::new(&dir).is_absolute() => {
            Ok(PathBuf::from(dir).join("chainz"))
        }
        _ => state_dir(),
    }
}

/// The pre-0.3 v1 config location, `~/.chainz.json`.
pub fn legacy_home_config() -> io::Result<PathBuf> {
    Ok(home()?.join(".chainz.json"))
}

/// Create `dir` (and parents) and restrict it to the owner.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(dir)?;
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
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

/// Atomically and durably replace `path` with `contents`, mode 0600:
/// temp file in the same directory, fsync, rename, fsync the directory.
pub fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    ensure_private_dir(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut temp = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .tempfile_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    temp.write_all(contents)?;
    temp.as_file().sync_all()?;
    let file = temp.persist(path).map_err(|e| e.error)?;
    file.sync_all()?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Read a file and tighten its permissions to 0600 if group/other bits are
/// set. `Ok(None)` when the file does not exist.
pub fn read_private(path: &Path) -> io::Result<Option<String>> {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = file.metadata()?;
        if meta.permissions().mode() & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(text))
}
