//! Minimal hex and JSON-RPC quantity helpers.

use crate::error::RpcError;

/// Encode bytes as a `0x`-prefixed lowercase hex string.
pub(crate) fn encode(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(2 + data.len() * 2);
    s.push_str("0x");
    for b in data {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0xf) as usize] as char);
    }
    s
}

/// Decode a `0x`-prefixed (or bare) hex string into bytes.
pub(crate) fn decode(s: &str) -> Result<Vec<u8>, RpcError> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if !s.len().is_multiple_of(2) {
        return Err(RpcError::InvalidResponse("odd-length hex data".into()));
    }
    let nibble = |c: u8| -> Result<u8, RpcError> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(RpcError::InvalidResponse("invalid hex data".into())),
        }
    };
    s.as_bytes()
        .chunks(2)
        .map(|p| Ok((nibble(p[0])? << 4) | nibble(p[1])?))
        .collect()
}

/// Parse a JSON-RPC quantity (`0x`-prefixed hex, no leading-zero rule enforced).
pub(crate) fn parse_quantity(s: &str) -> Result<u128, RpcError> {
    let digits = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .ok_or_else(|| RpcError::InvalidResponse("quantity is not 0x-prefixed".into()))?;
    if digits.is_empty() {
        return Err(RpcError::InvalidResponse("empty quantity".into()));
    }
    u128::from_str_radix(digits, 16)
        .map_err(|_| RpcError::InvalidResponse("invalid quantity".into()))
}

/// Parse a JSON-RPC quantity that must fit in a `u64`.
pub(crate) fn parse_u64(s: &str) -> Result<u64, RpcError> {
    u64::try_from(parse_quantity(s)?)
        .map_err(|_| RpcError::InvalidResponse("quantity out of range".into()))
}

/// Parse a JSON value holding a quantity string into a `u64`.
pub(crate) fn value_u64(v: &serde_json::Value) -> Result<u64, RpcError> {
    match v.as_str() {
        Some(s) => parse_u64(s),
        None => Err(RpcError::InvalidResponse("expected a hex quantity".into())),
    }
}

/// Format a `u64` as a JSON-RPC quantity.
pub(crate) fn quantity(n: u64) -> String {
    format!("0x{n:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        assert_eq!(encode(&[0xde, 0xad, 0x01]), "0xdead01");
        assert_eq!(decode("0xDEad01").unwrap(), vec![0xde, 0xad, 0x01]);
        assert_eq!(decode("0x").unwrap(), Vec::<u8>::new());
        assert!(decode("0x1").is_err());
        assert!(decode("0xzz").is_err());
        assert_eq!(parse_quantity("0x10").unwrap(), 16);
        assert!(parse_quantity("10").is_err());
        assert!(parse_quantity("0x").is_err());
        assert!(parse_u64("0x1ffffffffffffffff").is_err());
        assert_eq!(quantity(255), "0xff");
    }
}
