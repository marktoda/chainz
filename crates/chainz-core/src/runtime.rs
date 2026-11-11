//! The runtime registry: discovery of live processes chainz started.
//!
//! **Frozen interface** (Phase 0). Lives in `$XDG_RUNTIME_DIR/chainz/`,
//! falling back to the state dir when there is no runtime dir (macOS):
//!
//! | File | Written by | Meaning |
//! | --- | --- | --- |
//! | `serve.lock` | gateway | exclusive `flock` held for the gateway's whole life |
//! | `serve.json` | gateway | [`ServeInfo`]: port, auth token, pid (mode 0600) |
//! | `serve.sock` | gateway | control API unix socket (mode 0600) |
//! | `forks/<name>.json` | fork supervisor | [`ForkRecord`] |
//!
//! Liveness of the gateway is the lock, never a PID check: `exec` makes one
//! non-blocking `flock` attempt; if it gets the lock, no gateway runs.
//! Everything here is synchronous and opens no sockets.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Handle on the runtime directory. Construction touches nothing on disk.
#[derive(Debug, Clone)]
pub struct Runtime {
    dir: PathBuf,
}

/// The gateway's discovery record, `serve.json` (mode 0600).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServeInfo {
    /// Gateway process ID (informational only; liveness is the lock).
    pub pid: u32,
    /// Loopback TCP port.
    pub port: u16,
    /// Per-start random auth token; the first URL path segment.
    pub token: String,
    /// Unix seconds.
    pub started_at: u64,
    /// chainz version that started the gateway.
    pub version: String,
}

impl fmt::Debug for ServeInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServeInfo")
            .field("pid", &self.pid)
            .field("port", &self.port)
            .field("token", &"[REDACTED]")
            .field("started_at", &self.started_at)
            .field("version", &self.version)
            .finish()
    }
}

impl ServeInfo {
    /// `http://127.0.0.1:<port>/<token>/<chain>[/<endpoint>]`.
    pub fn chain_url(&self, chain: &str, endpoint: Option<&str>) -> String {
        match endpoint {
            Some(endpoint) => format!(
                "http://127.0.0.1:{}/{}/{chain}/{endpoint}",
                self.port, self.token
            ),
            None => format!("http://127.0.0.1:{}/{}/{chain}", self.port, self.token),
        }
    }
}

/// A live fork, `forks/<name>.json`. Forks resolve by name only and keep
/// their parent's chain ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkRecord {
    /// Fork name (e.g. `base-fork`); never collides with a configured chain.
    pub name: String,
    /// Supervisor process ID.
    pub pid: u32,
    /// Anvil's loopback port.
    pub port: u16,
    /// Parent chain's short name.
    pub parent: String,
    /// Chain ID the fork reports (the parent's unless `--chain-id`).
    pub chain_id: u64,
    /// Block the fork was pinned to, if any.
    pub fork_block: Option<u64>,
    /// Unix seconds.
    pub started_at: u64,
}

impl ForkRecord {
    /// The fork's JSON-RPC URL.
    pub fn rpc_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// The gateway's lifetime lock. Dropping it releases the lock.
#[derive(Debug)]
pub struct ServeLock {
    _file: std::fs::File,
}

impl Runtime {
    /// The standard runtime dir (see module docs).
    pub fn resolve() -> io::Result<Self> {
        Ok(Self::at(paths::runtime_dir()?))
    }

    /// A runtime rooted at `dir` (tests).
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Create the directory (and `forks/`) with mode 0700.
    pub fn ensure_dir(&self) -> io::Result<()> {
        paths::ensure_private_dir(&self.forks_dir())?;
        paths::ensure_private_dir(&self.dir)
    }

    /// `serve.lock`.
    pub fn serve_lock_path(&self) -> PathBuf {
        self.dir.join("serve.lock")
    }

    /// `serve.json`.
    pub fn serve_json_path(&self) -> PathBuf {
        self.dir.join("serve.json")
    }

    /// `serve.sock`.
    pub fn serve_sock_path(&self) -> PathBuf {
        self.dir.join("serve.sock")
    }

    /// `forks/`.
    pub fn forks_dir(&self) -> PathBuf {
        self.dir.join("forks")
    }

    /// `forks/<name>.json`.
    pub fn fork_path(&self, name: &str) -> PathBuf {
        self.forks_dir().join(format!("{name}.json"))
    }

