//! Tab 2: the selected chain's endpoints.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::grid::{Cell, Col, Grid};
use super::{Ctx, context_line, empty, scroll_hint, split_context};
use crate::fmt;
use crate::model::EndpointStatus;

pub(super) fn render(area: Rect, buf: &mut Buffer, ctx: Ctx) {
    let (top, body) = split_context(area);
    let t = ctx.theme;
    let Some(chain) = ctx.app.selected(ctx.state) else {
        empty(body, buf, vec![Line::styled("No chain selected.", t.dim())]);
        return;
    };

    let probed = match chain.probed_at_ms {
        Some(at) => Span::styled(
            format!(
                "probed {} ago",
                fmt::age(ctx.now_ms.saturating_sub(at) / 1_000)
            ),
            t.dim(),
        ),
        None if chain.endpoints.iter().any(|e| e.latest().is_some()) => {
            Span::styled("cached health · probing…", t.warn())
        }
        None => Span::styled("probing…", t.warn()),
    };
    context_line(
        top,
        buf,
        t,
        vec![
            vec![
                Span::styled(chain.plan.name.clone(), t.accent()),
                Span::styled(format!(" ({})", chain.plan.id), t.dim()),
            ],
            vec![Span::styled(
                format!("{} endpoints", chain.endpoints.len()),
                t.dim(),
            )],
            vec![
                Span::styled("active ", t.dim()),
                Span::styled(chain.plan.active.clone(), t.text()),
            ],
            vec![probed],
        ],
    );

    let statuses = chain.endpoint_statuses();
    let any_error = statuses
        .iter()
        .any(|(s, _)| matches!(s, EndpointStatus::Failing(_) | EndpointStatus::Invalid));
    let mut cols = vec![
        Col::new("ENDPOINT").max(18),
        Col::new("STATUS"),
        Col::new("LATENCY").right(),
        Col::new("P50").right().drop(4),
        Col::new("HEAD").right().drop(5),
        Col::new("LAG").right().drop(2),
    ];
    if any_error {
        cols.push(Col::new("ERROR").max(34).drop(1));
    }
    cols.push(Col::new("TAGS").max(28).drop(6));
    cols.push(Col::new("URL").max(60).drop(7));
    let mut grid = Grid::new(cols);

    let blank = || Cell::new("–", t.faint());
    for (endpoint, (status, lag)) in chain.endpoints.iter().zip(&statuses) {
        // Cached figures from an earlier run are shown, but dimmed.
        let fresh = if endpoint.live { t.text() } else { t.dim() };
        let sample = endpoint.latest();
        let mut row = vec![
            if endpoint.active {
                Cell::new("● ", t.accent()).then(endpoint.name.clone(), t.text())
            } else {
                Cell::new("  ", t.text()).then(endpoint.name.clone(), t.text())
            },
            match status {
                EndpointStatus::Unknown => Cell::new("…", t.faint()),
                EndpointStatus::Ok => Cell::new("ok", t.ok()),
                EndpointStatus::Lagging(_) => Cell::new("lagging", t.lag()),
                EndpointStatus::Failing(_) => Cell::new("failing", t.bad()),
                EndpointStatus::Websocket => Cell::new("ws", t.dim()),
                EndpointStatus::Invalid => Cell::new("invalid", t.bad()),
            },
            sample
                .filter(|s| s.error.is_none())
                .map_or_else(blank, |s| Cell::new(fmt::latency(s.latency_ms), fresh)),
            endpoint
                .p50()
                .map_or_else(blank, |ms| Cell::new(fmt::latency(ms), t.dim())),
            sample
                .and_then(|s| s.head)
                .map_or_else(blank, |head| Cell::new(fmt::group(head), fresh)),
            match lag {
                Some(0) => Cell::new("0", t.dim()),
                Some(n) => Cell::new(
                    fmt::group(*n),
                    if matches!(status, EndpointStatus::Lagging(_)) {
                        t.lag()
                    } else {
                        t.text()
                    },
                ),
                None => blank(),
            },
        ];
        if any_error {
            row.push(match status {
                EndpointStatus::Failing(class) => Cell::new(
                    match &endpoint.message {
                        Some(message) => message.clone(),
                        None => class.to_string(),
                    },
                    t.bad(),
                ),
                EndpointStatus::Invalid => {
                    Cell::new(endpoint.invalid.clone().unwrap_or_default(), t.bad())
                }
                _ => Cell::empty(),
            });
        }
        row.push(Cell::new(endpoint.tags.join(" "), t.dim()));
        row.push(Cell::new(endpoint.url.clone(), t.faint()));
        grid.rows.push(row);
    }
    let selected = ctx
        .app
        .endpoint
        .min(chain.endpoints.len().saturating_sub(1));
    let shown = grid.render(body, buf, t, Some(selected), Some(&ctx.app.scroll[1]));
    scroll_hint(top, buf, t, shown, chain.endpoints.len());
}
