//! The public chain registry at <https://chainid.network/chains.json>.
//!
//! Cached as `<cache_dir>/chains.json` for 24 hours; a stale cache is used
//! when the registry can't be reached.

use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, SystemTime};

/// The registry URL.
pub const CHAINLIST_URL: &str = "https://chainid.network/chains.json";
/// How long a cached copy is considered fresh.
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const CACHE_FILE: &str = "chains.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// One chain in the registry.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChainlistEntry {
    /// Human-readable name.
    pub name: String,
    /// Chain ID.
    #[serde(rename = "chainId")]
    pub chain_id: u64,
    /// Short name (e.g. `eth`, `base`).
    #[serde(rename = "shortName", default)]
    pub short_name: String,
    /// RPC URLs; some contain `${VAR}` placeholders for API keys.
    #[serde(default)]
    pub rpc: Vec<String>,
    /// The native currency.
    #[serde(rename = "nativeCurrency", default)]
    pub native_currency: Option<NativeCurrency>,
}

/// A chain's native currency.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct NativeCurrency {
    /// Name (e.g. `Ether`).
    pub name: String,
    /// Symbol (e.g. `ETH`).
    pub symbol: String,
    /// Decimals (usually 18).
    pub decimals: u8,
}

/// Why the registry could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ChainlistError {
    /// The download failed and there was no cached copy.
    #[error("could not download the chain registry: {0}")]
    Fetch(String),
    /// The registry (downloaded) was not valid JSON.
    #[error("the chain registry is malformed")]
    Parse,
    /// The cache could not be written.
    #[error("could not write the chain registry cache: {0}")]
    Io(#[from] io::Error),
    /// No entry with that chain ID.
    #[error("chain ID {0} is not in the chain registry")]
    NotFound(u64),
}

/// All registry entries: the cache when it is fresh (and `refresh` is false),
/// otherwise a download (which refreshes the cache), falling back to a stale
/// cache when the download fails.
pub async fn fetch_all(
    cache_dir: &Path,
    refresh: bool,
) -> Result<Vec<ChainlistEntry>, ChainlistError> {
    fetch_all_from(CHAINLIST_URL, cache_dir, refresh, CACHE_TTL).await
}

/// The cached entries, ignoring the TTL; `None` when absent or unreadable.
pub fn read_cache(cache_dir: &Path) -> Option<Vec<ChainlistEntry>> {
    let bytes = std::fs::read(cache_dir.join(CACHE_FILE)).ok()?;
    parse(&bytes).ok()
}

/// The entry for `chain_id` (see [`fetch_all`] for caching).
pub async fn find(
    cache_dir: &Path,
    chain_id: u64,
    refresh: bool,
) -> Result<ChainlistEntry, ChainlistError> {
    fetch_all(cache_dir, refresh)
        .await?
        .into_iter()
        .find(|e| e.chain_id == chain_id)
        .ok_or(ChainlistError::NotFound(chain_id))
}

/// Public RPC URLs usable without credentials: entries containing `${` are
/// dropped; `http(s)` and `ws(s)` URLs are kept, in registry order, without
/// duplicates.
pub fn public_rpcs(entry: &ChainlistEntry) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for url in &entry.rpc {
        let url = url.trim();
        if url.contains("${") {
            continue;
        }
        let lower = url.to_ascii_lowercase();
        let ok = ["http://", "https://", "ws://", "wss://"]
            .iter()
            .any(|s| lower.starts_with(s));
        if ok && !out.iter().any(|u| u == url) {
            out.push(url.to_string());
        }
    }
    out
}

async fn fetch_all_from(
    url: &str,
    cache_dir: &Path,
    refresh: bool,
    ttl: Duration,
) -> Result<Vec<ChainlistEntry>, ChainlistError> {
    if !refresh
        && cache_is_fresh(cache_dir, ttl)
        && let Some(entries) = read_cache(cache_dir)
    {
        return Ok(entries);
    }
    match download(url).await {
        Ok(bytes) => match parse(&bytes) {
            Ok(entries) => {
                // The download succeeded: a cache that can't be written
                // costs only the next fetch.
                if let Err(e) = write_cache(cache_dir, &bytes) {
                    eprintln!(
                        "warning: could not cache the chainlist registry in {}: {e}",
                        cache_dir.display()
                    );
                }
                Ok(entries)
            }
            Err(e) => read_cache(cache_dir).ok_or(e),
        },
        Err(e) => read_cache(cache_dir).ok_or(e),
    }
}

