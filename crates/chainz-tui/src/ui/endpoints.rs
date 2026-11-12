//! Tab 2: the selected chain's endpoints.

use ratatui::text::{Line, Span};

use super::grid::{Cell, Col, Grid};
use super::{Body, Content, Ctx};
use crate::app::ScrollSlot;
use crate::fmt;
use crate::model::EndpointStatus;
use crate::view;

/// The Endpoints tab: the selected chain's endpoints with their health.
pub(super) fn body(ctx: Ctx) -> Body {
    let t = ctx.theme;
    let Some(chain) = ctx.app.selected(ctx.state) else {
        return Body {
            context: None,
            content: Content::Message(vec![Line::styled("No chain selected.", t.dim())]),
        };
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
    let context = vec![
        vec![
            Span::styled(chain.info.name.clone(), t.accent()),
            Span::styled(format!(" ({})", chain.info.id), t.dim()),
        ],
        vec![Span::styled(
            format!("{} endpoints", chain.endpoints.len()),
            t.dim(),
        )],
        vec![
            Span::styled("active ", t.dim()),
            Span::styled(chain.info.active.clone(), t.text()),
        ],
        vec![probed],
    ];

    let rows = view::endpoint_rows(chain);
    let any_error = rows.iter().any(|r| {
        matches!(
            r.status,
            EndpointStatus::Failing(_) | EndpointStatus::Invalid
        )
    });
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
    for r in &rows {
        let endpoint = r.endpoint;
        // Cached figures from an earlier run are shown, but dimmed.
        let fresh = if endpoint.live { t.text() } else { t.dim() };
        let name = endpoint.info.name.clone();
        let mut row = vec![
            if endpoint.active {
                Cell::new("● ", t.accent()).then(name, t.text())
            } else {
                Cell::new("  ", t.text()).then(name, t.text())
            },
            match r.status {
                EndpointStatus::Unknown => Cell::new("…", t.faint()),
                EndpointStatus::Ok => Cell::new("ok", t.ok()),
                EndpointStatus::Lagging(_) => Cell::new("lagging", t.lag()),
                EndpointStatus::Failing(_) => Cell::new("failing", t.bad()),
                EndpointStatus::Websocket => Cell::new("ws", t.dim()),
                EndpointStatus::Invalid => Cell::new("invalid", t.bad()),
            },
            r.latency_ms
                .map_or_else(blank, |ms| Cell::new(fmt::latency(ms), fresh)),
            r.p50_ms
                .map_or_else(blank, |ms| Cell::new(fmt::latency(ms), t.dim())),
            r.latest
                .and_then(|s| s.head)
                .map_or_else(blank, |head| Cell::new(fmt::group(head), fresh)),
            match r.lag {
                Some(0) => Cell::new("0", t.dim()),
                Some(n) => Cell::new(
                    fmt::group(n),
                    if matches!(r.status, EndpointStatus::Lagging(_)) {
                        t.lag()
                    } else {
                        t.text()
                    },
                ),
                None => blank(),
            },
        ];
        if any_error {
            row.push(match r.status {
                EndpointStatus::Failing(class) => Cell::new(
                    match &endpoint.message {
                        Some(message) => message.clone(),
                        None => class.to_string(),
                    },
                    t.bad(),
                ),
                EndpointStatus::Invalid => Cell::new(
                    endpoint
                        .info
                        .invalid
                        .clone()
                        .unwrap_or_else(|| "invalid".into()),
                    t.bad(),
                ),
                _ => Cell::empty(),
            });
        }
        row.push(Cell::new(endpoint.tags.join(" "), t.dim()));
        row.push(Cell::new(endpoint.info.url.clone(), t.faint()));
        grid.rows.push(row);
    }
    let selected = ctx
        .app
        .endpoint
        .min(chain.endpoints.len().saturating_sub(1));
    Body {
        context: Some(context),
        content: Content::Table {
            grid,
            selected: Some(selected),
            scroll: ScrollSlot::Endpoints,
        },
    }
}
