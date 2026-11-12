//! The watchers: a gateway starting or stopping, its metrics and request
//! stream while it runs, and the config files changing on disk.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chainz_core::runtime::{GatewayState, Runtime, ServeInfo};
use chainz_core::store::StorePaths;
use chainz_sys::FileStamp;
use tokio::sync::watch;

use super::{Event, Shared, Signals, Update, traffic_snapshot};
use crate::model::FEED_CAP;

/// The gateway as last seen, for the workers that route through it and
/// the one that follows its traffic.
#[derive(Debug)]
pub(super) struct GatewayWatch {
    runtime: Runtime,
    current: watch::Sender<Option<ServeInfo>>,
    /// The last error reading the gateway's record, so it is said once.
    error: std::sync::Mutex<Option<String>>,
}

/// What one look at the gateway found that is worth saying.
pub(super) struct Seen {
    /// A gateway record that can't be read, the first time it fails so.
    pub(super) warning: Option<String>,
}

impl GatewayWatch {
    pub(super) fn new(runtime: Runtime) -> Self {
        GatewayWatch {
            runtime,
            current: watch::channel(None).0,
            error: std::sync::Mutex::new(None),
        }
    }

    /// The running gateway, as last seen.
    pub(super) fn info(&self) -> Option<ServeInfo> {
        self.current.borrow().clone()
    }

    /// The control socket's path.
    pub(super) fn socket(&self) -> PathBuf {
        self.runtime.serve_sock_path()
    }

    /// Wakes when the gateway starts, stops or moves.
    pub(super) fn subscribe(&self) -> watch::Receiver<Option<ServeInfo>> {
        self.current.subscribe()
    }

    /// Look again. On a change `changed` runs first, then the watchers
    /// see it: whoever `changed` tells hears of a gateway before anything
    /// read from it.
    pub(super) fn detect(&self, changed: impl FnOnce(Option<&ServeInfo>)) -> Seen {
        let (found, error) = match self.runtime.gateway() {
            Ok(GatewayState::Up(info)) => (Some(info), None),
            Ok(GatewayState::Down | GatewayState::Starting) => (None, None),
            Err(error) => (None, Some(format!("gateway state unreadable: {error}"))),
        };
        let mut last = super::lock(&self.error);
        let warning = error
            .as_ref()
            .filter(|e| last.as_ref() != Some(*e))
            .cloned();
        *last = error;
        drop(last);
        if *self.current.borrow() != found {
            changed(found.as_ref());
            self.current.send_replace(found);
        }
        Seen { warning }
    }
}

/// Look for a gateway every `watch_every`. The first look reports what it
/// finds, gateway or not.
pub(super) async fn gateway_loop(shared: Arc<Shared>) {
    shared.watch_gateway();
    shared.send(Update::Gateway(shared.gateway()));
    loop {
        tokio::time::sleep(shared.tuning.watch_every).await;
        shared.watch_gateway();
    }
}

/// While a gateway runs: poll its status and metrics every
/// `traffic_every` (and on a refresh), and follow its request stream.
pub(super) async fn traffic_loop(shared: Arc<Shared>, signals: Arc<Signals>) {
    let mut gateway = shared.gateway.subscribe();
    loop {
        if gateway.borrow_and_update().is_none() {
            if gateway.changed().await.is_err() {
                return;
            }
            continue;
        }
        // Dropped (and so stopped) when the gateway stops or moves.
        let mut feed = tokio::task::JoinSet::new();
        feed.spawn(follow_feed(shared.clone()));
        loop {
            shared.send(Update::Traffic(traffic_snapshot(&shared).await));
            tokio::select! {
                () = tokio::time::sleep(shared.tuning.traffic_every) => {}
                () = signals.traffic.notified() => {}
                changed = gateway.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    break;
                }
            }
        }
    }
}

/// Subscribe to the gateway's request stream and pass every record on;
/// subscribe again when the stream ends. A failed subscription is a
/// warning, said once until one succeeds.
async fn follow_feed(shared: Arc<Shared>) {
    let mut failing: Option<String> = None;
    loop {
        match shared.link().events(FEED_CAP).await {
            Ok(mut events) => {
                failing = None;
                while let Some(item) = events.next().await {
                    shared.send(Update::Feed(item));
                }
            }
            Err(error) => {
                let text = format!("gateway request feed unavailable: {error}");
                if failing.as_ref() != Some(&text) {
                    shared.warn(text.clone());
                    failing = Some(text);
                }
            }
        }
        tokio::time::sleep(shared.tuning.traffic_every).await;
    }
}

/// The files a config is read from.
#[derive(Debug, Clone)]
pub struct ConfigFiles {
    config: PathBuf,
    secrets: PathBuf,
}

/// The [`FileStamp`]s of a [`ConfigFiles`]: different whenever either
/// file was rewritten, even within the same second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigStamp([Option<FileStamp>; 2]);

impl ConfigFiles {
    /// `config.toml` and `secrets.toml` under `paths`.
    pub fn new(paths: &StorePaths) -> Self {
        ConfigFiles {
            config: paths.config().to_path_buf(),
            secrets: paths.secrets().to_path_buf(),
        }
    }

    /// The files as they are now.
    pub fn stamp(&self) -> ConfigStamp {
        let of = |path: &Path| FileStamp::of(path);
        ConfigStamp([of(&self.config), of(&self.secrets)])
    }
}

/// Say [`Event::ConfigChanged`] whenever `files` differ from what was
/// seen last, starting from `since`.
pub(super) async fn config_loop(shared: Arc<Shared>, files: ConfigFiles, since: ConfigStamp) {
    let mut seen = since;
    loop {
        tokio::time::sleep(shared.tuning.watch_every).await;
        let now = files.stamp();
        if now != seen {
            seen = now;
            shared.emit(Event::ConfigChanged);
        }
    }
}
