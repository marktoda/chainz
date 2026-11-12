//! The fork registry: `forks/<name>.lock` and `forks/<name>.json`.
//!
//! A fork's supervisor holds an exclusive `flock` on `forks/<name>.lock`
//! for the fork's whole life, the same model as `serve.lock`: liveness is
//! the lock, never a PID check, so a dead supervisor's files are simply
//! stale. Claiming a name *is* taking the lock. The lock file holds the
//! supervisor's PID (so a fork can be stopped before it has a record);
//! the record, [`ForkRecord`], is written once anvil is spawned and marked
//! `ready` once the fork serves.
//!
//! Files are only ever removed by a process holding the name's lock (the
//! supervisor on exit, or a reaper that took a dead fork's lock), and a
//! claimant checks that the file it locked is still the one at the path,
//! so removing a lock file never lets two processes own one name.
//!
//! Lock files are opened `O_CLOEXEC` (as std always does), so anvil and
//! any other child never inherit the lock.

use super::{Runtime, loopback_url, read_json, remove_if_exists, write_json};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A fork, `forks/<name>.json`. Forks resolve by name only and keep their
/// parent's chain ID unless started with `--chain-id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkRecord {
    /// Record format, [`ForkRecord::FORMAT`].
    pub format: u32,
    /// Fork name (e.g. `base-fork`); never a configured chain's name.
    pub name: String,
    /// Supervisor process ID.
    pub pid: u32,
    /// Anvil's process ID.
    pub anvil_pid: u32,
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
    /// Whether the fork serves (anvil answered and funding is done). Only
    /// ready forks resolve as chains or are listed.
    pub ready: bool,
}

impl ForkRecord {
    /// The `forks/<name>.json` format this chainz reads and writes.
    pub const FORMAT: u32 = 1;

    /// The fork's JSON-RPC URL.
    pub fn rpc_url(&self) -> String {
        loopback_url(self.port)
    }
}

/// One name in the registry, from [`Runtime::forks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkEntry {
    /// Fork name.
    pub name: String,
    /// Whether a supervisor holds the name's lock.
    pub live: bool,
    /// The supervisor's PID: the record's, else the lock file's.
    pub supervisor: Option<u32>,
    /// The record: `Ok(None)` before anvil is spawned, `Err` when it
    /// can't be read (the reason).
    pub record: Result<Option<ForkRecord>, String>,
}

impl ForkEntry {
    /// The record, if the fork is live and ready.
    pub fn ready(&self) -> Option<&ForkRecord> {
        match &self.record {
            Ok(Some(record)) if self.live && record.ready => Some(record),
            _ => None,
        }
    }
}

/// A supervisor's hold on a fork name: the lock on `forks/<name>.lock`.
/// Dropping it removes the fork's files, then releases the name.
#[derive(Debug)]
pub struct ForkClaim {
    file: File,
    lock: PathBuf,
    record: PathBuf,
    name: String,
}

impl ForkClaim {
    /// The claimed name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Write (or rewrite) the fork's record, atomically. Its name must be
    /// the claimed one.
    pub fn register(&self, record: &ForkRecord) -> io::Result<()> {
        if record.name != self.name {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("record '{}' under claim '{}'", record.name, self.name),
            ));
        }
        write_json(&self.record, record)
    }

    /// Record `pid` as the owner in the lock file.
    fn write_owner(&mut self, pid: u32) -> io::Result<()> {
        self.file.set_len(0)?;
        self.file.rewind()?;
        self.file.write_all(pid.to_string().as_bytes())
    }
}

impl Drop for ForkClaim {
    fn drop(&mut self) {
        // Best effort, and only while the lock is still ours: the record
        // first, so nothing sees a record without a lock holder.
        let _ = remove_if_exists(&self.record);
        let _ = remove_if_exists(&self.lock);
    }
}

/// How often, and how long apart, a claim retries a lock that a reader is
/// probing (a probe holds a shared lock for microseconds).
const CLAIM_ATTEMPTS: u32 = 50;
const CLAIM_RETRY: Duration = Duration::from_millis(2);

/// Whether `name` can be a file name in the registry. Fork names follow
/// the chain-name rules ([`crate::forks::check_fork_name`]); this is only
/// the path guard for lookups by arbitrary text.
fn safe_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\', '.', '\0'])
}

impl Runtime {
    /// `forks/<name>.json`.
    fn fork_record_path(&self, name: &str) -> PathBuf {
        self.forks_dir().join(format!("{name}.json"))
    }

    /// `forks/<name>.lock`.
    fn fork_lock_path(&self, name: &str) -> PathBuf {
        self.forks_dir().join(format!("{name}.lock"))
    }

