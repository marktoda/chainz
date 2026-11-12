//! OS primitives shared by the chainz crates.
//!
//! A leaf crate: it depends on no other chainz crate, prints nothing and
//! has no async runtime. Everything private is owner-only: directories are
//! mode 0700, files 0600.
//!
//! - [`create_private_dir`], [`write_private`] (atomic and durable),
//!   [`read_private`] (tightens loose permissions), [`open_private`],
//!   [`open_private_log`];
//! - [`try_lock`], one non-blocking `flock` attempt;
//! - [`FileStamp`], enough metadata to see that a file changed;
//! - [`now_ms`] and [`now_secs`], the wall clock as unix time;
//! - [`launch_detached`] and [`detached_ready`], running this program in
//!   the background until it says it is ready.
//!
//! chainz supports Linux and macOS only.

#![warn(missing_docs)]

#[cfg(not(unix))]
compile_error!("chainz supports Linux and macOS only");

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Create `dir` (and any missing parents) and restrict it to the owner.
///
/// A new `dir` is created mode 0700. An existing one with group or other
/// bits set is tightened to 0700. Parents are created with the default
/// mode.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a directory", dir.display()),
        ));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Atomically and durably replace `path` with `contents`, mode 0600:
/// temp file in the same directory, fsync, rename, fsync the directory.
///
/// A symlinked `path` (a dotfiles checkout, say) stays a symlink: the file
/// it points to is replaced instead. Only a directory chainz owns, the one
/// holding a regular `path`, is created and restricted to the owner.
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let target = resolve_symlinks(path)?;
    let dir = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    if target == path {
        create_private_dir(dir)?;
    }
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut temp = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .tempfile_in(dir)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temp.write_all(contents)?;
    temp.as_file().sync_all()?;
    temp.persist(&target).map_err(|e| e.error)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Follow `path` through any symlinks to the file they finally name (which
/// need not exist yet). A path that is not a symlink is returned as is.
fn resolve_symlinks(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    // The kernel's own limit on nested links (Linux MAXSYMLINKS).
    for _ in 0..40 {
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(&current)?;
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

/// The text of a file read by [`read_private`].
#[derive(Debug)]
pub struct PrivateText {
    /// The file's contents.
    pub text: String,
    /// Set when the file was too open and could not be tightened to 0600.
    pub warning: Option<PermissionWarning>,
}

/// A file that [`read_private`] could not restrict to mode 0600. Not an
/// error: the file was still read.
#[derive(Debug)]
pub struct PermissionWarning {
    /// The file.
    pub path: PathBuf,
    /// Why `chmod` failed.
    pub error: io::Error,
}

impl fmt::Display for PermissionWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "could not restrict {} to mode 0600: {}",
            self.path.display(),
            self.error
        )
    }
}

/// Read a file and tighten its permissions to 0600 if group/other bits are
/// set. `Ok(None)` when the file does not exist.
///
/// Only a file the current user owns is tightened, and a failed `chmod` is
/// a [`PermissionWarning`], not an error: a read-only config (a
/// home-manager symlink into `/nix/store`, say) must stay readable.
pub fn read_private(path: &Path) -> io::Result<Option<PrivateText>> {
    read_private_as(path, current_uid())
}

fn read_private_as(path: &Path, uid: u32) -> io::Result<Option<PrivateText>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    let mut warning = None;
    if meta.permissions().mode() & 0o077 != 0
        && meta.uid() == uid
        && let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600))
    {
        warning = Some(PermissionWarning {
            path: path.to_path_buf(),
            error,
        });
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(PrivateText { text, warning }))
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// Open `path` for reading and writing, creating it mode 0600 if missing
/// and never truncating it: a lock file, say.
pub fn open_private(path: &Path) -> io::Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

/// How [`open_private_log`] treats what a log already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogMode {
    /// Keep it and write after it.
    Append,
    /// Empty it first.
    Truncate,
}

/// Open a log for appending, creating it mode 0600 if missing.
pub fn open_private_log(path: &Path, mode: LogMode) -> io::Result<File> {
    let file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    if mode == LogMode::Truncate {
        file.set_len(0)?;
    }
    Ok(file)
}

/// One non-blocking attempt at an exclusive `flock` on `file`: `Ok(true)`
/// when it is now held (until `file` is closed or unlocked), `Ok(false)`
/// when another open file description holds it.
pub fn try_lock(file: &File) -> io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(fs::TryLockError::WouldBlock) => Ok(false),
        Err(fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Enough of a file's metadata to see that it changed. Config writes are
/// atomic renames, so the inode changes even when the size and the
/// (coarse-grained) modification time come out the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
    dev: u64,
    ino: u64,
    ctime: (i64, i64),
}

