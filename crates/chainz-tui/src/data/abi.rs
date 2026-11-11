//! The little ABI the dashboard needs: Multicall3 `aggregate3`, ERC-20
//! `balanceOf`, and hex helpers. Hand-rolled to keep alloy out of the tree.

use chainz_core::Address;

/// Multicall3, deployed at the same address on every chain that has it.
pub const MULTICALL3: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

const AGGREGATE3: [u8; 4] = [0x82, 0xad, 0x56, 0xcb];
const GET_ETH_BALANCE: [u8; 4] = [0x4d, 0x23, 0x01, 0xcc];
const BALANCE_OF: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];

/// The Multicall3 address as bytes.
pub fn multicall3() -> Address {
    MULTICALL3.parse().expect("the Multicall3 address is valid")
}

fn with_address(selector: [u8; 4], holder: &Address) -> Vec<u8> {
    let mut data = Vec::with_capacity(36);
    data.extend_from_slice(&selector);
    data.extend_from_slice(&[0; 12]);
    data.extend_from_slice(&holder.0);
    data
}

/// Calldata for ERC-20 `balanceOf(holder)`.
pub fn balance_of(holder: &Address) -> Vec<u8> {
    with_address(BALANCE_OF, holder)
}

/// Calldata for Multicall3 `getEthBalance(holder)`.
pub fn get_eth_balance(holder: &Address) -> Vec<u8> {
    with_address(GET_ETH_BALANCE, holder)
}

fn word(n: usize) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&(n as u64).to_be_bytes());
    w
}

/// Calldata for `aggregate3((address target, bool allowFailure, bytes
/// callData)[])`, every call allowed to fail.
pub fn encode_aggregate3(calls: &[(Address, Vec<u8>)]) -> Vec<u8> {
    let mut out = AGGREGATE3.to_vec();
    out.extend_from_slice(&word(32));
    out.extend_from_slice(&word(calls.len()));
    let mut offset = calls.len() * 32;
    for (_, data) in calls {
        out.extend_from_slice(&word(offset));
        offset += 4 * 32 + data.len().div_ceil(32) * 32;
    }
    for (target, data) in calls {
        out.extend_from_slice(&[0; 12]);
        out.extend_from_slice(&target.0);
        out.extend_from_slice(&word(1));
        out.extend_from_slice(&word(96));
        out.extend_from_slice(&word(data.len()));
        out.extend_from_slice(data);
        out.resize(out.len() + (data.len().div_ceil(32) * 32 - data.len()), 0);
    }
    out
}

fn read_usize(data: &[u8], at: usize) -> Option<usize> {
    let w = data.get(at..at.checked_add(32)?)?;
    if w[..24].iter().any(|b| *b != 0) {
        return None;
    }
    usize::try_from(u64::from_be_bytes(w[24..].try_into().ok()?)).ok()
}

/// Decode the `(bool success, bytes returnData)[]` returned by
/// `aggregate3` into one `uint256` per call: `None` where the call failed
/// or returned fewer than 32 bytes. The outer `None` means the data is not
/// a well-formed result array of `expected` entries.
pub fn decode_aggregate3(data: &[u8], expected: usize) -> Option<Vec<Option<u128>>> {
    let array = read_usize(data, 0)?;
    if read_usize(data, array)? != expected {
        return None;
    }
    let heads = array.checked_add(32)?;
    (0..expected)
        .map(|i| {
            let item = heads.checked_add(read_usize(data, heads + i * 32)?)?;
            let success = read_usize(data, item)? == 1;
            let bytes = item.checked_add(read_usize(data, item + 32)?)?;
            let len = read_usize(data, bytes)?;
            let value = data.get(bytes + 32..(bytes + 32).checked_add(len)?)?;
            Some(if success && len >= 32 {
                Some(uint(&value[..32]))
            } else {
                None
            })
        })
        .collect()
}

