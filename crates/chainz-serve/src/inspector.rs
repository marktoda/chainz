//! The request inspector: the [`EventRing`] behind `/_chainz/events`, and an
//! optional JSONL log file.
//!
//! The log never slows a request down: records go to the writer task
//! through a bounded queue, and when the writer falls behind they are
//! dropped and counted. Drops and a failed write are reported as notices.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chainz_rpc::control::{RequestRecord, write_line};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::events::EventRing;

/// Records queued for the log writer before new ones are dropped.
const LOG_QUEUE: usize = 8192;
/// How often buffered log lines are flushed (and drops reported).
const LOG_FLUSH: Duration = Duration::from_secs(1);

/// Keeps the last records, fans new ones out, and feeds the `--log` file.
pub struct Inspector {
    ring: EventRing,
    log: Mutex<Option<mpsc::Sender<RequestRecord>>>,
    /// Records the log writer had no room for.
    dropped: Arc<AtomicU64>,
}

impl std::fmt::Debug for Inspector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inspector").finish_non_exhaustive()
    }
}

impl Inspector {
    /// An inspector, plus the log writer task when `log` is given. The file
    /// is opened for append (created 0600) before this returns, so a bad
    /// path fails the start. Log trouble (dropped records, a failed write)
    /// is reported on `notices`.
    pub async fn new(
        log: Option<&Path>,
        notices: broadcast::Sender<String>,
    ) -> std::io::Result<(Self, Option<JoinHandle<()>>)> {
        Inspector::with_queue(log, notices, LOG_QUEUE).await
    }

    /// [`Inspector::new`] with room for `queue` records before the log
    /// writer drops them.
    async fn with_queue(
        log: Option<&Path>,
        notices: broadcast::Sender<String>,
        queue: usize,
    ) -> std::io::Result<(Self, Option<JoinHandle<()>>)> {
        let dropped = Arc::new(AtomicU64::new(0));
        let mut writer = None;
        let mut sender = None;
        if let Some(path) = log {
            let file = chainz_os::open_private_log(path, chainz_os::LogMode::Append)?;
            let file = tokio::fs::File::from_std(file);
            let (tx, rx) = mpsc::channel(queue);
            sender = Some(tx);
            writer = Some(tokio::spawn(write_log(file, rx, dropped.clone(), notices)));
        }
        let inspector = Inspector {
            ring: EventRing::new(),
            log: Mutex::new(sender),
            dropped,
        };
        Ok((inspector, writer))
    }

