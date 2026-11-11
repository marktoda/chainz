//! The gateway's traffic, in the dashboard's own shape.

use chainz_rpc::ErrorClass;

/// Request records kept for the Traffic feed.
pub const FEED_CAP: usize = 500;

/// The gateway's view of one chain.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GatewayChain {
    /// Chain name.
    pub chain: String,
    /// Active endpoint.
    pub active: String,
    /// Head high-water mark.
    pub head: Option<u64>,
}

/// Totals for one method or one endpoint within a metrics window.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct TrafficRow {
    /// Method name, or `chain/endpoint`.
    pub name: String,
    /// Requests.
    pub requests: u64,
    /// Errors of every class.
    pub errors: u64,
    /// HTTP 429s.
    pub rate_limited: u64,
    /// Failovers.
    pub failovers: u64,
    /// Median latency (request-weighted across merged rows).
    pub p50_ms: u32,
    /// Worst 95th percentile among merged rows.
    pub p95_ms: u32,
}

/// One metrics window, aggregated two ways.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct TrafficWindow {
    /// Window length.
    pub window_secs: u64,
    /// Requests in the window.
    pub requests: u64,
    /// Errors in the window.
    pub errors: u64,
    /// 429s in the window.
    pub rate_limited: u64,
    /// Failovers in the window.
    pub failovers: u64,
    /// Response bytes in the window.
    pub bytes: u64,
    /// Per method, busiest first.
    pub methods: Vec<TrafficRow>,
    /// Per `chain/endpoint`, busiest first.
    pub endpoints: Vec<TrafficRow>,
}

/// Gateway status plus metrics at one instant.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct TrafficSnapshot {
    /// Gateway version.
    pub version: String,
    /// Gateway port.
    pub port: u16,
    /// Seconds since the gateway started.
    pub uptime_secs: u64,
    /// Served chains.
    pub chains: Vec<GatewayChain>,
    /// Metrics windows, shortest first.
    pub windows: Vec<TrafficWindow>,
}

/// One proxied request in the live feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedItem {
    /// Record ID.
    pub id: u64,
    /// Arrival time, unix milliseconds.
    pub at_ms: u64,
    /// Chain name.
    pub chain: String,
    /// JSON-RPC method.
    pub method: String,
    /// The gateway's labelled, already-redacted parameter summary.
    pub summary: String,
    /// Endpoint that served it.
    pub upstream: String,
    /// Attempts across endpoints.
    pub attempts: u32,
    /// Total latency.
    pub latency_ms: u32,
    /// Error class and JSON-RPC code, when it failed.
    pub error: Option<(ErrorClass, Option<i64>)>,
}
