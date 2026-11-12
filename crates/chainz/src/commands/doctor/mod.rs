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
use indexmap::IndexMap;

#[derive(Default)]
struct Report {
    problems: usize,
    fixed: usize,
    /// `-v`: list each finding of a group.
    verbose: bool,
    /// What `--fix` would repair, when not fixing: file modes, literal
    /// credentials, active endpoints.
    would_chmod: usize,
    would_move: usize,
    would_switch: usize,
}

/// One finding on one record: the chain (or explorer) it is about, and
/// the line it prints alone.
struct Finding {
    on: String,
    alone: String,
}

/// Findings keyed by cause (what, and what is wrong with it), in
/// first-seen order: one cause across many chains prints as one line.
type Groups<T = Finding> = IndexMap<(String, String), Vec<T>>;

/// Where a group's findings are: the names when there are a few, else a
/// count (`eth, base`, `17 chains`).
fn places(findings: &[Finding], noun: &str) -> String {
    if findings.len() <= 3 {
        findings
            .iter()
            .map(|f| f.on.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        ui::count(findings.len(), noun)
    }
}

impl Report {
    /// Report a group of problems with one cause: a single finding as its
    /// own line, more as `line(places)` (each finding under it with `-v`).
    fn problems(&mut self, findings: &[Finding], line: impl FnOnce(&str) -> String) {
        match findings {
            [] => {}
            [one] => self.problem(&one.alone),
            _ => {
                self.problems += findings.len();
                println!("{}", ui::fail(&line(&places(findings, "chain"))));
                if self.verbose {
                    for f in findings {
                        println!("    {}", ui::dim(&f.alone));
                    }
                }
            }
        }
    }

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
    let mut report = Report {
        verbose: args.verbose,
        ..Report::default()
    };

    println!("{}", ui::section("files"));
    files(&chainz, fix, &mut report);

    println!("{}", ui::section("config"));
    let secrets = match chainz.secrets() {
        Ok(secrets) => Some(secrets.clone()),
        Err(e) => {
            report.problem(&format!("{e:#}"));
            None
        }
    };
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
    println!("{}", report.summary(fix));
    Ok(if report.problems > report.fixed { 1 } else { 0 })
}

impl Report {
    /// The closing line: no problems, how many were fixed, or what
    /// `--fix` would do.
    fn summary(&self, fix: bool) -> String {
        if self.problems == 0 {
            return ui::success("no problems found");
        }
        let problems = ui::count(self.problems, "problem");
        if fix {
            return ui::fail(&format!("{problems}, {} fixed", self.fixed));
        }
        let would: Vec<String> = [
            (self.would_chmod, "fix", "file mode"),
            (self.would_move, "move", "credential"),
            (self.would_switch, "switch", "active endpoint"),
        ]
        .into_iter()
        .filter(|(n, ..)| *n > 0)
        .map(|(n, verb, noun)| format!("{verb} {}", ui::count(n, noun)))
        .collect();
        if would.is_empty() {
            ui::fail(&format!("{problems}; `--fix` can't repair these"))
        } else {
            ui::fail(&format!(
                "{problems}; `chainz doctor --fix` would {}",
                would.join(", ")
            ))
        }
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
        report.note("no config.toml yet (add a chain with `chainz add`)");
    }
    for (what, path) in locations(chainz) {
        println!("  {}", ui::dim(&format!("{what:<8} {}", path.display())));
    }
    for (path, want) in [
        (&p.dir(), chainz_os::PRIVATE_DIR_MODE),
        (&p.secrets(), chainz_os::PRIVATE_FILE_MODE),
        (&p.config(), chainz_os::PRIVATE_FILE_MODE),
    ] {
        let Some(m) = mode(path) else { continue };
        if !chainz_os::is_private_mode(m) {
            let msg = format!("{} is mode {m:o}, expected {want:o}", path.display());
            report.problem(&msg);
            if !fix {
                report.would_chmod += 1;
            } else {
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

/// Every other place chainz keeps files, for the `files` section: where
/// to look, not a check (a directory that can't be resolved is left out).
fn locations(chainz: &Chainz) -> Vec<(&'static str, std::path::PathBuf)> {
    let mut out = vec![("secrets", chainz.paths().secrets().to_path_buf())];
    out.extend(paths::state_dir().ok().map(|d| ("state", d)));
    out.extend(paths::cache_dir().ok().map(|d| ("cache", d)));
    out.extend(
        chainz_core::runtime::Runtime::resolve()
            .ok()
            .map(|r| ("runtime", r.dir().to_path_buf())),
    );
    out.extend(paths::serve_log().ok().map(|f| ("serve", f)));
    out.extend(paths::fork_log("<name>").ok().map(|f| ("fork", f)));
    out
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
    // in the query or an unknown provider's path), one line per endpoint
    // name and kind.
    let mut manual = Groups::new();
    for (chain, c) in &config.chains {
        for (ep, e) in &c.rpcs {
            if let Some(kind) = extract::manual_credential(&e.url) {
                let problem = format!(
                    "literal credential in the URL ({kind}); move it into a variable by hand"
                );
                manual
                    .entry((format!("'{ep}'"), problem.clone()))
                    .or_default()
                    .push(Finding {
                        on: chain.clone(),
                        alone: format!("chains.{chain}.rpcs.{ep}: {problem}"),
                    });
            }
        }
    }
    // Credentials `--fix` moves, one line per record name and kind.
    let mut movable = Groups::new();
    for moved in &plan.moved {
        let (on, subject, problem) = match &moved.location {
            Location::Url { chain, endpoint } => (
                chain,
                format!("'{endpoint}'"),
                "literal API key in the URL".to_string(),
            ),
            Location::Header {
                chain,
                endpoint,
                header,
            } => (
                chain,
                format!("'{endpoint}'"),
                format!("header '{header}' holds a literal value"),
            ),
            Location::ExplorerKey { explorer } => (
                explorer,
                format!("explorers.{explorer}.key"),
                "literal API key".to_string(),
            ),
        };
        movable
            .entry((subject, problem.clone()))
            .or_default()
            .push(Finding {
                on: on.clone(),
                alone: match &moved.location {
                    Location::Header { .. } => format!("{} holds a literal value", moved.location),
                    _ => format!("{}: {problem}", moved.location),
                },
            });
    }
    for ((subject, problem), findings) in manual.iter().chain(&movable) {
        found += findings.len();
        report.problems(findings, |places| {
            format!("{subject} on {places}: {problem}")
        });
    }
    if !fix {
        report.would_move += plan.moved.len();
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
    // Failing active endpoints, one line per endpoint name and problem;
    // whether a healthy endpoint could replace each.
    let mut failing: Groups<(Finding, Option<Repair>)> = Groups::new();
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
        let finding = Finding {
            on: p.chain.clone(),
            alone: format!("{}: active endpoint '{}' {problem}", p.chain, c.rpc),
        };
        let repair = Repair::propose(&p.chain, c, p, &ranking);
        failing
            .entry((c.rpc.clone(), problem))
            .or_default()
            .push((finding, repair));
    }
    for ((ep, problem), group) in failing {
        let (findings, proposed): (Vec<Finding>, Vec<Option<Repair>>) = group.into_iter().unzip();
        report.problems(&findings, |places| {
            format!("active endpoint '{ep}' {problem} on {places}")
        });
        let proposed: Vec<Repair> = proposed.into_iter().flatten().collect();
        if fix {
            repairs.extend(proposed.into_iter().map(|r| (ep.clone(), r)));
            continue;
        }
        report.would_switch += proposed.len();
        match (findings.len(), proposed.as_slice()) {
            (_, []) => {}
            (1, [repair]) => {
                println!("  {}", ui::dim(&format!("'{}' is healthy", repair.active)));
            }
            (n, _) => println!(
                "  {}",
                ui::dim(&format!(
                    "a healthy endpoint can replace it on {} of {n}",
                    proposed.len()
                ))
            ),
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
