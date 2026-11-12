//! The hot path: `exec`, `shell`, `env`. Only `chainz-core` APIs: no
//! async runtime, no sockets, no threads before `execvp`.

use super::{open, runtime, select_target};
use crate::cli::{ContextArgs, ExecArgs, ShellKind};
use crate::ui;
use anyhow::{Result, anyhow, bail};
use chainz_core::Chainz;
use chainz_core::context::{self, CONTRACT_VARS, Context, EnvVar, ExecOptions, Expander};
use chainz_core::runtime::Runtime;
use chainz_core::signer::{self, SystemKeyBackend};

/// The options and resolved context for `exec`, `env` or `shell`.
fn context_for(
    chainz: &Chainz,
    runtime: &Runtime,
    args: &ContextArgs,
) -> Result<(ExecOptions, Context)> {
    let opts = ExecOptions {
        signer: args.signer.clone(),
        rpc: args.rpc.clone(),
        direct: args.direct,
    };
    let target = select_target(chainz, runtime, args.chain.as_deref())?;
    let ctx = context::build(chainz, runtime, &target, &opts)?;
    Ok((opts, ctx))
}

/// `chainz exec`.
pub fn exec(args: ExecArgs) -> Result<i32> {
    let chainz = open()?;
    let runtime = runtime()?;
    let (opts, ctx) = context_for(&chainz, &runtime, &args.context)?;
    let expander = Expander::new(&chainz, &runtime, &ctx, &opts);
    let argv = expander.expand_args(&args.command)?;
    for warning in expander.warnings() {
        eprintln!("{}", ui::warn(&warning));
    }
    let mut env = ctx.env()?;
    if args.expose_key {
        let signer = expose_key_signer(&ctx)?;
        // --explain shows the variable redacted: no reason to unlock.
        let value = if args.explain {
            String::new()
        } else {
            unlock(&chainz, signer)?
        };
        env.push(EnvVar {
            name: "RAW_PRIVATE_KEY",
            value,
            secret: true,
        });
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

/// Why a command could not be started, with the shell's exit code for it:
/// 127 when it does not exist, 126 when it can't be executed.
fn spawn_error(program: &str, error: &std::io::Error) -> anyhow::Error {
    let (code, why) = match error.kind() {
        std::io::ErrorKind::NotFound => (127, "command not found".to_string()),
        std::io::ErrorKind::PermissionDenied => (126, "permission denied".to_string()),
        _ => (126, error.to_string()),
    };
    anyhow::Error::new(ui::ExitCode {
        code,
        message: format!("failed to run '{program}': {why}"),
    })
}

fn run(mut command: std::process::Command, program: &str) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    // execvp: same PID; signals and the exit status belong to the command.
    let error = command.exec();
    Err(spawn_error(program, &error))
}

/// The signer `--expose-key` would unlock, if it may: never on a fork.
fn expose_key_signer(ctx: &Context) -> Result<&str> {
    if ctx.fork.is_some() {
        bail!(
            "--expose-key is refused on a fork: a fork shares the real chain ID, so its signatures replay on the real chain. Use `cast send --unlocked` (anvil impersonates ETH_FROM)"
        );
    }
    ctx.signer
        .as_ref()
        .map(|s| s.name.as_str())
        .ok_or_else(|| anyhow!("chain '{}' has no signer: pass --signer", ctx.chain))
}

/// The signer's private key: keyring, password prompt or `op read`.
fn unlock(chainz: &Chainz, name: &str) -> Result<String> {
    let signer = &chainz.config().signers[name];
    let key = signer::unlock(&SystemKeyBackend, name, signer, chainz.secrets()?)?;
    Ok(key.to_string())
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
pub fn shell(args: ContextArgs) -> Result<i32> {
    let chainz = open()?;
    let runtime = runtime()?;
    let (_, ctx) = context_for(&chainz, &runtime, &args)?;
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
        .map_err(|error| spawn_error(&shell, &error))?;
    eprintln!("{}", ui::dim(&format!("left {} shell", ctx.chain)));
    Ok(status.code().unwrap_or(1))
}

/// `chainz env`: shell exports for `eval`.
pub fn env(args: ContextArgs, shell: Option<ShellKind>) -> Result<i32> {
    let chainz = open()?;
    let runtime = runtime()?;
    let (_, ctx) = context_for(&chainz, &runtime, &args)?;
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
    fn missing_and_unrunnable_commands_have_shell_exit_codes() {
        use std::io::{Error, ErrorKind};
        let code = |kind| ui::exit_code(&spawn_error("x", &Error::from(kind)));
        assert_eq!(code(ErrorKind::NotFound), 127);
        assert_eq!(code(ErrorKind::PermissionDenied), 126);
        let error = spawn_error("nope", &Error::from(ErrorKind::NotFound));
        assert_eq!(error.to_string(), "failed to run 'nope': command not found");
    }

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
