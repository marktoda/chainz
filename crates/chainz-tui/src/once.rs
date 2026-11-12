//! `--once`: poll every chain once, then print the Chains table and one
//! Balances table per signer as plain text, or the JSON snapshot.

use std::sync::Arc;
use std::sync::mpsc::channel;

use anyhow::{Context, Result};
use chainz_core::Chainz;
use chainz_core::runtime::Runtime;
use chainz_sys::now_ms;
use ratatui::text::Line;
use unicode_width::UnicodeWidthChar;

use crate::app::{App, Tab};
use crate::data::{self, Event, Shared, Tuning};
use crate::model::State;
use crate::plan::Plan;
use crate::ui::theme::Theme;
use crate::ui::{self, Body, Content, Ctx};
use crate::{Options, load_store, snapshot};

/// The wait before retrying a rate-limited request (then twice that).
const RETRY_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// The width the text tables are laid out for.
const WIDTH: u16 = 120;

/// Poll every chain once and print the result.
pub(crate) fn run(chainz: &Chainz, runtime: Runtime, options: &Options) -> Result<i32> {
    let plan = Arc::new(Plan::build(chainz, options.reveal));
    let store = load_store();
    let mut state = State::new(&plan, &store);
    let (events, received) = channel();
    let shared = Shared::new(store, runtime, Tuning::default(), events);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    let collected = rt.block_on(data::collect_once(&shared, &plan, RETRY_WAIT));
    for event in received.try_iter() {
        match event {
            Event::Update(update) => state.apply(update),
            Event::Warning(text) => eprintln!("warning: {text}"),
            Event::ConfigChanged => {}
        }
    }
    for update in collected {
        state.apply(update);
    }
    let now = now_ms();
    if options.json {
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
/// table, or the message centred a third of the way down `room` lines.
fn frame(body: Body, heading: Option<String>, room: u16) -> String {
    let theme = Theme::new(false);
    let mut lines = vec![match (heading, body.context) {
        (Some(heading), _) => heading,
        (None, Some(parts)) => plain(&ui::context_line(WIDTH, theme, parts)),
        (None, None) => String::new(),
    }];
    match body.content {
        Content::Message(message) => {
            let height = u16::try_from(message.len()).unwrap_or(u16::MAX).min(room);
            lines.extend((0..(room - height) / 3).map(|_| String::new()));
            for line in &message {
                let width = u16::try_from(line.width()).unwrap_or(u16::MAX);
                let indent = (WIDTH / 2).saturating_sub(width / 2);
                let text = " ".repeat(usize::from(indent)) + &plain(line);
                lines.push(cut(&text, WIDTH));
            }
        }
        Content::Table { grid, .. } => {
            lines.extend(grid.plain_lines(WIDTH).iter().map(plain));
        }
    }
    let lines: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
    lines.join("\n").trim_end().to_string()
}

/// A line's text, cut to [`WIDTH`] cells.
fn plain(line: &Line) -> String {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    cut(&text, WIDTH)
}

/// `text` cut to at most `width` terminal cells, trailing blanks trimmed.
fn cut(text: &str, width: u16) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > usize::from(width) {
            break;
        }
        out.push(c);
        used += w;
    }
    out.truncate(out.trim_end().len());
    out
}
