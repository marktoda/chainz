//! The exec contract: the context handed to a tool (one chain, one
//! endpoint, optionally one signer and explorer), its environment table,
//! and `@token` expansion of arguments.
//!
//! **Environment** (exported only when the value exists; every other
//! contract variable is removed from the child's environment so a value
//! never leaks in from an enclosing `chainz shell`):
//!
//! | Variable | Value |
//! | --- | --- |
//! | `ETH_RPC_URL` | gateway URL for the chain, else the active endpoint expanded |
//! | `ETH_RPC_HEADERS` | `name: value` pairs, direct mode only |
//! | `CHAIN_ID`, `CHAINZ_CHAIN` | chain ID and short (or fork) name |
//! | `ETHERSCAN_API_KEY` | the chain's explorer key |
//! | `VERIFIER_URL`, `VERIFIER_API_KEY` | explorer API URL and key, non-Etherscan explorers only |
//! | `ETH_FROM`, `WALLET_ADDRESS` | the signer's cached address |
//! | `ETH_KEYSTORE` | keystore signer path (never on a fork) |
//! | `RAW_PRIVATE_KEY` | only with `--expose-key`, never on a fork (added by the caller) |
//! | `CHAIN_NAME` | same as `CHAINZ_CHAIN`; v1 name, kept for one release |

use crate::address::Address;
use crate::grammar::{self, Piece};
use crate::labels::{LabelError, LabelIndex};
use crate::model::Backend;
use crate::names::{self, CONTEXT_TOKENS, DEPRECATED_TOKENS};
use crate::redact::redact;
use crate::resolve::{self, ChainRef};
use crate::runtime::{ForkRecord, Runtime};
use crate::secrets::{Builtins, VarResolver};
use crate::store::Chainz;
use anyhow::{Context as _, Result, anyhow, bail};
use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

/// Every variable the exec contract may set. Variables not set for a
/// context are removed from the child environment.
pub const CONTRACT_VARS: [&str; 12] = [
    "ETH_RPC_URL",
    "ETH_RPC_HEADERS",
    "CHAIN_ID",
    "CHAINZ_CHAIN",
    "CHAIN_NAME",
    "ETHERSCAN_API_KEY",
    "VERIFIER_URL",
    "VERIFIER_API_KEY",
    "ETH_FROM",
    "WALLET_ADDRESS",
    "ETH_KEYSTORE",
    "RAW_PRIVATE_KEY",
];

/// Per-invocation options.
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// `--signer`: override the chain's signer.
    pub signer: Option<String>,
    /// `--rpc`: use this endpoint instead of the active one.
    pub rpc: Option<String>,
    /// `--direct`: bypass a running gateway.
    pub direct: bool,
}

/// The resolved RPC for a context.
#[derive(Clone)]
pub struct RpcContext {
    /// The URL handed to the tool (secrets expanded).
    pub url: String,
    /// Expanded headers (direct mode only; empty through the gateway).
    pub headers: Vec<(String, String)>,
    /// Endpoint name, when a configured endpoint was used.
    pub endpoint: Option<String>,
    /// Whether the URL is the local gateway.
    pub via_gateway: bool,
    /// Safe-to-print form of the URL.
    pub display: String,
}

impl fmt::Debug for RpcContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcContext")
            .field("url", &self.display)
            .field(
                "headers",
                &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .field("endpoint", &self.endpoint)
            .field("via_gateway", &self.via_gateway)
            .finish()
    }
}

/// The signer for a context.
#[derive(Debug, Clone)]
pub struct SignerContext {
    /// Signer name.
    pub name: String,
    /// Backend.
    pub backend: Backend,
    /// Cached address.
    pub address: Option<Address>,
    /// Keystore file (keystore backend).
    pub keystore: Option<PathBuf>,
}

/// The explorer for a context.
#[derive(Clone)]
pub struct ExplorerContext {
    /// Explorer name.
    pub name: String,
    /// API URL, expanded.
    pub api: String,
    /// API key, expanded.
    pub key: Option<String>,
    /// Etherscan family (forge handles it natively).
    pub etherscan: bool,
}

impl fmt::Debug for ExplorerContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExplorerContext")
            .field("name", &self.name)
            .field("api", &redact(&self.api))
            .field("key", &self.key.as_ref().map(|_| "[REDACTED]"))
            .field("etherscan", &self.etherscan)
            .finish()
    }
}

/// Everything `exec` hands a tool.
#[derive(Debug, Clone)]
pub struct Context {
    /// `CHAINZ_CHAIN`: short name, or fork name.
    pub chain: String,
    /// Chain ID.
    pub chain_id: u64,
    /// The fork, when the chain is a live fork.
    pub fork: Option<ForkRecord>,
    /// RPC.
    pub rpc: RpcContext,
    /// Signer.
    pub signer: Option<SignerContext>,
    /// Explorer.
    pub explorer: Option<ExplorerContext>,
}

