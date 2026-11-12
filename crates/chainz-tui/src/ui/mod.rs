//! Rendering. Pure functions from ([`App`], [`State`], a clock) to a
//! ratatui buffer: no I/O and no wall-clock reads, so every frame is
//! reproducible in a test.

mod balances;
mod chains;
mod endpoints;
pub mod grid;
#[cfg(test)]
pub(crate) mod tests;
pub mod theme;
mod traffic;

use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::app::{App, Tab};
use crate::fmt;
use crate::model::State;
use theme::Theme;

/// What every tab renderer gets.
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    /// UI state.
    pub app: &'a App,
    /// Data.
    pub state: &'a State,
    /// The palette.
    pub theme: Theme,
    /// The frame's time, unix milliseconds.
    pub now_ms: u64,
}

impl<'a> Ctx<'a> {
    /// A frame context stamped with the current time.
    pub fn now(app: &'a App, state: &'a State, theme: Theme) -> Self {
        Ctx {
            app,
            state,
            theme,
            now_ms: chainz_sys::now_ms(),
        }
    }
}

/// Smallest terminal the dashboard lays out in.
const MIN_WIDTH: u16 = 28;
const MIN_HEIGHT: u16 = 5;

/// Draw one frame.
pub fn draw(frame: &mut Frame, ctx: Ctx) {
    render(frame.area(), frame.buffer_mut(), ctx);
}

/// Draw the dashboard into `buf`.
pub fn render(area: Rect, buf: &mut Buffer, ctx: Ctx) {
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        Paragraph::new("terminal too small")
            .style(ctx.theme.dim())
            .wrap(Wrap { trim: true })
            .render(area, buf);
        return;
    }
    let row = |y: u16| Rect::new(area.x, area.y + y, area.width, 1);
    title_bar(row(0), buf, ctx);
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 2);
    match ctx.app.tab {
        Tab::Chains => chains::render(body, buf, ctx),
        Tab::Endpoints => endpoints::render(body, buf, ctx),
        Tab::Balances => balances::render(body, buf, ctx),
        Tab::Traffic => traffic::render(body, buf, ctx),
    }
    footer(row(area.height - 1), buf, ctx);
}

/// Render one frame off-screen and return it as text, one line per row,
/// trailing blanks trimmed. Used by `--once` and by the tests.
pub fn render_text(ctx: Ctx, width: u16, height: u16) -> String {
    let mut terminal =
        Terminal::new(TestBackend::new(width, height)).expect("a test backend cannot fail");
    terminal
        .draw(|frame| draw(frame, ctx))
        .expect("a test backend cannot fail");
    buffer_text(terminal.backend().buffer())
}

/// A buffer's symbols as lines of text.
pub fn buffer_text(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in area.y..area.bottom() {
        let mut line = String::new();
        for x in area.x..area.right() {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn line_width(spans: &[Span]) -> u16 {
    let cells: usize = spans.iter().map(|s| fmt::width(&s.content)).sum();
    u16::try_from(cells).unwrap_or(u16::MAX)
}

/// The top row: product, tabs, gateway. Sheds detail as it narrows.
fn title_bar(area: Rect, buf: &mut Buffer, ctx: Ctx) {
    let t = ctx.theme;
    let tabs = |names: bool| -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for (i, tab) in Tab::ALL.into_iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(if names { "  " } else { " " }));
            }
            let active = tab == ctx.app.tab;
            let label = if names || active {
                format!("[{}] {}", i + 1, tab.title())
            } else {
                format!("[{}]", i + 1)
            };
            spans.push(Span::styled(
                label,
                if active { t.tab_active() } else { t.dim() },
            ));
        }
        spans
    };
    let gateway = |long: bool| -> Vec<Span<'static>> {
        let label = if long { "serve: " } else { "gw " };
        match &ctx.state.gateway {
            Some(info) => vec![
                Span::styled(label, t.dim()),
                Span::styled(format!("on :{}", info.port), t.ok()),
                Span::raw(" "),
            ],
            None => vec![Span::styled(format!("{label}off "), t.dim())],
        }
    };
    let brand = || -> Vec<Span<'static>> {
        vec![
            Span::styled(
                " chainz top",
                t.accent().add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::styled(" ─ ", t.faint()),
        ]
    };
    // (brand, tab names, long gateway label), most detail first.
    let variants = [
        (true, true, true),
        (false, true, true),
        (false, true, false),
        (false, false, false),
    ];
    for (with_brand, names, long) in variants {
        let mut left = if with_brand {
            brand()
        } else {
            vec![Span::raw(" ")]
        };
        left.extend(tabs(names));
        let right = gateway(long);
        let last = (with_brand, names, long) == variants[variants.len() - 1];
        let (left_width, right_width) = (line_width(&left), line_width(&right));
        let fits = left_width + 2 + right_width <= area.width;
        if fits || last {
            buf.set_line(area.x, area.y, &Line::from(left), area.width);
            // On the narrowest terminals the tabs win over the gateway.
            if fits {
                let x = area.right() - right_width;
                buf.set_line(x, area.y, &Line::from(right), right_width);
            }
            return;
        }
    }
}