    /// Store a record (its `id` is assigned here) and publish it.
    pub fn push(&self, record: RequestRecord) {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        self.ring.push_numbered(record, |record| {
            if let Some(log) = &*log
                && let Err(mpsc::error::TrySendError::Full(_)) = log.try_send(record.clone())
            {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        });
    }

    /// The records behind `/_chainz/events`.
    pub fn events(&self) -> &EventRing {
        &self.ring
    }

    /// Stop logging: the writer task drains what was queued and exits.
    pub fn close_log(&self) {
        *self.log.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// The log writer: one JSON line per record, buffered, flushed every
/// [`LOG_FLUSH`] and at the end.
async fn write_log(
    file: tokio::fs::File,
    mut records: mpsc::Receiver<RequestRecord>,
    dropped: Arc<AtomicU64>,
    notices: broadcast::Sender<String>,
) {
    let mut out = BufWriter::new(file);
    let mut tick = tokio::time::interval(LOG_FLUSH);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut reported = 0;
    let mut report_drops = |notices: &broadcast::Sender<String>| {
        let now = dropped.load(Ordering::Relaxed);
        if now > reported {
            let _ = notices.send(format!(
                "request log: {} records dropped (the log file can't keep up)",
                now - reported
            ));
            reported = now;
        }
    };
    let written: std::io::Result<()> = async {
        loop {
            tokio::select! {
                record = records.recv() => {
                    let Some(record) = record else { break };
                    match write_line(&record) {
                        Ok(line) => out.write_all(&line).await?,
                        Err(error) => {
                            let _ = notices.send(format!(
                                "request log: record {} skipped: {error}",
                                record.id
                            ));
                        }
                    }
                }
                _ = tick.tick() => {
                    out.flush().await?;
                    report_drops(&notices);
                }
            }
        }
        out.flush().await
    }
    .await;
    report_drops(&notices);
    if let Err(error) = written {
        let _ = notices.send(format!(
            "request log: write failed, logging stopped: {error}"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EVENTS_BUFFER;
    use chainz_rpc::control::{Backlog, EventsOptions, RecordStatus};

    fn record(n: u64) -> RequestRecord {
        RequestRecord {
            id: 0,
            at_ms: n,
            chain: "eth".into(),
            method: "eth_call".into(),
            params_summary: String::new(),
            upstream: "a".into(),
            attempts: 1,
            latency_ms: 1,
            status: RecordStatus::Ok,
            response_bytes: 0,
        }
    }

    fn notices() -> broadcast::Sender<String> {
        broadcast::channel(16).0
    }

    #[tokio::test]
    async fn ring_is_bounded_and_ids_are_monotonic() {
        let (inspector, writer) = Inspector::new(None, notices()).await.unwrap();
        assert!(writer.is_none());
        for n in 0..(EVENTS_BUFFER as u64 + 5) {
            inspector.push(record(n));
        }
        let (all, rx) = inspector.events().subscribe(EventsOptions {
            backlog: Backlog::All,
            after_id: None,
            follow: false,
        });
        assert!(rx.is_none());
        assert_eq!(all.len(), EVENTS_BUFFER);
        assert_eq!(all[0].id, 6);
        assert_eq!(all.last().unwrap().id, EVENTS_BUFFER as u64 + 5);
        let (last, rx) = inspector.events().subscribe(EventsOptions {
            backlog: Backlog::Last(2),
            after_id: None,
            follow: true,
        });
        assert_eq!(last.len(), 2);
        let mut rx = rx.unwrap();
        inspector.push(record(99));
        assert_eq!(rx.recv().await.unwrap().at_ms, 99);
    }

    #[tokio::test]
    async fn log_file_gets_one_json_line_per_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.jsonl");
        std::fs::write(&path, "existing\n").unwrap();
        let (inspector, writer) = Inspector::new(Some(&path), notices()).await.unwrap();
        inspector.push(record(1));
        inspector.push(record(2));
        inspector.close_log();
        writer.unwrap().await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "appended, not truncated");
        let parsed: RequestRecord = serde_json::from_str(lines[2]).unwrap();
        assert_eq!((parsed.id, parsed.at_ms), (2, 2));
    }

    #[tokio::test]
    async fn a_full_log_queue_drops_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.jsonl");
        let notices = notices();
        let mut heard = notices.subscribe();
        let (inspector, writer) = Inspector::with_queue(Some(&path), notices, 4)
            .await
            .unwrap();
        // The current-thread test runtime doesn't run the writer until we
        // yield: everything past the queue's 4 slots is dropped.
        for n in 0..10 {
            inspector.push(record(n));
        }
        assert_eq!(inspector.dropped.load(Ordering::Relaxed), 6);
        // The ring keeps them all regardless.
        let (all, _) = inspector.events().subscribe(EventsOptions {
            backlog: Backlog::Last(100),
            after_id: None,
            follow: false,
        });
        assert_eq!(all.len(), 10);
        inspector.close_log();
        writer.unwrap().await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 4);
        let notice = heard.recv().await.unwrap();
        assert!(notice.contains("6 records dropped"), "{notice}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_failed_write_is_reported() {
        // Every write to /dev/full fails with ENOSPC.
        let notices = notices();
        let mut heard = notices.subscribe();
        let (inspector, writer) = Inspector::new(Some(Path::new("/dev/full")), notices)
            .await
            .unwrap();
        inspector.push(record(1));
        inspector.close_log();
        writer.unwrap().await.unwrap();
        let notice = heard.recv().await.unwrap();
        assert!(notice.contains("write failed"), "{notice}");
    }
}