    /// Whether a supervisor holds `name`'s lock (the fork is alive,
    /// starting or not): one shared, non-blocking `flock` attempt. Creates
    /// nothing.
    pub fn fork_live(&self, name: &str) -> io::Result<bool> {
        if !safe_name(name) {
            return Ok(false);
        }
        let file = match File::open(self.fork_lock_path(name)) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        match file.try_lock_shared() {
            Ok(()) => Ok(false),
            Err(std::fs::TryLockError::WouldBlock) => Ok(true),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }

    /// Take `name`'s lock: `Ok(None)` when someone else holds it. With
    /// `patient`, a lock held only briefly (a reader probing it) is
    /// retried.
    fn lock_fork_name(&self, name: &str, patient: bool) -> io::Result<Option<ForkClaim>> {
        if !safe_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("'{name}' is not a fork name"),
            ));
        }
        self.ensure_dir()?;
        let lock = self.fork_lock_path(name);
        let attempts = if patient { CLAIM_ATTEMPTS } else { 1 };
        for attempt in 1..=attempts {
            let file = chainz_sys::open_private(&lock)?;
            if !chainz_sys::try_lock(&file)? {
                if attempt < attempts {
                    std::thread::sleep(CLAIM_RETRY);
                }
                continue;
            }
            // The holder before us may have removed the file after we
            // opened it: then the lock is on a file nobody else will see.
            if !same_file(&file, &lock)? {
                continue;
            }
            return Ok(Some(ForkClaim {
                file,
                record: self.fork_record_path(name),
                lock,
                name: name.to_string(),
            }));
        }
        Ok(None)
    }

    /// Claim `name` for this process (a fork supervisor): `Ok(None)` when a
    /// live fork has it. A record left by a dead fork of the same name is
    /// removed.
    pub fn claim_fork(&self, name: &str) -> io::Result<Option<ForkClaim>> {
        let Some(mut claim) = self.lock_fork_name(name, true)? else {
            return Ok(None);
        };
        remove_if_exists(&claim.record)?;
        claim.write_owner(std::process::id())?;
        Ok(Some(claim))
    }

    /// The live, ready fork named `name`, if there is one: what a chain
    /// reference resolves to. Creates nothing.
    pub fn fork(&self, name: &str) -> io::Result<Option<ForkRecord>> {
        if !safe_name(name) || !self.fork_live(name)? {
            return Ok(None);
        }
        Ok(read_record(&self.fork_record_path(name))?.filter(|r| r.ready))
    }

    /// Every name in the registry, by name, with its liveness. Read-only:
    /// dead forks are listed (`live: false`) until [`Runtime::reap_forks`].
    pub fn forks(&self) -> io::Result<Vec<ForkEntry>> {
        self.fork_names()?
            .into_iter()
            .map(|name| self.fork_entry(name))
            .collect()
    }

    /// Remove every dead fork's files. `stop_orphan` is called first with
    /// each dead fork's record, under the fork's lock, to stop an anvil its
    /// supervisor left behind (Linux's `PR_SET_PDEATHSIG` normally has
    /// already; macOS has no equivalent). Returns the names reaped.
    pub fn reap_forks(&self, mut stop_orphan: impl FnMut(&ForkRecord)) -> io::Result<Vec<String>> {
        let mut reaped = Vec::new();
        for name in self.fork_names()? {
            if self.reap_fork(&name, &mut stop_orphan)? {
                reaped.push(name);
            }
        }
        Ok(reaped)
    }

    /// [`Runtime::reap_forks`] for one name: `Ok(false)` when it is live
    /// (or has no files).
    pub fn reap_fork(
        &self,
        name: &str,
        mut stop_orphan: impl FnMut(&ForkRecord),
    ) -> io::Result<bool> {
        let record_path = self.fork_record_path(name);
        if !record_path.exists() && !self.fork_lock_path(name).exists() {
            return Ok(false);
        }
        let Some(claim) = self.lock_fork_name(name, false)? else {
            return Ok(false);
        };
        // Unreadable means nothing to stop; the files go regardless.
        if let Ok(Some(record)) = read_record(&record_path) {
            stop_orphan(&record);
        }
        drop(claim);
        Ok(true)
    }

    /// The names with a lock file or a record, sorted.
    fn fork_names(&self) -> io::Result<Vec<String>> {
        let entries = match std::fs::read_dir(self.forks_dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut names = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let ours = path.extension().is_some_and(|e| e == "json" || e == "lock");
            if let (true, Some(stem)) = (ours, path.file_stem().and_then(|s| s.to_str()))
                && safe_name(stem)
            {
                names.push(stem.to_string());
            }
        }
        names.sort();
        names.dedup();
        Ok(names)
    }

    fn fork_entry(&self, name: String) -> io::Result<ForkEntry> {
        let live = self.fork_live(&name)?;
        let record = read_record(&self.fork_record_path(&name)).map_err(|e| e.to_string());
        let supervisor = match &record {
            Ok(Some(record)) => Some(record.pid),
            _ => lock_owner(&self.fork_lock_path(&name)),
        };
        Ok(ForkEntry {
            name,
            live,
            supervisor,
            record,
        })
    }
}

