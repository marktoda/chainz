//! The exec contract's names and the environment seam: which variables
//! chainz exports to a tool (and removes when it does not set them), and
//! where the process environment is read. A leaf: template resolution,
//! chain selection and the context all build on it.

/// `CHAINZ_CHAIN`: the chain (or fork) a context runs. Its presence also
/// marks "inside a chainz context" (see [`crate::secrets::VarResolver`])
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

/// Every variable the exec contract may set. Variables not set for a
/// context are removed from the child environment, so a value never
/// leaks in from an enclosing `chainz shell`.
pub const CONTRACT_VARS: [&str; 12] = [
    "ETH_RPC_URL",
    "ETH_RPC_HEADERS",
    "CHAIN_ID",
    CHAINZ_CHAIN,
    "CHAIN_NAME",
    ETHERSCAN_API_KEY,
    "VERIFIER_URL",
    "VERIFIER_API_KEY",
    "ETH_FROM",
    "WALLET_ADDRESS",
    "ETH_KEYSTORE",
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
    pub fn chainz_chain(&self) -> Option<String> {
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
