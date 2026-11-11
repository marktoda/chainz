//! Tab 1: every chain at a glance.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::grid::{Cell, Col, Grid};
use super::theme::Theme;
use super::{Ctx, context_line, empty, scroll_hint, split_context};
use crate::fmt;
use crate::model::{ChainState, Status};

/// Sparkline width, in samples.
const SPARK: usize = 8;

pub(super) fn status_cell(status: Status, t: Theme) -> Cell {
    match status {
        Status::Pending => Cell::new("…", t.faint()),
        Status::Ok => Cell::new("ok", t.ok()),
        Status::Lag(n) => Cell::new(format!("lag {}", fmt::group(n)), t.lag()),
        Status::Slow => Cell::new("slow", t.warn()),
        Status::Down(class) => Cell::new(format!("down {class}"), t.bad()),
    }
}

fn spark_cell(chain: &ChainState, t: Theme) -> Cell {
    let samples: Vec<Option<u32>> = chain.samples.iter().copied().collect();
    let glyphs = fmt::sparkline(&samples, SPARK);
    let mut cell = Cell::new(" ".repeat(SPARK - glyphs.len()), Style::new());
    for glyph in glyphs {
        cell = match glyph {
            Some(bar) => cell.then(bar.to_string(), t.accent()),
            None => cell.then("×", t.bad()),
        };
    }
    cell
}

fn row(chain: &ChainState, status: Status, ctx: Ctx) -> Vec<Cell> {
    let t = ctx.theme;
    let blank = || Cell::new("–", t.faint());
    // A head that could not be refreshed is shown, but visibly stale.
    let live = if chain.fault.is_some() {
        t.dim()
    } else {
        t.text()
    };
    let head = chain.head.as_ref();
    vec![
        Cell::new(chain.plan.name.clone(), t.text()),
        Cell::new(chain.plan.id.to_string(), t.dim()),
        match head {
            Some(h) => Cell::new(fmt::group(h.number), live),
            None if chain.fault.is_some() => blank(),
            None => Cell::new("…", t.faint()),
        },
        chain.block_age(ctx.now_ms).map_or_else(blank, |age| {
            let style = match status {
                Status::Lag(_) => t.lag(),
                Status::Down(_) => t.dim(),
                _ => t.text(),
            };
            Cell::new(fmt::age(age), style)
        }),
        chain
            .block_time
            .map_or_else(blank, |secs| Cell::new(fmt::block_time(secs), t.dim())),
        head.and_then(|h| h.base_fee).map_or_else(blank, |fee| {
            Cell::new(fmt::gwei(fee), live).then(" gwei", t.faint())
        }),
        Cell::new(chain.plan.active.clone(), t.text()),
        chain.p50().map_or_else(blank, |ms| {
            let style = if status == Status::Slow {
                t.warn()
            } else {
                t.text()
            };
            Cell::new(fmt::latency(ms), style)
        }),
        spark_cell(chain, t),
        status_cell(status, t),
    ]
}

pub(super) fn render(area: Rect, buf: &mut Buffer, ctx: Ctx) {
    let (top, body) = split_context(area);
    let t = ctx.theme;
    if ctx.state.chains.is_empty() {
        empty(
            body,
            buf,
            vec![
                Line::styled("No chains configured yet.", t.text()),
                Line::raw(""),
                Line::from(vec![
                    Span::styled("Add one with  ", t.dim()),
                    Span::styled("chainz chain add base", t.accent()),
                ]),
            ],
        );
        return;
    }
    let slow_ms = ctx.state.slow_threshold();
    let visible = ctx.app.visible(ctx.state);
    let statuses: Vec<Status> = visible.iter().map(|c| c.status(slow_ms)).collect();

    let count = |pick: fn(&Status) -> bool| statuses.iter().filter(|s| pick(s)).count();
    let mut parts = vec![vec![Span::styled(
        format!("{} chains", ctx.state.chains.len()),
        t.dim(),
    )]];
    let tallies: [(usize, &str, Style); 5] = [
        (count(|s| matches!(s, Status::Ok)), "ok", t.ok()),
        (count(|s| matches!(s, Status::Lag(_))), "lag", t.lag()),
        (count(|s| matches!(s, Status::Slow)), "slow", t.warn()),
        (count(|s| matches!(s, Status::Down(_))), "down", t.bad()),
        (count(|s| matches!(s, Status::Pending)), "pending", t.dim()),
    ];
    for (n, word, style) in tallies {
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
    context_line(top, buf, t, parts);

    if visible.is_empty() {
        empty(
            body,
            buf,
            vec![Line::styled(
                format!("No chain matches /{}", ctx.app.filter),
                t.dim(),
            )],
        );
        return;
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
    grid.rows = visible
        .iter()
        .zip(&statuses)
        .map(|(chain, status)| row(chain, *status, ctx))
        .collect();
    let shown = grid.render(
        body,
        buf,
        t,
        Some(ctx.app.chain_index(ctx.state)),
        Some(&ctx.app.scroll[0]),
    );
    scroll_hint(top, buf, t, shown, visible.len());
}