/// A fork record, refusing a format this chainz does not know.
fn read_record(path: &Path) -> io::Result<Option<ForkRecord>> {
    #[derive(Deserialize)]
    struct Format {
        format: Option<u32>,
    }
    let Some(format) = read_json::<Format>(path)? else {
        return Ok(None);
    };
    match format.format {
        Some(ForkRecord::FORMAT) => read_json(path),
        Some(other) => Err(super::unknown_format(path, other)),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} has no record format", path.display()),
        )),
    }
}

/// The PID a supervisor wrote into its lock file, if any.
fn lock_owner(path: &Path) -> Option<u32> {
    let mut text = String::new();
    File::open(path).ok()?.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

/// Whether `file` is still the file at `path`.
fn same_file(file: &File, path: &Path) -> io::Result<bool> {
    let held = file.metadata()?;
    match std::fs::metadata(path) {
        Ok(now) => Ok((held.dev(), held.ino()) == (now.dev(), now.ino())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, ready: bool) -> ForkRecord {
        ForkRecord {
            format: ForkRecord::FORMAT,
            name: name.into(),
            pid: std::process::id(),
            anvil_pid: 0,
            port: 8601,
            parent: "base".into(),
            chain_id: 8453,
            fork_block: Some(1),
            started_at: 0,
            ready,
        }
    }

    #[test]
    fn a_claim_is_the_name_and_its_files_go_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::at(dir.path());
        assert!(rt.forks().unwrap().is_empty());
        assert_eq!(rt.fork("base-fork").unwrap(), None);

        let claim = rt.claim_fork("base-fork").unwrap().expect("free");
        assert!(rt.claim_fork("base-fork").unwrap().is_none(), "taken");
        // Claimed but not yet registered: live, stoppable, not resolvable.
        let entries = rt.forks().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].live);
        assert_eq!(entries[0].supervisor, Some(std::process::id()));
        assert_eq!(entries[0].record, Ok(None));
        assert_eq!(rt.fork("base-fork").unwrap(), None);

        claim.register(&record("base-fork", false)).unwrap();
        assert_eq!(rt.fork("base-fork").unwrap(), None, "not ready yet");
        claim.register(&record("base-fork", true)).unwrap();
        assert_eq!(
            rt.fork("base-fork").unwrap(),
            Some(record("base-fork", true))
        );
        assert!(rt.forks().unwrap()[0].ready().is_some());
        assert!(claim.register(&record("other", true)).is_err());
        // Nothing to reap while it lives.
        assert!(rt.reap_forks(|_| panic!("live")).unwrap().is_empty());

        drop(claim);
        assert!(rt.forks().unwrap().is_empty());
        assert!(std::fs::read_dir(rt.forks_dir()).unwrap().next().is_none());
        assert!(rt.claim_fork("base-fork").unwrap().is_some(), "free again");
        assert!(rt.fork("../x").unwrap().is_none());
        assert!(rt.claim_fork("../x").is_err());
    }

    #[test]
    fn dead_forks_are_listed_then_reaped_with_their_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::at(dir.path());
        rt.ensure_dir().unwrap();
        // A record nobody holds a lock for: its supervisor died.
        write_json(
            &rt.fork_record_path("gone-fork"),
            &record("gone-fork", true),
        )
        .unwrap();
        // A lock file nobody holds: died before spawning anvil.
        std::fs::write(rt.fork_lock_path("half-fork"), "1").unwrap();
        assert_eq!(
            rt.fork("gone-fork").unwrap(),
            None,
            "dead forks never resolve"
        );
        let entries = rt.forks().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| !e.live));
        assert_eq!(entries[1].supervisor, Some(1));

        let mut orphans = Vec::new();
        let reaped = rt.reap_forks(|r| orphans.push(r.name.clone())).unwrap();
        assert_eq!(reaped, ["gone-fork", "half-fork"]);
        assert_eq!(orphans, ["gone-fork"]);
        assert!(std::fs::read_dir(rt.forks_dir()).unwrap().next().is_none());

        // A claim replaces a dead fork's leftover record.
        write_json(&rt.fork_record_path("x"), &record("x", true)).unwrap();
        let _claim = rt.claim_fork("x").unwrap().unwrap();
        assert_eq!(rt.fork("x").unwrap(), None);
    }

    #[test]
    fn unknown_record_formats_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::at(dir.path());
        let claim = rt.claim_fork("new-fork").unwrap().unwrap();
        let newer = ForkRecord {
            format: ForkRecord::FORMAT + 1,
            ..record("new-fork", true)
        };
        write_json(&rt.fork_record_path("new-fork"), &newer).unwrap();
        assert!(
            rt.fork("new-fork")
                .unwrap_err()
                .to_string()
                .contains("does not read")
        );
        assert!(rt.forks().unwrap()[0].record.is_err());
        drop(claim);
    }

    #[test]
    fn the_lock_is_not_inherited_by_children() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::at(dir.path());
        let claim = rt.claim_fork("base-fork").unwrap().unwrap();
        // SAFETY: F_GETFD on a descriptor we own.
        let flags = unsafe { libc::fcntl(claim.file.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0, "{flags}");
    }
}
