//! Fork funding: the signer's native balance (`anvil_setBalance`) and
//! ERC-20 balances (`anvil_dealERC20`). Works from the signer's cached
//! address; no key is ever unlocked.

use anyhow::{Result, anyhow, bail};
use chainz_core::Address;
use chainz_core::labels::{self, LabelIndex, NATIVE_DECIMALS};
use chainz_core::model::Config;
use chainz_rpc::abi::{self, units};
use chainz_rpc::{BlockId, RpcClient, Target};
use serde_json::{Value, json};

/// Native units given to the signer.
const NATIVE_AMOUNT: &str = "1000";

/// One `name=amount` pair, resolved before anvil starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenGrant {
    /// The label as typed (lowercased).
    pub label: String,
    /// Token contract on the parent chain.
    pub address: Address,
    /// Registry decimals; `None` asks the contract on the fork.
    pub decimals: Option<u8>,
    /// Display symbol: the registry's, else the label uppercased.
    pub symbol: String,
    /// The amount as typed, in whole tokens.
    pub amount: String,
}

/// Who and what to fund.
#[derive(Debug, Clone)]
pub struct FundPlan {
    /// Signer name.
    pub signer: String,
    /// Its cached address.
    pub address: Address,
    /// Native token symbol, for display.
    pub native_symbol: &'static str,
    /// Token grants, in the order given.
    pub tokens: Vec<TokenGrant>,
}

/// What happened.
#[derive(Debug, Default)]
pub struct FundReport {
    /// `"1000 ETH"`, `"10000 USDC"`, …
    pub funded: Vec<String>,
    /// `(what, why)` for each grant that failed.
    pub failed: Vec<(String, String)>,
}

/// Split `usdc=10000,weth=5` into `(name, amount)` pairs.
fn parse_spec(spec: &str) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (name, amount) = part.split_once('=').ok_or_else(|| {
            anyhow!("--fund: '{part}' is not NAME=AMOUNT (e.g. usdc=10000,weth=5)")
        })?;
        let name = name.trim().trim_start_matches('@').to_ascii_lowercase();
        let amount = amount.trim();
        if name.is_empty() || amount.is_empty() {
            bail!("--fund: '{part}' is not NAME=AMOUNT (e.g. usdc=10000,weth=5)");
        }
        // Validates the shape; decimals are applied later.
        units::check(amount)?;
        pairs.push((name, amount.to_string()));
    }
    Ok(pairs)
}

/// Resolve each pair through the address book on the parent chain.
pub fn plan_tokens(
    config: &Config,
    chain_name: &str,
    chain_id: u64,
    spec: &str,
) -> Result<Vec<TokenGrant>> {
    let pairs = parse_spec(spec)?;
    if pairs.is_empty() {
        return Ok(Vec::new());
    }
    let index = LabelIndex::new(config);
    let mut grants = Vec::with_capacity(pairs.len());
    for (name, amount) in pairs {
        let label = index
            .resolve(&name, chain_id)
            .map_err(|e| anyhow!("--fund: {e} ('{chain_name}')"))?;
        let registry = labels::builtin_token_at(chain_id, &label.address);
        let grant = TokenGrant {
            label: name.clone(),
            address: label.address,
            decimals: registry.map(|t| t.decimals),
            symbol: registry
                .map(|t| t.symbol.clone())
                .unwrap_or_else(|| name.to_ascii_uppercase()),
            amount,
        };
        if let Some(decimals) = grant.decimals {
            units::scale(&grant.amount, decimals)
                .map_err(|e| anyhow!("--fund {}: {e}", grant.label))?;
        }
        grants.push(grant);
    }
    Ok(grants)
}

/// Fund `plan.address` on the fork at `anvil`. A grant that fails is
/// reported and the rest continue.
pub async fn fund(client: &RpcClient, anvil: &Target, plan: &FundPlan) -> FundReport {
    let mut report = FundReport::default();
    let owner = plan.address.to_checksum();
    let native = units::scale(NATIVE_AMOUNT, NATIVE_DECIMALS).expect("constant fits");
    let what = format!("{NATIVE_AMOUNT} {}", plan.native_symbol);
    match client
        .request::<Value>(anvil, "anvil_setBalance", json!([owner, native]))
        .await
    {
        Ok(_) => report.funded.push(what),
        Err(e) => report.failed.push((what, e.to_string())),
    }
    for token in &plan.tokens {
        let what = format!("{} {}", token.amount, token.symbol);
        match deal(client, anvil, &owner, token).await {
            Ok(()) => report.funded.push(what),
            Err(e) => report.failed.push((what, format!("{e:#}"))),
        }
    }
    report
}

async fn deal(client: &RpcClient, anvil: &Target, owner: &str, token: &TokenGrant) -> Result<()> {
    let address = token.address.to_checksum();
    let decimals = match token.decimals {
        Some(decimals) => decimals,
        None => {
            let data = client
                .call(anvil, &address, &abi::DECIMALS, BlockId::Latest)
                .await
                .map_err(|e| anyhow!("could not read decimals(): {e}"))?;
            abi::decode_u8(&data)
                .ok_or_else(|| anyhow!("{address} does not answer decimals(): not an ERC-20?"))?
        }
    };
    let amount = units::scale(&token.amount, decimals)?;
    client
        .request::<Value>(anvil, "anvil_dealERC20", json!([owner, address, amount]))
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fund_specs() {
        assert_eq!(parse_spec("").unwrap(), vec![]);
        assert_eq!(
            parse_spec("usdc=10000, @WETH=0.5,").unwrap(),
            vec![
                ("usdc".to_string(), "10000".to_string()),
                ("weth".to_string(), "0.5".to_string())
            ]
        );
        for bad in ["usdc", "usdc=", "=5", "usdc=abc", "usdc=1e6"] {
            assert!(parse_spec(bad).is_err(), "{bad:?}");
        }
    }

    fn config() -> Config {
        Config::parse(
            r#"
version = 2
[chains.base]
id = 8453
rpc = "a"
rpcs.a = "https://a"
[addresses.mytoken]
base = "0x00000000000000000000000000000000000c0de1"
"#,
        )
        .unwrap()
    }

    #[test]
    fn plans_use_registry_decimals_and_symbols() {
        let grants = plan_tokens(&config(), "base", 8453, "usdc=1000.5,mytoken=2").unwrap();
        assert_eq!(grants[0].symbol, "USDC");
        assert_eq!(grants[0].decimals, Some(6));
        assert_eq!(
            grants[0].address.to_lower_hex(),
            "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
        );
        // A user label is not in the registry: decimals come from the fork.
        assert_eq!(grants[1].symbol, "MYTOKEN");
        assert_eq!(grants[1].decimals, None);

        let err = |spec| {
            plan_tokens(&config(), "base", 8453, spec)
                .unwrap_err()
                .to_string()
        };
        assert!(err("udsc=1").contains("did you mean @usdc?"));
        assert!(err("usdc=0.0000001").contains("more than 6 decimal places"));
        assert!(plan_tokens(&config(), "base", 8453, "").unwrap().is_empty());
    }
}
