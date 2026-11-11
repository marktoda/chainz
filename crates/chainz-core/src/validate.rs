//! `Config` invariants: the single definition, run before every write and
//! on every load. Messages name chains, endpoints, headers and variables
//! but never echo a URL, header value or secret.

use crate::address::Address;
use crate::model::{Backend, CONFIG_VERSION, Config};
use crate::names;
use crate::secrets::{BUILTIN_VARS, Secrets};
use crate::template;
use std::collections::HashMap;

/// Every invariant violation in `config` (and `secrets`, when given).
pub fn issues(config: &Config, secrets: Option<&Secrets>) -> Vec<String> {
    let mut out = Vec::new();
    let mut err = |msg: String| out.push(msg);

    if config.version != CONFIG_VERSION {
        err(format!(
            "config version is {}, expected {CONFIG_VERSION}",
            config.version
        ));
    }

    for (name, explorer) in &config.explorers {
        if !names::is_valid_endpoint_name(name) {
            err(format!(
                "explorer name '{name}' must match [a-z0-9][a-z0-9_-]*"
            ));
        }
        if explorer.api.trim().is_empty() {
            err(format!("explorer '{name}' has an empty api URL"));
        }
        for value in std::iter::once(&explorer.api).chain(explorer.key.iter()) {
            if let Err(e) = template::references(value) {
                err(format!("explorer '{name}': {e}"));
            }
        }
    }

    let mut taken: HashMap<String, String> = HashMap::new();
    let mut ids: HashMap<u64, &str> = HashMap::new();
    for (name, chain) in &config.chains {
        if !names::is_valid_chain_name(name) {
            err(format!(
                "chain name '{name}' must match [a-z0-9][a-z0-9_-]* and not be all digits"
            ));
        }
        if let Some(other) = ids.insert(chain.id, name) {
            err(format!(
                "chain ID {} is used by both '{other}' and '{name}'",
                chain.id
            ));
        }
        for n in std::iter::once(name.as_str()).chain(chain.aliases.iter().map(String::as_str)) {
            if n.trim().is_empty() || n.chars().any(char::is_control) {
                err(format!("chain '{name}' has an empty or invalid alias"));
                continue;
            }
            if n.parse::<u64>().is_ok() {
                err(format!(
                    "chain '{name}' alias '{n}' is a number; numbers mean chain IDs"
                ));
            }
            if let Some(other) = taken.insert(n.to_ascii_lowercase(), name.clone())
                && other != *name
            {
                err(format!(
                    "name or alias '{n}' is shared by '{other}' and '{name}'"
                ));
            }
        }
        if chain.rpcs.is_empty() {
            err(format!("chain '{name}' has no endpoints"));
        }
        if !chain.rpcs.contains_key(&chain.rpc) {
            err(format!(
                "chain '{name}' active endpoint '{}' is not in its rpcs",
                chain.rpc
            ));
        }
        for (i, f) in chain.failover.iter().enumerate() {
            match chain.rpcs.get(f) {
                None => err(format!(
                    "chain '{name}' failover endpoint '{f}' is not in its rpcs"
                )),
                Some(e) if e.is_websocket() => err(format!(
                    "chain '{name}' failover endpoint '{f}' is a websocket; the gateway routes HTTP only"
                )),
                Some(_) => {}
            }
            if *f == chain.rpc {
                err(format!(
                    "chain '{name}' lists its active endpoint '{f}' as failover"
                ));
            }
            if chain.failover[..i].contains(f) {
                err(format!(
                    "chain '{name}' lists failover endpoint '{f}' twice"
                ));
            }
        }
        for (ep_name, endpoint) in &chain.rpcs {
            if !names::is_valid_endpoint_name(ep_name) {
                err(format!(
                    "chain '{name}' endpoint name '{ep_name}' must match [a-z0-9][a-z0-9_-]*"
                ));
            }
            if endpoint.url.trim().is_empty() || endpoint.url.chars().any(char::is_whitespace) {
                err(format!(
                    "chain '{name}' endpoint '{ep_name}' has an empty or malformed URL"
                ));
            } else if let Err(e) = template::references(&endpoint.url) {
                err(format!("chain '{name}' endpoint '{ep_name}': {e}"));
            }
            for (header, value) in &endpoint.headers {
                if header.is_empty() || !header.bytes().all(is_header_name_byte) {
                    err(format!(
                        "chain '{name}' endpoint '{ep_name}' has an invalid header name '{header}'"
                    ));
                }
                if value.contains(['\r', '\n']) {
                    err(format!(
                        "chain '{name}' endpoint '{ep_name}' header '{header}' contains a line break"
                    ));
                }
                if value.contains(',') && !template::has_templates(value) {
                    err(format!(
                        "chain '{name}' endpoint '{ep_name}' header '{header}' contains a comma, \
                         which cannot be exported via ETH_RPC_HEADERS"
                    ));
                }
                if let Err(e) = template::references(value) {
                    err(format!(
                        "chain '{name}' endpoint '{ep_name}' header '{header}': {e}"
                    ));
                }
            }
        }
        if let Some(signer) = &chain.signer
            && !config.signers.contains_key(signer)
        {
            err(format!(
                "chain '{name}' references missing signer '{signer}'"
            ));
        }
        if let Some(explorer) = &chain.explorer
            && !config.explorers.contains_key(explorer)
        {
            err(format!(
                "chain '{name}' references missing explorer '{explorer}'"
            ));
        }
    }

    if let Some(default) = &config.default
        && !config.chains.contains_key(default)
    {
        err(format!("default chain '{default}' is not configured"));
    }

    for (name, signer) in &config.signers {
        if !names::is_valid_label_name(name) || name.bytes().any(|b| b.is_ascii_uppercase()) {
            err(format!("signer name '{name}' must match [a-z0-9_-]+"));
        }
        if names::CONTEXT_TOKENS.contains(&name.as_str()) || name == "key" {
            err(format!(
                "signer name '{name}' is reserved for a context token"
            ));
        }
        let has = |f: &Option<String>| f.as_deref().is_some_and(|v| !v.trim().is_empty());
        let fields_ok = match signer.backend {
            Backend::Keyring => {
                signer.path.is_none() && signer.vault.is_none() && signer.item.is_none()
            }
            Backend::Keystore => {
                has(&signer.path) && signer.vault.is_none() && signer.service.is_none()
            }
            Backend::OnePassword => {
                has(&signer.vault) && has(&signer.item) && signer.path.is_none()
            }
            Backend::Encrypted | Backend::Plaintext => {
                signer.path.is_none() && signer.vault.is_none() && signer.service.is_none()
            }
        };
        if !fields_ok {
            err(format!(
                "signer '{name}' has fields that do not fit backend '{}'",
                signer.backend
            ));
        }
        if let Some(secrets) = secrets {
            let secret = secrets.signers.get(name);
            match signer.backend {
                Backend::Plaintext => match secret.and_then(|s| s.private_key.as_deref()) {
                    None => err(format!(
                        "signer '{name}' (plaintext) has no key in secrets.toml"
                    )),
                    Some(key) => match Address::from_private_key(key) {
                        Err(_) => err(format!("signer '{name}' has an invalid private key")),
                        Ok(derived) if signer.address.is_some_and(|a| a != derived) => err(
                            format!("signer '{name}' cached address does not match its key"),
                        ),
                        Ok(_) => {}
                    },
                },
                Backend::Encrypted => {
                    if let Err(e) = secret
                        .map(crate::signer::validate_envelope)
                        .unwrap_or_else(|| Err(anyhow::anyhow!("no encrypted key in secrets.toml")))
                    {
                        err(format!("signer '{name}' (encrypted): {e}"));
                    }
                }
                _ => {}
            }
        }
    }

    for (label, entries) in &config.addresses {
        if !names::is_valid_label_name(label) || label.bytes().any(|b| b.is_ascii_uppercase()) {
            err(format!("label name '{label}' must match [a-z0-9_-]+"));
        }
        if names::CONTEXT_TOKENS.contains(&label.as_str())
            || names::DEPRECATED_TOKENS.iter().any(|(n, _)| n == label)
            || label == "key"
        {
            err(format!(
                "label '{label}' uses a reserved context-token name"
            ));
        }
        if config.signers.contains_key(label) {
            err(format!("label '{label}' collides with a signer name"));
        }
        for (scope, address) in entries {
            if scope != "*" && !config.chains.contains_key(scope) {
                err(format!("label '{label}' names unknown chain '{scope}'"));
            }
            if address.parse::<Address>().is_err() {
                err(format!(
                    "label '{label}' on '{scope}' has an invalid address"
                ));
            }
        }
    }

    if let Some(secrets) = secrets {
        for name in secrets.vars.keys() {
            if !template::is_valid_name(name) {
                err(format!(
                    "variable name '{name}' must match [A-Za-z_][A-Za-z0-9_]*"
                ));
            }
            if BUILTIN_VARS.contains(&name.as_str()) {
                err(format!("variable name '{name}' is reserved for a built-in"));
            }
        }
        for name in secrets.signers.keys() {
            match config.signers.get(name) {
                Some(s) if matches!(s.backend, Backend::Plaintext | Backend::Encrypted) => {}
                _ => {} // orphaned key material is reported by doctor, not fatal
            }
        }
    }
    out
}

