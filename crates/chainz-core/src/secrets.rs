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

use crate::template;
use indexmap::IndexMap;
use serde::Deserialize;
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
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignerSecret {
    /// Plaintext hex key.
    #[serde(default)]
    pub private_key: Option<String>,
    /// Encrypted envelope: base64 AES-256-GCM ciphertext.
    #[serde(default)]
    pub ciphertext: Option<String>,
    /// Base64 12-byte nonce.
    #[serde(default)]
    pub nonce: Option<String>,
    /// Base64 Argon2id salt (>= 16 bytes).
    #[serde(default)]
    pub salt: Option<String>,
    /// Envelope version (1).
    #[serde(default)]
    pub version: Option<u8>,
    /// Argon2id memory cost in KiB.
    #[serde(default)]
    pub kdf_memory_kib: Option<u32>,
    /// Argon2id iterations.
    #[serde(default)]
    pub kdf_iterations: Option<u32>,
    /// Argon2id lanes.
    #[serde(default)]
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
pub struct Builtins<'a> {
    /// `${CHAIN_ID}`.
    pub chain_id: Option<u64>,
    /// `${CHAIN}`: the chain's short name.
    pub chain: Option<&'a str>,
}

/// Names that always resolve from the chain context.
pub const BUILTIN_VARS: [&str; 2] = ["CHAIN_ID", "CHAIN"];

/// Resolves `${NAME}` for one context: built-ins, then the environment,
/// then `secrets.toml` (loaded lazily, only when the environment misses).
pub struct VarResolver<'a> {
    builtins: Builtins<'a>,
    secrets: &'a dyn Fn() -> anyhow::Result<&'a Secrets>,
}

impl<'a> VarResolver<'a> {
    /// A resolver over lazily loaded secrets.
    pub fn new(
        builtins: Builtins<'a>,
        secrets: &'a dyn Fn() -> anyhow::Result<&'a Secrets>,
    ) -> Self {
        Self { builtins, secrets }
    }

    /// Look up one name.
    pub fn lookup(&self, name: &str) -> anyhow::Result<Option<Cow<'a, str>>> {
        match name {
            "CHAIN_ID" => {
                if let Some(id) = self.builtins.chain_id {
                    return Ok(Some(Cow::Owned(id.to_string())));
                }
            }
            "CHAIN" => {
                if let Some(chain) = self.builtins.chain {
                    return Ok(Some(Cow::Borrowed(chain)));
                }
            }
            _ => {}
        }
        if let Ok(value) = std::env::var(name) {
            return Ok(Some(Cow::Owned(value)));
        }
        let secrets = (self.secrets)()?;
        Ok(secrets.vars.get(name).map(|v| Cow::Borrowed(v.as_str())))
    }

    /// Expand every template in `input`. Errors name the variable, never a
    /// value.
    pub fn expand<'i>(&self, input: &'i str) -> anyhow::Result<Cow<'i, str>>
    where
        'a: 'i,
    {
        template::expand(input, |name| {
            self.lookup(name)
                .map(|v| v.map(|v| Cow::Owned(v.into_owned())))
        })
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
        );
        assert_eq!(
            r.expand("${CHAIN_ID}/${CHAIN}/${FROM_FILE}").unwrap(),
            "8453/base/file"
        );
        let err = r.expand("${CHAINZ_SURELY_UNSET_VAR}").unwrap_err();
        assert!(err.to_string().contains("CHAINZ_SURELY_UNSET_VAR"));
    }

    #[test]
    fn secrets_are_not_loaded_when_unneeded() {
        let loader = || -> anyhow::Result<&Secrets> { anyhow::bail!("must not load") };
        let r = VarResolver::new(Builtins::default(), &loader);
        assert_eq!(r.expand("https://plain").unwrap(), "https://plain");
    }
}