fn cache_is_fresh(cache_dir: &Path, ttl: Duration) -> bool {
    std::fs::metadata(cache_dir.join(CACHE_FILE))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < ttl)
}

async fn download(url: &str) -> Result<Vec<u8>, ChainlistError> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(concat!("chainz/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| ChainlistError::Fetch("could not build HTTP client".into()))?;
    let resp = client.get(url).send().await.map_err(|e| {
        ChainlistError::Fetch(if e.is_timeout() {
            "timed out".into()
        } else {
            "could not connect".into()
        })
    })?;
    if !resp.status().is_success() {
        return Err(ChainlistError::Fetch(format!(
            "HTTP status {}",
            resp.status().as_u16()
        )));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|_| ChainlistError::Fetch("could not read response".into()))
}

/// Parse the registry, skipping individual entries that don't fit the schema.
fn parse(bytes: &[u8]) -> Result<Vec<ChainlistEntry>, ChainlistError> {
    let values: Vec<serde_json::Value> =
        serde_json::from_slice(bytes).map_err(|_| ChainlistError::Parse)?;
    Ok(values
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect())
}

fn write_cache(cache_dir: &Path, bytes: &[u8]) -> io::Result<()> {
    std::fs::create_dir_all(cache_dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(cache_dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(cache_dir.join(CACHE_FILE))
        .map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
      {"name":"Ethereum Mainnet","chainId":1,"shortName":"eth",
       "rpc":["https://mainnet.infura.io/v3/${INFURA_API_KEY}","https://eth.llamarpc.com","wss://eth.example.org","ftp://nope","https://eth.llamarpc.com"],
       "nativeCurrency":{"name":"Ether","symbol":"ETH","decimals":18},"extra":true},
      {"name":"Bare","chainId":8453},
      {"name":"Broken","chainId":"not-a-number"}
    ]"#;

    /// A local URL that refuses connections (nothing listens there).
    fn dead_url() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        format!("http://127.0.0.1:{port}/chains.json")
    }

    #[test]
    fn parse_skips_bad_entries_and_defaults() {
        let entries = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].short_name, "eth");
        assert_eq!(entries[0].native_currency.as_ref().unwrap().decimals, 18);
        assert_eq!(entries[1].chain_id, 8453);
        assert!(entries[1].rpc.is_empty());
        assert!(entries[1].native_currency.is_none());
        assert!(matches!(parse(b"{}"), Err(ChainlistError::Parse)));
    }

    #[test]
    fn public_rpcs_filters() {
        let entries = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(
            public_rpcs(&entries[0]),
            vec!["https://eth.llamarpc.com", "wss://eth.example.org"]
        );
    }

    #[tokio::test]
    async fn fresh_cache_is_used_without_fetching() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CACHE_FILE), SAMPLE).unwrap();
        let entries = fetch_all_from(&dead_url(), dir.path(), false, CACHE_TTL)
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(read_cache(dir.path()).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn stale_cache_used_when_offline() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CACHE_FILE), SAMPLE).unwrap();
        // ttl 0 => stale; refresh => forced fetch; both fall back to cache.
        let entries = fetch_all_from(&dead_url(), dir.path(), false, Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        let entries = fetch_all_from(&dead_url(), dir.path(), true, CACHE_TTL)
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn no_cache_and_offline_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = fetch_all_from(&dead_url(), dir.path(), false, CACHE_TTL)
            .await
            .unwrap_err();
        assert!(matches!(err, ChainlistError::Fetch(_)), "{err}");
        assert!(read_cache(dir.path()).is_none());
    }

    /// Serve `SAMPLE` once; returns the URL.
    async fn serve_sample_once() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                SAMPLE.len(),
                SAMPLE
            );
            s.write_all(resp.as_bytes()).await.unwrap();
        });
        format!("http://{addr}/chains.json")
    }

    #[tokio::test]
    async fn download_writes_cache() {
        let url = serve_sample_once().await;
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("nested");
        let entries = fetch_all_from(&url, &cache, false, CACHE_TTL)
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(read_cache(&cache).unwrap()[0].chain_id, 1);
    }

    #[tokio::test]
    async fn a_failed_cache_write_keeps_the_download() {
        let url = serve_sample_once().await;
        let dir = tempfile::tempdir().unwrap();
        // The cache dir is a file: the cache can't be written.
        let cache = dir.path().join("not-a-dir");
        std::fs::write(&cache, "").unwrap();
        let entries = fetch_all_from(&url, &cache, true, CACHE_TTL)
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
    }
}
