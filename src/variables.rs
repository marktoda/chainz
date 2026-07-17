use crate::{chain::ChainInstance, config::Chainz, opt::VarCommand};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::io::{IsTerminal, Read};
use zeroize::Zeroize;

#[derive(Default, Serialize, Deserialize)]
pub struct GlobalVariables {
    /// INFURA_API_KEY etc
    #[serde(flatten)]
    rpc_expansions: HashMap<String, String>,
}

impl fmt::Debug for GlobalVariables {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<_> = self.rpc_expansions.keys().collect();
        names.sort();
        f.debug_struct("GlobalVariables")
            .field("names", &names)
            .finish()
    }
}

pub struct ChainVariables {
    env: HashMap<String, String>,
    expansions: HashMap<String, String>,
}

impl ChainVariables {
    pub fn new(chain: &ChainInstance, command: &[String], expose_key: bool) -> Result<Self> {
        let needs_key_arg = command.iter().any(|arg| arg.contains("@key"));
        let needs_wallet = command.iter().any(|arg| arg.contains("@wallet"));

        let mut env = HashMap::new();
        let mut expansions = HashMap::new();

        // Always set non-key variables
        let basic_vars = [
            ("ETH_RPC_URL", "@rpc", chain.rpc_url.clone()),
            (
                "CHAIN_ID",
                "@chainid",
                chain.definition.chain_id.to_string(),
            ),
            ("CHAIN_NAME", "@chainname", chain.definition.name.clone()),
            (
                "VERIFIER_URL",
                "@verification_url",
                chain
                    .definition
                    .verification_url
                    .clone()
                    .unwrap_or("UNDEFINED".to_string()),
            ),
            (
                "VERIFIER_API_KEY",
                "@verifier_api_key",
                chain
                    .definition
                    .verification_api_key
                    .clone()
                    .unwrap_or("UNDEFINED".to_string()),
            ),
        ];

        for (env_var, expansion, val) in &basic_vars {
            env.insert(env_var.to_string(), val.clone());
            expansions.insert(expansion.to_string(), val.clone());
        }

        // Only resolve the private key when the command explicitly needs it.
        // New safe-storage records cache the public address, so @wallet does
        // not need to unlock the backing credential store. Legacy records
        // without that metadata derive it once at execution time.
        // Note: private_key() returns Zeroizing<String>, but we convert to String here
        // because std::process::Command::envs() requires String values and doesn't zeroize
        // internally. The child process also holds the key in its environment unzeroed.
        // The real security boundary is the lazy check above — encrypted keys are never
        // decrypted unless the command explicitly needs them.
        let cached_address = needs_wallet
            .then(|| chain.key.address_noninteractive())
            .flatten();
        let must_resolve_key =
            needs_key_arg || expose_key || (needs_wallet && cached_address.is_none());

        if must_resolve_key || cached_address.is_some() {
            let private_key = must_resolve_key
                .then(|| chain.key.private_key())
                .transpose()?;
            if needs_wallet {
                let address = match cached_address {
                    Some(address) => address,
                    None => crate::key::Key::address_from_private_key(
                        private_key
                            .as_deref()
                            .expect("private key is resolved for legacy wallet metadata"),
                    )?
                    .to_string(),
                };
                env.insert("WALLET_ADDRESS".to_string(), address.clone());
                expansions.insert("@wallet".to_string(), address);
            }
            if needs_key_arg || expose_key {
                env.insert(
                    "RAW_PRIVATE_KEY".to_string(),
                    private_key
                        .as_deref()
                        .expect("private key is resolved when explicitly requested")
                        .to_string(),
                );
            }
            if needs_key_arg {
                eprintln!(
                    "Warning: @key expands the private key into process arguments; prefer --expose-key with $RAW_PRIVATE_KEY"
                );
                expansions.insert(
                    "@key".to_string(),
                    private_key
                        .as_deref()
                        .expect("private key is resolved for @key")
                        .to_string(),
                );
            }
        }

        Ok(Self { env, expansions })
    }

    pub fn as_map(&self) -> &HashMap<String, String> {
        &self.env
    }