/// One exported variable.
#[derive(Clone)]
pub struct EnvVar {
    /// Name.
    pub name: &'static str,
    /// Value.
    pub value: String,
    /// Whether the value is secret (shown redacted by `--explain`).
    pub secret: bool,
}

impl fmt::Debug for EnvVar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shown = if self.secret {
            "[REDACTED]"
        } else {
            self.value.as_str()
        };
        write!(f, "{}={}", self.name, shown)
    }
}

impl EnvVar {
    /// The value as `--explain` prints it.
    pub fn display(&self) -> String {
        match self.name {
            "ETH_RPC_URL" | "VERIFIER_URL" => redact(&self.value),
            "ETH_RPC_HEADERS" => self
                .value
                .split(',')
                .map(|pair| match pair.split_once(':') {
                    Some((name, _)) => format!("{name}: [REDACTED]"),
                    None => "[REDACTED]".to_string(),
                })
                .collect::<Vec<_>>()
                .join(","),
            _ if self.secret => "[REDACTED]".to_string(),
            _ => self.value.clone(),
        }
    }
}

/// Build the context for a resolved chain reference.
pub fn build(
    chainz: &Chainz,
    runtime: &Runtime,
    target: &ChainRef,
    opts: &ExecOptions,
) -> Result<Context> {
    let config = chainz.config();
    let (chain_name, fork) = match target {
        ChainRef::Configured(name) => (name.clone(), None),
        ChainRef::Fork(fork) => (fork.parent.clone(), Some(fork.clone())),
    };
    let chain = config
        .chain(&chain_name)
        .ok_or_else(|| anyhow!("chain '{chain_name}' is not configured"))?;
    let chain_id = fork.as_ref().map_or(chain.id, |f| f.chain_id);
    let exported_name = target.name().to_string();
    let secrets_loader = || chainz.secrets();
    let vars = VarResolver::new(
        Builtins {
            chain_id: Some(chain_id),
            chain: Some(&chain_name),
        },
        &secrets_loader,
    );

    let rpc = if let Some(fork) = &fork {
        if opts.rpc.is_some() {
            bail!("--rpc does not apply to a fork; it always uses its local anvil");
        }
        let url = fork.rpc_url();
        RpcContext {
            display: url.clone(),
            url,
            headers: vec![],
            endpoint: None,
            via_gateway: false,
        }
    } else {
        let endpoint_name = opts.rpc.clone().unwrap_or_else(|| chain.rpc.clone());
        let endpoint = chain.rpcs.get(&endpoint_name).ok_or_else(|| {
            anyhow!(
                "chain '{chain_name}' has no endpoint '{endpoint_name}' (endpoints: {})",
                chain
                    .rpcs
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        let gateway = if opts.direct || endpoint.is_websocket() {
            None
        } else {
            runtime.gateway_url(&chain_name, opts.rpc.as_deref())
        };
        match gateway {
            Some(url) => RpcContext {
                display: redact(&url),
                url,
                headers: vec![],
                endpoint: Some(endpoint_name),
                via_gateway: true,
            },
            None => {
                let url = vars
                    .expand(&endpoint.url)
                    .with_context(|| {
                        format!("chain '{chain_name}' endpoint '{endpoint_name}' URL")
                    })?
                    .into_owned();
                let mut headers = Vec::with_capacity(endpoint.headers.len());
                for (name, value) in &endpoint.headers {
                    let value = vars
                        .expand(value)
                        .with_context(|| {
                            format!(
                                "chain '{chain_name}' endpoint '{endpoint_name}' header '{name}'"
                            )
                        })?
                        .into_owned();
                    headers.push((name.clone(), value));
                }
                RpcContext {
                    display: redact(&endpoint.url),
                    url,
                    headers,
                    endpoint: Some(endpoint_name),
                    via_gateway: false,
                }
            }
        }
    };

    let signer_name = opts.signer.clone().or_else(|| chain.signer.clone());
    let signer = match signer_name {
        None => None,
        Some(name) => {
            let signer = config
                .signers
                .get(&name)
                .ok_or_else(|| anyhow!("signer '{name}' not found"))?;
            Some(SignerContext {
                keystore: (signer.backend == Backend::Keystore)
                    .then(|| signer.path.as_deref().map(crate::paths::expand_tilde))
                    .flatten(),
                name,
                backend: signer.backend,
                address: signer.address,
            })
        }
    };

    let explorer = match (&fork, &chain.explorer) {
        (None, Some(name)) => {
            let explorer = config
                .explorers
                .get(name)
                .ok_or_else(|| anyhow!("explorer '{name}' not found"))?;
            Some(ExplorerContext {
                name: name.clone(),
                api: vars
                    .expand(&explorer.api)
                    .with_context(|| format!("explorer '{name}' api"))?
                    .into_owned(),
                key: explorer
                    .key
                    .as_deref()
                    .map(|k| vars.expand(k).map(|v| v.into_owned()))
                    .transpose()
                    .with_context(|| format!("explorer '{name}' key"))?,
                etherscan: explorer.is_etherscan(),
            })
        }
        _ => None,
    };

    Ok(Context {
        chain: exported_name,
        chain_id,
        fork,
        rpc,
        signer,
        explorer,
    })
}

impl Context {
    /// The environment for this context (without `RAW_PRIVATE_KEY`, which
    /// the caller adds for `--expose-key`).
    pub fn env(&self) -> Result<Vec<EnvVar>> {
        let mut env = Vec::with_capacity(12);
        let mut push = |name, value: String, secret| {
            env.push(EnvVar {
                name,
                value,
                secret,
            })
        };
        push("ETH_RPC_URL", self.rpc.url.clone(), true);
        if !self.rpc.via_gateway && !self.rpc.headers.is_empty() {
            let mut pairs = Vec::with_capacity(self.rpc.headers.len());
            for (name, value) in &self.rpc.headers {
                if value.contains([',', '\r', '\n']) {
                    bail!(
                        "RPC header '{name}' expands to a value with a comma or line break, which cannot be exported via ETH_RPC_HEADERS"
                    );
                }
                pairs.push(format!("{name}: {value}"));
            }
            push("ETH_RPC_HEADERS", pairs.join(","), true);
        }
        push("CHAIN_ID", self.chain_id.to_string(), false);
        push("CHAINZ_CHAIN", self.chain.clone(), false);
        push("CHAIN_NAME", self.chain.clone(), false);
        if let Some(explorer) = &self.explorer {
            if let Some(key) = &explorer.key {
                push("ETHERSCAN_API_KEY", key.clone(), true);
            }
            if !explorer.etherscan {
                push("VERIFIER_URL", explorer.api.clone(), false);
                if let Some(key) = &explorer.key {
                    push("VERIFIER_API_KEY", key.clone(), true);
                }
            }
        }
        if let Some(signer) = &self.signer {
            if let Some(address) = signer.address {
                push("ETH_FROM", address.to_string(), false);
                push("WALLET_ADDRESS", address.to_string(), false);
            }
            if self.fork.is_none()
                && let Some(path) = &signer.keystore
            {
                push("ETH_KEYSTORE", path.display().to_string(), false);
            }
        }
        Ok(env)
    }
}

/// An expanded argument.
#[derive(Clone)]
pub struct ExpandedArg {
    /// What the tool receives.
    pub value: String,
    /// Safe-to-print form (`--explain`).
    pub display: String,
}

impl fmt::Debug for ExpandedArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display)
    }
}

