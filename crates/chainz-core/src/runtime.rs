//! The runtime registry: discovery of live processes chainz started.
//!
//! Lives in `$XDG_RUNTIME_DIR/chainz/`, falling back to the state dir when
//! there is no runtime dir (macOS):
//!
//! | File | Written by | Meaning |
//! | --- | --- | --- |
//! | `serve.lock` | gateway | exclusive `flock` held for the gateway's whole life |
//! | `serve.json` | gateway | [`ServeInfo`]: port, auth token, pid (mode 0600) |
//! | `serve.sock` | gateway | control API unix socket (mode 0600) |
//! | `forks/<name>.lock` | fork supervisor | exclusive `flock` held for the fork's whole life; holds its PID |
//! | `forks/<name>.json` | fork supervisor | [`ForkRecord`] |
//! | `ports/<port>.lock` | fork supervisor | exclusive `flock` on its anvil's port, from before anvil is spawned for the fork's whole life |
//!
//! Liveness of the gateway and of each fork is its lock, never a PID check: `exec` makes one
//! non-blocking `flock` attempt; if it gets the lock, no gateway runs.
//! [`Runtime::gateway`] is the one answer to "is a gateway running".
//! Everything here is synchronous and opens no sockets.
//!
//! Each record carries a `format` version; a reader refuses a format it
//! does not know rather than misreading it.

use crate::paths;
use chainz_os::Contention;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

mod fork_registry;
pub use fork_registry::{ForkClaim, ForkEntry, ForkRecord, PortReservation};

/// The address every chainz server binds and hands out (IPv4 only).
pub const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// The gateway's default port (8545 stays free for anvil).
pub const DEFAULT_PORT: u16 = 18545;

/// `http://127.0.0.1:<port>`: a loopback server with no token (anvil).
pub fn loopback_url(port: u16) -> String {
    format!("http://{LOOPBACK}:{port}")
}

/// Where a tokenised loopback server (the gateway; a fork's pinned proxy)
/// listens, and the one place its URLs are formatted and parsed:
/// `http://127.0.0.1:<port>/<token>[/<chain>[/<endpoint>]]`.
#[derive(Clone, PartialEq, Eq)]
pub struct GatewayAddr {
    /// Loopback TCP port.
    pub port: u16,
    /// The auth token: the first URL path segment.
    pub token: String,
}

impl fmt::Debug for GatewayAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayAddr")
            .field("port", &self.port)
            .field("token", &crate::redact::REDACTED)
            .finish()
    }
}

/// A request path on a tokenised loopback server, split by
/// [`GatewayAddr::parse_path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddrPath<'a> {
    /// The first segment, to compare with the server's token.
    pub token: &'a str,
    /// The chain reference, if the path has one.
    pub chain: Option<&'a str>,
    /// The pinned endpoint, if the path has one.
    pub endpoint: Option<&'a str>,
}

impl GatewayAddr {
    /// `/<token>`, then `/<chain>` and `/<endpoint>` when given.
    pub fn path(&self, chain: Option<&str>, endpoint: Option<&str>) -> String {
        let mut path = format!("/{}", self.token);
        for segment in [chain, endpoint].into_iter().flatten() {
            path.push('/');
            path.push_str(segment);
        }
        path
    }

    /// `http://127.0.0.1:<port>/<token>`: the base URL (a pinned proxy's
    /// whole URL). Contains the token: treat it as a secret.
    pub fn base_url(&self) -> String {
        format!("{}{}", loopback_url(self.port), self.path(None, None))
    }

    /// `http://127.0.0.1:<port>/<token>/<chain>[/<endpoint>]`. Contains the
    /// token: treat it as a secret.
    pub fn url(&self, chain: &str, endpoint: Option<&str>) -> String {
        format!(
            "{}{}",
            loopback_url(self.port),
            self.path(Some(chain), endpoint)
        )
    }

