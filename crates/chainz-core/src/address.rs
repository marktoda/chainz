//! 20-byte EVM addresses: parsing, EIP-55 checksumming and derivation from a
//! secp256k1 private key (`k256` + `sha3`; no alloy).

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha3::{Digest, Keccak256};
use std::fmt;
use std::str::FromStr;

/// An EVM address. `Display` is EIP-55 checksummed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Address([u8; 20]);

/// Why a string is not a valid address.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressError {
    /// Not `0x` and 40 hex characters.
    #[error("address must be 0x followed by 40 hex characters")]
    Format,
    /// Mixed case that is not the EIP-55 checksum.
    #[error("address has an invalid EIP-55 checksum")]
    Checksum,
}

/// Why a private key was rejected. Never carries key material.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid private key: expected 32 bytes of hex for a valid secp256k1 scalar")]
pub struct PrivateKeyError;

impl Address {
    /// The zero address.
    #[cfg(test)]
    pub(crate) const ZERO: Address = Address([0; 20]);

    /// The address with these 20 bytes.
    pub fn from_bytes(bytes: [u8; 20]) -> Self {
        Address(bytes)
    }

    /// The 20 bytes (ABI encoding).
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Lowercase `0x…` hex, without checksum.
    pub fn to_lower_hex(&self) -> String {
        format!("0x{}", hex::encode(self.0))
    }

    /// EIP-55 checksummed form.
    pub fn to_checksum(&self) -> String {
        let lower = hex::encode(self.0);
        let hash = Keccak256::digest(lower.as_bytes());
        let mut out = String::with_capacity(42);
        out.push_str("0x");
        for (i, c) in lower.chars().enumerate() {
            let nibble = (hash[i / 2] >> (if i % 2 == 0 { 4 } else { 0 })) & 0x0f;
            if c.is_ascii_alphabetic() && nibble >= 8 {
                out.push(c.to_ascii_uppercase());
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Derive the address of a hex private key (with or without `0x`).
    pub fn from_private_key(private_key: &str) -> Result<Self, PrivateKeyError> {
        let bytes = parse_private_key(private_key)?;
        let secret = k256::SecretKey::from_slice(&bytes[..]).map_err(|_| PrivateKeyError)?;
        let point = {
            use k256::elliptic_curve::sec1::ToEncodedPoint;
            secret.public_key().to_encoded_point(false)
        };
        let hash = Keccak256::digest(&point.as_bytes()[1..]);
        let mut out = [0u8; 20];
        out.copy_from_slice(&hash[12..]);
        Ok(Address(out))
    }
}

/// Why `private_key` cannot back signer `name` with cached address
/// `cached`, if it cannot: it is not a key, or it derives another address
/// (no cached address never differs). The one check and wording behind
/// validation, unlock and v1 migration.
pub(crate) fn cached_address_problem(
    name: &str,
    private_key: &str,
    cached: Option<Address>,
) -> Option<String> {
    match Address::from_private_key(private_key) {
        Err(_) => Some(format!("signer '{name}' holds an invalid private key")),
        Ok(derived) if cached.is_some_and(|cached| cached != derived) => Some(format!(
            "signer '{name}' cached address does not match its key"
        )),
        Ok(_) => None,
    }
}

fn parse_private_key(input: &str) -> Result<zeroize::Zeroizing<[u8; 32]>, PrivateKeyError> {
    let hex_str = input.trim();
    let hex_str = hex_str
        .strip_prefix("0x")
        .or_else(|| hex_str.strip_prefix("0X"))
        .unwrap_or(hex_str);
    if hex_str.len() != 64 {
        return Err(PrivateKeyError);
    }
    let mut bytes = zeroize::Zeroizing::new([0u8; 32]);
    hex::decode_to_slice(hex_str, &mut bytes[..]).map_err(|_| PrivateKeyError)?;
    Ok(bytes)
}

impl FromStr for Address {
    type Err = AddressError;

    /// Accepts all-lowercase, all-uppercase, or a correctly checksummed
    /// mixed-case address.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let body = s.strip_prefix("0x").ok_or(AddressError::Format)?;
        if body.len() != 40 {
            return Err(AddressError::Format);
        }
        let mut out = [0u8; 20];
        hex::decode_to_slice(body, &mut out).map_err(|_| AddressError::Format)?;
        let address = Address(out);
        let has_lower = body.bytes().any(|b| b.is_ascii_lowercase());
        let has_upper = body.bytes().any(|b| b.is_ascii_uppercase());
        if has_lower && has_upper && address.to_checksum()[2..] != *body {
            return Err(AddressError::Checksum);
        }
        Ok(address)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_checksum())
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address({})", self.to_checksum())
    }
}

impl Serialize for Address {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_checksum())
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_known_addresses() {
        let one = "0000000000000000000000000000000000000000000000000000000000000001";
        assert_eq!(
            Address::from_private_key(one).unwrap().to_string(),
            "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"
        );
        let anvil0 = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        assert_eq!(
            Address::from_private_key(anvil0).unwrap().to_string(),
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
        );
    }

    #[test]
    fn rejects_bad_keys_without_echoing_them() {
        for bad in ["", "zz", &"0".repeat(64), &"f".repeat(64), "0x1234"] {
            let err = Address::from_private_key(bad).unwrap_err().to_string();
            assert!(!err.contains("1234"));
        }
    }

    #[test]
    fn parses_and_checks_checksums() {
        let a: Address = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
            .parse()
            .unwrap();
        assert_eq!(a.to_string(), "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        assert_eq!(
            "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".parse::<Address>(),
            Ok(a)
        );
        // Mixed case whose checksum does not match these bytes.
        assert_eq!(
            "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02912".parse::<Address>(),
            Err(AddressError::Checksum)
        );
        assert_eq!(
            "0x833589fcd6edb6e08f4c7c32d4f71b54bda0291z".parse::<Address>(),
            Err(AddressError::Format)
        );
        assert_eq!(
            "0x833589fcd6edb6e08f4c7c32d4f71b54bda029".parse::<Address>(),
            Err(AddressError::Format)
        );
        assert_eq!(
            "0x833589FCD6eDb6E08f4c7C32D4f71b54bdA02913".parse::<Address>(),
            Err(AddressError::Checksum)
        );
        assert!(
            "833589fcd6edb6e08f4c7c32d4f71b54bda02913"
                .parse::<Address>()
                .is_err()
        );
    }
}
