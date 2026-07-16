use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

const CHAINLIST_URL: &str = "https://chainid.network/chains.json";
/// chains.json is several MB and changes rarely; re-download at most daily.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Deserialize, Debug, Clone)]
pub struct ChainlistEntry {
    pub name: String,
    #[serde(rename = "chainId")]
    pub chain_id: u64,
    pub rpc: Vec<String>,
}

/// Fetch the chainlist, served from a local cache unless it is stale or
/// `refresh` is set. Falls back to a stale cache if the network fails.
pub async fn fetch_all_chains(refresh: bool) -> Result<Vec<ChainlistEntry>> {
    let cache = cache_path();

    if !refresh && let Some(chains) = read_cache(cache.as_deref(), CACHE_TTL).await {
        return Ok(chains);
    }

    match fetch_from_network().await {
        Ok(body) => {
            let chains = serde_json::from_str(&body)?;
            if let Some(path) = &cache {
                // Best-effort: a failed cache write shouldn't fail the command
                if let Some(dir) = path.parent() {
                    let _ = tokio::fs::create_dir_all(dir).await;
                }
                let _ = tokio::fs::write(path, &body).await;
            }
            Ok(chains)
        }
        Err(e) => {
            // Network down: a stale cache beats no data
            match read_cache(cache.as_deref(), Duration::MAX).await {
                Some(chains) => {
                    eprintln!("Warning: chainlist fetch failed ({e}); using cached copy");
                    Ok(chains)
                }
                None => Err(e),
            }
        }
    }
}

async fn fetch_from_network() -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    Ok(client
        .get(CHAINLIST_URL)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

fn cache_path() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("chainz").join("chains.json"))
}

async fn read_cache(path: Option<&std::path::Path>, ttl: Duration) -> Option<Vec<ChainlistEntry>> {
    let path = path?;
    let age = tokio::fs::metadata(path)
        .await
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()?;
    if age > ttl {
        return None;
    }
    let json = tokio::fs::read_to_string(path).await.ok()?;
    // A corrupt cache is treated as missing (it will be re-downloaded)
    serde_json::from_str(&json).ok()
}

pub async fn fetch_chain_data(
    chain_id: Option<u64>,
    name: Option<String>,
    refresh: bool,
) -> Result<ChainlistEntry> {
    let chains = fetch_all_chains(refresh).await?;

    let chain = if let Some(id) = chain_id {
        chains.into_iter().find(|c| c.chain_id == id)
    } else if let Some(name) = name {
        let name_lower = name.to_lowercase();
        chains
            .into_iter()
            .find(|c| c.name.to_lowercase() == name_lower)
    } else {
        None
    };

    chain.ok_or_else(|| anyhow!("Chain not found in chainlist"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_single_chain_entry() {
        let json = r#"{"name":"Ethereum Mainnet","chainId":1,"rpc":["https://eth.llamarpc.com","https://rpc.ankr.com/eth"]}"#;
        let entry: ChainlistEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.name, "Ethereum Mainnet");
        assert_eq!(entry.chain_id, 1);
        assert_eq!(entry.rpc.len(), 2);
        assert_eq!(entry.rpc[0], "https://eth.llamarpc.com");
        assert_eq!(entry.rpc[1], "https://rpc.ankr.com/eth");
    }

    #[test]
    fn deserialize_with_empty_rpcs() {
        let json = r#"{"name":"No RPCs","chainId":999,"rpc":[]}"#;
        let entry: ChainlistEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.name, "No RPCs");
        assert_eq!(entry.chain_id, 999);
        assert!(entry.rpc.is_empty());
    }

    #[test]
    fn deserialize_array_of_chains() {
        let json = r#"[
            {"name":"Ethereum Mainnet","chainId":1,"rpc":["https://rpc.example.com"]},
            {"name":"Optimism","chainId":10,"rpc":["https://opt.example.com","https://opt2.example.com"]}
        ]"#;
        let entries: Vec<ChainlistEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].chain_id, 1);
        assert_eq!(entries[1].name, "Optimism");
        assert_eq!(entries[1].rpc.len(), 2);
    }

    #[test]
    fn deserialize_ignores_extra_fields() {
        let json = r#"{"name":"Ethereum Mainnet","chainId":1,"rpc":["https://rpc.example.com"],"nativeCurrency":{"name":"Ether","symbol":"ETH","decimals":18},"explorers":[]}"#;
        let entry: ChainlistEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.name, "Ethereum Mainnet");
        assert_eq!(entry.chain_id, 1);
        assert_eq!(entry.rpc, vec!["https://rpc.example.com"]);
    }
}
