use super::Rpc;
use crate::variables::GlobalVariables;
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use anyhow::Result;

pub async fn test_rpc(rpc: &Rpc, expected_chain_id: u64) -> Result<()> {
    let chain_id = rpc
        .provider
        .get_chain_id()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {}: {}", rpc.rpc_url, e))?;
    if chain_id != expected_chain_id {
        anyhow::bail!(
            "Chain ID mismatch on {}: expected {}, got {}",
            rpc.rpc_url,
            expected_chain_id,
            chain_id
        );
    }
    Ok(())
}

pub async fn resolve_rpcs(rpc_urls: Vec<String>, globals: &GlobalVariables) -> Result<Vec<Rpc>> {
    let mut result = Vec::new();
    for rpc in rpc_urls {
        if let Ok(r) = resolve_rpc(&rpc, globals).await {
            result.push(r);
        }
    }
    Ok(result)
}

pub async fn resolve_rpc(rpc_url: &str, globals: &GlobalVariables) -> Result<Rpc> {
    let rpc_url = globals.expand_rpc_url(rpc_url);
    Ok(Rpc {
        rpc_url: rpc_url.clone(),
        provider: create_provider(&rpc_url).await?,
    })
}

pub async fn create_provider(rpc_url: &str) -> Result<DynProvider> {
    let provider = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        ProviderBuilder::new().connect(rpc_url),
    )
    .await
    .map_err(|_| anyhow::anyhow!("RPC connection timed out: {}", rpc_url))??;
    Ok(provider.erased())
}