/// Expands `@tokens` in argv for one context. Label and other-chain
/// lookups are built lazily, only when a token needs them.
pub struct Expander<'a> {
    chainz: &'a Chainz,
    runtime: &'a Runtime,
    ctx: &'a Context,
    direct: bool,
    labels: OnceCell<LabelIndex>,
    others: RefCell<HashMap<String, Context>>,
    warnings: RefCell<Vec<String>>,
}

struct Value {
    value: String,
    display: String,
}

impl Value {
    fn public(v: String) -> Self {
        Self {
            display: v.clone(),
            value: v,
        }
    }
}

impl<'a> Expander<'a> {
    /// An expander for `ctx`.
    pub fn new(
        chainz: &'a Chainz,
        runtime: &'a Runtime,
        ctx: &'a Context,
        opts: &ExecOptions,
    ) -> Self {
        Self {
            chainz,
            runtime,
            ctx,
            direct: opts.direct,
            labels: OnceCell::new(),
            others: RefCell::new(HashMap::new()),
            warnings: RefCell::new(Vec::new()),
        }
    }

    /// Deprecation warnings collected during expansion (print each once).
    pub fn warnings(&self) -> Vec<String> {
        let mut w = self.warnings.borrow().clone();
        w.dedup();
        w
    }

    /// Expand every argument.
    pub fn expand_args(&self, args: &[String]) -> Result<Vec<ExpandedArg>> {
        args.iter().map(|arg| self.expand_arg(arg)).collect()
    }

