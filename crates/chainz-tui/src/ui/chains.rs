//! Tab 1: every chain at a glance.

use chainz_core::fmt as core_fmt;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::grid::{Cell, Col, Grid};
use super::theme::Theme;
use super::{Body, Content, Ctx};
use crate::app::ScrollSlot;
use crate::fmt;
use crate::model::{FaultClass, Status};
use crate::view::{self, ChainRow};

/// Sparkline width, in samples.
const SPARK: usize = 8;

pub(super) fn status_cell(status: Status, t: Theme) -> Cell {
    let word = status.word();
    match status {
        Status::Pending => Cell::new("…", t.faint()),
        Status::Ok => Cell::new(word, t.ok()),
        Status::Lag(n) => Cell::new(format!("{word} {}", core_fmt::group(n)), t.lag()),
        Status::Slow => Cell::new(word, t.warn()),
        Status::Down(class) => Cell::new(format!("{word} {class}"), t.bad()),
    }
}

fn spark_cell(samples: &[Option<u32>], t: Theme) -> Cell {
    let glyphs = fmt::sparkline(samples, SPARK);
    let mut cell = Cell::new(" ".repeat(SPARK - glyphs.len()), Style::new());
    for glyph in glyphs {
        cell = match glyph {
            Some(bar) => cell.then(bar.to_string(), t.accent()),
            None => cell.then("×", t.bad()),
        };
    }
    cell
}

fn row(r: &ChainRow, t: Theme) -> Vec<Cell> {
    let chain = r.chain;
    let blank = || Cell::new("–", t.faint());
    // A head that could not be refreshed is shown, but visibly stale.
    let live = if chain.fault.is_some() {
        t.dim()
    } else {
        t.text()
    };
    let head = chain.head.as_ref();
    let samples: Vec<Option<u32>> = chain.samples.iter().copied().collect();
    vec![
        Cell::new(chain.info.name.clone(), t.text()),
        Cell::new(chain.info.id.to_string(), t.dim()),
        match head {
            Some(h) => Cell::new(core_fmt::group(h.number), live),
            None if chain.fault.is_some() => blank(),
            None => Cell::new("…", t.faint()),
        },
        r.block_age_secs.map_or_else(blank, |age| {
            let style = match r.status {
                Status::Lag(_) => t.lag(),
                Status::Down(_) => t.dim(),
                _ => t.text(),
            };
            Cell::new(core_fmt::age(age), style)
        }),
        r.block_time_secs
            .map_or_else(blank, |secs| Cell::new(fmt::block_time(secs), t.dim())),
        r.base_fee_gwei().map_or_else(blank, |gwei| {
            Cell::new(fmt::gwei(gwei), live).then(" gwei", t.faint())
        }),
        Cell::new(r.rpc, t.text()),
        r.p50_ms.map_or_else(blank, |ms| {
            let style = if r.status == Status::Slow {
                t.warn()
            } else {
                t.text()
            };
            Cell::new(fmt::latency(ms), style)
        }),
        spark_cell(&samples, t),
        status_cell(r.status, t),
    ]
}

/// The Chains tab: a tally and one row per visible chain.
pub(crate) fn body(ctx: Ctx) -> Body {
    let t = ctx.theme;
    if ctx.state.chains.is_empty() {
        return Body {
            context: None,
            content: Content::Message(vec![
                Line::styled("No chains configured yet.", t.text()),
                Line::raw(""),
                Line::from(vec![
                    Span::styled("Add one with  ", t.dim()),
                    Span::styled("chainz add base", t.accent()),
                ]),
            ]),
        };
    }
    let visible = ctx.app.visible(ctx.state);
    let rows = view::chain_rows(ctx.state, visible.iter().copied(), ctx.now_ms);

    let mut parts = vec![vec![Span::styled(
        format!("{} chains", ctx.state.chains.len()),
        t.dim(),
    )]];
    let tallies: [(&str, Style); 5] = [
        (Status::Ok.word(), t.ok()),
        (Status::Lag(0).word(), t.lag()),
        (Status::Slow.word(), t.warn()),
        (Status::Down(FaultClass::Internal).word(), t.bad()),
        (Status::Pending.word(), t.dim()),
    ];
    for (word, style) in tallies {
        let n = rows.iter().filter(|r| r.status.word() == word).count();
        if n > 0 {
            parts.push(vec![Span::styled(format!("{n} {word}"), style)]);
        }
    }
    if !ctx.app.filter.is_empty() {
        parts.push(vec![
            Span::styled("filter ", t.dim()),
            Span::styled(format!("/{}", ctx.app.filter), t.accent()),
            Span::styled(format!(" ({} shown)", visible.len()), t.dim()),
        ]);
    }
    if visible.is_empty() {
        return Body {
            context: Some(parts),
            content: Content::Message(vec![Line::styled(
                format!("No chain matches /{}", ctx.app.filter),
                t.dim(),
            )]),
        };
    }
    let mut grid = Grid::new(vec![
        Col::new("CHAIN").max(14),
        Col::new("ID").drop(6),
        Col::new("HEAD").right(),
        Col::new("AGE").right().drop(1),
        Col::new("BLOCK").right().drop(5),
        Col::new("BASE FEE").right().drop(4),
        Col::new("RPC").max(14).drop(2),
        Col::new("P50").right().drop(3),
        Col::new("HEALTH").drop(7),
        Col::new("STATUS").max(20),
    ]);
    grid.rows = rows.iter().map(|r| row(r, t)).collect();
    Body {
        context: Some(parts),
        content: Content::Table {
            grid,
            selected: Some(ctx.app.chain_index(ctx.state)),
            scroll: ScrollSlot::Chains,
        },
    }
}
