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
use crate::model::{Backend, Chain, Config};
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
use std::rc::Rc;

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
    /// Why the configured key could not be expanded (an unset variable,
    /// say). The key is then not exported, and only `@explorer_key` fails.
    pub key_error: Option<String>,
    /// Etherscan family (forge handles it natively).
    pub etherscan: bool,
}

impl fmt::Debug for ExplorerContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExplorerContext")
            .field("name", &self.name)
            .field("api", &redact(&self.api))
            .field("key", &self.key.as_ref().map(|_| "[REDACTED]"))
            .field("key_error", &self.key_error)
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
        write!(f, "{}={}", self.name, self.display())
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

/// The configured chain behind `target`: its config key (the parent, for a
/// fork), the chain, and the fork record if any.
fn chain_of<'c>(
    config: &'c Config,
    target: &ChainRef,
) -> Result<(String, &'c Chain, Option<ForkRecord>)> {
    let (chain_name, fork) = match target {
        ChainRef::Configured(name) => (name.clone(), None),
        ChainRef::Fork(fork) => (fork.parent.clone(), Some(fork.clone())),
    };
    let chain = config
        .chain(&chain_name)
        .ok_or_else(|| anyhow!("chain '{chain_name}' is not configured"))?;
    Ok((chain_name, chain, fork))
}

/// Build the context for a resolved chain reference.
pub fn build(
    chainz: &Chainz,
    runtime: &Runtime,
    target: &ChainRef,
    opts: &ExecOptions,
) -> Result<Context> {
    let (chain_name, chain, fork) = chain_of(chainz.config(), target)?;
    let rpc = rpc_context(chainz, runtime, &chain_name, chain, fork.as_ref(), opts)?;
    let signer = signer_context(
        chainz.config(),
        opts.signer.as_deref().or(chain.signer.as_deref()),
    )?;
    let explorer = match fork {
        None => explorer_context(chainz, &chain_name, chain)?,
        Some(_) => None,
    };
    Ok(Context {
        chain: target.name().to_string(),
        chain_id: fork.as_ref().map_or(chain.id, |f| f.chain_id),
        fork,
        rpc,
        signer,
        explorer,
    })
}

/// The RPC: a fork's anvil, the gateway, or the endpoint expanded.
fn rpc_context(
    chainz: &Chainz,
    runtime: &Runtime,
    chain_name: &str,
    chain: &Chain,
    fork: Option<&ForkRecord>,
    opts: &ExecOptions,
) -> Result<RpcContext> {
    if let Some(fork) = fork {
        if opts.rpc.is_some() {
            bail!("--rpc does not apply to a fork; it always uses its local anvil");
        }
        let url = fork.rpc_url();
        return Ok(RpcContext {
            display: url.clone(),
            url,
            headers: vec![],
            endpoint: None,
            via_gateway: false,
        });
    }
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
        runtime.gateway_url(chain_name, opts.rpc.as_deref())
    };
    Ok(match gateway {
        Some(url) => RpcContext {
            display: redact(&url),
            url,
            headers: vec![],
            endpoint: Some(endpoint_name),
            via_gateway: true,
        },
        None => {
            let expanded =
                chainz.expand_endpoint(chain_name, chain.id, &endpoint_name, endpoint)?;
            RpcContext {
                display: redact(&endpoint.url),
                url: expanded.url,
                headers: expanded.headers,
                endpoint: Some(endpoint_name),
                via_gateway: false,
            }
        }
    })
}

/// The signer named `name`, if any.
fn signer_context(config: &Config, name: Option<&str>) -> Result<Option<SignerContext>> {
    let Some(name) = name else { return Ok(None) };
    let signer = config
        .signers
        .get(name)
        .ok_or_else(|| anyhow!("signer '{name}' not found"))?;
    Ok(Some(SignerContext {
        keystore: (signer.backend == Backend::Keystore)
            .then(|| signer.path.as_deref().map(crate::paths::expand_tilde))
            .flatten(),
        name: name.to_string(),
        backend: signer.backend,
        address: signer.address,
    }))
}

