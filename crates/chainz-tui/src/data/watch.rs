//! The watchers: a gateway starting or stopping, its metrics and request
//! stream while it runs, and the config files changing on disk.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use chainz_core::runtime::{GatewayState, Runtime, ServeInfo};
use chainz_core::{StorePaths, StoreStamp};
use tokio::sync::watch;

use super::{SayOnce, Shared, Signals, Update, traffic_snapshot};
use crate::model::FEED_CAP;

/// The gateway as last seen, for the workers that route through it and
/// the one that follows its traffic.
#[derive(Debug)]
pub(super) struct GatewayWatch {
    runtime: Runtime,
    current: watch::Sender<Option<ServeInfo>>,
    /// Errors reading the gateway's record, so each is said once.
    error: SayOnce,
}

impl GatewayWatch {
    pub(super) fn new(runtime: Runtime) -> Self {
        GatewayWatch {
            runtime,
            current: watch::channel(None).0,
            error: SayOnce::default(),
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
    /// read from it. Returns a gateway record that can't be read, the
    /// first time it fails so.
    pub(super) fn detect(&self, changed: impl FnOnce(Option<&ServeInfo>)) -> Option<String> {
        let (found, error) = match self.runtime.gateway() {
            Ok(GatewayState::Up(info)) => (Some(info), None),
            Ok(GatewayState::Down | GatewayState::Starting) => (None, None),
            Err(error) => (None, Some(format!("gateway state unreadable: {error}"))),
        };
        let warning = self.error.report(error);
        if *self.current.borrow() != found {
            changed(found.as_ref());
            self.current.send_replace(found);
        }
        warning
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
    let failing = SayOnce::default();
    loop {
        match shared.link().events(FEED_CAP).await {
            Ok(mut events) => {
                failing.report(None);
                while let Some(item) = events.next().await {
                    shared.send(Update::Feed(item));
                }
            }
            Err(error) => {
                let text = format!("gateway request feed unavailable: {error}");
                if let Some(text) = failing.report(Some(text)) {
                    shared.warn(text);
                }
            }
        }
        tokio::time::sleep(shared.tuning.traffic_every).await;
    }
}

/// Raise [`Shared::take_config_changed`] whenever the config under `paths`
/// differs from what was seen last, starting from `since`.
pub(super) async fn config_loop(shared: Arc<Shared>, paths: StorePaths, since: StoreStamp) {
    let mut seen = since;
    loop {
        tokio::time::sleep(shared.tuning.watch_every).await;
        let now = paths.stamp();
        if now != seen {
            seen = now;
            shared.config_changed.store(true, Ordering::Relaxed);
        }
    }
}
