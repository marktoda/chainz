//! Fork funding: the signer's native balance (`anvil_setBalance`) and
//! ERC-20 balances (`anvil_dealERC20`). Works from the signer's cached
//! address; no key is ever unlocked.

use anyhow::{Result, anyhow, bail};
use chainz_core::labels::{LabelIndex, builtin_tokens};
use chainz_core::{Address, Config};
use chainz_rpc::erc20;
use chainz_rpc::{BlockId, RpcClient, Target};
use serde_json::{Value, json};

/// Native units given to the signer.
pub const NATIVE_AMOUNT: &str = "1000";
const NATIVE_DECIMALS: u8 = 18;

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
pub fn parse_spec(spec: &str) -> Result<Vec<(String, String)>> {
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
        split_amount(amount)?;
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
        let registry = builtin_tokens()
            .iter()
            .find(|t| t.chain_id == chain_id && t.address == label.address);
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
            scale_amount(&grant.amount, decimals)
                .map_err(|e| anyhow!("--fund {}: {e}", grant.label))?;
        }
        grants.push(grant);
    }
    Ok(grants)
}

/// Integer and fraction digits of a plain decimal amount (`10000`, `0.5`,
/// `.5`, `5.`). No sign, exponent or separators.
fn split_amount(amount: &str) -> Result<(&str, &str)> {
    let (int, frac) = amount.split_once('.').unwrap_or((amount, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if (int.is_empty() && frac.is_empty()) || !digits(int) || !digits(frac) {
        bail!("'{amount}' is not a decimal amount (e.g. 10000 or 0.5)");
    }
    Ok((int, frac))
}

/// `amount` whole tokens as base units, a `0x` quantity. Exact: decimal
/// string arithmetic, never floats. Fails on more fraction digits than
/// `decimals` (unless they are zeros) and on overflow of 256 bits.
pub fn scale_amount(amount: &str, decimals: u8) -> Result<String> {
    let (int, frac) = split_amount(amount)?;
    let decimals = usize::from(decimals);
    let (kept, excess) = frac.split_at(frac.len().min(decimals));
    if excess.bytes().any(|b| b != b'0') {
        bail!("'{amount}' has more than {decimals} decimal places");
    }
    let mut digits = String::with_capacity(int.len() + decimals);
    digits.push_str(int);
    digits.push_str(kept);
    digits.extend(std::iter::repeat_n('0', decimals - kept.len()));
    decimal_to_quantity(&digits).ok_or_else(|| anyhow!("'{amount}' does not fit in 256 bits"))
}

/// A string of decimal digits as a `0x` quantity; `None` past 256 bits.
fn decimal_to_quantity(digits: &str) -> Option<String> {
    // Little-endian base-2^32 limbs.
    let mut limbs: Vec<u64> = Vec::new();
    for digit in digits.bytes() {
        let mut carry = u64::from(digit - b'0');
        for limb in &mut limbs {
            let value = *limb * 10 + carry;
            *limb = value & 0xffff_ffff;
            carry = value >> 32;
        }
        if carry > 0 {
            limbs.push(carry);
        }
        if limbs.len() > 8 {
            return None;
        }
    }
    let mut out = String::from("0x");
    match limbs.split_last() {
        None => out.push('0'),
        Some((top, rest)) => {
            out.push_str(&format!("{top:x}"));
            for limb in rest.iter().rev() {
                out.push_str(&format!("{limb:08x}"));
            }
        }
    }
    Some(out)
}

/// The native token's symbol, for display only.
pub fn native_symbol(chain_id: u64) -> &'static str {
    match chain_id {
        56 | 97 => "BNB",
        100 => "xDAI",
        137 | 80002 => "POL",
        196 => "OKB",
        250 => "FTM",
        5000 => "MNT",
        5042 => "USDC",
        42220 => "CELO",
        43114 | 43113 => "AVAX",
        _ => "ETH",
    }
}

/// Fund `plan.address` on the fork at `anvil`. A grant that fails is
/// reported and the rest continue.
pub async fn fund(client: &RpcClient, anvil: &Target, plan: &FundPlan) -> FundReport {
    let mut report = FundReport::default();
    let owner = plan.address.to_checksum();
    let native = scale_amount(NATIVE_AMOUNT, NATIVE_DECIMALS).expect("constant fits");
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
                .call(anvil, &address, &erc20::DECIMALS, BlockId::Latest)
                .await
                .map_err(|e| anyhow!("could not read decimals(): {e}"))?;
            erc20::decode_u8(&data)
                .ok_or_else(|| anyhow!("{address} does not answer decimals(): not an ERC-20?"))?
        }
    };
    let amount = scale_amount(&token.amount, decimals)?;
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
    fn scales_whole_and_fractional_amounts_exactly() {
        assert_eq!(scale_amount("10000", 6).unwrap(), "0x2540be400");
        assert_eq!(scale_amount("5", 18).unwrap(), "0x4563918244f40000");
        assert_eq!(scale_amount("0.5", 18).unwrap(), "0x6f05b59d3b20000");
        assert_eq!(scale_amount(".5", 6).unwrap(), "0x7a120");
        assert_eq!(scale_amount("5.", 0).unwrap(), "0x5");
        assert_eq!(scale_amount("1.500000", 6).unwrap(), "0x16e360");
        assert_eq!(scale_amount("0", 18).unwrap(), "0x0");
        assert_eq!(scale_amount("000.000", 18).unwrap(), "0x0");
        assert_eq!(scale_amount("0.000001", 6).unwrap(), "0x1");
        assert_eq!(scale_amount("1000", 18).unwrap(), "0x3635c9adc5dea00000");
        // 0.1 + 0.2 territory: floats would give ...0004.
        assert_eq!(
            scale_amount("0.3", 18).unwrap(),
            format!("0x{:x}", 300_000_000_000_000_000u128)
        );
        // Beyond f64 and u128 precision.
        assert_eq!(
            scale_amount("123456789012345678901234567890.123456789012345678", 18).unwrap(),
            format!(
                "0x{}",
                big_hex("123456789012345678901234567890123456789012345678")
            )
        );
    }

    /// Independent decimal→hex (schoolbook division by 16) for cross-checks.
    fn big_hex(decimal: &str) -> String {
        let mut digits: Vec<u8> = decimal.bytes().map(|b| b - b'0').collect();
        let mut hex = Vec::new();
        while digits.iter().any(|d| *d != 0) {
            let mut rem = 0u32;
            for d in &mut digits {
                let cur = rem * 10 + u32::from(*d);
                *d = (cur / 16) as u8;
                rem = cur % 16;
            }
            hex.push(std::char::from_digit(rem, 16).unwrap());
        }
        if hex.is_empty() {
            hex.push('0');
        }
        hex.iter().rev().collect()
    }

    #[test]
    fn scaling_matches_integer_arithmetic() {
        for (amount, decimals, expected) in [
            ("1", 0u8, 1u128),
            ("1", 6, 1_000_000),
            ("4294967296", 0, 1 << 32),
            ("18446744073709551616", 0, 1 << 64),
            ("340282366920938463463.374607431768211455", 18, u128::MAX),
            ("12.345678", 8, 1_234_567_800),
        ] {
            assert_eq!(
                scale_amount(amount, decimals).unwrap(),
                format!("0x{expected:x}"),
                "{amount} @ {decimals}"
            );
        }
    }

    #[test]
    fn rejects_lossy_malformed_and_oversized_amounts() {
        let err = |a, d| scale_amount(a, d).unwrap_err().to_string();
        assert!(err("0.0000001", 6).contains("more than 6 decimal places"));
        assert!(err("1.5", 0).contains("more than 0 decimal places"));
        for bad in [
            "", ".", "1e6", "-1", "+1", "1,000", "1.2.3", "0x10", " 1", "ten",
        ] {
            assert!(scale_amount(bad, 18).is_err(), "{bad:?}");
        }
        let max = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
        assert_eq!(
            scale_amount(max, 0).unwrap(),
            format!("0x{}", "f".repeat(64))
        );
        let over = "115792089237316195423570985008687907853269984665640564039457584007913129639936";
        assert!(err(over, 0).contains("256 bits"));
        assert!(err("1", 78).contains("256 bits"));
    }

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

    #[test]
    fn native_symbols() {
        assert_eq!(native_symbol(8453), "ETH");
        assert_eq!(native_symbol(137), "POL");
    }
}