/// The bottom row: key hints (or the filter prompt), and a notice.
fn footer(area: Rect, buf: &mut Buffer, ctx: Ctx) {
    let t = ctx.theme;
    let app = ctx.app;
    let notice: Vec<Span> = match &app.notice {
        Some(n) => vec![
            Span::styled(n.text.clone(), if n.error { t.bad() } else { t.accent() }),
            Span::raw(" "),
        ],
        None => Vec::new(),
    };
    let notice_width = line_width(&notice).min(area.width);
    let room = area
        .width
        .saturating_sub(notice_width + u16::from(notice_width > 0));
    if app.filtering {
        let prompt = Line::from(vec![
            Span::styled(" /", t.accent()),
            Span::raw(app.filter.clone()),
            Span::styled("▏", t.accent()),
            Span::styled("  enter: keep  esc: clear", t.dim()),
        ]);
        buf.set_line(area.x, area.y, &prompt, room);
    } else {
        let hints: &[(&str, &str)] = match app.tab {
            Tab::Chains => &[
                ("enter", "endpoints"),
                ("b", "balances"),
                ("r", "refresh"),
                ("s", "set rpc"),
                ("e", "exec shell"),
                ("/", "filter"),
                ("y", "copy url"),
                ("q", "quit"),
            ],
            Tab::Endpoints => &[
                ("s", "set active"),
                ("h/l", "chain"),
                ("r", "probe now"),
                ("y", "copy url"),
                ("e", "exec shell"),
                ("esc", "back"),
                ("q", "quit"),
            ],
            Tab::Balances => &[
                ("h/l", "signer"),
                ("r", "refresh"),
                ("y", "copy address"),
                ("/", "filter"),
                ("e", "exec shell"),
                ("q", "quit"),
            ],
            Tab::Traffic => &[("r", "refresh"), ("1-4", "tabs"), ("q", "quit")],
        };
        // Keep the last hint (quit) and as many of the others as fit.
        let width_of = |hints: &[&(&str, &str)]| -> u16 {
            let cells: usize = hints.iter().map(|(k, v)| k.len() + v.len() + 4).sum();
            u16::try_from(cells).unwrap_or(u16::MAX)
        };
        let mut shown: Vec<&(&str, &str)> = hints.iter().collect();
        while shown.len() > 1 && width_of(&shown) > room {
            shown.remove(shown.len() - 2);
        }
        let mut spans = vec![Span::raw(" ")];
        for (key, what) in shown {
            spans.push(Span::styled(*key, t.key()));
            spans.push(Span::styled(format!(": {what}  "), t.dim()));
        }
        buf.set_line(area.x, area.y, &Line::from(spans), room);
    }
    if notice_width > 0 {
        buf.set_line(
            area.right() - notice_width,
            area.y,
            &Line::from(notice),
            notice_width,
        );
    }
}

/// The one-line summary under the title bar: `a · b · c`, keeping only
/// the leading parts that fit whole.
fn context_line(area: Rect, buf: &mut Buffer, theme: Theme, parts: Vec<Vec<Span<'static>>>) {
    let mut line = vec![Span::raw(" ")];
    let mut used = 1;
    for (i, part) in parts.into_iter().enumerate() {
        let width = line_width(&part) + if i > 0 { 3 } else { 0 };
        if i > 0 && used + width > area.width {
            break;
        }
        if i > 0 {
            line.push(Span::styled(" · ", theme.faint()));
        }
        line.extend(part);
        used += width;
    }
    buf.set_line(area.x, area.y, &Line::from(line), area.width);
}

/// Say which rows are on screen when the table is longer than the
/// terminal: right-aligned on the context line.
fn scroll_hint(
    area: Rect,
    buf: &mut Buffer,
    theme: Theme,
    shown: Option<(usize, usize)>,
    total: usize,
) {
    let Some((first, last)) = shown else { return };
    let text = format!(" {first}–{last} of {total} ");
    let width = u16::try_from(fmt::width(&text)).unwrap_or(u16::MAX);
    if width + 20 <= area.width {
        let line = Line::styled(text, theme.dim());
        buf.set_line(area.right() - width, area.y, &line, width);
    }
}

/// A centred message for a tab with nothing to show.
fn empty(area: Rect, buf: &mut Buffer, lines: Vec<Line<'static>>) {
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    let top = area.y + (area.height - height) / 3;
    Paragraph::new(lines)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .render(Rect::new(area.x, top, area.width, area.bottom() - top), buf);
}

/// Split `area` into the context line and the rest.
fn split_context(area: Rect) -> (Rect, Rect) {
    let top = Rect::new(area.x, area.y, area.width, 1.min(area.height));
    let rest = Rect::new(
        area.x,
        area.y + top.height,
        area.width,
        area.height - top.height,
    );
    (top, rest)
}
