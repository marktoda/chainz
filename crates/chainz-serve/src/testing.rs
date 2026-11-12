//! Test support (feature `testing`): the real control handler over canned
//! state, for tests of control clients such as the dashboard.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chainz_rpc::control::{MetricsSnapshot, RequestRecord, SocketPath, Status};
use tokio::net::UnixListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::control::{self, ControlState};
use crate::events::EventRing;
use crate::http;

struct Canned {
    status: Mutex<Status>,
    metrics: Mutex<MetricsSnapshot>,
    reloads: AtomicU64,
    events: EventRing,
}

impl ControlState for Canned {
    fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn metrics(&self) -> MetricsSnapshot {
        self.metrics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn events(&self) -> &EventRing {
        &self.events
    }

    async fn reload(&self) -> anyhow::Result<()> {
        self.reloads.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A control socket served by the gateway's own handler over the status,
/// metrics and events a test gives it. Every reload succeeds and is
/// counted.
///
/// Dropping it stops the server, closes every connection and removes the
/// socket file.
pub struct ControlServer {
    path: PathBuf,
    state: Arc<Canned>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl std::fmt::Debug for ControlServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlServer")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl ControlServer {
    /// Bind `socket` (mode 0600; a stale file there is replaced) and serve.
    pub async fn start(
        socket: &Path,
        status: Status,
        metrics: MetricsSnapshot,
    ) -> io::Result<Self> {
        match std::fs::remove_file(socket) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let listener = UnixListener::bind(SocketPath::new(socket)?.as_path())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
        let state = Arc::new(Canned {
            status: Mutex::new(status),
            metrics: Mutex::new(metrics),
            reloads: AtomicU64::new(0),
            events: EventRing::new(),
        });
        let (stop, stopped) = watch::channel(false);
        let handler = {
            let state = state.clone();
            let stopped = stopped.clone();
            move |request| {
                let state = state.clone();
                let stopped = stopped.clone();
                async move { control::handle(&*state, request, stopped).await }
            }
        };
        let task = tokio::spawn(http::serve(listener, handler, stopped, Duration::ZERO));
        Ok(ControlServer {
            path: socket.to_path_buf(),
            state,
            stop,
            task,
        })
    }

    /// Keep a record for `backlog` requests and send it to every connected
    /// events client, with the ID it has.
    pub fn push_event(&self, record: RequestRecord) {
        self.state.events.push(record);
    }

    /// Replace the snapshot served by `/_chainz/metrics`.
    pub fn set_metrics(&self, metrics: MetricsSnapshot) {
        *self.state.metrics.lock().unwrap_or_else(|e| e.into_inner()) = metrics;
    }

    /// How many `POST /_chainz/reload` requests were served.
    pub fn reload_count(&self) -> u64 {
        self.state.reloads.load(Ordering::SeqCst)
    }

    /// How many events streams are following right now.
    pub fn event_subscribers(&self) -> usize {
        self.state.events.subscribers()
    }

    /// The socket path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}