/// A big-endian `uint256` as `u128`, saturating.
pub fn uint(word: &[u8]) -> u128 {
    let (high, low) = word.split_at(word.len().saturating_sub(16));
    if high.iter().any(|b| *b != 0) {
        return u128::MAX;
    }
    low.iter().fold(0u128, |n, b| n << 8 | u128::from(*b))
}

/// `0x`-prefixed lowercase hex.
pub fn hex(data: &[u8]) -> String {
    let mut s = String::with_capacity(2 + data.len() * 2);
    s.push_str("0x");
    for b in data {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Decode `0x`-prefixed (or bare) hex.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    // `from_str_radix` takes a sign: "+f" would decode as 0x0f.
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Parse a JSON-RPC quantity (`0x…`), saturating at `u128::MAX`.
pub fn quantity(s: &str) -> Option<u128> {
    let digits = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    let digits = digits.trim_start_matches('0');
    if digits.len() > 32 {
        return digits
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
            .then_some(u128::MAX);
    }
    if digits.is_empty() {
        return s.len().gt(&2).then_some(0);
    }
    u128::from_str_radix(digits, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOLDER: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";
    const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";

    // `cast calldata 'aggregate3((address,bool,bytes)[])' '[(mc3,true,getEthBalance(holder)),(usdc,true,balanceOf(holder))]'`
    const CAST_CALLDATA: &str = "0x82ad56cb0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000100000000000000000000000000ca11bde05977b3631167028862be2a173976ca110000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000244d2301cc0000000000000000000000007e5f4552091a69125d5dfcb7b8c2659029395bdf00000000000000000000000000000000000000000000000000000000000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000002470a082310000000000000000000000007e5f4552091a69125d5dfcb7b8c2659029395bdf00000000000000000000000000000000000000000000000000000000";

    // `cast abi-encode 'f((bool,bytes)[])' '[(true,1234),(false,0x),(true,42)]'`
    const CAST_RESULT: &str = "0x00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000e0000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000004d2000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000002a";

    #[test]
    fn aggregate3_calldata_matches_cast() {
        let holder: Address = HOLDER.parse().unwrap();
        let calls = vec![
            (multicall3(), get_eth_balance(&holder)),
            (USDC.parse().unwrap(), balance_of(&holder)),
        ];
        assert_eq!(hex(&encode_aggregate3(&calls)), CAST_CALLDATA);
    }

    #[test]
    fn aggregate3_result_decodes() {
        let data = unhex(CAST_RESULT).unwrap();
        assert_eq!(
            decode_aggregate3(&data, 3),
            Some(vec![Some(1234), None, Some(42)])
        );
        // Wrong arity, truncation and garbage are rejected, never a panic.
        assert_eq!(decode_aggregate3(&data, 2), None);
        assert_eq!(decode_aggregate3(&data[..data.len() - 40], 3), None);
        assert_eq!(decode_aggregate3(&[], 0), None);
        assert_eq!(decode_aggregate3(&[0xff; 96], 1), None);
    }

    #[test]
    fn uint_and_quantity_saturate() {
        let mut w = [0u8; 32];
        w[31] = 7;
        assert_eq!(uint(&w), 7);
        w[0] = 1;
        assert_eq!(uint(&w), u128::MAX);
        assert_eq!(quantity("0x0"), Some(0));
        assert_eq!(quantity("0x1b"), Some(27));
        assert_eq!(quantity("0x"), None);
        assert_eq!(quantity("12"), None);
        assert_eq!(quantity(&format!("0x1{}", "0".repeat(40))), Some(u128::MAX));
        assert_eq!(unhex("0x0a0B"), Some(vec![10, 11]));
        assert_eq!(unhex("0xabc"), None);
        assert_eq!(unhex("0x+f"), None);
        assert_eq!(unhex("+f-1"), None);
        assert_eq!(unhex("0x"), Some(vec![]));
    }
}
