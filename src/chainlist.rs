use anyhow::{anyhow, Result};
use serde::Deserialize;

#[derive(Deserialize, Debug, Clone)]
pub struct ChainlistEntry {
    pub name: String,
    #[serde(rename = "chainId")]
    pub chain_id: u64,
    pub rpc: Vec<String>,
}

pub async fn fetch_all_chains() -> Result<Vec<ChainlistEntry>> {
    let url = "https://chainid.network/chains.json";
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    Ok(client.get(url).send().await?.json().await?)
}

pub async fn fetch_chain_data(
    chain_id: Option<u64>,
    name: Option<String>,
) -> Result<ChainlistEntry> {
    let chains = fetch_all_chains().await?;

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
