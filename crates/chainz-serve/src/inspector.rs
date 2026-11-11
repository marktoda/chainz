//! The request inspector: a ring buffer of records, a live broadcast, and
//! an optional JSONL log file.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

use chainz_rpc::control::{EVENTS_BUFFER, EventsOptions, RequestRecord};
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

struct Ring {
    next_id: u64,
    records: VecDeque<RequestRecord>,
}

/// Keeps the last [`EVENTS_BUFFER`] records and fans new ones out.
pub struct Inspector {
    ring: Mutex<Ring>,
    live: broadcast::Sender<RequestRecord>,
    log: Mutex<Option<mpsc::UnboundedSender<RequestRecord>>>,
}

impl std::fmt::Debug for Inspector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inspector").finish_non_exhaustive()
    }
}

impl Inspector {
    /// An inspector, plus the log writer task when `log` is given. The file
    /// is opened for append (created 0600) before this returns, so a bad
    /// path fails the start.
    pub async fn new(log: Option<&Path>) -> std::io::Result<(Self, Option<JoinHandle<()>>)> {
        let (live, _) = broadcast::channel(4096);
        let mut writer = None;
        let mut sender = None;
        if let Some(path) = log {
            let mut options = tokio::fs::OpenOptions::new();
            options.append(true).create(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(path).await?;
            let (tx, mut rx) = mpsc::unbounded_channel::<RequestRecord>();
            sender = Some(tx);
            writer = Some(tokio::spawn(async move {
                while let Some(record) = rx.recv().await {
                    let Ok(mut line) = serde_json::to_vec(&record) else {
                        continue;
                    };
                    line.push(b'\n');
                    if file.write_all(&line).await.is_err() {
                        return;
                    }
                }
                let _ = file.flush().await;
            }));
        }
        let inspector = Inspector {
            ring: Mutex::new(Ring {
                next_id: 1,
                records: VecDeque::new(),
            }),
            live,
            log: Mutex::new(sender),
        };
        Ok((inspector, writer))
    }

    /// Store a record (its `id` is assigned here) and publish it.
    pub fn push(&self, mut record: RequestRecord) {
        // Broadcast under the lock, so a subscriber sees each record exactly
        // once: in its backlog or live.
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        record.id = ring.next_id;
        ring.next_id += 1;
        if ring.records.len() == EVENTS_BUFFER {
            ring.records.pop_front();
        }
        ring.records.push_back(record.clone());
        if let Some(log) = &*self.log.lock().unwrap_or_else(|e| e.into_inner()) {
            let _ = log.send(record.clone());
        }
        let _ = self.live.send(record);
    }

    /// The backlog asked for (oldest first) and, when following, a receiver
    /// for everything after it.
    pub fn subscribe(
        &self,
        options: EventsOptions,
    ) -> (
        Vec<RequestRecord>,
        Option<broadcast::Receiver<RequestRecord>>,
    ) {
        let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        let skip = ring.records.len().saturating_sub(options.backlog);
        let backlog = ring.records.iter().skip(skip).cloned().collect();
        (backlog, options.follow.then(|| self.live.subscribe()))
    }

    /// Stop logging: the writer task drains what was queued and exits.
    pub fn close_log(&self) {
        *self.log.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chainz_rpc::control::RecordStatus;

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

    #[tokio::test]
    async fn ring_is_bounded_and_ids_are_monotonic() {
        let (inspector, writer) = Inspector::new(None).await.unwrap();
        assert!(writer.is_none());
        for n in 0..(EVENTS_BUFFER as u64 + 5) {
            inspector.push(record(n));
        }
        let (all, rx) = inspector.subscribe(EventsOptions {
            backlog: usize::MAX,
            follow: false,
        });
        assert!(rx.is_none());
        assert_eq!(all.len(), EVENTS_BUFFER);
        assert_eq!(all[0].id, 6);
        assert_eq!(all.last().unwrap().id, EVENTS_BUFFER as u64 + 5);
        let (last, rx) = inspector.subscribe(EventsOptions {
            backlog: 2,
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
        let (inspector, writer) = Inspector::new(Some(&path)).await.unwrap();
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
}
