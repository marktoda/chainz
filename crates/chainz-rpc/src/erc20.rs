//! The few ERC-20 bits chainz needs: the `symbol()` and `decimals()`
//! selectors and decoders for their return data.

/// `symbol()`.
pub const SYMBOL: [u8; 4] = [0x95, 0xd8, 0x9b, 0x41];
/// `decimals()`.
pub const DECIMALS: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];

/// A big-endian ABI word at byte `at` as a `usize`, if it fits.
fn word(data: &[u8], at: usize) -> Option<usize> {
    let word = data.get(at..at.checked_add(32)?)?;
    if word[..24].iter().any(|b| *b != 0) {
        return None;
    }
    usize::try_from(u64::from_be_bytes(word[24..].try_into().ok()?)).ok()
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
    }
}
