//! `--once`: poll every chain once, then print the Chains table and one
//! Balances table per signer as plain text, or the JSON snapshot.

use std::sync::Arc;
use std::sync::mpsc::channel;

use anyhow::{Context, Result};
use chainz_core::Chainz;
use chainz_core::runtime::Runtime;
use chainz_os::now_ms;
use chainz_rpc::HealthStore;

use crate::app::{App, Tab};
use crate::data::{self, Event, Shared, Tuning};
use crate::model::{State, Status};
use crate::plan::Plan;
use crate::snapshot;
use crate::ui::theme::Theme;
use crate::ui::{self, Body, Content, Ctx};

/// Prints a warning for `--once` (an unreadable gateway record, a health
/// history that cannot be saved); the live dashboard shows them in its
/// footer instead.
pub type WarnFn = Box<dyn FnMut(&str)>;

/// The width the text tables are laid out for.
pub(crate) const WIDTH: u16 = 120;

/// Poll every chain once and print the result; warnings go to `warn`.
pub(crate) fn run(
    chainz: &Chainz,
    runtime: Runtime,
    store: HealthStore,
    reveal: bool,
    json: bool,
    mut warn: WarnFn,
) -> Result<i32> {
    let plan = Arc::new(Plan::build(chainz, reveal));
    let mut state = State::new(&plan, &store);
    let (events, received) = channel();
    let shared = Shared::new(store, runtime, Tuning::default(), events);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    rt.block_on(data::collect_once(&shared, &plan));
    for event in received.try_iter() {
        match event {
            Event::Update(update) => state.apply(update),
            Event::Warning(text) => warn(&text),
        }
    }
    let now = now_ms();
    if json {
        let snapshot = snapshot::Snapshot::new(&state, now);
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else {
        print!("{}", text(&state, now));
    }
    Ok(0)
}

/// The Chains table, then one Balances table per signer with a cached
/// address, as plain text: the live tabs' bodies without the tab bar, the
/// key hints, the selection or colour. Chains that are down (with no
/// balances read) are left out of the Balances tables and named in one
/// line after them.
pub(crate) fn text(state: &State, now_ms: u64) -> String {
    // The room a message gets: what the screen would leave under the
    // context line if it were just tall enough for every chain.
    let room = ui::screen_height(state.chains.len()) - ui::CHROME_ROWS;
    let mut app = App::default();
    fn ctx<'a>(app: &'a App, state: &'a State, now_ms: u64) -> Ctx<'a> {
        Ctx {
            app,
            state,
            theme: Theme::new(false),
            now_ms,
        }
    }
    let mut frames = vec![frame(
        ui::chains::body(ctx(&app, state, now_ms)),
        None,
        room,
    )];
    app.tab = Tab::Balances;
    let (up, down) = without_down_chains(state);
    let signed = state.signers.iter().any(|s| s.address.is_some());
    for (i, signer) in up.signers.iter().enumerate() {
        if let Some(address) = signer.address
            && !up.chains.is_empty()
        {
            app.signer = i;
            // The signer chips become one plain heading.
            let heading = format!(" signer {} {}", signer.name, address.to_checksum());
            frames.push(frame(
                ui::balances::body(ctx(&app, &up, now_ms)),
                Some(heading),
                room,
            ));
        }
    }
    if signed && !down.is_empty() {
        frames.push(skipped_note(&down));
    }
    let mut out = frames.join("\n\n");
    out.truncate(out.trim_end().len());
    out.push('\n');
    out
}

/// `state` without the chains that are down and have no balances to
/// show, and those chains' names: `--once` prints one line for them
/// instead of a row of `–` per signer.
pub(crate) fn without_down_chains(state: &State) -> (State, Vec<String>) {
    let slow = state.slow_threshold();
    let mut up = state.clone();
    let mut down = Vec::new();
    up.chains.retain(|c| {
        let skip = matches!(c.status(slow), Status::Down(_)) && c.balances.is_none();
        if skip {
            down.push(c.info.name.clone());
        }
        !skip
    });
    (up, down)
}

/// The line that stands for the balances of the chains that are down.
fn skipped_note(down: &[String]) -> String {
    format!(
        " balances skipped on down chains: {}",
        chainz_core::fmt::names_or_count(down, "chain")
    )
}

/// One tab's body as text: the context line (or `heading`), then the
/// table, or the message laid out as on a screen with `room` lines under
/// the context line.
fn frame(body: Body, heading: Option<String>, room: u16) -> String {
    let theme = Theme::new(false);
    let mut lines = vec![match (heading, body.context) {
        (Some(heading), _) => heading,
        (None, Some(parts)) => ui::plain(&ui::context_line(WIDTH, theme, parts), WIDTH),
        (None, None) => String::new(),
    }];
    match body.content {
        Content::Message(message) => lines.extend(ui::message_text(&message, WIDTH, room)),
        Content::Table { grid, .. } => lines.extend(grid.plain_lines(WIDTH)),
    }
    let lines: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
    lines.join("\n").trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::skipped_note;

    #[test]
    fn a_few_down_chains_are_named_and_many_are_counted() {
        let names = |n: usize| (0..n).map(|i| format!("c{i}")).collect::<Vec<_>>();
        assert_eq!(
            skipped_note(&names(2)),
            " balances skipped on down chains: c0, c1"
        );
        assert_eq!(
            skipped_note(&names(17)),
            " balances skipped on down chains: 17 chains"
        );
    }
}
