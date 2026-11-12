//! Short, secret-free parameter summaries for the inspector.
//!
//! A summary never contains raw params: addresses become labels or a
//! shortened form, calldata becomes a function name or a 4-byte selector,
//! and `eth_sendRawTransaction` becomes the transaction hash.

use chainz_core::Address;
use chainz_core::fmt::short_address;
use chainz_core::labels::LabelIndex;
use chainz_rpc::{abi, hex};
use serde_json::Value;
use sha3::{Digest, Keccak256};

use crate::classify::{self, Call, Params, Parsed};

/// Longest summary kept.
const MAX_LEN: usize = 96;
/// Longest method name kept in records and metrics.
pub const MAX_METHOD_LEN: usize = 48;

/// The method shown for a request: the method itself, or `batch(n)`.
pub fn method_label(parsed: &Parsed) -> String {
    match parsed {
        Parsed::Single(call) => clip_method(&call.method),
        Parsed::Batch(calls) => format!("batch({})", calls.len()),
    }
}

/// A method name safe to store: bounded, printable ASCII only.
pub fn clip_method(method: &str) -> String {
    let clean: String = method
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(MAX_METHOD_LEN)
        .collect();
    if clean.is_empty() {
        "(none)".to_string()
    } else {
        clean
    }
}

/// The params summary for a whole request.
pub fn summarize(parsed: &Parsed, chain_id: u64, labels: &LabelIndex) -> String {
    let text = match parsed {
        Parsed::Single(call) => summarize_call(call, chain_id, labels),
        Parsed::Batch(calls) => {
            // `eth_call×3, eth_blockNumber`, in first-seen order.
            let mut counts: Vec<(String, usize)> = Vec::new();
            for call in calls {
                let method = clip_method(&call.method);
                match counts.iter_mut().find(|(m, _)| *m == method) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((method, 1)),
                }
            }
            counts
                .iter()
                .map(|(m, n)| {
                    if *n == 1 {
                        m.clone()
                    } else {
                        format!("{m}×{n}")
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    clip(text)
}

fn clip(text: String) -> String {
    if text.chars().count() <= MAX_LEN {
        return text;
    }
    let mut out: String = text.chars().take(MAX_LEN - 1).collect();
    out.push('…');
    out
}

fn summarize_call(call: &Call, chain_id: u64, labels: &LabelIndex) -> String {
    let p = &call.params;
    let addr = |v: Option<&Value>| address(v, chain_id, labels);
    let block = classify::state_block_param(call);
    match classify::params(&call.method) {
        Params::RawTx => match p.get(0).and_then(Value::as_str).and_then(raw_tx_hash) {
            Some(hash) => format!("tx {hash}"),
            None => "tx (undecodable)".to_string(),
        },
        Params::Tx => {
            let tx = p.get(0);
            let to = tx.and_then(|t| t.get("to"));
            let data = tx
                .and_then(|t| t.get("data").or_else(|| t.get("input")))
                .and_then(Value::as_str);
            let target = if to.is_none_or(Value::is_null) {
                "create".to_string()
            } else {
                addr(to)
            };
            let out = match data.and_then(selector) {
                Some(function) => format!("{target}.{function}"),
                None => target,
            };
            with_block(out, block)
        }
        Params::Account => with_block(addr(p.get(0)), block),
        Params::Storage => with_block(
            format!("{} slot {}", addr(p.get(0)), short(p.get(1))),
            block,
        ),
        Params::LogFilter => {
            let filter = p.get(0);
            let mut parts = Vec::new();
            match filter.and_then(|f| f.get("address")) {
                Some(Value::Array(list)) if list.len() == 1 => parts.push(addr(list.first())),
                Some(Value::Array(list)) => parts.push(format!("{} addresses", list.len())),
                Some(one @ Value::String(_)) => parts.push(addr(Some(one))),
                _ => {}
            }
            if let Some(hash) = filter.and_then(|f| f.get("blockHash")) {
                parts.push(format!("block {}", short(Some(hash))));
            } else {
                let from = filter.and_then(|f| f.get("fromBlock")).and_then(block_tag);
                let to = filter.and_then(|f| f.get("toBlock")).and_then(block_tag);
                if from.is_some() || to.is_some() {
                    parts.push(format!(
                        "{}..{}",
                        from.unwrap_or_else(|| "latest".into()),
                        to.unwrap_or_else(|| "latest".into())
                    ));
                }
            }
            let topics = filter
                .and_then(|f| f.get("topics"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if topics > 0 {
                parts.push(format!("{topics} topics"));
            }
            parts.join(" ")
        }
        Params::Block => p.get(0).and_then(block_tag).unwrap_or_default(),
        Params::Hash => short(p.get(0)),
        Params::FeeHistory => format!(
            "{} blocks to {}",
            p.get(0)
                .and_then(block_tag)
                .unwrap_or_else(|| "?".to_string()),
            p.get(1)
                .and_then(block_tag)
                .unwrap_or_else(|| "latest".to_string())
        ),
        Params::FilterId | Params::Uninstall => format!("filter {}", short(p.get(0))),
        Params::Other => String::new(),
    }
}

fn with_block(mut head: String, block: Option<&Value>) -> String {
    if let Some(block) = block.and_then(block_tag) {
        head.push(' ');
        head.push_str(&block);
    }
    head
}

/// `@label`, or a shortened address; `?` when it is not an address.
fn address(value: Option<&Value>, chain_id: u64, labels: &LabelIndex) -> String {
    let Some(text) = value.and_then(Value::as_str) else {
        return "?".to_string();
    };
    match text.parse::<Address>() {
        Ok(address) => match labels.reverse(chain_id, &address) {
            Some(label) => format!("@{}", label.name),
            None => short_address(&address),
        },
        Err(_) => "?".to_string(),
    }
}

/// The function name for calldata, or its `0x` selector.
fn selector(data: &str) -> Option<String> {
    let digits = data.strip_prefix("0x").unwrap_or(data);
    let selector: [u8; 4] = hex::decode(digits.get(..8)?).ok()?.try_into().ok()?;
    Some(match abi::function_name(selector) {
        Some(name) => name.to_string(),
        None => hex::encode(&selector),
    })
}

/// A block tag or number, bounded and cleaned.
fn block_tag(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(s) => s.as_str(),
        Value::Number(n) => return Some(n.to_string()),
        Value::Object(obj) => {
            return obj
                .get("blockNumber")
                .and_then(block_tag)
                .or_else(|| obj.get("blockHash").map(|h| short(Some(h))));
        }
        _ => return None,
    };
    if let Some(n) = hex::parse_quantity(text)
        .ok()
        .and_then(|n| u64::try_from(n).ok())
    {
        return Some(n.to_string());
    }
    Some(clean(text, 12))
}

/// A shortened hex string such as a hash: `0x1234…cdef`.
fn short(value: Option<&Value>) -> String {
    let Some(text) = value.and_then(Value::as_str) else {
        return "?".to_string();
    };
    let text = clean(text, 80);
    if text.len() > 14 {
        format!("{}…{}", &text[..6], &text[text.len() - 4..])
    } else {
        text
    }
}

fn clean(text: &str, max: usize) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .take(max)
        .collect()
}

/// keccak256 of the raw transaction bytes: the transaction hash.
pub fn raw_tx_hash(raw: &str) -> Option<String> {
    let bytes = hex::decode(raw).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(hex::encode(&Keccak256::digest(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::parse;
    use chainz_core::model::Config;
    use serde_json::json;

    const USDC_BASE: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";

    fn labels() -> LabelIndex {
        LabelIndex::new(&Config::default())
    }

    fn sum(v: Value) -> String {
        summarize(&parse(v.to_string().as_bytes()).unwrap(), 8453, &labels())
    }

    #[test]
    fn eth_call_shows_label_and_function() {
        let s = sum(json!({"id":1,"method":"eth_call","params":[
            {"to": USDC_BASE, "data": "0x70a08231000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"},
            "latest"]}));
        assert_eq!(s, "@usdc.balanceOf latest");
        let s = sum(json!({"id":1,"method":"eth_call","params":[
            {"to": "0x1111111111111111111111111111111111111111", "input": "0xdeadbeef00"}, "0x10"]}));
        assert_eq!(s, "0x1111…1111.0xdeadbeef 16");
    }

    #[test]
    fn raw_transactions_are_recorded_as_their_hash_only() {
        let raw = "0x02f86c0180843b9aca00847735940082520894f39fd6e51aad88f6f4ce6ab8827279cfffb92266865af3107a400080c001a0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let s = sum(json!({"id":1,"method":"eth_sendRawTransaction","params":[raw]}));
        let hash = raw_tx_hash(raw).unwrap();
        assert_eq!(hash.len(), 66);
        assert_eq!(s, format!("tx {hash}"));
        assert!(!s.contains("aaaaaaaa"), "{s}");
        assert_eq!(
            sum(json!({"id":1,"method":"eth_sendRawTransaction","params":["zz"]})),
            "tx (undecodable)"
        );
        // keccak256("") sanity check for the hash function itself.
        assert_eq!(
            raw_tx_hash("0x00").unwrap(),
            "0xbc36789e7a1e281436464229828f817d6612f7b477d66591ff96a9e064bcc98a"
        );
    }

    #[test]
    fn other_methods() {
        assert_eq!(
            sum(json!({"id":1,"method":"eth_getBalance","params":[USDC_BASE,"latest"]})),
            "@usdc latest"
        );
        assert_eq!(
            sum(json!({"id":1,"method":"eth_getLogs","params":[
                {"address": USDC_BASE, "fromBlock":"0x1","toBlock":"0xa","topics":["0x00",null]}]})),
            "@usdc 1..10 2 topics"
        );
        assert_eq!(
            sum(json!({"id":1,"method":"eth_getBlockByNumber","params":["latest",false]})),
            "latest"
        );
        assert_eq!(
            sum(
                json!({"id":1,"method":"eth_getTransactionReceipt","params":[
                "0xbc36789e7a1e281436464229828f817d6612f7b477d66591ff96a9e064bcc98a"]})
            ),
            "0xbc36…c98a"
        );
        assert_eq!(sum(json!({"id":1,"method":"eth_chainId","params":[]})), "");
        assert_eq!(
            sum(
                json!([{"id":1,"method":"eth_call"},{"id":2,"method":"eth_call"},{"id":3,"method":"eth_blockNumber"}])
            ),
            "eth_call×2, eth_blockNumber"
        );
    }

    #[test]
    fn hostile_input_is_bounded_and_cleaned() {
        let long = "x".repeat(10_000);
        let s = sum(json!({"id":1,"method":"eth_getBlockByNumber","params":[long]}));
        assert!(s.len() <= 12, "{s}");
        let s = sum(json!({"id":1,"method":"eth_getBalance","params":["secret-not-an-address"]}));
        assert_eq!(s, "?");
        assert_eq!(clip_method("eth_call\n\u{1b}[31m"), "eth_call31m");
        assert_eq!(clip_method(&"m".repeat(500)).len(), MAX_METHOD_LEN);
        let many: Vec<Value> = (0..200)
            .map(|i| json!({"id":i,"method":format!("method_{i}")}))
            .collect();
        assert!(sum(Value::Array(many)).chars().count() <= MAX_LEN);
    }
}