    pub fn expand(&self, input: Vec<String>) -> Vec<String> {
        input
            .into_iter()
            .map(|arg| {
                let mut result = arg;
                for (key, value) in &self.expansions {
                    result = result.replace(key, value);
                }
                result
            })
            .collect()
    }
}

impl GlobalVariables {
    pub fn expand_rpc_url(&self, rpc_url: &str) -> String {
        interpolate_variables(rpc_url, &self.rpc_expansions)
    }

    pub fn add_rpc_expansion(&mut self, key: &str, value: &str) {
        self.rpc_expansions
            .insert(key.to_string(), value.to_string());
    }

    pub(crate) fn validate(&self) -> Result<()> {
        for key in self.rpc_expansions.keys() {
            if key.is_empty() || key.contains(['{', '}']) {
                anyhow::bail!("Invalid variable name '{}'", key);
            }
        }
        Ok(())
    }

    pub fn remove_rpc_expansion(&mut self, key: &str) -> Option<String> {
        self.rpc_expansions.remove(key)
    }

    pub fn get_rpc_expansion(&self, key: &str) -> Option<String> {
        self.rpc_expansions.get(key).cloned()
    }

    pub fn list_rpc_expansions(&self) -> &HashMap<String, String> {
        &self.rpc_expansions
    }
}

impl VarCommand {
    pub async fn handle(self, chainz: &mut Chainz) -> Result<()> {
        match self {
            VarCommand::Set { name, value, stdin } => {
                if stdin && value.is_some() {
                    anyhow::bail!("Provide a value or --stdin, not both");
                }
                let value = if stdin {
                    read_value_from_stdin()?
                } else if let Some(value) = value {
                    eprintln!(
                        "Warning: variable values in argv may be visible in shell history; prefer --stdin"
                    );
                    value
                } else if std::io::stdin().is_terminal() {
                    rpassword::prompt_password(format!("Value for {}: ", name))?
                } else {
                    anyhow::bail!("No value provided; use --stdin for scripts")
                };
                chainz.config.globals.add_rpc_expansion(&name, &value);
                chainz.save().await?;
                println!("Set variable {}", name);
            }
            VarCommand::Get { name, show } => {
                match chainz.config.globals.get_rpc_expansion(&name) {
                    Some(value) if show => println!("{} = {}", name, value),
                    Some(_) => println!("{} = [REDACTED]", name),
                    None => println!("Variable '{}' not found", name),
                }
            }
            VarCommand::List { show } => {
                let vars = chainz.config.globals.list_rpc_expansions();
                if vars.is_empty() {
                    println!("No variables set");
                } else {
                    println!("Variables:");
                    let mut entries: Vec<_> = vars.iter().collect();
                    entries.sort_by_key(|(name, _)| *name);
                    for (name, value) in entries {
                        println!(
                            "  {} = {}",
                            name,
                            if show { value.as_str() } else { "[REDACTED]" }
                        );
                    }
                }
            }
            VarCommand::Rm { name } => {
                chainz.config.globals.remove_rpc_expansion(&name);
                chainz.save().await?;
                println!("Removed variable '{}'", name);
            }
        }
        Ok(())
    }
}

fn read_value_from_stdin() -> Result<String> {
    let mut value = String::new();
    std::io::stdin().read_to_string(&mut value)?;
    let normalized = value.trim_end_matches(['\r', '\n']).to_string();
    value.zeroize();
    if normalized.is_empty() {
        anyhow::bail!("Value from stdin was empty");
    }
    Ok(normalized)
}

/// Redact credential-bearing URL shapes for display and JSON output.
pub fn redact_url(input: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(input) else {
        return "[REDACTED URL]".to_string();
    };
    if !url.username().is_empty() {
        let _ = url.set_username("REDACTED");
    }
    if url.password().is_some() {
        let _ = url.set_password(Some("REDACTED"));
    }
    if url.query().is_some() {
        let names: Vec<String> = url
            .query_pairs()
            .map(|(name, _)| name.into_owned())
            .collect();
        url.set_query(None);
        for name in names {
            url.query_pairs_mut().append_pair(&name, "REDACTED");
        }
    }
    if url.path() != "/" && !url.path().is_empty() {
        let readable_path = url.path().replace("%7B", "{").replace("%7D", "}");
        let templates = variable_templates(&readable_path);
        let redacted_path = if templates.is_empty() {
            "/REDACTED".to_string()
        } else {
            format!("/REDACTED/{}", templates.join("/"))
        };
        url.set_path(&redacted_path);
    }
    if url.fragment().is_some() {
        url.set_fragment(Some("REDACTED"));
    }
    // url::Url percent-encodes braces, but variable names are safe public
    // metadata and keeping ${NAME} intact makes redacted output actionable.
    url.to_string().replace("%7B", "{").replace("%7D", "}")
}

