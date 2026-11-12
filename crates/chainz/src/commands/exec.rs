//! The hot path: `exec` (with no command, a subshell) and `env`. Only `chainz-core` APIs: no
//! async runtime, no sockets, no threads before `execvp`.

use crate::cli::{ContextArgs, EnvArgs, ExecArgs, ShellKind};
use crate::load::open;
use crate::select::{Missing, NAME_A_CHAIN, select_target};
use crate::ui;
use anyhow::Result;
use chainz_core::Chainz;
use chainz_core::context::{self, Context, ExecOptions, Expander, Exports};
use chainz_core::contract::RAW_PRIVATE_KEY;
use chainz_core::runtime::Runtime;
use chainz_core::signer::SystemKeyBackend;

/// The resolved context for `command`: `exec` or `env`.
fn context_for(
    command: &str,
    chainz: &Chainz,
    runtime: &Runtime,
    args: &ContextArgs,
    expose_key: bool,
) -> Result<Context> {
    let opts = ExecOptions {
        expose_key,
        ..ExecOptions::from(args)
    };
    let off_tty = format!("pass one, {NAME_A_CHAIN}");
    let missing = Missing {
        command: &[command],
        off_tty: &off_tty,
    };
    let target = select_target(chainz, runtime, args.chain.as_deref(), &missing)?;
    context::build(chainz, runtime, &target, &opts)
}

/// The context's exports, with their warnings printed.
fn exports(ctx: &Context) -> Result<Exports> {
    let exports = ctx.exports()?;
    for warning in &exports.warnings {
        eprintln!("{}", ui::warn(warning));
    }
    Ok(exports)
}

/// `chainz exec`: the command with the context, or with none, a subshell.
pub fn exec(args: &ExecArgs) -> Result<i32> {
    let chainz = open()?;
    let runtime = Runtime::resolve()?;
    let mut ctx = context_for("exec", &chainz, &runtime, &args.context, args.expose_key)?;
    if args.command.is_empty() {
        // A subshell reads its commands from stdin: off a terminal that is
        // the caller's input, which was never meant for a shell.
        if !args.explain && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return Err(crate::cli::usage_error(
                &["exec"],
                "no command: pass it after `--` (a subshell opens only on a terminal)",
            ));
        }
        return shell(&chainz, ctx, args.explain);
    }
    let expander = Expander::new(&chainz, &runtime, &ctx, args.context.direct);
    let argv = expander.expand_args(&args.command)?;
    for warning in expander.warnings() {
        eprintln!("{}", ui::warn(&warning));
    }
    if args.explain {
        // RAW_PRIVATE_KEY shows redacted: no reason to unlock.
        let argv: Vec<String> = argv.iter().map(|a| shell_quote(&a.display)).collect();
        print!("{}", explain(&ctx, &exports(&ctx)?, &argv.join(" ")));
        return Ok(0);
    }
    ctx.unlock_key(&chainz, &SystemKeyBackend, &crate::prompt::Terminal)?;
    let mut command = std::process::Command::new(&argv[0].value);
    command.args(argv[1..].iter().map(|a| &a.value));
    exports(&ctx)?.apply(&mut command)?;
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

/// The `--explain` text; `argv` is the command line as it would run.
fn explain(ctx: &Context, exports: &Exports, argv: &str) -> String {
    use std::fmt::Write;
    let view = ctx.explain();
    let mut out = String::new();
    let fork = view
        .fork
        .map(|(parent, port)| format!(" fork of {parent} on :{port}"))
        .unwrap_or_default();
    let _ = writeln!(out, "chain     {} ({}){fork}", view.chain, view.chain_id);
    let route = if view.via_gateway {
        "via gateway"
    } else {
        "direct"
    };
    let _ = writeln!(
        out,
        "rpc       {} {} ({route})",
        view.endpoint.unwrap_or("-"),
        view.rpc
    );
    let _ = match view.signer {
        Some((name, address, backend)) => writeln!(
            out,
            "signer    {name} {} ({backend})",
            address
                .map(|a| a.to_string())
                .unwrap_or_else(|| "no cached address".into()),
        ),
        None => writeln!(out, "signer    -"),
    };
    let _ = writeln!(out, "explorer  {}", view.explorer.unwrap_or("-"));
    let _ = writeln!(out, "env");
    for var in &exports.set {
        let _ = writeln!(out, "  {}={}", var.name(), var.display());
    }
    if !exports.unset.is_empty() {
        let _ = writeln!(out, "unset     {}", exports.unset.join(" "));
    }
    let _ = writeln!(out, "argv      {argv}");
    out
}

/// `chainz exec CHAIN` with no command: `$SHELL` with the context
/// exported (the key too with `--expose-key`), until it exits.
fn shell(chainz: &Chainz, mut ctx: Context, explain_only: bool) -> Result<i32> {
    let shell = user_shell();
    if explain_only {
        print!(
            "{}",
            explain(
                &ctx,
                &exports(&ctx)?,
                &format!("{} (subshell)", shell_quote(&shell))
            )
        );
        return Ok(0);
    }
    ctx.unlock_key(chainz, &SystemKeyBackend, &crate::prompt::Terminal)?;
    let exports = exports(&ctx)?;
    eprintln!(
        "{}",
        ui::item(&format!("entering {} shell — ctrl-d to exit", ctx.chain()))
    );
    let mut command = std::process::Command::new(&shell);
    exports.apply(&mut command)?;
    command.env(
        "PS1",
        format!(
            "(⛓ {}) {}",
            ctx.chain(),
            std::env::var("PS1").unwrap_or_default()
        ),
    );
    let status = command
        .status()
        .map_err(|error| spawn_error(&shell, &error))?;
    eprintln!("{}", ui::dim(&format!("left {} shell", ctx.chain())));
    Ok(status.code().unwrap_or(1))
}

/// `chainz env`: shell exports for `eval`.
pub fn env(args: &EnvArgs) -> Result<i32> {
    let chainz = open()?;
    let runtime = Runtime::resolve()?;
    let ctx = context_for("env", &chainz, &runtime, &args.context, false)?;
    let exports = exports(&ctx)?;
    let kind =
        args.shell.unwrap_or_else(
            || match user_shell().rsplit('/').next().unwrap_or_default() {
                "fish" => ShellKind::Fish,
                "zsh" => ShellKind::Zsh,
                _ => ShellKind::Bash,
            },
        );
    let mut out = String::new();
    // A key the user exported themselves is theirs: `env` never unsets it.
    for name in exports.unset.iter().filter(|n| **n != RAW_PRIVATE_KEY) {
        out.push_str(&match kind {
            ShellKind::Fish => format!("set -e {name};\n"),
            _ => format!("unset {name};\n"),
        });
    }
    for var in &exports.set {
        out.push_str(&match kind {
            ShellKind::Fish => format!("set -gx {} {};\n", var.name(), fish_quote(var.value())),
            _ => format!("export {}={};\n", var.name(), shell_quote(var.value())),
        });
    }
    print!("{out}");
    Ok(0)
}

/// The user's shell: `$SHELL`, else `sh`.
fn user_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string())
}

/// POSIX single-quoting (only when needed).
fn shell_quote(value: &str) -> String {
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
