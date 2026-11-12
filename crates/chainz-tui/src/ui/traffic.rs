//! Tab 4: what the gateway is doing, when one runs.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::grid::{Cell, Col, Grid};
use super::{Ctx, context_line, empty, split_context};
use crate::fmt;
use crate::model::{TrafficRow, TrafficWindow};
use crate::view;

/// At this width the feed and the tables sit side by side.
const WIDE: u16 = 110;
/// Width of the side tables in the wide layout.
const SIDE: u16 = 50;

/// A titled table in `area`; rows beyond the height are cut.
fn section(area: Rect, buf: &mut Buffer, ctx: Ctx, title: String, grid: &Grid) {
    if area.height < 3 {
        return;
    }
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(title, ctx.theme.accent()),
    ]);
    buf.set_line(area.x, area.y, &title, area.width);
    // Leave the last row blank, so stacked sections do not touch.
    let body = Rect::new(area.x, area.y + 1, area.width, (area.height - 2).max(2));
    grid.render(body, buf, ctx.theme, None, None);
}

fn count(n: u64, t: super::theme::Theme, bad: bool) -> Cell {
    match (n, bad) {
        (0, _) => Cell::new("0", t.faint()),
        (n, true) => Cell::new(fmt::group(n), t.bad()),
        (n, false) => Cell::new(fmt::group(n), t.text()),
    }
}

fn feed_grid(ctx: Ctx, rows: usize) -> Grid {
    let t = ctx.theme;
    let mut grid = Grid::new(vec![
        Col::new("AGO").right(),
        Col::new("CHAIN").max(10),
        Col::new("METHOD").max(26),
        Col::new("DETAIL").max(44).drop(3),
        Col::new("VIA").max(14).drop(2),
        Col::new("MS").right(),
        Col::new("STATUS").max(18),
    ]);
    let feed = &ctx.state.feed;
    for item in feed.iter().skip(feed.len().saturating_sub(rows)) {
        let status = match &item.error {
            None => Cell::new("ok", t.ok()),
            Some(error) => Cell::new(error.clone(), t.bad()),
        };
        grid.rows.push(vec![
            Cell::new(
                fmt::age(ctx.now_ms.saturating_sub(item.at_ms) / 1_000),
                t.dim(),
            ),
            Cell::new(item.chain.clone(), t.text()),
            Cell::new(item.method.clone(), t.text()),
            Cell::new(item.summary.clone(), t.dim()),
            Cell::new(item.upstream.clone(), t.dim()),
            Cell::new(fmt::group(u64::from(item.latency_ms)), t.text()),
            if item.attempts > 1 {
                status.then(format!(" ×{}", item.attempts), t.warn())
            } else {
                status
            },
        ]);
    }
    grid
}

fn methods_grid(ctx: Ctx, rows: &[TrafficRow]) -> Grid {
    let t = ctx.theme;
    let mut grid = Grid::new(vec![
        Col::new("METHOD").max(28),
        Col::new("REQS").right(),
        Col::new("ERR").right(),
        Col::new("P50").right(),
        Col::new("P95").right().drop(1),
    ]);
    for r in rows {
        grid.rows.push(vec![
            Cell::new(r.name.clone(), t.text()),
            count(r.requests, t, false),
            count(r.errors, t, true),
            Cell::new(fmt::latency(r.p50_ms), t.text()),
            Cell::new(fmt::latency(r.p95_ms), t.dim()),
        ]);
    }
    grid
}

fn endpoints_grid(ctx: Ctx, rows: &[TrafficRow]) -> Grid {
    let t = ctx.theme;
    let mut grid = Grid::new(vec![
        Col::new("ENDPOINT").max(24),
        Col::new("REQS").right(),
        Col::new("ERR").right(),
        Col::new("429").right(),
        Col::new("FAILOVER").right().drop(1),
        Col::new("P50").right().drop(2),
    ]);
    for r in rows {
        grid.rows.push(vec![
            Cell::new(r.name.clone(), t.text()),
            count(r.requests, t, false),
            count(r.errors, t, true),
            match r.rate_limited {
                0 => Cell::new("0", t.faint()),
                n => Cell::new(fmt::group(n), t.warn()),
            },
            count(r.failovers, t, false),
            Cell::new(fmt::latency(r.p50_ms), t.text()),
        ]);
    }
    grid
}

