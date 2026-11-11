//! `doctor [--fix]`: files, invariants, literal credentials, signers and
//! RPC health. Messages never echo URLs, header values or secrets.

use super::open_lenient;
use super::rpc::{health_label, probe_chains};
use crate::{net, ui};
use anyhow::Result;
use chainz_core::model::{Backend, Config};
use chainz_core::{Chainz, edit, migrate, paths, template, validate};
use chainz_rpc::{Health, classify, rank};

#[derive(Default)]
struct Report {
    problems: usize,
    fixed: usize,
}

impl Report {
    fn ok(&self, msg: &str) {
        println!("{}", ui::success(msg));
    }
    fn problem(&mut self, msg: &str) {
        self.problems += 1;
        println!("{}", ui::fail(msg));
    }
    fn note(&self, msg: &str) {
        println!("{}", ui::warn(msg));
    }
    fn fixed(&mut self, msg: &str) {
        self.fixed += 1;
        println!("{}", ui::success(&format!("fixed: {msg}")));
    }
}

pub fn run(fix: bool) -> Result<i32> {
    let mut chainz = open_lenient()?;
    let mut report = Report::default();

    println!("{}", ui::section("files"));
    files(&chainz, fix, &mut report);

    println!("{}", ui::section("config"));
    let secrets = chainz.secrets().ok().cloned();
    if secrets.is_none() {
        report.problem("secrets.toml does not parse");
    }
    let issues = validate::issues(chainz.config(), secrets.as_ref());
    if issues.is_empty() {
        report.ok(&format!(
            "{} chains, {} signers, {} variables: valid",
            chainz.config().chains.len(),
            chainz.config().signers.len(),
            secrets.as_ref().map_or(0, |s| s.vars.len())
        ));
    }
    for issue in &issues {
        report.problem(issue);
    }
    if !issues.is_empty() {
        // Nothing below is meaningful on an invalid config, and --fix
        // could not write through validation anyway.
        println!("{}", ui::dim("fix these with `chainz config edit`"));
        return Ok(1);
    }

    println!("{}", ui::section("credentials"));
    credentials(&mut chainz, fix, &mut report)?;

    println!("{}", ui::section("signers"));
    signers(chainz.config(), &mut report);

    println!("{}", ui::section("rpc"));
    rpc_health(&mut chainz, fix, &mut report)?;

    println!();
    if report.problems == 0 {
        println!("{}", ui::success("no problems found"));
        Ok(0)
    } else {
        let hint = if fix { "" } else { " (try --fix)" };
        println!(
            "{}",
            ui::fail(&format!(
                "{} problem(s), {} fixed{hint}",
                report.problems, report.fixed
            ))
        );
        Ok(if report.problems > report.fixed { 1 } else { 0 })
    }
}