    /// Split a request path, `/<token>[/<chain>[/<endpoint>]]` with at most
    /// one trailing `/`. `None` for any other shape: no token, an empty
    /// segment, or more segments.
    pub fn parse_path(path: &str) -> Option<AddrPath<'_>> {
        let path = path.strip_prefix('/')?;
        let path = path.strip_suffix('/').unwrap_or(path);
        let mut segments = path.split('/');
        let token = segments.next().filter(|s| !s.is_empty())?;
        let mut next = || match segments.next() {
            Some("") => Err(()),
            other => Ok(other),
        };
        let chain = next().ok()?;
        let endpoint = next().ok()?;
        if next().ok()?.is_some() {
            return None;
        }
        Some(AddrPath {
            token,
            chain,
            endpoint,
        })
    }
}

/// Handle on the runtime directory. Construction touches nothing on disk.
#[derive(Debug, Clone)]
pub struct Runtime {
    dir: PathBuf,
}

/// The gateway's discovery record, `serve.json` (mode 0600).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServeInfo {
    /// Record format, [`ServeInfo::FORMAT`].
    pub format: u32,
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
            .field("format", &self.format)
            .field("pid", &self.pid)
            .field("port", &self.port)
            .field("token", &crate::redact::REDACTED)
            .field("started_at", &self.started_at)
            .field("version", &self.version)
            .finish()
    }
}

impl ServeInfo {
    /// The `serve.json` format this chainz reads and writes.
    pub const FORMAT: u32 = 1;

    /// The record for this process, a gateway listening on `addr`.
    pub fn new(addr: &GatewayAddr, started_at: u64) -> Self {
        ServeInfo {
            format: Self::FORMAT,
            pid: std::process::id(),
            port: addr.port,
            token: addr.token.clone(),
            started_at,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// The gateway's address: its URLs.
    pub fn addr(&self) -> GatewayAddr {
        GatewayAddr {
            port: self.port,
            token: self.token.clone(),
        }
    }
}

/// Whether a gateway runs, from [`Runtime::gateway`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayState {
    /// Nothing holds `serve.lock`.
    Down,
    /// A gateway holds the lock but has not published `serve.json` yet
    /// (or has already withdrawn it while stopping).
    Starting,
    /// A gateway serves, as `serve.json` describes.
    Up(ServeInfo),
}

/// The gateway's lifetime lock. Dropping it releases the lock.
#[derive(Debug)]
pub struct ServeLock {
    _file: std::fs::File,
    serve_json: PathBuf,
}

impl ServeLock {
    /// Publish `serve.json` (atomic, mode 0600). It is withdrawn when the
    /// returned guard drops, so it never outlives the gateway.
    pub fn publish(&self, info: &ServeInfo) -> io::Result<Published> {
        write_json(&self.serve_json, info)?;
        Ok(Published {
            path: self.serve_json.clone(),
        })
    }
}

/// A published `serve.json`; dropping it removes the file.
#[derive(Debug)]
pub struct Published {
    path: PathBuf,
}

impl Drop for Published {
    fn drop(&mut self) {
        // Best effort: the lock, not this file, says whether we run.
        let _ = remove_if_exists(&self.path);
    }
}

impl Runtime {
    /// The standard runtime dir (see module docs).
    pub fn resolve() -> io::Result<Self> {
        Ok(Self::at(paths::runtime_dir()?))
    }

    /// A runtime rooted at `dir` instead of `$XDG_RUNTIME_DIR/chainz`
    /// (tests and sandboxes).
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Create the directory (and `forks/`) with mode 0700.
    pub(crate) fn ensure_dir(&self) -> io::Result<()> {
        chainz_os::create_private_dir(&self.forks_dir())?;
        chainz_os::create_private_dir(&self.dir)
    }

