//! `secrets.toml` (mode 0600): variable values and key material for the
//! `plaintext` and `encrypted` signer backends.
//!
//! ```toml
//! [vars]
//! INFURA_KEY = "…"
//!
//! [signers.hot]          # backend = "plaintext"
//! private_key = "…"
//!
//! [signers.cold]         # backend = "encrypted"
//! ciphertext = "…"
//! nonce = "…"
//! salt = "…"
//! version = 1
//! ```
//!
//! Nothing here implements `Display`; `Debug` shows names only.

use crate::contract::{CONTRACT_VARS, Env};
use crate::template;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;
use zeroize::Zeroize;

/// Parsed `secrets.toml`.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    /// Variable values, referenced as `${NAME}`.
    #[serde(default)]
    pub vars: IndexMap<String, String>,
    /// Key material by signer name.
    #[serde(default)]
    pub signers: IndexMap<String, SignerSecret>,
}

/// Key material for one signer. Exactly one shape is valid per backend:
/// `private_key` (plaintext) or the encrypted envelope fields.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignerSecret {
    /// Plaintext hex key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    /// Encrypted envelope: base64 AES-256-GCM ciphertext.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ciphertext: Option<String>,
    /// Base64 12-byte nonce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    /// Base64 Argon2id salt (>= 16 bytes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salt: Option<String>,
    /// Envelope version (1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u8>,
    /// Argon2id memory cost in KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kdf_memory_kib: Option<u32>,
    /// Argon2id iterations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kdf_iterations: Option<u32>,
    /// Argon2id lanes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kdf_parallelism: Option<u32>,
}

impl Drop for Secrets {
    fn drop(&mut self) {
        for value in self.vars.values_mut() {
            value.zeroize();
        }
    }
}

impl Drop for SignerSecret {
    fn drop(&mut self) {
        if let Some(key) = &mut self.private_key {
            key.zeroize();
        }
    }
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Secrets")
            .field("vars", &self.vars.keys().collect::<Vec<_>>())
            .field("signers", &self.signers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl fmt::Debug for SignerSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shape = if self.private_key.is_some() {
            "plaintext"
        } else if self.ciphertext.is_some() {
            "encrypted"
        } else {
            "empty"
        };
        f.debug_struct("SignerSecret")
            .field("shape", &shape)
            .finish_non_exhaustive()
    }
}

impl Secrets {
    /// Parse `secrets.toml` text.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }
}

/// Built-in template values for one chain context.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Builtins<'a> {
    /// `${CHAIN_ID}`.
    pub chain_id: Option<u64>,
    /// `${CHAIN}`: the chain's short name.
    pub chain: Option<&'a str>,
}

/// `${CHAIN_ID}`: the chain's ID.
pub(crate) const CHAIN_ID_VAR: &str = crate::contract::CHAIN_ID;
/// `${CHAIN}`: the chain's short name.
pub(crate) const CHAIN_VAR: &str = "CHAIN";
/// Names that always resolve from the chain context.
pub(crate) const BUILTIN_VARS: [&str; 2] = [CHAIN_ID_VAR, CHAIN_VAR];

/// Why `name` cannot be a variable in `secrets.toml`, if it cannot: it must
/// be a valid template name and not a built-in.
pub fn check_var_name(name: &str) -> anyhow::Result<()> {
    if !crate::template::is_valid_name(name) {
        anyhow::bail!("variable name '{name}' must match [A-Za-z_][A-Za-z0-9_]*");
    }
    if BUILTIN_VARS.contains(&name) {
        anyhow::bail!("'{name}' is built in (the chain being run); pick another name");
    }
    Ok(())
}

/// Resolves `${NAME}` for one context: built-ins, then the environment,
/// then `secrets.toml` (loaded lazily, only when the environment misses).
///
/// Inside a chainz context (`CHAINZ_CHAIN` is set: `chainz shell`, `eval
/// "$(chainz env …)"`, or a tool `exec` started) the environment is not
/// consulted for the exec contract's own variables
/// ([`CONTRACT_VARS`](crate::contract::CONTRACT_VARS)): there they hold the
/// enclosing chain's values, so a nested `chainz exec eth` inside `chainz
/// shell celo` must not send celo's `ETHERSCAN_API_KEY` to Etherscan.
pub(crate) struct VarResolver<'a> {
    builtins: Builtins<'a>,
    secrets: &'a dyn Fn() -> anyhow::Result<&'a Secrets>,
    env: Env,
}

impl<'a> VarResolver<'a> {
    /// A resolver over lazily loaded secrets and `env`.
    pub(crate) fn new(
        builtins: Builtins<'a>,
        secrets: &'a dyn Fn() -> anyhow::Result<&'a Secrets>,
        env: Env,
    ) -> Self {
        Self {
            builtins,
            secrets,
            env,
        }
    }

