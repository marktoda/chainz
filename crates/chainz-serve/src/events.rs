//! The request records behind `/_chainz/events`: a bounded backlog plus a
//! live broadcast.

use std::collections::VecDeque;
use std::sync::Mutex;

use chainz_rpc::control::{Backlog, EventsOptions, RequestRecord};
use tokio::sync::broadcast;

/// Records kept for `backlog`.
pub(crate) const EVENTS_BUFFER: usize = 10_000;
/// Live records an events stream may fall behind by before it skips.
const LIVE_BUFFER: usize = 4096;

/// The last [`EVENTS_BUFFER`] request records plus a live broadcast.
///
/// A subscriber sees each record exactly once, in its backlog or live:
/// records are broadcast under the same lock the backlog is read under.
pub(crate) struct EventRing {
    inner: Mutex<RingInner>,
    live: broadcast::Sender<RequestRecord>,
}

struct RingInner {
    next_id: u64,
    records: VecDeque<RequestRecord>,
}

impl EventRing {
    /// An empty ring.
    pub(crate) fn new() -> Self {
        EventRing {
            inner: Mutex::new(RingInner {
                next_id: 1,
                records: VecDeque::new(),
            }),
            live: broadcast::channel(LIVE_BUFFER).0,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RingInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Keep `record` with the ID it has and broadcast it.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn push(&self, record: RequestRecord) {
        let mut inner = self.lock();
        inner.keep(record.clone());
        let _ = self.live.send(record);
    }

    /// Give `record` the next ID (1, 2, …), keep it and broadcast it.
    /// `also` sees the numbered record under the ring's lock, so whatever
    /// it does happens in ID order.
    pub(crate) fn push_numbered(
        &self,
        mut record: RequestRecord,
        also: impl FnOnce(&RequestRecord),
    ) {
        let mut inner = self.lock();
        record.id = inner.next_id;
        inner.next_id += 1;
        inner.keep(record.clone());
        also(&record);
        let _ = self.live.send(record);
    }

    /// The backlog asked for (oldest first) and, when following, a receiver
    /// for everything after it.
    pub(crate) fn subscribe(
        &self,
        options: EventsOptions,
    ) -> (
        Vec<RequestRecord>,
        Option<broadcast::Receiver<RequestRecord>>,
    ) {
        let inner = self.lock();
        let wanted = inner.records.iter().filter(|r| options.wants(r.id));
        let backlog = match options.backlog {
            Backlog::All => wanted.cloned().collect(),
            Backlog::Last(n) => {
                let mut last: Vec<RequestRecord> = wanted.rev().take(n).cloned().collect();
                last.reverse();
                last
            }
        };
        (backlog, options.follow.then(|| self.live.subscribe()))
    }

    /// Live subscribers right now.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn subscribers(&self) -> usize {
        self.live.receiver_count()
    }
}

impl RingInner {
    fn keep(&mut self, record: RequestRecord) {
        if self.records.len() == EVENTS_BUFFER {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }
}