    /// Expand one argument.
    pub fn expand_arg(&self, arg: &str) -> Result<ExpandedArg> {
        if !grammar::may_contain_tokens(arg) {
            return Ok(ExpandedArg {
                value: arg.to_string(),
                display: arg.to_string(),
            });
        }
        let whole = grammar::whole_token(arg).is_some();
        let mut value = String::with_capacity(arg.len());
        let mut display = String::with_capacity(arg.len());
        for piece in grammar::tokenize(arg) {
            match piece {
                Piece::Text(text) => {
                    value.push_str(&text);
                    display.push_str(&text);
                }
                Piece::Token(token) => match self.token(&token.name, token.chain)? {
                    Some(v) => {
                        value.push_str(&v.value);
                        display.push_str(&v.display);
                    }
                    None if whole => {
                        let suggestion = names::suggest(&token.name, self.known_names())
                            .map(|s| format!(" (did you mean @{s}?)"))
                            .unwrap_or_default();
                        bail!(
                            "unknown token {}{suggestion}; write @@ for a literal @",
                            token.raw
                        );
                    }
                    None => {
                        value.push_str(token.raw);
                        display.push_str(token.raw);
                    }
                },
            }
        }
        Ok(ExpandedArg { value, display })
    }

    fn known_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = CONTEXT_TOKENS.to_vec();
        names.extend(self.chainz.config().signers.keys().map(String::as_str));
        names.extend(self.labels().names());
        names
    }

    fn labels(&self) -> &LabelIndex {
        self.labels
            .get_or_init(|| LabelIndex::new(self.chainz.config()))
    }

    /// Resolve `name` (on the qualified chain if given). `Ok(None)` means
    /// unknown name; errors are known names that cannot be produced.
    fn token(&self, name: &str, chain: Option<&str>) -> Result<Option<Value>> {
        if name == "key" {
            bail!("@key was removed in v2: pass --expose-key and read $RAW_PRIVATE_KEY instead");
        }
        let canonical = match DEPRECATED_TOKENS.iter().find(|(old, _)| *old == name) {
            Some((old, new)) => {
                self.warnings
                    .borrow_mut()
                    .push(format!("@{old} is deprecated; use @{new}"));
                *new
            }
            None => name,
        };
        let is_context = CONTEXT_TOKENS.contains(&canonical);
        let is_signer = self.chainz.config().signers.contains_key(canonical);
        if !is_context && !is_signer && !self.labels().contains(canonical) {
            return Ok(None);
        }
        match chain {
            None => self.resolve_in(self.ctx, canonical).map(Some),
            Some(chain) => {
                let other = self.other(chain)?;
                let others = self.others.borrow();
                self.resolve_in(&others[&other], canonical).map(Some)
            }
        }
    }

    fn other(&self, query: &str) -> Result<String> {
        let target = resolve::resolve(self.chainz.config(), Some(self.runtime), query)
            .with_context(|| format!("in qualifier ':{query}'"))?;
        let key = target.name().to_string();
        if !self.others.borrow().contains_key(&key) {
            let opts = ExecOptions {
                direct: self.direct,
                ..ExecOptions::default()
            };
            let ctx = build(self.chainz, self.runtime, &target, &opts)?;
            self.others.borrow_mut().insert(key.clone(), ctx);
        }
        Ok(key)
    }

    fn resolve_in(&self, ctx: &Context, name: &str) -> Result<Value> {
        Ok(match name {
            "rpc" => Value {
                value: ctx.rpc.url.clone(),
                display: ctx.rpc.display.clone(),
            },
            "chainid" => Value::public(ctx.chain_id.to_string()),
            "chain" => Value::public(ctx.chain.clone()),
            "wallet" => {
                let signer = ctx.signer.as_ref().ok_or_else(|| {
                    anyhow!(
                        "chain '{}' has no signer: pass --signer or set `signer` on the chain",
                        ctx.chain
                    )
                })?;
                let address = signer.address.ok_or_else(|| {
                    anyhow!(
                        "signer '{}' has no cached address; re-add it with `chainz signer add`",
                        signer.name
                    )
                })?;
                Value::public(address.to_string())
            }
            "explorer" => {
                let explorer = ctx
                    .explorer
                    .as_ref()
                    .ok_or_else(|| anyhow!("chain '{}' has no explorer", ctx.chain))?;
                Value {
                    value: explorer.api.clone(),
                    display: redact(&explorer.api),
                }
            }
            "explorer_key" => {
                let key = ctx
                    .explorer
                    .as_ref()
                    .and_then(|e| e.key.clone())
                    .ok_or_else(|| anyhow!("chain '{}' has no explorer key", ctx.chain))?;
                Value {
                    value: key,
                    display: "[REDACTED]".to_string(),
                }
            }
            other => {
                if let Some(signer) = self.chainz.config().signers.get(other) {
                    let address = signer
                        .address
                        .ok_or_else(|| anyhow!("signer '{other}' has no cached address"))?;
                    return Ok(Value::public(address.to_string()));
                }
                match self.labels().resolve(other, ctx.chain_id) {
                    Ok(label) => Value::public(label.address.to_string()),
                    Err(LabelError::NotOnChain { name, .. }) => {
                        bail!(
                            "@{name} has no address on chain '{}' ({})",
                            ctx.chain,
                            ctx.chain_id
                        )
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