/// `Ok` when `config` satisfies every invariant, else the first violation.
pub fn validate(config: &Config, secrets: Option<&Secrets>) -> anyhow::Result<()> {
    match issues(config, secrets).into_iter().next() {
        None => Ok(()),
        Some(first) => Err(anyhow::anyhow!(first)),
    }
}

/// RFC 9110 token characters, the legal alphabet for HTTP header names.
pub fn is_header_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        toml_edit::de::from_str(text).unwrap()
    }

    const OK: &str = r#"
version = 2
[chains.eth]
id = 1
aliases = ["Ethereum Mainnet"]
rpc = "a"
failover = ["b"]
rpcs = { a = "https://a.example", b = "https://b.example", c = "wss://c.example" }
"#;

    #[test]
    fn valid_config_passes() {
        assert!(
            issues(&parse(OK), None).is_empty(),
            "{:?}",
            issues(&parse(OK), None)
        );
    }

    #[test]
    fn catches_reference_and_uniqueness_errors() {
        let bad = parse(&format!(
            "{OK}\n[chains.mainnet]\nid = 1\nrpc = \"x\"\nsigner = \"ghost\"\nrpcs = {{ y = \"https://y\" }}\n"
        ));
        let all = issues(&bad, None).join("\n");
        assert!(all.contains("chain ID 1 is used by both"), "{all}");
        assert!(all.contains("active endpoint 'x'"), "{all}");
        assert!(all.contains("missing signer 'ghost'"), "{all}");
    }

    #[test]
    fn failover_must_be_http_and_distinct() {
        let bad = parse(&OK.replace("failover = [\"b\"]", "failover = [\"c\", \"a\", \"zz\"]"));
        let all = issues(&bad, None).join("\n");
        assert!(all.contains("websocket"), "{all}");
        assert!(all.contains("active endpoint 'a' as failover"), "{all}");
        assert!(all.contains("'zz' is not in its rpcs"), "{all}");
    }

    #[test]
    fn header_errors_never_echo_values() {
        let bad = parse(
            "version = 2\n[chains.eth]\nid = 1\nrpc = \"a\"\n[chains.eth.rpcs]\na = { url = \"https://a\", headers = { \"bad name\" = \"x\", ok = \"sec,ret\" } }\n",
        );
        let all = issues(&bad, None).join("\n");
        assert!(all.contains("invalid header name 'bad name'"), "{all}");
        assert!(all.contains("comma"), "{all}");
        assert!(!all.contains("sec,ret"), "{all}");
    }

    #[test]
    fn labels_cannot_shadow_context_tokens_or_signers() {
        let bad = parse(&format!(
            "{OK}\n[signers.funder]\nbackend = \"keyring\"\n[addresses.rpc]\neth = \"0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf\"\n[addresses.funder]\n\"*\" = \"0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf\"\n[addresses.x]\nnowhere = \"0x1\"\n"
        ));
        let all = issues(&bad, None).join("\n");
        assert!(all.contains("label 'rpc' uses a reserved"), "{all}");
        assert!(all.contains("collides with a signer"), "{all}");
        assert!(all.contains("unknown chain 'nowhere'"), "{all}");
        assert!(all.contains("invalid address"), "{all}");
    }
}
