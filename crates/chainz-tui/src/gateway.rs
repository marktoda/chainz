//! The only module that touches the control protocol
//! (`chainz_rpc::control`). Everything else in this crate sees the plain
//! types in [`crate::model`], so a protocol change lands here and nowhere
//! else.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use chainz_rpc::control::{
    Backlog, ControlClient, EventsOptions, MetricsRow, MetricsSnapshot, MetricsWindow,
    RecordStatus, RequestRecord, Status,
};
use tokio::sync::mpsc;

use crate::model::{FeedItem, GatewayChain, TrafficRow, TrafficSnapshot, TrafficWindow};

/// A handle on a running gateway's control socket.
#[derive(Debug, Clone)]
pub struct GatewayLink {
    client: ControlClient,
}

/// The gateway's live request stream.
#[derive(Debug)]
pub struct EventStream {
    records: mpsc::Receiver<RequestRecord>,
}

impl EventStream {
    /// The next proxied request; `None` once the gateway closes the stream.
    pub async fn next(&mut self) -> Option<FeedItem> {
        self.records.recv().await.map(feed_item)
    }
}

impl GatewayLink {
    /// A link to the control socket at `socket`; connects per request.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        GatewayLink {
            client: ControlClient::new(socket),
        }
    }

    /// Status and metrics, folded into the dashboard's own shape.
    pub async fn snapshot(&self) -> io::Result<TrafficSnapshot> {
        let (status, metrics) = tokio::try_join!(self.client.status(), self.client.metrics())?;
        Ok(snapshot(status, metrics))
    }

    /// Subscribe to the request stream: first up to `backlog` requests the
    /// gateway already served (oldest first), then live ones.
    pub async fn events(&self, backlog: usize) -> io::Result<EventStream> {
        let options = EventsOptions {
            backlog: Backlog::Last(backlog),
            after_id: None,
            follow: true,
        };
        Ok(EventStream {
            records: self.client.events(options).await?,
        })
    }

    /// Ask the gateway to re-read the config now.
    pub async fn reload(&self) -> io::Result<()> {
        self.client.reload().await
    }
}

fn feed_item(record: RequestRecord) -> FeedItem {
    FeedItem {
        id: record.id,
        at_ms: record.at_ms,
        chain: record.chain,
        method: record.method,
        summary: record.params_summary,
        upstream: record.upstream,
        attempts: record.attempts,
        latency_ms: record.latency_ms,
        error: match record.status {
            RecordStatus::Ok => None,
            failed => Some(failed.to_string()),
        },
    }
}

fn snapshot(status: Status, metrics: MetricsSnapshot) -> TrafficSnapshot {
    let mut windows: Vec<TrafficWindow> = metrics.windows.into_iter().map(window).collect();
    windows.sort_by_key(|w| w.window_secs);
    TrafficSnapshot {
        version: status.version,
        port: status.port,
        uptime_secs: status.uptime_secs,
        chains: status
            .chains
            .into_iter()
            .map(|c| GatewayChain {
                chain: c.chain,
                active: c.active,
                head: c.head,
            })
            .collect(),
        windows,
    }
}

/// Rows of one method or one endpoint being merged: totals plus the
/// request-weighted p50 sum. (The gateway sends true percentiles per
/// window and per chain only; per method and per endpoint the dashboard
/// weights the rows' medians.)
#[derive(Default)]
struct Merge {
    row: TrafficRow,
    weighted_p50: u128,
}

impl Merge {
    fn add(&mut self, r: &MetricsRow) {
        self.row.requests += r.requests;
        self.row.errors += r.errors.values().sum::<u64>();
        self.row.rate_limited += r.rate_limited;
        self.row.failovers += r.failovers;
        self.row.p95_ms = self.row.p95_ms.max(r.p95_ms);
        self.weighted_p50 += u128::from(r.p50_ms) * u128::from(r.requests);
    }

    fn finish(mut self, name: String) -> TrafficRow {
        self.row.name = name;
        if self.row.requests > 0 {
            let p50 = self.weighted_p50 / u128::from(self.row.requests);
            self.row.p50_ms = u32::try_from(p50).unwrap_or(u32::MAX);
        }
        self.row
    }
}