fn summary(ctx: Ctx, window: Option<&TrafficWindow>) -> Vec<Vec<Span<'static>>> {
    let t = ctx.theme;
    let mut parts = vec![vec![
        Span::styled(format!("{:.1}", ctx.state.feed_rate(ctx.now_ms)), t.text()),
        Span::styled(" req/s", t.dim()),
    ]];
    if let Some(w) = window {
        let per = fmt::window(w.window_secs);
        parts.push(vec![
            Span::styled(fmt::group(w.requests), t.text()),
            Span::styled(format!(" req/{per}"), t.dim()),
        ]);
        let tally = |n: u64, one: &str, many: &str, style| {
            vec![Span::styled(
                format!("{} {}", fmt::group(n), if n == 1 { one } else { many }),
                if n == 0 { t.dim() } else { style },
            )]
        };
        parts.push(tally(w.errors, "error", "errors", t.bad()));
        parts.push(tally(w.rate_limited, "× 429", "× 429", t.warn()));
        parts.push(tally(w.failovers, "failover", "failovers", t.warn()));
    }
    if let Some(traffic) = &ctx.state.traffic {
        parts.push(vec![Span::styled(
            format!("up {}", fmt::uptime(traffic.uptime_secs)),
            t.dim(),
        )]);
        parts.push(vec![Span::styled(
            format!("v{}", traffic.version),
            t.faint(),
        )]);
    }
    parts
}

pub(super) fn render(area: Rect, buf: &mut Buffer, ctx: Ctx) {
    let (top, body) = split_context(area);
    let t = ctx.theme;
    let state = ctx.state;
    if state.gateway.is_none() {
        empty(
            body,
            buf,
            vec![
                Line::styled("No gateway is running.", t.text()),
                Line::raw(""),
                Line::from(vec![
                    Span::styled("Start one with  ", t.dim()),
                    Span::styled("chainz serve", t.accent()),
                ]),
                Line::raw(""),
                Line::styled(
                    "exec, your tools and this dashboard then share one failover-aware URL per chain,",
                    t.dim(),
                ),
                Line::styled("and every request shows up here.", t.dim()),
            ],
        );
        return;
    }
    let Some(traffic) = &state.traffic else {
        let lines = match &state.traffic_error {
            Some(error) => vec![
                Line::styled("The gateway's control socket did not answer.", t.bad()),
                Line::styled(error.clone(), t.dim()),
            ],
            None => vec![Line::styled("Connecting to the gateway…", t.dim())],
        };
        empty(body, buf, lines);
        return;
    };
    let window = view::traffic_window(traffic);
    let mut parts = summary(ctx, window);
    if let Some(error) = &state.traffic_error {
        // First, so a narrow terminal cannot cut the warning off.
        parts.insert(0, vec![Span::styled(format!("stale: {error}"), t.bad())]);
    }
    buf.set_line(top.x, top.y, &context_line(top.width, t, parts), top.width);

    let per = window.map_or_else(String::new, |w| {
        format!(" · {}", fmt::window(w.window_secs))
    });
    let no_rows: &[TrafficRow] = &[];
    let methods = methods_grid(ctx, window.map_or(no_rows, |w| &w.methods));
    let endpoints = endpoints_grid(ctx, window.map_or(no_rows, |w| &w.endpoints));
    let feed_in = |area: Rect, buf: &mut Buffer| {
        let rows = usize::from(area.height.saturating_sub(3).max(1));
        let grid = feed_grid(ctx, rows);
        section(area, buf, ctx, "LIVE REQUESTS".into(), &grid);
        if grid.rows.is_empty() && area.height >= 3 {
            let waiting = Line::styled("  waiting for requests…", t.dim());
            buf.set_line(area.x, area.y + 2, &waiting, area.width);
        }
    };
    let (methods_title, endpoints_title) = (format!("TOP METHODS{per}"), format!("ENDPOINTS{per}"));

    if body.width >= WIDE {
        let feed = Rect::new(body.x, body.y, body.width - SIDE - 1, body.height);
        feed_in(feed, buf);
        let x = feed.right() + 1;
        let upper = body.height / 2;
        section(
            Rect::new(x, body.y, SIDE, upper),
            buf,
            ctx,
            methods_title,
            &methods,
        );
        section(
            Rect::new(x, body.y + upper, SIDE, body.height - upper),
            buf,
            ctx,
            endpoints_title,
            &endpoints,
        );
    } else if body.height < 14 {
        feed_in(body, buf);
    } else {
        // Stacked: the feed gets half, the two tables share the rest.
        let feed = body.height / 2;
        let rest = body.height - feed;
        let upper = rest / 2;
        feed_in(Rect::new(body.x, body.y, body.width, feed), buf);
        section(
            Rect::new(body.x, body.y + feed, body.width, upper),
            buf,
            ctx,
            methods_title,
            &methods,
        );
        section(
            Rect::new(body.x, body.y + feed + upper, body.width, rest - upper),
            buf,
            ctx,
            endpoints_title,
            &endpoints,
        );
    }
}
