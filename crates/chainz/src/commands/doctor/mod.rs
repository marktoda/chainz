//! `doctor [--fix]`: files, invariants, literal credentials, signers and
//! RPC health. Messages never echo URLs, header values or secrets.
//! `doctor --tokens` checks the built-in token registry instead.

mod tokens;

use crate::cli::DoctorArgs;
use crate::load::open_lenient;
use crate::probe::{self, Depth, Repair, health_problem};
use crate::{net, ui};
use anyhow::Result;
use chainz_core::extract::{self, Location};
use chainz_core::labels::LabelIndex;
use chainz_core::model::{Backend, Config};
use chainz_core::{Chainz, paths, validate};

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

pub fn run(args: &DoctorArgs) -> Result<i32> {
    let fix = args.fix;
    if args.tokens {
        return tokens::run();
    }
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
        println!("{}", ui::dim("fix these with `chainz edit`"));
        return Ok(1);
    }
    shadowed_tokens(chainz.config(), &report);

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

fn mode(path: &std::path::Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777)
}

fn files(chainz: &Chainz, fix: bool, report: &mut Report) {
    let p = chainz.paths();
    if chainz.exists() {
        report.ok(&format!("config {}", p.config().display()));
    } else {
        report.note("no config.toml yet (add a chain with `chainz chain add`)");
    }
    for (path, want) in [
        (&p.dir(), 0o700),
        (&p.secrets(), 0o600),
        (&p.config(), 0o600),
    ] {
        let Some(m) = mode(path) else { continue };
        if m & 0o077 != 0 {
            let msg = format!("{} is mode {m:o}, expected {want:o}", path.display());
            report.problem(&msg);
            if fix {
                use std::os::unix::fs::PermissionsExt;
                match std::fs::set_permissions(path, std::fs::Permissions::from_mode(want)) {
                    Ok(()) => report.fixed(&msg),
                    Err(e) => report.note(&format!("could not chmod {}: {e}", path.display())),
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

/// Your own labels that hide a built-in token on a configured chain: a
/// note, since shadowing is allowed but easy to forget.
fn shadowed_tokens(config: &Config, report: &Report) {
    let index = LabelIndex::new(config);
    for (token, label) in index.shadowed_tokens() {
        if let Some((chain, _)) = config.chain_by_id(token.chain_id) {
            report.note(&format!(
                "{chain}: your @{} ({}) shadows the built-in token",
                token.name, label.address
            ));
        }
    }
}

fn credentials(chainz: &mut Chainz, fix: bool, report: &mut Report) -> Result<()> {
    let config = chainz.config().clone();
    // Planning against a secrets.toml that does not parse would propose
    // variables that may already exist: report it (above) and stop here.
    let Ok(secrets) = chainz.secrets() else {
        report.note("credentials not checked: secrets.toml does not parse");
        return Ok(());
    };
    let vars = secrets.vars.clone();
    let plan = extract::plan_secret_extraction(&config, &vars);
    let mut found = 0;
    // Credentials in a URL that only a person can move (user info, a key
    // in the query or an unknown provider's path).
    for (chain, c) in &config.chains {
        for (ep, e) in &c.rpcs {
            if let Some(kind) = extract::manual_credential(&e.url) {
                found += 1;
                report.problem(&format!(
                    "chains.{chain}.rpcs.{ep}: literal credential in the URL ({kind}); move it into a variable by hand"
                ));
            }
        }
    }
    for moved in &plan.moved {
        found += 1;
        report.problem(&match &moved.location {
            Location::Url { .. } => format!("{}: literal API key in the URL", moved.location),
            Location::Header { .. } => format!("{} holds a literal value", moved.location),
            Location::ExplorerKey { .. } => format!("{}: literal API key", moved.location),
        });
    }
    if found == 0 {
        report.ok("no literal credentials in config.toml");
    }
    if !fix || plan.is_empty() {
        return Ok(());
    }
    // Re-plan under the lock, against the files as they are now.
    let moved = chainz.transact(|tx| tx.extract_all_secrets())?;
    for m in moved {
        report.fixed(&m.to_string());
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
    let mut store = net::health_store();
    let probed = net::block_on(probe::probe_chains(
        chainz,
        &chains,
        Depth::Health,
        &mut store,
    ))?;
    let mut repairs: Vec<(String, Repair)> = Vec::new();
    for p in &probed {
        let c = &chainz.config().chains[&p.chain];
        let Some(active) = p.index_of(&c.rpc) else {
            report.note(&format!(
                "{}: active endpoint '{}' is a websocket (not probed)",
                p.chain, c.rpc
            ));
            continue;
        };
        let ranking = p.ranking(&store);
        let Some(problem) = health_problem(&ranking.health[active]) else {
            report.ok(&format!(
                "{}: {} {}ms",
                p.chain,
                c.rpc,
                p.probes[active].result.latency.as_millis()
            ));
            continue;
        };
        report.problem(&format!(
            "{}: active endpoint '{}' {problem}",
            p.chain, c.rpc
        ));
        if let Some(repair) = Repair::propose(&p.chain, c, p, &ranking) {
            if fix {
                repairs.push((c.rpc.clone(), repair));
            } else {
                println!("  {}", ui::dim(&format!("'{}' is healthy", repair.active)));
            }
        }
    }
    if !repairs.is_empty() {
        // Only the active endpoint: doctor leaves failover lists to
        // `rpc bench --fix`.
        chainz.transact(|tx| {
            for (_, repair) in &repairs {
                repair.apply(tx, false)?;
            }
            Ok(())
        })?;
        for (old, repair) in repairs {
            report.fixed(&format!(
                "{}: active {old} → {}",
                repair.chain, repair.active
            ));
        }
    }
    Ok(())
}