fn variable_templates(input: &str) -> Vec<String> {
    let mut templates = Vec::new();
    let mut remainder = input;
    while let Some((start, end)) = find_next_var(remainder) {
        templates.push(remainder[start..end].to_string());
        remainder = &remainder[end..];
    }
    templates
}

fn interpolate_variables(input: &str, variables: &HashMap<String, String>) -> String {
    let mut result = input.to_string();

    // First replace from config variables
    for (key, value) in variables {
        let pattern = format!("${{{}}}", key);
        result = result.replace(&pattern, value);
    }

    // Then try to replace any remaining ${VAR} patterns with environment variables
    let mut final_result = String::new();
    let mut last_end = 0;

    while let Some((start, end)) = find_next_var(&result[last_end..]) {
        let absolute_start = last_end + start;
        let absolute_end = last_end + end;

        // Add the part before the variable
        final_result.push_str(&result[last_end..absolute_start]);

        // Get the variable name
        let var_name = &result[absolute_start + 2..absolute_end - 1]; // strip ${ and }

        // Try to get the environment variable
        if let Ok(value) = std::env::var(var_name) {
            final_result.push_str(&value);
        } else {
            // If not found, keep the original ${VAR} syntax
            final_result.push_str(&result[absolute_start..absolute_end]);
        }

        last_end = absolute_end;
    }

    // Add any remaining part of the string
    final_result.push_str(&result[last_end..]);

    if final_result.is_empty() {
        result
    } else {
        final_result
    }
}

