//! The exec contract's names and the environment seam: which variables
//! chainz exports to a tool (and removes when it does not set them), and
//! where the process environment is read. A leaf: template resolution,
//! chain selection and the context all build on it.

/// `CHAINZ_CHAIN`: the chain (or fork) a context runs. Its presence also
/// marks "inside a chainz context" (see `secrets::VarResolver`)
/// and selects the chain when no argument does.
pub const CHAINZ_CHAIN: &str = "CHAINZ_CHAIN";

/// The variable forge and chainz both read an Etherscan key from: chainz
/// exports the chain's explorer key under this name, and an Etherscan
/// explorer's key is stored in a variable of the same name. The two
/// coincide on purpose, so `${ETHERSCAN_API_KEY}` from the environment and
/// from `secrets.toml` mean the same key.
pub const ETHERSCAN_API_KEY: &str = "ETHERSCAN_API_KEY";

/// `RAW_PRIVATE_KEY`: the signer's key, only with `--expose-key`.
pub const RAW_PRIVATE_KEY: &str = "RAW_PRIVATE_KEY";

/// `ETH_RPC_URL`: the chain's RPC (the gateway's, or the endpoint's).
pub const ETH_RPC_URL: &str = "ETH_RPC_URL";
/// `ETH_RPC_HEADERS`: the endpoint's headers (direct only).
pub const ETH_RPC_HEADERS: &str = "ETH_RPC_HEADERS";
/// `CHAIN_ID`: the chain ID (also the built-in `${CHAIN_ID}` variable).
pub const CHAIN_ID: &str = "CHAIN_ID";
/// `CHAIN_NAME`: the chain's name.
pub const CHAIN_NAME: &str = "CHAIN_NAME";
/// `VERIFIER_URL`: a non-Etherscan explorer's API.
pub const VERIFIER_URL: &str = "VERIFIER_URL";
/// `VERIFIER_API_KEY`: a non-Etherscan explorer's key.
pub const VERIFIER_API_KEY: &str = "VERIFIER_API_KEY";
/// `ETH_FROM`: the signer's address.
pub const ETH_FROM: &str = "ETH_FROM";
/// `WALLET_ADDRESS`: the signer's address.
pub const WALLET_ADDRESS: &str = "WALLET_ADDRESS";
/// `ETH_KEYSTORE`: a keystore signer's file.
pub const ETH_KEYSTORE: &str = "ETH_KEYSTORE";

/// Every variable the exec contract may set. Variables not set for a
/// context are removed from the child environment, so a value never
/// leaks in from an enclosing `chainz shell`.
pub const CONTRACT_VARS: [&str; 12] = [
    ETH_RPC_URL,
    ETH_RPC_HEADERS,
    CHAIN_ID,
    CHAINZ_CHAIN,
    CHAIN_NAME,
    ETHERSCAN_API_KEY,
    VERIFIER_URL,
    VERIFIER_API_KEY,
    ETH_FROM,
    WALLET_ADDRESS,
    ETH_KEYSTORE,
    RAW_PRIVATE_KEY,
];

/// Where environment variables are read: the process environment, or a
/// fixed lookup in tests. Reading is lazy (one `getenv` per lookup, as
/// before the seam existed).
#[derive(Clone, Copy)]
pub struct Env {
    get: fn(&str) -> Option<String>,
}

impl Env {
    /// The process environment.
    pub fn process() -> Self {
        Self {
            get: |name| std::env::var(name).ok(),
        }
    }

    /// An environment answered by `get` (tests).
    pub fn from_fn(get: fn(&str) -> Option<String>) -> Self {
        Self { get }
    }

    /// The value of `name`, if set (and valid UTF-8).
    pub fn var(&self, name: &str) -> Option<String> {
        (self.get)(name)
    }

    /// The chain `CHAINZ_CHAIN` names, if set and non-empty.
    pub(crate) fn chainz_chain(&self) -> Option<String> {
        self.var(CHAINZ_CHAIN).filter(|v| !v.is_empty())
    }
}

impl Default for Env {
    fn default() -> Self {
        Self::process()
    }
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Env")
    }
}
