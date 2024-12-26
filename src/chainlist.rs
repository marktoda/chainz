use anyhow::{anyhow, Result};
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct ChainlistEntry {
    pub name: String,
    #[serde(rename = "chainId")]
    pub chain_id: u64,
    pub rpc: Vec<String>,
}

pub async fn fetch_all_chains() -> Result<Vec<ChainlistEntry>> {
    let url = "https://chainid.network/chains.json";
    Ok(reqwest::get(url).await?.json().await?)
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