fn find_next_var(input: &str) -> Option<(usize, usize)> {
    let start = input.find("${")?;
    let end = input[start..].find("}")?.checked_add(start + 1)?;
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::Once;

    static INIT: Once = Once::new();

    /// Setup function that is only run once, even if called multiple times.
    fn setup() {
        INIT.call_once(|| {
            // SAFETY: set once before any test reads them; values never change after.
            unsafe {
                env::set_var("TEST_ENV_KEY", "env_key");
                env::set_var("TEST_OTHER_KEY", "other_value");
            }
        });
    }

    #[test]
    fn test_config_variables() {
        let mut globals = GlobalVariables::default();
        globals.add_rpc_expansion("API_KEY", "config_key");
        globals.add_rpc_expansion("EMPTY", "");

        assert_eq!(
            globals.expand_rpc_url("https://api.example.com/${API_KEY}/v1"),
            "https://api.example.com/config_key/v1"
        );

        assert_eq!(globals.expand_rpc_url("empty:${EMPTY}:end"), "empty::end");
    }

    #[test]
    fn test_environment_variables() {
        setup();
        let globals = GlobalVariables::default();

        assert_eq!(
            globals.expand_rpc_url("https://api.example.com/${TEST_ENV_KEY}/v1"),
            "https://api.example.com/env_key/v1"
        );
    }

    #[test]
    fn test_multiple_replacements() {
        setup();
        let mut globals = GlobalVariables::default();
        globals.add_rpc_expansion("API_KEY", "config_key");

        assert_eq!(
            globals.expand_rpc_url("${API_KEY} and ${TEST_ENV_KEY}"),
            "config_key and env_key"
        );
    }

    #[test]
    fn test_missing_variables() {
        let globals = GlobalVariables::default();

        assert_eq!(
            globals.expand_rpc_url("https://api.example.com/${MISSING_KEY}/v1"),
            "https://api.example.com/${MISSING_KEY}/v1"
        );
    }

    #[test]
    fn redact_url_hides_all_literal_credential_locations() {
        let redacted = redact_url(
            "https://user:password@rpc.example.com/v2/path-secret?token=query-secret#fragment-secret",
        );
        for secret in [
            "user",
            "password",
            "path-secret",
            "query-secret",
            "fragment-secret",
        ] {
            assert!(!redacted.contains(secret), "{redacted}");
        }
        assert!(redacted.contains("rpc.example.com"));
        assert!(redacted.contains("REDACTED"));
    }

    #[test]
    fn redact_url_preserves_variable_template_names() {
        let redacted = redact_url("https://rpc.example.com/literal-secret/${ALCHEMY_KEY}");
        assert!(redacted.contains("${ALCHEMY_KEY}"));
        assert!(!redacted.contains("literal-secret"));
    }

    #[test]
    fn redact_url_fails_closed_for_malformed_input() {
        let redacted = redact_url("not a URL containing literal-secret");
        assert_eq!(redacted, "[REDACTED URL]");
        assert!(!redacted.contains("literal-secret"));
    }

    #[test]
    fn test_no_variables() {
        let globals = GlobalVariables::default();

        assert_eq!(
            globals.expand_rpc_url("https://api.example.com/v1"),
            "https://api.example.com/v1"
        );
    }

    // ── GlobalVariables CRUD ─────────────────────────────────────────

    #[test]
    fn test_add_then_get_rpc_expansion() {
        let mut globals = GlobalVariables::default();
        globals.add_rpc_expansion("MY_KEY", "my_value");
        assert_eq!(
            globals.get_rpc_expansion("MY_KEY"),
            Some("my_value".to_string())
        );
    }

    #[test]
    fn test_get_rpc_expansion_missing_key_returns_none() {
        let globals = GlobalVariables::default();
        assert_eq!(globals.get_rpc_expansion("DOES_NOT_EXIST"), None);
    }

    #[test]
    fn test_remove_rpc_expansion_returns_old_value() {
        let mut globals = GlobalVariables::default();
        globals.add_rpc_expansion("TO_REMOVE", "old_val");
        let removed = globals.remove_rpc_expansion("TO_REMOVE");
        assert_eq!(removed, Some("old_val".to_string()));
        // After removal, get returns None
        assert_eq!(globals.get_rpc_expansion("TO_REMOVE"), None);
    }

    #[test]
    fn test_remove_rpc_expansion_missing_key_returns_none() {
        let mut globals = GlobalVariables::default();
        assert_eq!(globals.remove_rpc_expansion("NONEXISTENT"), None);
    }

    #[test]
    fn test_list_rpc_expansions_returns_all_entries() {
        let mut globals = GlobalVariables::default();
        globals.add_rpc_expansion("A", "1");
        globals.add_rpc_expansion("B", "2");
        globals.add_rpc_expansion("C", "3");

        let listing = globals.list_rpc_expansions();
        assert_eq!(listing.len(), 3);
        assert_eq!(listing.get("A"), Some(&"1".to_string()));
        assert_eq!(listing.get("B"), Some(&"2".to_string()));
        assert_eq!(listing.get("C"), Some(&"3".to_string()));
    }

    #[test]
    fn test_list_rpc_expansions_empty() {
        let globals = GlobalVariables::default();
        assert!(globals.list_rpc_expansions().is_empty());
    }

    #[test]
    fn test_add_rpc_expansion_overwrites_existing() {
        let mut globals = GlobalVariables::default();
        globals.add_rpc_expansion("KEY", "first");
        globals.add_rpc_expansion("KEY", "second");
        assert_eq!(globals.get_rpc_expansion("KEY"), Some("second".to_string()));
        assert_eq!(globals.list_rpc_expansions().len(), 1);
    }

    #[test]
    fn variable_validation_preserves_legacy_names_but_rejects_template_syntax() {
        let mut globals = GlobalVariables::default();
        globals.add_rpc_expansion("legacy-name.with punctuation", "value");
        assert!(globals.validate().is_ok());

        globals.add_rpc_expansion("BAD}", "value");
        assert!(globals.validate().is_err());
    }

    #[test]
    fn global_variable_debug_never_contains_values() {
        let mut globals = GlobalVariables::default();
        globals.add_rpc_expansion("TOKEN", "literal-secret");
        let output = format!("{globals:?}");
        assert!(output.contains("TOKEN"));
        assert!(!output.contains("literal-secret"));
    }

    // ── ChainVariables::expand() ─────────────────────────────────────

    fn make_chain_variables() -> ChainVariables {
        let mut env = HashMap::new();
        let mut expansions = HashMap::new();

        let vars = [
            ("WALLET_ADDRESS", "@wallet", "0xABCD"),
            ("ETH_RPC_URL", "@rpc", "http://localhost:8545"),
            ("CHAIN_ID", "@chainid", "1"),
            ("CHAIN_NAME", "@chainname", "mainnet"),
            ("RAW_PRIVATE_KEY", "@key", "0xdeadbeef"),
            ("VERIFIER_URL", "@verification_url", "https://etherscan.io"),
            ("VERIFIER_API_KEY", "@verifier_api_key", "abc123"),
        ];

        for (env_key, expansion, val) in &vars {
            env.insert(env_key.to_string(), val.to_string());
            expansions.insert(expansion.to_string(), val.to_string());
        }

        ChainVariables { env, expansions }
    }

    #[test]
    fn test_expand_wallet_token() {
        let cv = make_chain_variables();
        let result = cv.expand(vec!["--from".into(), "@wallet".into()]);
        assert_eq!(result, vec!["--from", "0xABCD"]);
    }

    #[test]
    fn cached_wallet_address_does_not_unlock_keyring() {
        let chain = crate::chain::ChainInstance::new(
            crate::chain::ChainDefinition {
                name: "mainnet".into(),
                aliases: vec![],
                chain_id: 1,
                rpc_urls: vec!["http://localhost:8545".into()],
                selected_rpc: "http://localhost:8545".into(),
                verification_api_key: None,
                verification_url: None,
                key_name: "deployer".into(),
            },
            "http://localhost:8545".into(),
            crate::key::Key {
                name: "deployer".into(),
                address: Some("0xABCD".into()),
                kind: crate::key::KeyType::Keyring {
                    service: "deliberately-unavailable".into(),
                    username: "missing".into(),
                },
            },
        );

        let cv = ChainVariables::new(&chain, &["echo".into(), "@wallet".into()], false)
            .expect("cached address should avoid keyring access");
        assert_eq!(cv.as_map().get("WALLET_ADDRESS"), Some(&"0xABCD".into()));
        assert!(!cv.as_map().contains_key("RAW_PRIVATE_KEY"));
    }

    #[test]
    fn test_expand_rpc_token() {
        let cv = make_chain_variables();
        let result = cv.expand(vec!["--rpc-url".into(), "@rpc".into()]);
        assert_eq!(result, vec!["--rpc-url", "http://localhost:8545"]);
    }

    #[test]
    fn test_expand_chainid_token() {
        let cv = make_chain_variables();
        let result = cv.expand(vec!["--chain".into(), "@chainid".into()]);
        assert_eq!(result, vec!["--chain", "1"]);
    }

    #[test]
    fn test_expand_chainname_token() {
        let cv = make_chain_variables();
        let result = cv.expand(vec!["echo".into(), "@chainname".into()]);
        assert_eq!(result, vec!["echo", "mainnet"]);
    }

    #[test]
    fn test_expand_key_token() {
        let cv = make_chain_variables();
        let result = cv.expand(vec!["--private-key".into(), "@key".into()]);
        assert_eq!(result, vec!["--private-key", "0xdeadbeef"]);
    }

    #[test]
    fn test_expand_multiple_tokens_in_args() {
        let cv = make_chain_variables();
        let result = cv.expand(vec![
            "cast".into(),
            "send".into(),
            "--from".into(),
            "@wallet".into(),
            "--rpc-url".into(),
            "@rpc".into(),
            "--chain".into(),
            "@chainid".into(),
        ]);
        assert_eq!(
            result,
            vec![
                "cast",
                "send",
                "--from",
                "0xABCD",
                "--rpc-url",
                "http://localhost:8545",
                "--chain",
                "1"
            ]
        );
    }

    #[test]
    fn test_expand_token_embedded_in_string() {
        let cv = make_chain_variables();
        let result = cv.expand(vec!["network=@chainname".into()]);
        assert_eq!(result, vec!["network=mainnet"]);
    }

    #[test]
    fn test_expand_leaves_unknown_tokens_unchanged() {
        let cv = make_chain_variables();
        let result = cv.expand(vec!["@unknown".into(), "plain".into()]);
        assert_eq!(result, vec!["@unknown", "plain"]);
    }

    #[test]
    fn test_expand_empty_input() {
        let cv = make_chain_variables();
        let result = cv.expand(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_expand_no_tokens_passes_through() {
        let cv = make_chain_variables();
        let result = cv.expand(vec!["echo".into(), "hello".into(), "world".into()]);
        assert_eq!(result, vec!["echo", "hello", "world"]);
    }

    // ── ChainVariables::as_map() ─────────────────────────────────────

    #[test]
    fn test_as_map_has_correct_keys() {
        let cv = make_chain_variables();
        let map = cv.as_map();

        let expected_keys = [
            "WALLET_ADDRESS",
            "ETH_RPC_URL",
            "CHAIN_ID",
            "CHAIN_NAME",
            "RAW_PRIVATE_KEY",
            "VERIFIER_URL",
            "VERIFIER_API_KEY",
        ];

        for key in &expected_keys {
            assert!(map.contains_key(*key), "Missing key: {}", key);
        }
        assert_eq!(map.len(), expected_keys.len());
    }

    #[test]
    fn test_as_map_has_correct_values() {
        let cv = make_chain_variables();
        let map = cv.as_map();

        assert_eq!(map.get("WALLET_ADDRESS").unwrap(), "0xABCD");
        assert_eq!(map.get("ETH_RPC_URL").unwrap(), "http://localhost:8545");
        assert_eq!(map.get("CHAIN_ID").unwrap(), "1");
        assert_eq!(map.get("CHAIN_NAME").unwrap(), "mainnet");
        assert_eq!(map.get("RAW_PRIVATE_KEY").unwrap(), "0xdeadbeef");
        assert_eq!(map.get("VERIFIER_URL").unwrap(), "https://etherscan.io");
        assert_eq!(map.get("VERIFIER_API_KEY").unwrap(), "abc123");
    }

    // ── Lazy key resolution ────────────────────────────────────────────

    fn make_chain_variables_without_key() -> ChainVariables {
        let mut env = HashMap::new();
        let mut expansions = HashMap::new();

        let vars = [
            ("ETH_RPC_URL", "@rpc", "http://localhost:8545"),
            ("CHAIN_ID", "@chainid", "1"),
            ("CHAIN_NAME", "@chainname", "mainnet"),
            ("VERIFIER_URL", "@verification_url", "https://etherscan.io"),
            ("VERIFIER_API_KEY", "@verifier_api_key", "abc123"),
        ];

        for (env_key, expansion, val) in &vars {
            env.insert(env_key.to_string(), val.to_string());
            expansions.insert(expansion.to_string(), val.to_string());
        }

        ChainVariables { env, expansions }
    }

    #[test]
    fn test_no_key_vars_when_command_doesnt_need_key() {
        let cv = make_chain_variables_without_key();
        let map = cv.as_map();

        assert!(!map.contains_key("WALLET_ADDRESS"));
        assert!(!map.contains_key("RAW_PRIVATE_KEY"));
        assert_eq!(map.len(), 5);
    }

    #[test]
    fn test_key_token_unexpanded_without_key_vars() {
        let cv = make_chain_variables_without_key();
        let result = cv.expand(vec!["--private-key".into(), "@key".into()]);
        assert_eq!(result, vec!["--private-key", "@key"]);
    }

    #[test]
    fn test_wallet_token_unexpanded_without_key_vars() {
        let cv = make_chain_variables_without_key();
        let result = cv.expand(vec!["--from".into(), "@wallet".into()]);
        assert_eq!(result, vec!["--from", "@wallet"]);
    }

    #[test]
    fn test_non_key_vars_still_expand_without_key() {
        let cv = make_chain_variables_without_key();
        let result = cv.expand(vec![
            "--rpc-url".into(),
            "@rpc".into(),
            "--chain".into(),
            "@chainid".into(),
        ]);
        assert_eq!(
            result,
            vec!["--rpc-url", "http://localhost:8545", "--chain", "1"]
        );
    }
}
