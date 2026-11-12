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

/// Follow `path` through any symlinks to the file they finally name (which
/// need not exist yet). A path that is not a symlink is returned as is.
fn resolve_symlinks(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    // The kernel's own limit on nested links (Linux MAXSYMLINKS).
    for _ in 0..40 {
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&current)?;
                current = match current.parent() {
                    Some(parent) if target.is_relative() => parent.join(target),
                    _ => target,
                };
            }
            Ok(_) => return Ok(current),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other(format!(
        "too many levels of symbolic links at {}",
        path.display()
    )))
}

/// Atomically and durably replace `path` with `contents`, mode 0600:
/// temp file in the same directory, fsync, rename, fsync the directory.
///
/// A symlinked `path` (a dotfiles checkout, say) stays a symlink: the file
/// it points to is replaced instead. Only a directory chainz owns, the one
/// holding a regular `path`, is created and restricted to the owner.
pub fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let target = resolve_symlinks(path)?;
    let dir = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    if target == path {
        ensure_private_dir(dir)?;
    }
    let name = target
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
    temp.persist(&target).map_err(|e| e.error)?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Read a file and tighten its permissions to 0600 if group/other bits are
/// set. `Ok(None)` when the file does not exist.
///
/// Only a file the current user owns is tightened, and a failed `chmod` is
/// a warning on stderr, not an error: a read-only config (a home-manager
/// symlink into `/nix/store`, say) must stay readable.
pub fn read_private(path: &Path) -> io::Result<Option<String>> {
    #[cfg(unix)]
    let uid = current_uid();
    #[cfg(not(unix))]
    let uid = 0;
    read_private_as(path, uid)
}

fn read_private_as(path: &Path, uid: u32) -> io::Result<Option<String>> {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = file.metadata()?;
        if meta.permissions().mode() & 0o077 != 0
            && meta.uid() == uid
            && let Err(e) = file.set_permissions(std::fs::Permissions::from_mode(0o600))
        {
            eprintln!(
                "chainz: warning: could not restrict {} to mode 0600: {e}",
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = uid;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(text))
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn writes_through_a_symlink_to_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("dotfiles");
        std::fs::create_dir(&real_dir).unwrap();
        let real = real_dir.join("config.toml");
        std::fs::write(&real, "old").unwrap();
        let config_dir = dir.path().join("chainz");
        std::fs::create_dir(&config_dir).unwrap();
        let link = config_dir.join("config.toml");
        std::os::unix::fs::symlink("../dotfiles/config.toml", &link).unwrap();

        write_atomically(&link, b"new").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "new");

        // A plain path is still replaced in place.
        let plain = config_dir.join("secrets.toml");
        write_atomically(&plain, b"s").unwrap();
        assert_eq!(std::fs::read_to_string(&plain).unwrap(), "s");
    }

    #[cfg(unix)]
    #[test]
    fn read_private_leaves_files_of_other_users_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

        // Someone else's file (a home-manager link into /nix/store, say)
        // is read without a chmod.
        let other = current_uid().wrapping_add(1);
        assert_eq!(read_private_as(&path, other).unwrap().as_deref(), Some("x"));
        assert_eq!(mode(&path), 0o644);

        // Our own file is tightened.
        assert_eq!(read_private(&path).unwrap().as_deref(), Some("x"));
        assert_eq!(mode(&path), 0o600);
    }
}
