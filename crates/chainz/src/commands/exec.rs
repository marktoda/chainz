//! The hot path: `exec`, `shell`, `env`. Only `chainz-core` APIs: no
//! async runtime, no sockets, no threads before `execvp`.

use super::{open, select_target};
use crate::cli::{ExecArgs, ShellKind};
use crate::ui;
use anyhow::{Context as _, Result, anyhow, bail};
use chainz_core::Chainz;
use chainz_core::context::{self, CONTRACT_VARS, Context, EnvVar, ExecOptions, Expander};
use chainz_core::runtime::Runtime;
use chainz_core::signer::{self, SystemKeyBackend};

fn context_for(
    chainz: &Chainz,
    runtime: &Runtime,
    chain: Option<&str>,
    opts: &ExecOptions,
) -> Result<Context> {
    let target = select_target(chainz, runtime, chain)?;
    context::build(chainz, runtime, &target, opts)
}

fn runtime() -> Result<Runtime> {
    Runtime::resolve().context("unable to find the runtime directory")
}

/// `chainz exec`.
pub fn exec(args: ExecArgs) -> Result<i32> {
    let chainz = open()?;
    let runtime = runtime()?;
    let opts = ExecOptions {
        signer: args.signer.clone(),
        rpc: args.rpc.clone(),
        direct: args.direct,
    };
    let ctx = context_for(&chainz, &runtime, args.chain.as_deref(), &opts)?;
    let expander = Expander::new(&chainz, &runtime, &ctx, &opts);
    let argv = expander.expand_args(&args.command)?;
    for warning in expander.warnings() {
        eprintln!("chainz: warning: {warning}");
    }
    let mut env = ctx.env()?;
    if args.expose_key {
        env.push(expose_key(&chainz, &ctx)?);
    }
    if args.explain {
        print!("{}", explain(&ctx, &env, &argv));
        return Ok(0);
    }
    let mut command = std::process::Command::new(&argv[0].value);
    command.args(argv[1..].iter().map(|a| &a.value));
    for name in CONTRACT_VARS {
        command.env_remove(name);
    }
    for var in &env {
        command.env(var.name, &var.value);
    }
    run(command, &argv[0].display)
}

#[cfg(unix)]
fn run(mut command: std::process::Command, program: &str) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    // execvp: same PID; signals and the exit status belong to the command.
    let error = command.exec();
    Err(anyhow!("failed to run '{program}': {error}"))
}

#[cfg(not(unix))]
fn run(mut command: std::process::Command, program: &str) -> Result<i32> {
    let status = command
        .status()
        .with_context(|| format!("failed to run '{program}'"))?;
    Ok(status.code().unwrap_or(1))
}

fn expose_key(chainz: &Chainz, ctx: &Context) -> Result<EnvVar> {
    if ctx.fork.is_some() {
        bail!(
            "--expose-key is refused on a fork: a fork shares the real chain ID, so its signatures replay on the real chain. Use `cast send --unlocked` (anvil impersonates ETH_FROM)"
        );
    }
    let signer_ctx = ctx
        .signer
        .as_ref()
        .ok_or_else(|| anyhow!("chain '{}' has no signer: pass --signer", ctx.chain))?;
    let signer = &chainz.config().signers[&signer_ctx.name];
    let key = signer::unlock(
        &SystemKeyBackend,
        &signer_ctx.name,
        signer,
        chainz.secrets()?,
    )?;
    Ok(EnvVar {
        name: "RAW_PRIVATE_KEY",
        value: key.to_string(),
        secret: true,
    })
}

