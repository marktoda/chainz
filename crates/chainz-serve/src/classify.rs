//! Parse a JSON-RPC body and decide how it may be routed.
//!
//! Only methods known to be idempotent reads may leave the active endpoint.
//! Everything unknown is treated like a send: active endpoint only.

use serde_json::Value;

/// One JSON-RPC call.
#[derive(Debug, Clone)]
pub struct Call {
    /// The method name (empty when missing).
    pub method: String,
    /// The params (`Null` when missing).
    pub params: Value,
    /// The request ID (`Null` for notifications).
    pub id: Value,
}

/// A parsed request body.
#[derive(Debug, Clone)]
pub enum Parsed {
    /// One call.
    Single(Call),
    /// A batch of calls.
    Batch(Vec<Call>),
}

impl Parsed {
    /// Every call in the body.
    pub fn calls(&self) -> &[Call] {
        match self {
            Parsed::Single(call) => std::slice::from_ref(call),
            Parsed::Batch(calls) => calls,
        }
    }

    /// The ID to answer a gateway-made error with.
    pub fn reply_id(&self) -> Value {
        match self {
            Parsed::Single(call) => call.id.clone(),
            Parsed::Batch(_) => Value::Null,
        }
    }
}

/// Why a body was rejected before routing: `(JSON-RPC code, message)`.
pub type ParseError = (i64, &'static str);

/// Parse a request body.
pub fn parse(body: &[u8]) -> Result<Parsed, ParseError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| (-32700, "parse error"))?;
    match value {
        Value::Object(_) => Ok(Parsed::Single(call(value)?)),
        Value::Array(items) if items.is_empty() => Err((-32600, "empty batch")),
        Value::Array(items) => Ok(Parsed::Batch(
            items.into_iter().map(call).collect::<Result<_, _>>()?,
        )),
        _ => Err((-32600, "invalid request")),
    }
}

fn call(value: Value) -> Result<Call, ParseError> {
    let Value::Object(mut obj) = value else {
        return Err((-32600, "invalid request"));
    };
    let Some(Value::String(method)) = obj.remove("method") else {
        return Err((-32600, "invalid request: missing method"));
    };
    Ok(Call {
        method,
        params: obj.remove("params").unwrap_or(Value::Null),
        id: obj.remove("id").unwrap_or(Value::Null),
    })
}

/// How a method may be routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An idempotent read: may fail over.
    Read,
    /// A send, or anything not known to be a read: active endpoint only.
    ActiveOnly,
    /// Creates a filter on the active endpoint; the filter ID becomes sticky.
    FilterCreate,
    /// Uses a filter: goes to the endpoint that created it.
    FilterUse,
}

/// Classify one method.
pub fn kind(method: &str) -> Kind {
    match method {
        "eth_newFilter" | "eth_newBlockFilter" | "eth_newPendingTransactionFilter" => {
            Kind::FilterCreate
        }
        "eth_getFilterChanges" | "eth_getFilterLogs" | "eth_uninstallFilter" => Kind::FilterUse,
        "eth_call"
        | "eth_callMany"
        | "eth_simulateV1"
        | "eth_estimateGas"
        | "eth_createAccessList"
        | "eth_blockNumber"
        | "eth_chainId"
        | "eth_gasPrice"
        | "eth_maxPriorityFeePerGas"
        | "eth_feeHistory"
        | "eth_blobBaseFee"
        | "eth_syncing"
        | "eth_protocolVersion" => Kind::Read,
        m if m.starts_with("eth_get") => Kind::Read,
        m if m.starts_with("net_") || m.starts_with("web3_") => Kind::Read,
        m if m.starts_with("debug_trace") || m.starts_with("debug_get") => Kind::Read,
        m if m.starts_with("trace_") => Kind::Read,
        _ => Kind::ActiveOnly,
    }
}

/// Capabilities a request needs from its endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Needs {
    /// `debug_*`.
    pub debug: bool,
    /// `trace_*`.
    pub trace: bool,
    /// A state read at a block older than [`ARCHIVE_DEPTH`].
    pub archive: bool,
}

impl Needs {
    /// Whether anything is needed.
    pub fn any(&self) -> bool {
        self.debug || self.trace || self.archive
    }

    /// Both sets of needs.
    pub fn union(self, other: Needs) -> Needs {
        Needs {
            debug: self.debug || other.debug,
            trace: self.trace || other.trace,
            archive: self.archive || other.archive,
        }
    }
}

/// Non-archive nodes keep state for about this many recent blocks.
pub const ARCHIVE_DEPTH: u64 = 128;

/// What one call needs. `head` is the chain's head high-water mark.
pub fn needs(call: &Call, head: Option<u64>) -> Needs {
    Needs {
        debug: call.method.starts_with("debug_"),
        trace: call.method.starts_with("trace_"),
        archive: match (state_block(call), head) {
            (Some(block), Some(head)) => block.saturating_add(ARCHIVE_DEPTH) < head,
            _ => false,
        },
    }
}