    /// `serve.lock`.
    pub(crate) fn serve_lock_path(&self) -> PathBuf {
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

    /// **The** answer to "is a gateway running": a probe of `serve.lock`
    /// ([`chainz_os::probe_lock`]), then (only while the lock is held) a
    /// read of `serve.json`. Creates nothing. An unreadable lock, or an
    /// unreadable or unknown `serve.json` under a held lock, is an error,
    /// never [`GatewayState::Down`].
    pub fn gateway(&self) -> io::Result<GatewayState> {
        if !chainz_os::probe_lock(&self.serve_lock_path())? {
            return Ok(GatewayState::Down);
        }
        let path = self.serve_json_path();
        Ok(match read_record(&path, ServeInfo::FORMAT)? {
            None => GatewayState::Starting,
            Some(info) => GatewayState::Up(info),
        })
    }

    /// For the gateway: take `serve.lock` for the process lifetime, and
    /// remove the `serve.json` a dead gateway left (failing if it can't be
    /// removed, so nobody reads a dead gateway's record while we run).
    /// `Ok(None)` when another gateway already holds the lock; a probe
    /// ([`Runtime::gateway`]) that holds it for a moment is outwaited.
    pub fn acquire_serve_lock(&self) -> io::Result<Option<ServeLock>> {
        self.ensure_dir()?;
        let lock = self.serve_lock_path();
        let Some(file) = chainz_os::try_lock_file(&lock, Contention::OutwaitProbes)? else {
            return Ok(None);
        };
        let serve_json = self.serve_json_path();
        remove_if_exists(&serve_json).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("can't remove the stale {}: {e}", serve_json.display()),
            )
        })?;
        Ok(Some(ServeLock {
            _file: file,
            serve_json,
        }))
    }
}