impl FileStamp {
    /// The stamp of the file at `path` (following symlinks); `None` when it
    /// can't be read (a missing file, say).
    pub fn of(path: &Path) -> Option<FileStamp> {
        let meta = fs::metadata(path).ok()?;
        Some(FileStamp {
            modified: meta.modified().ok(),
            len: meta.len(),
            dev: meta.dev(),
            ino: meta.ino(),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        })
    }

    /// The modification time, where the platform reports one.
    pub fn modified(&self) -> Option<SystemTime> {
        self.modified
    }

    /// The size in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the file was empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Unix time in milliseconds (0 before the epoch).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Unix time in whole seconds (0 before the epoch).
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Set (to `1`) in the environment of a process [`launch_detached`]
/// started: the background copy knows it is one.
pub const DETACHED_ENV: &str = "CHAINZ_DETACHED";

/// The flag [`launch_detached`] drops from the re-executed command line.
pub const DETACH_FLAG: &str = "--detach";

/// Whether this process is the background copy [`launch_detached`]
/// started.
pub fn is_detached() -> bool {
    std::env::var_os(DETACHED_ENV).is_some()
}

/// A background copy of this process, from [`launch_detached`].
#[derive(Debug)]
pub struct Detached {
    child: Child,
    ready: Option<ChildStdout>,
}

/// Run this program again in the background: the full command line minus
/// [`DETACH_FLAG`], marked with [`DETACHED_ENV`], in a new session (`setsid`:
/// no controlling terminal, so the terminal's Ctrl-C and hangup never reach
/// it). Its stdin is empty, its stderr is the private log at `log` (opened
/// per `mode`), and its stdout is a pipe back to us: the child prints what
/// the user should see, then closes the pipe with [`detached_ready`].
pub fn launch_detached(log: &Path, mode: LogMode) -> io::Result<Detached> {
    use std::os::unix::process::CommandExt;
    let log = open_private_log(log, mode)?;
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .args(std::env::args_os().skip(1).filter(|arg| arg != DETACH_FLAG))
        .env(DETACHED_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let ready = child.stdout.take();
    Ok(Detached { child, ready })
}

impl Detached {
    /// The background process's ID.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Wait until the child is ready (it closed its stdout, see
    /// [`detached_ready`]) or has exited, and return what it printed. An
    /// [`io::ErrorKind::TimedOut`] error after `timeout`, if one is given.
    pub fn wait_ready(&mut self, timeout: Option<Duration>) -> io::Result<String> {
        use std::os::fd::AsRawFd;
        let Some(mut pipe) = self.ready.take() else {
            return Ok(String::new());
        };
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut out = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let wait_ms = match deadline {
                None => -1,
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        self.ready = Some(pipe);
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "the background process did not get ready in time",
                        ));
                    }
                    i32::try_from(left.as_millis().max(1)).unwrap_or(i32::MAX)
                }
            };
            let mut poll = libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd for the duration of the call.
            match unsafe { libc::poll(&mut poll, 1, wait_ms) } {
                -1 => {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
                0 => {}
                _ => match pipe.read(&mut chunk) {
                    Ok(0) => return Ok(String::from_utf8_lossy(&out).into_owned()),
                    Ok(n) => out.extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                },
            }
        }
    }

    /// The exit status, if the child has exited.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Wait for the child to exit.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    /// Kill the child (SIGKILL) and reap it.
    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()?;
        self.child.wait().map(drop)
    }
}