/// The block number a state read is pinned to, when it names one.
fn state_block(call: &Call) -> Option<u64> {
    let index = match call.method.as_str() {
        "eth_call"
        | "eth_estimateGas"
        | "eth_createAccessList"
        | "eth_getBalance"
        | "eth_getCode"
        | "eth_getTransactionCount" => 1,
        "eth_getStorageAt" | "eth_getProof" => 2,
        _ => return None,
    };
    let param = call.params.get(index)?;
    let text = match param {
        Value::String(s) => s.as_str(),
        Value::Object(obj) => obj.get("blockNumber")?.as_str()?,
        _ => return None,
    };
    if text == "earliest" {
        return Some(0);
    }
    u64::from_str_radix(text.strip_prefix("0x")?, 16).ok()
}

/// The filter ID a [`Kind::FilterUse`] call refers to, normalised.
pub fn filter_id(call: &Call) -> Option<String> {
    Some(call.params.get(0)?.as_str()?.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn single(v: Value) -> Call {
        match parse(v.to_string().as_bytes()).unwrap() {
            Parsed::Single(c) => c,
            Parsed::Batch(_) => panic!("batch"),
        }
    }

    #[test]
    fn parses_single_and_batch_and_rejects_garbage() {
        let c = single(json!({"jsonrpc":"2.0","id":7,"method":"eth_chainId","params":[]}));
        assert_eq!((c.method.as_str(), &c.id), ("eth_chainId", &json!(7)));
        let batch = parse(br#"[{"id":1,"method":"a"},{"id":2,"method":"b"}]"#).unwrap();
        assert_eq!(batch.calls().len(), 2);
        assert_eq!(batch.reply_id(), Value::Null);
        assert_eq!(parse(b"nope").unwrap_err().0, -32700);
        assert_eq!(parse(b"[]").unwrap_err().0, -32600);
        assert_eq!(parse(b"3").unwrap_err().0, -32600);
        assert_eq!(parse(br#"{"id":1}"#).unwrap_err().0, -32600);
        assert_eq!(parse(br#"[1]"#).unwrap_err().0, -32600);
    }

    #[test]
    fn only_known_reads_may_fail_over() {
        for m in [
            "eth_call",
            "eth_getLogs",
            "eth_getBalance",
            "eth_blockNumber",
            "eth_getTransactionReceipt",
            "net_version",
            "debug_traceTransaction",
            "trace_block",
        ] {
            assert_eq!(kind(m), Kind::Read, "{m}");
        }
        for m in [
            "eth_sendRawTransaction",
            "eth_sendTransaction",
            "eth_sendRawTransactionSync",
            "eth_sendBundle",
            "eth_sign",
            "eth_subscribe",
            "personal_sign",
            "anvil_setBalance",
            "evm_mine",
            "debug_setHead",
            "made_up",
            "",
        ] {
            assert_eq!(kind(m), Kind::ActiveOnly, "{m}");
        }
        assert_eq!(kind("eth_newFilter"), Kind::FilterCreate);
        assert_eq!(kind("eth_newBlockFilter"), Kind::FilterCreate);
        // `eth_get*` must not swallow the filter reads.
        assert_eq!(kind("eth_getFilterChanges"), Kind::FilterUse);
        assert_eq!(kind("eth_getFilterLogs"), Kind::FilterUse);
        assert_eq!(kind("eth_uninstallFilter"), Kind::FilterUse);
    }

    #[test]
    fn capability_needs() {
        let head = Some(1_000);
        let call_at = |block: Value| {
            single(json!({"id":1,"method":"eth_call","params":[{"to":"0x00"}, block]}))
        };
        assert!(!needs(&call_at(json!("latest")), head).any());
        assert!(!needs(&call_at(json!("0x3e0")), head).any(), "992: recent");
        assert!(needs(&call_at(json!("0x64")), head).archive);
        assert!(needs(&call_at(json!("earliest")), head).archive);
        assert!(needs(&call_at(json!({"blockNumber":"0x1"})), head).archive);
        assert!(
            !needs(&call_at(json!("0x64")), None).any(),
            "unknown head: no claim"
        );
        let storage =
            single(json!({"id":1,"method":"eth_getStorageAt","params":["0x00","0x0","0x1"]}));
        assert!(needs(&storage, head).archive);
        let logs = single(json!({"id":1,"method":"eth_getLogs","params":[{"fromBlock":"0x1"}]}));
        assert!(!needs(&logs, head).any());
        let debug = single(json!({"id":1,"method":"debug_traceTransaction","params":["0x"]}));
        assert_eq!(
            needs(&debug, head),
            Needs {
                debug: true,
                ..Needs::default()
            }
        );
        assert!(
            needs(
                &single(json!({"id":1,"method":"trace_block","params":[]})),
                head
            )
            .trace
        );
    }

    #[test]
    fn filter_ids_are_normalised() {
        let c = single(json!({"id":1,"method":"eth_getFilterChanges","params":["0xAB"]}));
        assert_eq!(filter_id(&c).as_deref(), Some("0xab"));
        let c = single(json!({"id":1,"method":"eth_getFilterChanges","params":[]}));
        assert_eq!(filter_id(&c), None);
    }
}
