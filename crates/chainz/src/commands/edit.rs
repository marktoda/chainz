//! `edit [CHAIN]`: config.toml, or one chain's table, in `$EDITOR`
//! (validated on save).

use crate::cli::EditArgs;
use crate::load::open_lenient;
use crate::prompt::{Ask, Tty};
use crate::ui;
use anyhow::{Context, Result, bail};
use chainz_core::resolve::{self, Policy};

pub fn run(args: &EditArgs) -> Result<i32> {
    // Lenient, so a config that no longer validates can be fixed here;
    // opening also runs any pending v1 migration.
    let mut chainz = open_lenient()?;
    let saved = match &args.chain {
        None => {
            let original = chainz.config_text()?;
            edit_until_saved(&original, |edited| chainz.replace_text(&original, edited))?
                .then(|| format!("Saved {}", chainz.paths().config().display()))
        }
        Some(query) => {
            let name = resolve::configured(chainz.config(), query, Policy::Prefix)?
                .0
                .to_string();
            let snippet = chainz.chain_snippet(&name)?;
            let mut changes = Vec::new();
            edit_until_saved(&snippet.text, |edited| {
                changes = chainz.replace_chain_snippet(&snippet, edited)?;
                Ok(())
            })?
            .then(|| {
                let mut report = format!("Updated chain '{name}'");
                for change in &changes {
                    report.push_str(&format!("\n  {change}"));
                }
                report
            })
        }
    };
    println!("{}", saved.as_deref().unwrap_or("No changes"));
    Ok(0)
}

fn editor() -> String {
    std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string())
}

/// Open `text` in `$EDITOR` and return the edited text.
fn edit_text(text: &str, suffix: &str) -> Result<String> {
    let mut file = tempfile::Builder::new()
        .prefix("chainz-")
        .suffix(suffix)
        .tempfile()?;
    std::io::Write::write_all(&mut file, text.as_bytes())?;
    let path = file.path().to_path_buf();
    let editor = editor();
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .status()
        .with_context(|| format!("failed to start {editor}"))?;
    if !status.success() {
        bail!("{editor} exited with {status}; nothing changed");
    }
    Ok(std::fs::read_to_string(&path)?)
}

/// Edit `text` as TOML in `$EDITOR` until `save` accepts it. On a TTY a
/// rejected edit is shown and the user may edit again (from their edit);
/// off a TTY the error is returned. `Ok(false)` when nothing changed.
fn edit_until_saved(text: &str, mut save: impl FnMut(&str) -> Result<()>) -> Result<bool> {
    let mut current = text.to_string();
    loop {
        let edited = edit_text(&current, ".toml")?;
        if edited == current {
            return Ok(false);
        }
        match save(&edited) {
            Ok(()) => return Ok(true),
            Err(e) if Tty.is_interactive() => {
                eprintln!("{}", ui::fail(&format!("{e:#}")));
                if !Tty.confirm("Edit again?", true)? {
                    return Err(ui::cancelled());
                }
                current = edited;
            }
            Err(e) => return Err(e),
        }
    }
}
