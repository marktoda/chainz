//! `--once`: poll every chain once, then print the Chains table and one
//! Balances table per signer as plain text, or the JSON snapshot.

use std::sync::Arc;
use std::sync::mpsc::channel;

use anyhow::{Context, Result};
use chainz_core::Chainz;
use chainz_core::runtime::Runtime;
use chainz_rpc::HealthStore;
use chainz_os::now_ms;

use crate::app::{App, Tab};
use crate::data::{self, Event, Shared, Tuning};
use crate::model::State;
use crate::plan::Plan;
use crate::ui::theme::Theme;
use crate::ui::{self, Body, Content, Ctx};
use crate::{WarnFn, snapshot};

/// The wait before retrying a rate-limited request (then twice that).
const RETRY_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// The width the text tables are laid out for.
const WIDTH: u16 = 120;

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
    rt.block_on(data::collect_once(&shared, &plan, RETRY_WAIT));
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
/// key hints, the selection or colour.
pub(crate) fn text(state: &State, now_ms: u64) -> String {
    // The room a message gets: what the screen would leave under the
    // context line if it were just tall enough for every chain.
    let rows = u16::try_from(state.chains.len()).unwrap_or(u16::MAX - 8);
    let room = rows.max(6) + 1;
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
    for (i, signer) in state.signers.iter().enumerate() {
        if let Some(address) = signer.address {
            app.signer = i;
            // The signer chips become one plain heading.
            let heading = format!(" signer {} {}", signer.name, address.to_checksum());
            frames.push(frame(
                ui::balances::body(ctx(&app, state, now_ms)),
                Some(heading),
                room,
            ));
        }
    }
    let mut out = frames.join("\n\n");
    out.truncate(out.trim_end().len());
    out.push('\n');
    out
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