    /// Look up one name.
    pub(crate) fn lookup(&self, name: &str) -> anyhow::Result<Option<Cow<'a, str>>> {
        if name == CHAIN_ID_VAR
            && let Some(id) = self.builtins.chain_id
        {
            return Ok(Some(Cow::Owned(id.to_string())));
        }
        if name == CHAIN_VAR
            && let Some(chain) = self.builtins.chain
        {
            return Ok(Some(Cow::Borrowed(chain)));
        }
        let shadowed = CONTRACT_VARS.contains(&name) && self.env.chainz_chain().is_some();
        if !shadowed && let Some(value) = self.env.var(name) {
            return Ok(Some(Cow::Owned(value)));
        }
        let secrets = (self.secrets)()?;
        Ok(secrets.vars.get(name).map(|v| Cow::Borrowed(v.as_str())))
    }

    /// Expand every template in `input`. Errors name the variable, never a
    /// value.
    pub(crate) fn expand<'i>(&self, input: &'i str) -> anyhow::Result<Cow<'i, str>> {
        template::expand(input, |name| self.lookup(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_debug_redacts() {
        let s = Secrets::parse(
            "[vars]\nINFURA_KEY = \"var-secret\"\n[signers.hot]\nprivate_key = \"key-secret\"\n",
        )
        .unwrap();
        assert_eq!(s.vars["INFURA_KEY"], "var-secret");
        let debug = format!("{s:?} {:?}", s.signers["hot"]);
        assert!(
            !debug.contains("var-secret") && !debug.contains("key-secret"),
            "{debug}"
        );
        assert!(debug.contains("INFURA_KEY"));
    }

    #[test]
    fn builtins_win_then_env_then_secrets() {
        let secrets = Secrets::parse("[vars]\nFROM_FILE = \"file\"\nCHAIN_ID = \"999\"\n").unwrap();
        let loader = || Ok(&secrets);
        let r = VarResolver::new(
            Builtins {
                chain_id: Some(8453),
                chain: Some("base"),
            },
            &loader,
            Env::from_fn(|name| (name == "CHAIN_ID").then(|| "1".to_string())),
        );
        assert_eq!(
            r.expand("${CHAIN_ID}/${CHAIN}/${FROM_FILE}").unwrap(),
            "8453/base/file"
        );
        let err = r.expand("${CHAINZ_SURELY_UNSET_VAR}").unwrap_err();
        assert!(err.to_string().contains("CHAINZ_SURELY_UNSET_VAR"));
    }

    fn shell_env(name: &str) -> Option<String> {
        match name {
            "CHAINZ_CHAIN" => Some("celo".into()),
            "ETHERSCAN_API_KEY" => Some("celo-explorer-key".into()),
            "ETH_RPC_URL" => Some("https://celo.example".into()),
            "INFURA_KEY" => Some("env-infura".into()),
            _ => None,
        }
    }

    fn plain_env(name: &str) -> Option<String> {
        shell_env(name).filter(|_| name != "CHAINZ_CHAIN")
    }

    #[test]
    fn inside_a_chainz_context_contract_vars_skip_the_environment() {
        let secrets = Secrets::parse(
            "[vars]\nETHERSCAN_API_KEY = \"own-key\"\nINFURA_KEY = \"file-infura\"\n",
        )
        .unwrap();
        let loader = || Ok(&secrets);
        let mut r = VarResolver::new(Builtins::default(), &loader, Env::from_fn(shell_env));
        // A contract variable comes from secrets.toml, not the enclosing
        // chain's export; other variables still prefer the environment.
        assert_eq!(r.expand("${ETHERSCAN_API_KEY}").unwrap(), "own-key");
        assert_eq!(r.expand("${INFURA_KEY}").unwrap(), "env-infura");
        let err = r.expand("${ETH_RPC_URL}").unwrap_err().to_string();
        assert!(err.contains("ETH_RPC_URL"), "{err}");

        // Outside chainz, the environment wins as usual.
        r.env = Env::from_fn(plain_env);
        assert_eq!(
            r.expand("${ETHERSCAN_API_KEY}").unwrap(),
            "celo-explorer-key"
        );
    }

    #[test]
    fn secrets_are_not_loaded_when_unneeded() {
        let loader = || -> anyhow::Result<&Secrets> { anyhow::bail!("must not load") };
        let r = VarResolver::new(Builtins::default(), &loader, Env::from_fn(|_| None));
        assert_eq!(r.expand("https://plain").unwrap(), "https://plain");
    }
}
