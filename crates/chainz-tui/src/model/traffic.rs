//! The gateway's traffic, in the dashboard's own shape.

use chainz_rpc::control::MetricsTotals;
use serde::Serialize;

/// Request records kept for the Traffic feed.
pub const FEED_CAP: usize = 500;

/// The gateway's view of one chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GatewayChain {
    pub chain: String,
    /// Active endpoint.
    pub active: String,
    /// Head high-water mark.
    pub head: Option<u64>,
}

/// Totals for one method or one endpoint within a metrics window.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct TrafficRow {
    /// Method name, or `chain/endpoint`.
    pub name: String,
    pub requests: u64,
    /// Errors of every class.
    pub errors: u64,
    /// HTTP 429s.
    pub rate_limited: u64,
    pub failovers: u64,
    /// Median latency (request-weighted across merged rows).
    pub p50_ms: u32,
    /// Worst 95th percentile among merged rows.
    pub p95_ms: u32,
}

/// One metrics window, aggregated two ways.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct TrafficWindow {
    pub window_secs: u64,
    /// The gateway's totals for the window. The rows can't stand in for
    /// them: they miss requests the rate limiter turned away.
    #[serde(flatten)]
    pub totals: MetricsTotals,
    /// Per method, busiest first.
    pub methods: Vec<TrafficRow>,
    /// Per `chain/endpoint`, busiest first.
    pub endpoints: Vec<TrafficRow>,
}

/// Gateway status plus metrics at one instant; `top --once --json` prints
/// it as it is.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct TrafficSnapshot {
    pub version: String,
    pub port: u16,
    /// Seconds since the gateway started.
    pub uptime_secs: u64,
    pub chains: Vec<GatewayChain>,
    /// Metrics windows, shortest first.
    pub windows: Vec<TrafficWindow>,
}

/// One proxied request in the live feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedItem {
    pub id: u64,
    /// Arrival time, unix milliseconds.
    pub at_ms: u64,
    pub chain: String,
    pub method: String,
    /// The gateway's labelled, already-redacted parameter summary.
    pub summary: String,
    /// Endpoint that served it.
    pub upstream: String,
    /// Attempts across endpoints.
    pub attempts: u32,
    pub latency_ms: u32,
    /// How it failed, as `serve log` shows it (`timeout`,
    /// `rpc(-32000)`); `None` when it succeeded.
    pub error: Option<String>,
}
