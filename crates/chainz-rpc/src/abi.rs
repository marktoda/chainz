//! The little ABI chainz needs, hand-rolled to keep alloy out of the tree:
//! the selectors it calls or names, ABI word decoding, Multicall3
//! `aggregate3`, ERC-20 `symbol()`/`decimals()` return values, and exact
//! decimal [`units`]. Hex and quantities are [`crate::hex`].

/// ERC-20 `symbol()`.
pub const SYMBOL: [u8; 4] = [0x95, 0xd8, 0x9b, 0x41];
/// ERC-20 `decimals()`.
pub const DECIMALS: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];
/// ERC-20 `balanceOf(address)`.
pub const BALANCE_OF: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
/// Multicall3 `aggregate3((address,bool,bytes)[])`.
pub const AGGREGATE3: [u8; 4] = [0x82, 0xad, 0x56, 0xcb];
/// Multicall3 `getEthBalance(address)`.
pub const GET_ETH_BALANCE: [u8; 4] = [0x4d, 0x23, 0x01, 0xcc];

/// Multicall3, deployed at the same address on every chain that has it.
pub const MULTICALL3: [u8; 20] = [
    0xca, 0x11, 0xbd, 0xe0, 0x59, 0x77, 0xb3, 0x63, 0x11, 0x67, 0x02, 0x88, 0x62, 0xbe, 0x2a, 0x17,
    0x39, 0x76, 0xca, 0x11,
];

/// Functions the inspector names by selector.
const FUNCTIONS: &[([u8; 4], &str)] = &[
    (BALANCE_OF, "balanceOf"),
    ([0xa9, 0x05, 0x9c, 0xbb], "transfer"),
    ([0x23, 0xb8, 0x72, 0xdd], "transferFrom"),
    ([0x09, 0x5e, 0xa7, 0xb3], "approve"),
    ([0xdd, 0x62, 0xed, 0x3e], "allowance"),
    ([0x18, 0x16, 0x0d, 0xdd], "totalSupply"),
    (DECIMALS, "decimals"),
    (SYMBOL, "symbol"),
    ([0x06, 0xfd, 0xde, 0x03], "name"),
    ([0x8d, 0xa5, 0xcb, 0x5b], "owner"),
    ([0xd0, 0xe3, 0x0d, 0xb0], "deposit"),
    ([0x2e, 0x1a, 0x7d, 0x4d], "withdraw"),
    (AGGREGATE3, "aggregate3"),
    ([0x25, 0x2d, 0xba, 0x42], "aggregate"),
    ([0xbc, 0xe3, 0x8b, 0xd7], "tryAggregate"),
    ([0x39, 0x95, 0x42, 0xe9], "tryBlockAndAggregate"),
    (GET_ETH_BALANCE, "getEthBalance"),
    ([0x38, 0x50, 0xc7, 0xbd], "slot0"),
    ([0x09, 0x02, 0xf1, 0xac], "getReserves"),
    ([0x63, 0x52, 0x21, 0x1e], "ownerOf"),
    ([0x01, 0xff, 0xc9, 0xa7], "supportsInterface"),
    ([0x36, 0x44, 0xe5, 0x15], "DOMAIN_SEPARATOR"),
    ([0x7e, 0xce, 0xbe, 0x00], "nonces"),
];

/// The name of a well-known function, by selector.
pub fn function_name(selector: [u8; 4]) -> Option<&'static str> {
    FUNCTIONS
        .iter()
        .find(|(s, _)| *s == selector)
        .map(|(_, name)| *name)
}

/// Calldata for a function whose one argument is an address, such as
/// [`BALANCE_OF`] or [`GET_ETH_BALANCE`].
pub fn call_with_address(selector: [u8; 4], address: &[u8; 20]) -> Vec<u8> {
    let mut data = Vec::with_capacity(36);
    data.extend_from_slice(&selector);
    data.extend_from_slice(&[0; 12]);
    data.extend_from_slice(address);
    data
}

/// A big-endian ABI word at byte `at` as a `usize`, if it fits.
pub fn word(data: &[u8], at: usize) -> Option<usize> {
    let word = data.get(at..at.checked_add(32)?)?;
    if word[..24].iter().any(|b| *b != 0) {
        return None;
    }
    usize::try_from(u64::from_be_bytes(word[24..].try_into().ok()?)).ok()
}

/// A big-endian `uint256` as `u128`, saturating.
pub fn uint(word: &[u8]) -> u128 {
    let (high, low) = word.split_at(word.len().saturating_sub(16));
    if high.iter().any(|b| *b != 0) {
        return u128::MAX;
    }
    low.iter().fold(0u128, |n, b| n << 8 | u128::from(*b))
}

