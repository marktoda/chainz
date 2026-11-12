//! The ABI encodings test mocks need. Selectors are written out by hand
//! (`cast sig`), never taken from chainz: a mock built from chainz's own
//! constants would agree with a wrong one.

/// `symbol()`.
pub const SYMBOL: &str = "0x95d89b41";
/// `decimals()`.
pub const DECIMALS: &str = "0x313ce567";
/// `balanceOf(address)`.
pub const BALANCE_OF: &str = "0x70a08231";
/// `aggregate3((address,bool,bytes)[])` on Multicall3.
pub const AGGREGATE3: &str = "0x82ad56cb";

/// One 32-byte word holding `n`, as 64 hex digits.
pub fn word(n: u128) -> String {
    format!("{n:064x}")
}

/// The return data of a function returning `string`: `0x` and the words.
pub fn string(s: &str) -> String {
    let mut data = word(32) + &word(s.len() as u128);
    let mut bytes = s.as_bytes().to_vec();
    bytes.resize(s.len().div_ceil(32) * 32, 0);
    data.extend(bytes.iter().map(|b| format!("{b:02x}")));
    format!("0x{data}")
}

/// The return data of a function returning one `uint`.
pub fn uint(n: u128) -> String {
    format!("0x{}", word(n))
}

/// Calldata for `balanceOf(holder)`.
pub fn balance_of(holder: &str) -> String {
    format!("{BALANCE_OF}{:0>64}", holder.trim_start_matches("0x"))
}

/// What Multicall3's `aggregate3` returns, `(bool success, bytes
/// returnData)[]`: one `uint256` per `Some`, a failed call per `None`.
pub fn aggregate3_result(values: &[Option<u128>]) -> String {
    let mut out = word(32) + &word(values.len() as u128);
    let mut offset = values.len() as u128 * 32;
    let mut items = String::new();
    for value in values {
        out += &word(offset);
        match value {
            Some(v) => {
                items += &(word(1) + &word(64) + &word(32) + &word(*v));
                offset += 4 * 32;
            }
            None => {
                items += &(word(0) + &word(64) + &word(0));
                offset += 3 * 32;
            }
        }
    }
    format!("0x{out}{items}")
}

/// The target address of every call in `aggregate3` calldata, as 40
/// lower-case hex digits without `0x`.
pub fn aggregate3_targets(calldata: &str) -> Vec<String> {
    assert!(
        calldata.starts_with(AGGREGATE3),
        "not aggregate3 calldata: {calldata}"
    );
    let args = &calldata[10..];
    let word = |i: usize| &args[i * 64..(i + 1) * 64];
    let number = |i: usize| usize::from_str_radix(&word(i)[48..], 16).unwrap();
    let count = number(1);
    (0..count)
        .map(|i| word(2 + number(2 + i) / 32)[24..].to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_pads_to_whole_words() {
        let usdc = string("USDC");
        assert_eq!(usdc.len(), 2 + 3 * 64);
        assert!(usdc.ends_with(&format!("{:0<64}", "55534443")));
    }

    #[test]
    fn aggregate3_offsets_point_at_each_entry() {
        let data = aggregate3_result(&[Some(7), None, Some(9)]);
        let words: Vec<&str> = data.as_bytes()[2..]
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        let at = |i: usize| usize::from_str_radix(words[i], 16).unwrap();
        // Offsets are relative to the start of the array's contents.
        let base = 2;
        assert_eq!(words[base + at(2) / 32], word(1));
        assert_eq!(words[base + at(2) / 32 + 3], word(7));
        assert_eq!(words[base + at(3) / 32], word(0));
        assert_eq!(words[base + at(4) / 32 + 3], word(9));
    }
}
