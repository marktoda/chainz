//! Shared unit-test fixtures.

use crate::key::{Key, KeyType};

/// Private key 1: the smallest valid secp256k1 scalar.
pub(crate) const TEST_PRIVATE_KEY: &str =
    "0000000000000000000000000000000000000000000000000000000000000001";
/// Checksummed address of `TEST_PRIVATE_KEY`.
pub(crate) const TEST_ADDRESS: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";

/// A plaintext key record named `name` holding `TEST_PRIVATE_KEY`.
pub(crate) fn plaintext_key(name: &str) -> Key {
    Key::new(
        name.to_string(),
        KeyType::PrivateKey {
            value: TEST_PRIVATE_KEY.to_string(),
        },
    )
}