fn window(w: MetricsWindow) -> TrafficWindow {
    let mut out = TrafficWindow {
        window_secs: w.window_secs,
        totals: w.totals,
        ..TrafficWindow::default()
    };
    let mut methods: BTreeMap<String, Merge> = BTreeMap::new();
    let mut endpoints: BTreeMap<String, Merge> = BTreeMap::new();
    for row in &w.rows {
        methods.entry(row.method.clone()).or_default().add(row);
        endpoints
            .entry(format!("{}/{}", row.chain, row.endpoint))
            .or_default()
            .add(row);
    }
    let finish = |merged: BTreeMap<String, Merge>| {
        let mut rows: Vec<TrafficRow> = merged
            .into_iter()
            .map(|(name, merge)| merge.finish(name))
            .collect();
        // Busiest first; the map already ordered ties by name.
        rows.sort_by_key(|row| std::cmp::Reverse(row.requests));
        rows
    };
    out.methods = finish(methods);
    out.endpoints = finish(endpoints);
    out
}

#[cfg(test)]
pub(crate) mod testing {
    //! Fixtures in the control protocol's own types, for the tests that
    //! drive a `ControlServer`.

    use super::*;
    use chainz_rpc::ErrorClass;
    use chainz_rpc::control::{ChainStatus, MetricsTotals};
    pub(crate) use chainz_serve::testing::ControlServer;

    pub(crate) fn status() -> Status {
        Status {
            version: "2.0.0-test".into(),
            pid: 4242,
            port: 18545,
            uptime_secs: 7_380,
            chains: vec![
                ChainStatus {
                    chain: "eth".into(),
                    chain_id: 1,
                    active: "infura".into(),
                    failover: vec!["llama".into()],
                    head: Some(23_481_220),
                },
                ChainStatus {
                    chain: "base".into(),
                    chain_id: 8453,
                    active: "infura".into(),
                    failover: vec![],
                    head: Some(38_112_904),
                },
            ],
        }
    }

    fn row(
        chain: &str,
        endpoint: &str,
        method: &str,
        requests: u64,
        p50_ms: u32,
        errors: &[(ErrorClass, u64)],
        rate_limited: u64,
    ) -> MetricsRow {
        MetricsRow {
            chain: chain.into(),
            endpoint: endpoint.into(),
            method: method.into(),
            requests,
            errors: errors.iter().copied().collect(),
            p50_ms,
            p95_ms: p50_ms * 3,
            rate_limited,
            failovers: u64::from(rate_limited > 0),
            bytes: requests * 512,
        }
    }

    pub(crate) fn metrics() -> MetricsSnapshot {
        MetricsSnapshot {
            at_ms: 1_700_000_000_000,
            windows: vec![
                metrics_window(
                    900,
                    vec![row("eth", "infura", "eth_call", 9_000, 50, &[], 0)],
                ),
                metrics_window(
                    60,
                    vec![
                        row("eth", "infura", "eth_call", 400, 40, &[], 0),
                        row("base", "infura", "eth_call", 200, 70, &[], 12),
                        row(
                            "eth",
                            "llama",
                            "eth_getLogs",
                            120,
                            210,
                            &[(ErrorClass::Timeout, 2), (ErrorClass::Rpc, 1)],
                            0,
                        ),
                        row("eth", "infura", "eth_blockNumber", 22, 31, &[], 0),
                    ],
                ),
            ],
        }
    }

    /// A window with the totals a current gateway sends: here, its rows
    /// added up (percentiles aside).
    pub(crate) fn metrics_window(window_secs: u64, rows: Vec<MetricsRow>) -> MetricsWindow {
        let mut totals = MetricsTotals::default();
        for row in &rows {
            totals.requests += row.requests;
            totals.errors += row.errors.values().sum::<u64>();
            totals.rate_limited += row.rate_limited;
            totals.failovers += row.failovers;
            totals.bytes += row.bytes;
        }
        MetricsWindow {
            window_secs,
            rows,
            totals,
            chains: BTreeMap::new(),
        }
    }

    pub(crate) fn record(id: u64, at_ms: u64, method: &str, summary: &str) -> RequestRecord {
        RequestRecord {
            id,
            at_ms,
            chain: "base".into(),
            method: method.into(),
            params_summary: summary.into(),
            upstream: "infura".into(),
            attempts: 1,
            latency_ms: 61,
            status: RecordStatus::Ok,
            response_bytes: 322,
        }
    }