/// A runtime record (`serve.json`, a fork record) of format `expected`,
/// `None` when the file is missing. The `format` field is read first, so a
/// record of another format is refused as such, whatever its other fields.
fn read_record<T: for<'de> Deserialize<'de>>(path: &Path, expected: u32) -> io::Result<Option<T>> {
    let Some(value) = read_json::<serde_json::Value>(path)? else {
        return Ok(None);
    };
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
    match value.get("format").and_then(serde_json::Value::as_u64) {
        Some(format) if format == u64::from(expected) => {}
        Some(format) => {
            return Err(invalid(format!(
                "{} has format {format}, which this chainz ({}) does not read",
                path.display(),
                env!("CARGO_PKG_VERSION")
            )));
        }
        None => return Err(invalid(format!("{} has no record format", path.display()))),
    }
    T::deserialize(value)
        .map(Some)
        .map_err(|e| invalid(format!("{} is unreadable: {e}", path.display())))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is unreadable: {e}", path.display()),
            )
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    chainz_os::write_private(path, &bytes)
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

    fn info() -> ServeInfo {
        ServeInfo::new(
            &GatewayAddr {
                port: 18545,
                token: "tok".into(),
            },
            0,
        )
    }

    #[test]
    fn gateway_state_needs_the_lock_and_serve_json() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::at(dir.path().join("chainz"));
        assert_eq!(rt.gateway().unwrap(), GatewayState::Down);
        assert!(!rt.dir().exists(), "probing must not create files");

        let lock = rt.acquire_serve_lock().unwrap().expect("free");
        assert!(
            rt.acquire_serve_lock().unwrap().is_none(),
            "second gateway refused"
        );
        assert_eq!(rt.gateway().unwrap(), GatewayState::Starting);
        let info = info();
        let published = lock.publish(&info).unwrap();
        assert_eq!(rt.gateway().unwrap(), GatewayState::Up(info.clone()));
        assert!(!format!("{info:?}").contains("tok\""));
        drop(published);
        assert_eq!(rt.gateway().unwrap(), GatewayState::Starting);
        assert!(!rt.serve_json_path().exists(), "withdrawn on drop");
        drop(lock);
        assert_eq!(rt.gateway().unwrap(), GatewayState::Down);
    }

    #[test]
    fn a_probe_never_makes_a_starting_gateway_fail() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::at(dir.path().join("chainz"));
        drop(rt.acquire_serve_lock().unwrap().unwrap());

        // A probe (exec, top) holding the lock right as the gateway starts.
        let probe = std::fs::File::open(rt.serve_lock_path()).unwrap();
        probe.try_lock_shared().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            drop(probe);
        });
        assert!(rt.acquire_serve_lock().unwrap().is_some());
        release.join().unwrap();

        // And under a stream of probes.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let prober = {
            let (rt, stop) = (rt.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = rt.gateway();
                }
            })
        };
        for _ in 0..200 {
            assert!(rt.acquire_serve_lock().unwrap().is_some());
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        prober.join().unwrap();
    }

    #[test]
    fn a_broken_serve_json_is_an_error_not_down() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::at(dir.path());
        // A dead gateway's record is ignored, then removed by the next.
        write_json(&rt.serve_json_path(), &info()).unwrap();
        assert_eq!(rt.gateway().unwrap(), GatewayState::Down);
        let lock = rt.acquire_serve_lock().unwrap().unwrap();
        assert!(!rt.serve_json_path().exists(), "stale record removed");

        std::fs::write(rt.serve_json_path(), "{").unwrap();
        let err = rt.gateway().unwrap_err().to_string();
        assert!(err.contains("serve.json is unreadable"), "{err}");
        let newer = ServeInfo {
            format: ServeInfo::FORMAT + 1,
            ..info()
        };
        write_json(&rt.serve_json_path(), &newer).unwrap();
        let err = rt.gateway().unwrap_err().to_string();
        assert!(err.contains("does not read"), "{err}");
        // A newer format is refused as such, whatever its fields.
        let other_fields = serde_json::json!({ "format": ServeInfo::FORMAT + 1 });
        write_json(&rt.serve_json_path(), &other_fields).unwrap();
        let err = rt.gateway().unwrap_err().to_string();
        assert!(err.contains("does not read"), "{err}");
        drop(lock);

        // A record that can't be removed fails the next gateway's start.
        std::fs::remove_file(rt.serve_json_path()).unwrap();
        std::fs::create_dir_all(rt.serve_json_path().join("x")).unwrap();
        let err = rt.acquire_serve_lock().unwrap_err().to_string();
        assert!(err.contains("stale"), "{err}");
    }

    #[test]
    fn gateway_addresses_format_and_parse() {
        let addr = GatewayAddr {
            port: 18545,
            token: "tok".into(),
        };
        assert_eq!(addr.base_url(), "http://127.0.0.1:18545/tok");
        assert_eq!(addr.url("base", None), "http://127.0.0.1:18545/tok/base");
        assert_eq!(
            addr.url("base", Some("llama")),
            "http://127.0.0.1:18545/tok/base/llama"
        );
        assert_eq!(addr.path(Some("base"), None), "/tok/base");
        assert!(!format!("{addr:?}").contains("tok\""));

        let parse = |p| GatewayAddr::parse_path(p).map(|a| (a.token, a.chain, a.endpoint));
        assert_eq!(parse("/tok"), Some(("tok", None, None)));
        assert_eq!(parse("/tok/"), Some(("tok", None, None)));
        assert_eq!(parse("/tok/base"), Some(("tok", Some("base"), None)));
        assert_eq!(parse("/tok/base/"), Some(("tok", Some("base"), None)));
        assert_eq!(
            parse("/tok/base/llama/"),
            Some(("tok", Some("base"), Some("llama")))
        );
        for bad in [
            "",
            "/",
            "//",
            "tok",
            "/tok//",
            "//base",
            "/tok//llama",
            "/a/b/c/d",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
        // Every URL we format parses back.
        let url = addr.url("base", Some("llama"));
        let path = url.strip_prefix("http://127.0.0.1:18545").unwrap();
        assert_eq!(parse(path), Some(("tok", Some("base"), Some("llama"))));
    }
}