fn explain(ctx: &Context, env: &[EnvVar], argv: &[context::ExpandedArg]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let fork = ctx
        .fork
        .as_ref()
        .map(|f| format!(" fork of {} on :{}", f.parent, f.port))
        .unwrap_or_default();
    writeln!(out, "chain     {} ({}){fork}", ctx.chain, ctx.chain_id).ok();
    let route = if ctx.rpc.via_gateway {
        "via gateway"
    } else {
        "direct"
    };
    writeln!(
        out,
        "rpc       {} {} ({route})",
        ctx.rpc.endpoint.as_deref().unwrap_or("-"),
        ctx.rpc.display
    )
    .ok();
    match &ctx.signer {
        Some(s) => writeln!(
            out,
            "signer    {} {} ({})",
            s.name,
            s.address
                .map(|a| a.to_string())
                .unwrap_or_else(|| "no cached address".into()),
            s.backend
        ),
        None => writeln!(out, "signer    -"),
    }
    .ok();
    writeln!(
        out,
        "explorer  {}",
        ctx.explorer
            .as_ref()
            .map(|e| e.name.as_str())
            .unwrap_or("-")
    )
    .ok();
    writeln!(out, "env").ok();
    for var in env {
        writeln!(out, "  {}={}", var.name, var.display()).ok();
    }
    let unset: Vec<&str> = CONTRACT_VARS
        .iter()
        .copied()
        .filter(|name| !env.iter().any(|v| v.name == *name))
        .collect();
    if !unset.is_empty() {
        writeln!(out, "unset     {}", unset.join(" ")).ok();
    }
    let argv: Vec<String> = argv.iter().map(|a| shell_quote(&a.display)).collect();
    writeln!(out, "argv      {}", argv.join(" ")).ok();
    out
}

/// `chainz shell`: a subshell with the context exported.
pub fn shell(chain: Option<String>, signer: Option<String>, direct: bool) -> Result<i32> {
    let chainz = open()?;
    let runtime = runtime()?;
    let opts = ExecOptions {
        signer,
        rpc: None,
        direct,
    };
    let ctx = context_for(&chainz, &runtime, chain.as_deref(), &opts)?;
    let env = ctx.env()?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    eprintln!(
        "{}",
        ui::item(&format!("entering {} shell — ctrl-d to exit", ctx.chain))
    );
    let mut command = std::process::Command::new(&shell);
    for name in CONTRACT_VARS {
        command.env_remove(name);
    }
    for var in &env {
        command.env(var.name, &var.value);
    }
    command.env(
        "PS1",
        format!(
            "(⛓ {}) {}",
            ctx.chain,
            std::env::var("PS1").unwrap_or_default()
        ),
    );
    let status = command
        .status()
        .with_context(|| format!("failed to start {shell}"))?;
    eprintln!("{}", ui::dim(&format!("left {} shell", ctx.chain)));
    Ok(status.code().unwrap_or(1))
}

/// `chainz env`: shell exports for `eval`.
pub fn env(
    chain: Option<String>,
    shell: Option<ShellKind>,
    signer: Option<String>,
    rpc: Option<String>,
    direct: bool,
) -> Result<i32> {
    let chainz = open()?;
    let runtime = runtime()?;
    let opts = ExecOptions {
        signer,
        rpc,
        direct,
    };
    let ctx = context_for(&chainz, &runtime, chain.as_deref(), &opts)?;
    let env = ctx.env()?;
    let kind = shell.unwrap_or_else(|| {
        match std::env::var("SHELL")
            .unwrap_or_default()
            .rsplit('/')
            .next()
            .unwrap_or_default()
        {
            "fish" => ShellKind::Fish,
            "zsh" => ShellKind::Zsh,
            _ => ShellKind::Bash,
        }
    });
    let mut out = String::new();
    for name in CONTRACT_VARS {
        if !env.iter().any(|v| v.name == name) && name != "RAW_PRIVATE_KEY" {
            out.push_str(&match kind {
                ShellKind::Fish => format!("set -e {name};\n"),
                _ => format!("unset {name};\n"),
            });
        }
    }
    for var in &env {
        out.push_str(&match kind {
            ShellKind::Fish => format!("set -gx {} {};\n", var.name, fish_quote(&var.value)),
            _ => format!("export {}={};\n", var.name, shell_quote(&var.value)),
        });
    }
    print!("{out}");
    Ok(0)
}

/// POSIX single-quoting (only when needed).
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&b))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn fish_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', r"\\").replace('\'', r"\'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(
            shell_quote("https://x.example/v3/abc"),
            "https://x.example/v3/abc"
        );
        assert_eq!(shell_quote("a b'c"), r"'a b'\''c'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(fish_quote(r"a'b\c"), r"'a\'b\\c'");
    }
}