/// In the background copy, once it is ready: flush stdout and point it at
/// the log (its stderr), which closes the pipe [`Detached::wait_ready`]
/// waits on. Does nothing in a process [`launch_detached`] did not start.
pub fn detached_ready() -> io::Result<()> {
    if !is_detached() {
        return Ok(());
    }
    io::stdout().flush()?;
    // SAFETY: dup2 of the standard descriptors onto each other.
    if unsafe { libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn private_dirs_are_created_and_tightened() {
        let dir = tempfile::tempdir().unwrap();
        let new = dir.path().join("a/b/chainz");
        create_private_dir(&new).unwrap();
        assert_eq!(mode(&new), 0o700);
        fs::set_permissions(&new, fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&new).unwrap();
        assert_eq!(mode(&new), 0o700);

        let file = dir.path().join("file");
        fs::write(&file, "x").unwrap();
        assert!(create_private_dir(&file).is_err());
    }

    #[test]
    fn writes_are_private_and_create_the_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chainz/config.toml");
        write_private(&path, b"one").unwrap();
        write_private(&path, b"two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        // No temp files are left behind.
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn writes_through_a_symlink_to_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("dotfiles");
        fs::create_dir(&real_dir).unwrap();
        let real = real_dir.join("config.toml");
        fs::write(&real, "old").unwrap();
        let config_dir = dir.path().join("chainz");
        fs::create_dir(&config_dir).unwrap();
        let link = config_dir.join("config.toml");
        std::os::unix::fs::symlink("../dotfiles/config.toml", &link).unwrap();

        write_private(&link, b"new").unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(fs::read_to_string(&link).unwrap(), "new");

        // A plain path is still replaced in place.
        let plain = config_dir.join("secrets.toml");
        write_private(&plain, b"s").unwrap();
        assert_eq!(fs::read_to_string(&plain).unwrap(), "s");
    }

    #[test]
    fn read_private_leaves_files_of_other_users_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "x").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let text = |read: Option<PrivateText>| {
            let read = read.unwrap();
            assert!(read.warning.is_none());
            read.text
        };

        // Someone else's file (a home-manager link into /nix/store, say)
        // is read without a chmod.
        let other = current_uid().wrapping_add(1);
        assert_eq!(text(read_private_as(&path, other).unwrap()), "x");
        assert_eq!(mode(&path), 0o644);

        // Our own file is tightened.
        assert_eq!(text(read_private(&path).unwrap()), "x");
        assert_eq!(mode(&path), 0o600);

        assert!(read_private(&dir.path().join("missing")).unwrap().is_none());
    }

    #[test]
    fn permission_warnings_name_the_file() {
        let warning = PermissionWarning {
            path: PathBuf::from("/x/config.toml"),
            error: io::Error::from_raw_os_error(libc::EPERM),
        };
        let text = warning.to_string();
        assert!(
            text.starts_with("could not restrict /x/config.toml to mode 0600: "),
            "{text}"
        );
    }

    #[test]
    fn logs_are_private_and_appended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.log");
        open_private_log(&path, LogMode::Append)
            .unwrap()
            .write_all(b"a")
            .unwrap();
        open_private_log(&path, LogMode::Append)
            .unwrap()
            .write_all(b"b")
            .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "ab");
        open_private_log(&path, LogMode::Truncate)
            .unwrap()
            .write_all(b"c")
            .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "c");
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn try_lock_reports_a_held_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let first = open_private(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert!(try_lock(&first).unwrap());
        // flock locks belong to the open file description: a second open
        // of the same file contends, even in the same process.
        let second = open_private(&path).unwrap();
        assert!(!try_lock(&second).unwrap());
        drop(first);
        assert!(try_lock(&second).unwrap());
    }

    #[test]
    fn a_same_size_atomic_rewrite_changes_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "a = 1\n").unwrap();
        let before = FileStamp::of(&path).unwrap();
        // Same length, same modification time: only the inode (and ctime)
        // tell the files apart.
        let tmp = dir.path().join("config.toml.tmp");
        fs::write(&tmp, "a = 2\n").unwrap();
        let file = File::options().write(true).open(&tmp).unwrap();
        file.set_modified(before.modified().unwrap()).unwrap();
        drop(file);
        fs::rename(&tmp, &path).unwrap();
        let after = FileStamp::of(&path).unwrap();
        assert_eq!(
            (after.len(), after.modified()),
            (before.len(), before.modified())
        );
        assert_ne!(after, before);
        assert!(FileStamp::of(&dir.path().join("missing")).is_none());
    }

    fn background(script: &str) -> Detached {
        let mut child = Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let ready = child.stdout.take();
        Detached { child, ready }
    }

    #[test]
    fn readiness_is_the_child_closing_its_stdout() {
        // Ready while still running: what it printed comes back.
        let mut up = background("printf 'up\n'; exec 1>&2; sleep 5");
        let started = Instant::now();
        let said = up.wait_ready(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(said, "up\n");
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(up.try_wait().unwrap().is_none(), "still running");
        up.kill().unwrap();

        // Exiting counts too; the caller tells the two apart.
        let mut failed = background("printf 'no'; exit 3");
        assert_eq!(failed.wait_ready(None).unwrap(), "no");
        assert_eq!(failed.wait().unwrap().code(), Some(3));

        // Never ready: a timeout, and the child is left to the caller.
        let mut stuck = background("sleep 5");
        let err = stuck
            .wait_ready(Some(Duration::from_millis(100)))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        stuck.kill().unwrap();
    }

    #[test]
    fn the_clock_agrees_with_itself() {
        let (ms, secs) = (now_ms(), now_secs());
        assert!(ms / 1000 <= secs + 1 && secs <= ms / 1000 + 1);
    }
}