    /// Whether a gateway holds `serve.lock`: one non-blocking `flock`
    /// attempt. A missing lock file means no gateway; nothing is created.
    pub fn serve_running(&self) -> bool {
        use fs2::FileExt;
        let Ok(file) = std::fs::File::open(self.serve_lock_path()) else {
            return false;
        };
        match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = FileExt::unlock(&file);
                false
            }
            Err(_) => true,
        }
    }

    /// For the gateway: take `serve.lock` for the process lifetime.
    /// `Ok(None)` when another gateway already holds it.
    pub fn acquire_serve_lock(&self) -> io::Result<Option<ServeLock>> {
        use fs2::FileExt;
        self.ensure_dir()?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(self.serve_lock_path())?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(ServeLock { _file: file })),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Read `serve.json`; `Ok(None)` if absent.
    pub fn read_serve_info(&self) -> io::Result<Option<ServeInfo>> {
        read_json(&self.serve_json_path())
    }

    /// Write `serve.json` atomically, mode 0600.
    pub fn write_serve_info(&self, info: &ServeInfo) -> io::Result<()> {
        self.ensure_dir()?;
        write_json(&self.serve_json_path(), info)
    }

    /// Remove `serve.json` (missing is fine).
    pub fn remove_serve_info(&self) -> io::Result<()> {
        remove_if_exists(&self.serve_json_path())
    }

    /// **Exec seam for the gateway.** The loopback URL `exec` should use for
    /// `chain` (pinned to `endpoint` when given), or `None` when no gateway
    /// is running. Phase 0 has no gateway, so this returns `None` until the
    /// Gateway stream holds `serve.lock` and writes `serve.json`.
    pub fn gateway_url(&self, chain: &str, endpoint: Option<&str>) -> Option<String> {
        if !self.serve_running() {
            return None;
        }
        let info = self.read_serve_info().ok().flatten()?;
        Some(info.chain_url(chain, endpoint))
    }

    /// Every fork record (unparseable files are skipped).
    pub fn list_forks(&self) -> io::Result<Vec<ForkRecord>> {
        let entries = match std::fs::read_dir(self.forks_dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut forks = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json")
                && let Ok(Some(record)) = read_json::<ForkRecord>(&path)
            {
                forks.push(record);
            }
        }
        forks.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(forks)
    }

    /// One fork by exact name; `Ok(None)` if absent. Never creates files.
    pub fn read_fork(&self, name: &str) -> io::Result<Option<ForkRecord>> {
        if name.is_empty() || name.contains(['/', '\\', '.']) {
            return Ok(None);
        }
        read_json(&self.fork_path(name))
    }

    /// Write `forks/<name>.json` atomically.
    pub fn write_fork(&self, record: &ForkRecord) -> io::Result<()> {
        self.ensure_dir()?;
        write_json(&self.fork_path(&record.name), record)
    }

    /// Remove `forks/<name>.json` (missing is fine).
    pub fn remove_fork(&self, name: &str) -> io::Result<()> {
        remove_if_exists(&self.fork_path(name))
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    paths::write_atomically(path, &bytes)
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_url_needs_the_lock_and_serve_json() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::at(dir.path().join("chainz"));
        assert!(!rt.serve_running());
        assert_eq!(rt.gateway_url("base", None), None);
        assert!(!rt.dir().exists(), "probing must not create files");

        let lock = rt.acquire_serve_lock().unwrap().expect("free");
        assert!(
            rt.acquire_serve_lock().unwrap().is_none(),
            "second gateway refused"
        );
        assert!(rt.serve_running());
        assert_eq!(rt.gateway_url("base", None), None, "no serve.json yet");
        let info = ServeInfo {
            pid: 1,
            port: 18545,
            token: "tok".into(),
            started_at: 0,
            version: "2".into(),
        };
        rt.write_serve_info(&info).unwrap();
        assert_eq!(
            rt.gateway_url("base", None).as_deref(),
            Some("http://127.0.0.1:18545/tok/base")
        );
        assert_eq!(
            rt.gateway_url("base", Some("llama")).as_deref(),
            Some("http://127.0.0.1:18545/tok/base/llama")
        );
        assert!(!format!("{info:?}").contains("tok\""));
        drop(lock);
        assert!(!rt.serve_running());
        assert_eq!(rt.gateway_url("base", None), None);
    }

    #[test]
    fn fork_records_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::at(dir.path());
        assert!(rt.list_forks().unwrap().is_empty());
        let record = ForkRecord {
            name: "base-fork".into(),
            pid: 42,
            port: 8601,
            parent: "base".into(),
            chain_id: 8453,
            fork_block: Some(1),
            started_at: 0,
        };
        rt.write_fork(&record).unwrap();
        assert_eq!(rt.read_fork("base-fork").unwrap(), Some(record.clone()));
        assert_eq!(rt.read_fork("../x").unwrap(), None);
        assert_eq!(rt.list_forks().unwrap(), vec![record]);
        rt.remove_fork("base-fork").unwrap();
        assert!(rt.list_forks().unwrap().is_empty());
    }
}