fn encode_word(n: usize) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&(n as u64).to_be_bytes());
    w
}

/// Calldata for `aggregate3((address target, bool allowFailure, bytes
/// callData)[])`, every call allowed to fail.
pub fn encode_aggregate3(calls: &[([u8; 20], Vec<u8>)]) -> Vec<u8> {
    let mut out = AGGREGATE3.to_vec();
    out.extend_from_slice(&encode_word(32));
    out.extend_from_slice(&encode_word(calls.len()));
    let mut offset = calls.len() * 32;
    for (_, data) in calls {
        out.extend_from_slice(&encode_word(offset));
        offset += 4 * 32 + data.len().div_ceil(32) * 32;
    }
    for (target, data) in calls {
        out.extend_from_slice(&[0; 12]);
        out.extend_from_slice(target);
        out.extend_from_slice(&encode_word(1));
        out.extend_from_slice(&encode_word(96));
        out.extend_from_slice(&encode_word(data.len()));
        out.extend_from_slice(data);
        out.resize(out.len() + (data.len().div_ceil(32) * 32 - data.len()), 0);
    }
    out
}

/// Decode the `(bool success, bytes returnData)[]` returned by
/// `aggregate3` into one `uint256` per call: `None` where the call failed
/// or returned fewer than 32 bytes. The outer `None` means the data is not
/// a well-formed result array of `expected` entries.
pub fn decode_aggregate3(data: &[u8], expected: usize) -> Option<Vec<Option<u128>>> {
    let array = word(data, 0)?;
    if word(data, array)? != expected {
        return None;
    }
    let heads = array.checked_add(32)?;
    (0..expected)
        .map(|i| {
            let item = heads.checked_add(word(data, heads + i * 32)?)?;
            let success = word(data, item)? == 1;
            let bytes = item.checked_add(word(data, item + 32)?)?;
            let len = word(data, bytes)?;
            let value = data.get(bytes + 32..(bytes + 32).checked_add(len)?)?;
            Some(if success && len >= 32 {
                Some(uint(&value[..32]))
            } else {
                None
            })
        })
        .collect()
}

/// Decode an ABI `string` return value. Also accepts the `bytes32` that a
/// few old tokens return (NUL padded). `None` for anything malformed.
pub fn decode_string(data: &[u8]) -> Option<String> {
    let bytes = if data.len() == 32 {
        let end = data.iter().position(|b| *b == 0).unwrap_or(32);
        &data[..end]
    } else {
        let offset = word(data, 0)?;
        let len = word(data, offset)?;
        let start = offset.checked_add(32)?;
        data.get(start..start.checked_add(len)?)?
    };
    String::from_utf8(bytes.to_vec()).ok()
}

/// Decode an ABI `uint8` return value: exactly one word, at most 255.
pub fn decode_u8(data: &[u8]) -> Option<u8> {
    if data.len() != 32 {
        return None;
    }
    u8::try_from(word(data, 0)?).ok()
}

/// Exact conversions between decimal token amounts and base units: decimal
/// string arithmetic, never floats.
pub mod units {
    /// Why an amount can't be scaled.
    #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
    pub enum UnitsError {
        /// Not a plain decimal.
        #[error("'{0}' is not a decimal amount (e.g. 10000 or 0.5)")]
        NotDecimal(String),
        /// More fraction digits than the token has (and not zeros).
        #[error("'{0}' has more than {1} decimal places")]
        TooPrecise(String, u8),
        /// Past 256 bits.
        #[error("'{0}' does not fit in 256 bits")]
        TooLarge(String),
    }

    /// Integer and fraction digits of a plain decimal amount (`10000`,
    /// `0.5`, `.5`, `5.`). No sign, exponent or separators.
    fn split(amount: &str) -> Result<(&str, &str), UnitsError> {
        let (int, frac) = amount.split_once('.').unwrap_or((amount, ""));
        let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
        if (int.is_empty() && frac.is_empty()) || !digits(int) || !digits(frac) {
            return Err(UnitsError::NotDecimal(amount.to_string()));
        }
        Ok((int, frac))
    }

    /// Whether `amount` is a plain decimal amount, before its decimals are
    /// known.
    pub fn check(amount: &str) -> Result<(), UnitsError> {
        split(amount).map(|_| ())
    }