/// The chain's explorer, if it names one. A key that does not expand is
/// recorded in [`ExplorerContext::key_error`] rather than failing the
/// whole context: most commands never use it.
fn explorer_context(
    chainz: &Chainz,
    chain_name: &str,
    chain: &Chain,
) -> Result<Option<ExplorerContext>> {
    let Some(name) = &chain.explorer else {
        return Ok(None);
    };
    let explorer = chainz
        .config()
        .explorers
        .get(name)
        .ok_or_else(|| anyhow!("explorer '{name}' not found"))?;
    let secrets_loader = || chainz.secrets();
    let vars = VarResolver::new(
        Builtins {
            chain_id: Some(chain.id),
            chain: Some(chain_name),
        },
        &secrets_loader,
    );
    let api = vars
        .expand(&explorer.api)
        .with_context(|| format!("explorer '{name}' api"))?
        .into_owned();
    let (key, key_error) = match explorer.key.as_deref().map(|k| vars.expand(k)) {
        None => (None, None),
        Some(Ok(key)) => (Some(key.into_owned()), None),
        Some(Err(e)) => (None, Some(format!("explorer '{name}' key: {e:#}"))),
    };
    Ok(Some(ExplorerContext {
        name: name.clone(),
        api,
        key,
        key_error,
        etherscan: explorer.is_etherscan(),
    }))
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
            if let Some(error) = &explorer.key_error {
                eprintln!("chainz: warning: not exporting the explorer key: {error}");
            }
            if let Some(key) = &explorer.key {
                push("ETHERSCAN_API_KEY", key.clone(), true);
            }
            if !explorer.etherscan {
                // The API URL may carry a key (`?apikey=`).
                push("VERIFIER_URL", explorer.api.clone(), true);
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
    others: RefCell<HashMap<String, Rc<Other>>>,
    warnings: RefCell<Vec<String>>,
}

/// Another chain, named by a `:chain` qualifier. Only what its tokens need
/// is built: ID, name and signer up front; the RPC and explorer on first
/// use by `@rpc:x`, `@explorer:x` or `@explorer_key:x`, so `@myrouter:x`
/// never expands (or fails on) that chain's endpoint secrets.
struct Other {
    target: ChainRef,
    chain: String,
    chain_id: u64,
    signer: Option<SignerContext>,
    rpc: OnceCell<RpcContext>,
    explorer: OnceCell<Option<ExplorerContext>>,
}

/// What a token resolves against.
#[derive(Clone, Copy)]
enum Scope<'s> {
    /// The exec context itself.
    Own(&'s Context),
    /// A qualified chain.
    Other(&'s Other),
}

impl<'s> Scope<'s> {
    fn chain(self) -> &'s str {
        match self {
            Scope::Own(ctx) => &ctx.chain,
            Scope::Other(other) => &other.chain,
        }
    }

    fn chain_id(self) -> u64 {
        match self {
            Scope::Own(ctx) => ctx.chain_id,
            Scope::Other(other) => other.chain_id,
        }
    }

    fn signer(self) -> Option<&'s SignerContext> {
        match self {
            Scope::Own(ctx) => ctx.signer.as_ref(),
            Scope::Other(other) => other.signer.as_ref(),
        }
    }
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

    /// Deprecation warnings collected during expansion, each once, in the
    /// order first seen.
    pub fn warnings(&self) -> Vec<String> {
        self.warnings.borrow().clone()
    }

    fn warn(&self, warning: String) {
        let mut warnings = self.warnings.borrow_mut();
        if !warnings.contains(&warning) {
            warnings.push(warning);
        }
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
        let pieces = grammar::tokenize(arg);
        let whole = grammar::sole_token(&pieces, arg).is_some();
        let mut value = String::with_capacity(arg.len());
        let mut display = String::with_capacity(arg.len());
        for piece in &pieces {
            match piece {
                Piece::Text(text) => {
                    value.push_str(text);
                    display.push_str(text);
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
                self.warn(format!("@{old} is deprecated; use @{new}"));
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
            None => self.resolve_in(Scope::Own(self.ctx), canonical).map(Some),
            Some(chain) => {
                let other = self.other(chain)?;
                self.resolve_in(Scope::Other(&other), canonical).map(Some)
            }
        }
    }

    /// The light context of the chain `query` names, built once.
    fn other(&self, query: &str) -> Result<Rc<Other>> {
        let target = resolve::resolve(self.chainz.config(), Some(self.runtime), query)
            .with_context(|| format!("in qualifier ':{query}'"))?;
        let key = target.name().to_string();
        if let Some(other) = self.others.borrow().get(&key) {
            return Ok(Rc::clone(other));
        }
        let config = self.chainz.config();
        let (_, chain, fork) = chain_of(config, &target)?;
        let other = Rc::new(Other {
            chain: key.clone(),
            chain_id: fork.as_ref().map_or(chain.id, |f| f.chain_id),
            signer: signer_context(config, chain.signer.as_deref())?,
            rpc: OnceCell::new(),
            explorer: OnceCell::new(),
            target,
        });
        self.others.borrow_mut().insert(key, Rc::clone(&other));
        Ok(other)
    }

    fn rpc_of<'s>(&self, scope: Scope<'s>) -> Result<&'s RpcContext> {
        let other = match scope {
            Scope::Own(ctx) => return Ok(&ctx.rpc),
            Scope::Other(other) => other,
        };
        if other.rpc.get().is_none() {
            let (chain_name, chain, fork) = chain_of(self.chainz.config(), &other.target)?;
            let opts = ExecOptions {
                direct: self.direct,
                ..ExecOptions::default()
            };
            let rpc = rpc_context(
                self.chainz,
                self.runtime,
                &chain_name,
                chain,
                fork.as_ref(),
                &opts,
            )?;
            let _ = other.rpc.set(rpc);
        }
        Ok(other.rpc.get().expect("set above"))
    }

    fn explorer_of<'s>(&self, scope: Scope<'s>) -> Result<Option<&'s ExplorerContext>> {
        let other = match scope {
            Scope::Own(ctx) => return Ok(ctx.explorer.as_ref()),
            Scope::Other(other) => other,
        };
        if other.explorer.get().is_none() {
            let explorer = match &other.target {
                ChainRef::Fork(_) => None,
                ChainRef::Configured(_) => {
                    let (chain_name, chain, _) = chain_of(self.chainz.config(), &other.target)?;
                    explorer_context(self.chainz, &chain_name, chain)?
                }
            };
            let _ = other.explorer.set(explorer);
        }
        Ok(other.explorer.get().expect("set above").as_ref())
    }

    fn resolve_in(&self, scope: Scope<'_>, name: &str) -> Result<Value> {
        let chain = scope.chain();
        Ok(match name {
            "rpc" => {
                let rpc = self.rpc_of(scope)?;
                Value {
                    value: rpc.url.clone(),
                    display: rpc.display.clone(),
                }
            }
            "chainid" => Value::public(scope.chain_id().to_string()),
            "chain" => Value::public(chain.to_string()),
            "wallet" => {
                let signer = scope.signer().ok_or_else(|| {
                    anyhow!(
                        "chain '{chain}' has no signer: pass --signer or set `signer` on the chain"
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
                let explorer = self
                    .explorer_of(scope)?
                    .ok_or_else(|| anyhow!("chain '{chain}' has no explorer"))?;
                Value {
                    value: explorer.api.clone(),
                    display: redact(&explorer.api),
                }
            }
            "explorer_key" => {
                let explorer = self.explorer_of(scope)?;
                let key = match explorer {
                    Some(ExplorerContext { key: Some(key), .. }) => key.clone(),
                    Some(ExplorerContext {
                        key_error: Some(error),
                        ..
                    }) => bail!("{error}"),
                    _ => bail!("chain '{chain}' has no explorer key"),
                };
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
                let chain_id = scope.chain_id();
                match self.labels().resolve(other, chain_id) {
                    Ok(label) => Value::public(label.address.to_string()),
                    Err(LabelError::NotOnChain { name, .. }) => {
                        bail!("@{name} has no address on chain '{chain}' ({chain_id})")
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