#[cfg(unix)]
fn mode(path: &std::path::Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn mode(_: &std::path::Path) -> Option<u32> {
    None
}

fn files(chainz: &Chainz, fix: bool, report: &mut Report) {
    let p = chainz.paths();
    if chainz.exists() {
        report.ok(&format!("config {}", p.config.display()));
    } else {
        report.note("no config.toml yet (add a chain with `chainz chain add`)");
    }
    for (path, want) in [(&p.dir, 0o700), (&p.secrets, 0o600), (&p.config, 0o600)] {
        let Some(m) = mode(path) else { continue };
        if m & 0o077 != 0 {
            let msg = format!("{} is mode {m:o}, expected {want:o}", path.display());
            report.problem(&msg);
            #[cfg(unix)]
            if fix {
                use std::os::unix::fs::PermissionsExt;
                if std::fs::set_permissions(path, std::fs::Permissions::from_mode(want)).is_ok() {
                    report.fixed(&msg);
                }
            }
        }
    }
    if chainz.exists()
        && let Some(v1) = p.v1_source()
    {
        report.note(&format!(
            "{} is left over from v1 and ignored",
            v1.display()
        ));
    }
}

/// A literal (non-template) value that looks like a credential.
fn literal(value: &str) -> bool {
    !value.is_empty() && !template::has_templates(value)
}

fn credentials(chainz: &mut Chainz, fix: bool, report: &mut Report) -> Result<()> {
    let config = chainz.config().clone();
    let mut extractable: Vec<(String, String)> = Vec::new();
    let mut found = 0;
    for (chain, c) in &config.chains {
        for (ep, e) in &c.rpcs {
            if let Some(kind) = migrate::inline_credential(&e.url) {
                found += 1;
                report.problem(&format!(
                    "chains.{chain}.rpcs.{ep}: literal credential in the URL ({kind})"
                ));
                if migrate::extract_known_key(&e.url).is_some() {
                    extractable.push((chain.clone(), ep.clone()));
                }
            }
            for (h, v) in &e.headers {
                if literal(v) && v.len() >= 16 {
                    found += 1;
                    report.problem(&format!(
                        "chains.{chain}.rpcs.{ep}: header '{h}' holds a literal value; move it into a variable"
                    ));
                }
            }
        }
    }
    let mut keys: Vec<String> = Vec::new();
    for (name, e) in &config.explorers {
        if e.key.as_deref().is_some_and(literal) {
            found += 1;
            report.problem(&format!("explorers.{name}: literal API key"));
            keys.push(name.clone());
        }
    }
    if found == 0 {
        report.ok("no literal credentials in config.toml");
    }
    if !fix || (extractable.is_empty() && keys.is_empty()) {
        return Ok(());
    }
    let moved = chainz.transact(|tx| {
        let secrets = tx.secrets_view()?;
        let mut vars = secrets.vars.clone();
        let mut intern = |base: &str, secret: &str| -> String {
            if let Some((n, _)) = vars.iter().find(|(_, v)| v.as_str() == secret) {
                return n.clone();
            }
            let name = (1..)
                .map(|n| {
                    if n == 1 {
                        base.to_string()
                    } else {
                        format!("{base}_{n}")
                    }
                })
                .find(|n| !vars.contains_key(n))
                .expect("a free name");
            vars.insert(name.clone(), secret.to_string());
            name
        };
        let mut moved = Vec::new();
        for (chain, ep) in &extractable {
            let endpoint = &config.chains[chain].rpcs[ep];
            let (before, after, base, key) =
                migrate::extract_known_key(&endpoint.url).expect("checked above");
            let var = intern(base, key);
            let mut new = endpoint.clone();
            new.url = format!("{before}${{{var}}}{after}");
            edit::set_endpoint(&mut tx.doc, chain, ep, &new)?;
            moved.push(format!("chains.{chain}.rpcs.{ep} → ${{{var}}}"));
        }
        for name in &keys {
            let key = config.explorers[name]
                .key
                .as_deref()
                .expect("checked above");
            let base = format!("{}_API_KEY", name.to_ascii_uppercase().replace('-', "_"));
            let var = intern(&base, key);
            edit::set_explorer_key(&mut tx.doc, name, &format!("${{{var}}}"))?;
            moved.push(format!("explorers.{name} → ${{{var}}}"));
        }
        for (n, v) in &vars {
            if secrets.vars.get(n) != Some(v) {
                edit::set_var(&mut tx.secrets, n, v)?;
            }
        }
        Ok(moved)
    })?;
    for m in moved {
        report.fixed(&m);
    }
    Ok(())
}

fn signers(config: &Config, report: &mut Report) {
    if config.signers.is_empty() {
        report.ok("no signers");
        return;
    }
    for (name, s) in &config.signers {
        match s.backend {
            Backend::Plaintext => report.problem(&format!(
                "signer '{name}' is stored in plaintext; run `chainz signer migrate {name}`"
            )),
            Backend::Keystore => {
                let path = paths::expand_tilde(s.path.as_deref().unwrap_or_default());
                if !path.exists() {
                    report.problem(&format!(
                        "signer '{name}': keystore {} does not exist",
                        path.display()
                    ));
                } else {
                    report.ok(&format!("signer '{name}' (keystore)"));
                }
            }
            b => report.ok(&format!("signer '{name}' ({b})")),
        }
        if s.address.is_none() {
            report.note(&format!(
                "signer '{name}' has no cached address (@wallet and ETH_FROM are unavailable)"
            ));
        }
    }
}

fn rpc_health(chainz: &mut Chainz, fix: bool, report: &mut Report) -> Result<()> {
    let chains: Vec<String> = chainz.config().chains.keys().cloned().collect();
    if chains.is_empty() {
        report.ok("no chains");
        return Ok(());
    }
    let probed = net::block_on(probe_chains(chainz, &chains, false))?;
    let store = net::health_store();
    let mut switches: Vec<(String, String, String)> = Vec::new();
    for p in &probed {
        let c = &chainz.config().chains[&p.chain];
        let Some(active) = p.names.iter().position(|n| *n == c.rpc) else {
            report.note(&format!(
                "{}: active endpoint '{}' is a websocket (not probed)",
                p.chain, c.rpc
            ));
            continue;
        };
        let health = classify(&p.results);
        if health[active] == Health::Healthy {
            report.ok(&format!(
                "{}: {} {}ms",
                p.chain,
                c.rpc,
                p.results[active].latency.as_millis()
            ));
            continue;
        }
        report.problem(&format!(
            "{}: active endpoint '{}' is {}",
            p.chain,
            c.rpc,
            health_label(&health[active])
        ));
        let p50: Vec<_> = p.names.iter().map(|n| store.p50(c.id, n, 16)).collect();
        let best = rank(&p.results, &p50)[0];
        if health[best] == Health::Healthy {
            if fix {
                switches.push((p.chain.clone(), c.rpc.clone(), p.names[best].clone()));
            } else {
                println!("  {}", ui::dim(&format!("'{}' is healthy", p.names[best])));
            }
        }
    }
    if !switches.is_empty() {
        chainz.transact(|tx| {
            for (chain, _, best) in &switches {
                edit::set_active(&mut tx.doc, chain, best)?;
            }
            Ok(())
        })?;
        for (chain, old, best) in switches {
            report.fixed(&format!("{chain}: active {old} → {best}"));
        }
    }
    Ok(())
}