    /// `amount` whole tokens as base units, a `0x` quantity. Fails on more
    /// fraction digits than `decimals` (unless they are zeros) and past
    /// 256 bits.
    pub fn scale(amount: &str, decimals: u8) -> Result<String, UnitsError> {
        let (int, frac) = split(amount)?;
        let places = usize::from(decimals);
        let (kept, excess) = frac.split_at(frac.len().min(places));
        if excess.bytes().any(|b| b != b'0') {
            return Err(UnitsError::TooPrecise(amount.to_string(), decimals));
        }
        let mut digits = String::with_capacity(int.len() + places);
        digits.push_str(int);
        digits.push_str(kept);
        digits.extend(std::iter::repeat_n('0', places - kept.len()));
        decimal_to_quantity(&digits).ok_or_else(|| UnitsError::TooLarge(amount.to_string()))
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

    /// `raw` base units as an exact decimal amount, without trailing zeros:
    /// `1234500000000000000` at 18 decimals is `1.2345`.
    pub fn format_exact(raw: u128, decimals: u8) -> String {
        let digits = raw.to_string();
        let decimals = usize::from(decimals);
        if decimals == 0 {
            return digits;
        }
        let padded = format!("{digits:0>width$}", width = decimals + 1);
        let (whole, fraction) = padded.split_at(padded.len() - decimals);
        let fraction = fraction.trim_end_matches('0');
        if fraction.is_empty() {
            whole.to_string()
        } else {
            format!("{whole}.{fraction}")
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn scales_whole_and_fractional_amounts_exactly() {
            assert_eq!(scale("10000", 6).unwrap(), "0x2540be400");
            assert_eq!(scale("5", 18).unwrap(), "0x4563918244f40000");
            assert_eq!(scale("0.5", 18).unwrap(), "0x6f05b59d3b20000");
            assert_eq!(scale(".5", 6).unwrap(), "0x7a120");
            assert_eq!(scale("5.", 0).unwrap(), "0x5");
            assert_eq!(scale("1.500000", 6).unwrap(), "0x16e360");
            assert_eq!(scale("0", 18).unwrap(), "0x0");
            assert_eq!(scale("000.000", 18).unwrap(), "0x0");
            assert_eq!(scale("0.000001", 6).unwrap(), "0x1");
            assert_eq!(scale("1000", 18).unwrap(), "0x3635c9adc5dea00000");
            // 0.1 + 0.2 territory: floats would give ...0004.
            assert_eq!(
                scale("0.3", 18).unwrap(),
                format!("0x{:x}", 300_000_000_000_000_000u128)
            );
            // Beyond f64 and u128 precision.
            assert_eq!(
                scale("123456789012345678901234567890.123456789012345678", 18).unwrap(),
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
                    scale(amount, decimals).unwrap(),
                    format!("0x{expected:x}"),
                    "{amount} @ {decimals}"
                );
            }
        }

        #[test]
        fn rejects_lossy_malformed_and_oversized_amounts() {
            let err = |a, d| scale(a, d).unwrap_err().to_string();
            assert!(err("0.0000001", 6).contains("more than 6 decimal places"));
            assert!(err("1.5", 0).contains("more than 0 decimal places"));
            for bad in [
                "", ".", "1e6", "-1", "+1", "1,000", "1.2.3", "0x10", " 1", "ten",
            ] {
                assert!(scale(bad, 18).is_err(), "{bad:?}");
            }
            let max =
                "115792089237316195423570985008687907853269984665640564039457584007913129639935";
            assert_eq!(scale(max, 0).unwrap(), format!("0x{}", "f".repeat(64)));
            let over =
                "115792089237316195423570985008687907853269984665640564039457584007913129639936";
            assert!(err(over, 0).contains("256 bits"));
            assert!(err("1", 78).contains("256 bits"));
        }

        #[test]
        fn formats_exactly() {
            assert_eq!(format_exact(1_234_500_000_000_000_000, 18), "1.2345");
            assert_eq!(format_exact(1, 18), "0.000000000000000001");
            assert_eq!(format_exact(5_000_000, 6), "5");
            assert_eq!(format_exact(42, 0), "42");
        }

        #[test]
        fn format_and_scale_are_inverse() {
            for (raw, decimals) in [(1u128, 18u8), (1_500_000, 6), (u128::MAX, 18), (0, 0)] {
                let text = format_exact(raw, decimals);
                assert_eq!(scale(&text, decimals).unwrap(), format!("0x{raw:x}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abi_string(s: &str) -> Vec<u8> {
        let mut out = vec![0u8; 64];
        out[31] = 32;
        out[63] = s.len() as u8;
        out.extend_from_slice(s.as_bytes());
        out.resize(64 + s.len().div_ceil(32) * 32, 0);
        out
    }

    fn abi_u8(n: u8) -> Vec<u8> {
        let mut out = vec![0u8; 32];
        out[31] = n;
        out
    }

    #[test]
    fn decodes_abi_strings_and_bytes32() {
        assert_eq!(decode_string(&abi_string("USDC")).as_deref(), Some("USDC"));
        assert_eq!(
            decode_string(&abi_string("USD₮0")).as_deref(),
            Some("USD₮0")
        );
        assert_eq!(decode_string(&abi_string("")).as_deref(), Some(""));
        let mut bytes32 = [0u8; 32];
        bytes32[..3].copy_from_slice(b"MKR");
        assert_eq!(decode_string(&bytes32).as_deref(), Some("MKR"));
        // Truncated, empty, or with an offset or length that points nowhere.
        assert_eq!(decode_string(&[]), None);
        assert_eq!(decode_string(&abi_string("USDC")[..66]), None);
        let mut far = abi_string("USDC");
        far[..32].fill(0xff);
        assert_eq!(decode_string(&far), None);
        let mut long = abi_string("USDC");
        long[32..64].fill(0xff);
        assert_eq!(decode_string(&long), None);
        let mut not_utf8 = abi_string("USDC");
        not_utf8[64] = 0xff;
        assert_eq!(decode_string(&not_utf8), None);
    }

    #[test]
    fn decodes_uint8() {
        assert_eq!(decode_u8(&abi_u8(6)), Some(6));
        assert_eq!(decode_u8(&abi_u8(18)), Some(18));
        assert_eq!(decode_u8(&[]), None);
        assert_eq!(decode_u8(&[0; 64]), None);
        let mut big = abi_u8(0);
        big[30] = 1; // 256
        assert_eq!(decode_u8(&big), None);
        let mut high = abi_u8(6);
        high[0] = 1;
        assert_eq!(decode_u8(&high), None);
    }

    #[test]
    fn selectors() {
        assert_eq!(crate::hex::encode(&SYMBOL), "0x95d89b41");
        assert_eq!(crate::hex::encode(&DECIMALS), "0x313ce567");
        assert_eq!(
            crate::hex::encode(&MULTICALL3),
            "0xca11bde05977b3631167028862be2a173976ca11"
        );
        assert_eq!(function_name(BALANCE_OF), Some("balanceOf"));
        assert_eq!(function_name([0; 4]), None);
    }

    const HOLDER: [u8; 20] = [
        0x7e, 0x5f, 0x45, 0x52, 0x09, 0x1a, 0x69, 0x12, 0x5d, 0x5d, 0xfc, 0xb7, 0xb8, 0xc2, 0x65,
        0x90, 0x29, 0x39, 0x5b, 0xdf,
    ];
    const USDC: [u8; 20] = [
        0xa0, 0xb8, 0x69, 0x91, 0xc6, 0x21, 0x8b, 0x36, 0xc1, 0xd1, 0x9d, 0x4a, 0x2e, 0x9e, 0xb0,
        0xce, 0x36, 0x06, 0xeb, 0x48,
    ];

    // `cast calldata 'aggregate3((address,bool,bytes)[])' '[(mc3,true,getEthBalance(holder)),(usdc,true,balanceOf(holder))]'`
    const CAST_CALLDATA: &str = "0x82ad56cb0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000100000000000000000000000000ca11bde05977b3631167028862be2a173976ca110000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000244d2301cc0000000000000000000000007e5f4552091a69125d5dfcb7b8c2659029395bdf00000000000000000000000000000000000000000000000000000000000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000002470a082310000000000000000000000007e5f4552091a69125d5dfcb7b8c2659029395bdf00000000000000000000000000000000000000000000000000000000";

    // `cast abi-encode 'f((bool,bytes)[])' '[(true,1234),(false,0x),(true,42)]'`
    const CAST_RESULT: &str = "0x00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000e0000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000004d2000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000002a";

    #[test]
    fn aggregate3_calldata_matches_cast() {
        let calls = vec![
            (MULTICALL3, call_with_address(GET_ETH_BALANCE, &HOLDER)),
            (USDC, call_with_address(BALANCE_OF, &HOLDER)),
        ];
        assert_eq!(
            crate::hex::encode(&encode_aggregate3(&calls)),
            CAST_CALLDATA
        );
    }

    #[test]
    fn aggregate3_result_decodes() {
        let data = crate::hex::decode(CAST_RESULT).unwrap();
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
    fn uint_saturates() {
        let mut w = [0u8; 32];
        w[31] = 7;
        assert_eq!(uint(&w), 7);
        w[0] = 1;
        assert_eq!(uint(&w), u128::MAX);
    }
}