    pub(crate) fn failed_record(id: u64, at_ms: u64) -> RequestRecord {
        RequestRecord {
            chain: "eth".into(),
            upstream: "llama".into(),
            attempts: 2,
            latency_ms: 4_000,
            status: RecordStatus::Error {
                class: ErrorClass::RateLimited,
                code: None,
            },
            ..record(id, at_ms, "eth_getLogs", "@usdc Transfer 2,000 blocks")
        }
    }

    /// The fixtures as the dashboard sees them.
    pub(crate) fn traffic() -> TrafficSnapshot {
        snapshot(status(), metrics())
    }

    pub(crate) fn feed() -> Vec<FeedItem> {
        vec![
            feed_item(record(
                1,
                1_700_000_000_000,
                "eth_call",
                "@usdc.balanceOf(@wallet)",
            )),
            feed_item(record(2, 1_700_000_001_000, "eth_blockNumber", "")),
            feed_item(failed_record(3, 1_700_000_002_000)),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use chainz_rpc::control::MetricsTotals;
    use std::time::Duration;

    #[test]
    fn metrics_fold_by_method_and_endpoint() {
        let t = traffic();
        assert_eq!((t.port, t.uptime_secs), (18545, 7_380));
        assert_eq!(t.chains[0].head, Some(23_481_220));
        // Shortest window first, whatever order the gateway sent.
        let w = &t.windows[0];
        assert_eq!(w.window_secs, 60);
        let totals = w.totals;
        assert_eq!(
            (totals.requests, totals.errors, totals.rate_limited),
            (742, 3, 12)
        );
        let call = &w.methods[0];
        assert_eq!((call.name.as_str(), call.requests), ("eth_call", 600));
        // (400 × 40 + 200 × 70) / 600
        assert_eq!(call.p50_ms, 50);
        assert_eq!(call.p95_ms, 210);
        let names: Vec<&str> = w.endpoints.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["eth/infura", "base/infura", "eth/llama"]);
        assert_eq!(w.endpoints[1].rate_limited, 12);
        assert_eq!(w.endpoints[2].errors, 3);
    }

    #[test]
    fn window_totals_are_the_gateways() {
        let mut minute = metrics().windows.remove(1);
        // The gateway counts what the rows cannot show (requests its rate
        // limiter turned away, say): its totals are used as they are.
        let totals = MetricsTotals {
            requests: 750,
            errors: 4,
            rate_limited: 13,
            failovers: 2,
            p50_ms: 45,
            p95_ms: 200,
            bytes: 1_000,
        };
        minute.totals = totals;
        let w = window(minute);
        assert_eq!(w.totals, totals);
        // The per-method rows still come from the rows.
        assert_eq!(w.methods[0].requests, 600);
    }

    #[tokio::test]
    async fn reads_status_metrics_events_and_reloads_over_the_socket() {
        let dir = crate::data::testing::short_tempdir();
        let socket = dir.path().join("serve.sock");
        let server = ControlServer::start(&socket, status(), metrics())
            .await
            .unwrap();
        let link = GatewayLink::new(&socket);

        assert_eq!(link.snapshot().await.unwrap(), traffic());

        // Served before the dashboard opened: replayed as backlog.
        server.push_event(record(5, 500, "eth_chainId", ""));
        server.push_event(record(6, 600, "eth_gasPrice", ""));
        let mut events = link.events(1).await.unwrap();
        let replayed = tokio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((replayed.id, replayed.method.as_str()), (6, "eth_gasPrice"));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while server.event_subscribers() == 0 {
            assert!(std::time::Instant::now() < deadline, "no events subscriber");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        server.push_event(record(7, 1_000, "eth_call", "@usdc.balanceOf(@wallet)"));
        server.push_event(failed_record(8, 2_000));
        let first = tokio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (first.id, first.summary.as_str()),
            (7, "@usdc.balanceOf(@wallet)")
        );
        assert_eq!(first.error, None);
        let second = tokio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.error.as_deref(), Some("rate_limited"));

        link.reload().await.unwrap();
        assert_eq!(server.reload_count(), 1);

        // A dead socket is an error, not a hang.
        drop(server);
        assert!(link.snapshot().await.is_err());
    }
}
