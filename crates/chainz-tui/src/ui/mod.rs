//! Rendering, from ([`App`], [`State`], a clock) to a ratatui buffer. No
//! I/O; renderers read the time from [`Ctx`] (only [`Ctx::now`] looks at
//! the clock), and tables remember their scroll position in `App`'s
//! cells. So every frame is reproducible in a test.
//!
//! The Chains, Endpoints and Balances tabs describe what they show as a
//! [`Body`] (built from [`crate::view`] rows); [`render`] draws it, and
//! `--once` prints the same body as plain text.

pub(crate) mod balances;
pub(crate) mod chains;
mod endpoints;
pub mod grid;
#[cfg(test)]
pub(crate) mod tests;
pub mod theme;
mod traffic;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::app::{App, ScrollSlot, Tab};
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
            now_ms: chainz_os::now_ms(),
        }
    }
}

/// Rows of a screen that are not the tab's table or message: the title
/// bar, the context line and the footer.
pub const CHROME_ROWS: u16 = 3;

/// The height of a screen just tall enough for a table of `rows` rows and
/// its header (and never under six rows): what `--once` lays a message out
/// in, chrome included.
pub fn screen_height(rows: usize) -> u16 {
    u16::try_from(rows).unwrap_or(u16::MAX - 8).max(6) + 1 + CHROME_ROWS
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
        Tab::Chains => draw_body(body, buf, ctx, chains::body(ctx)),
        Tab::Endpoints => draw_body(body, buf, ctx, endpoints::body(ctx)),
        Tab::Balances => draw_body(body, buf, ctx, balances::body(ctx)),
        Tab::Traffic => traffic::render(body, buf, ctx),
    }
    footer(row(area.height - 1), buf, ctx);
}

/// What a tab shows under the title bar.
pub struct Body {
    /// The one-line summary (`a · b · c`), if the tab has one.
    pub context: Option<Vec<Vec<Span<'static>>>>,
    /// The rest.
    pub content: Content,
}

/// The main part of a [`Body`].
pub enum Content {
    /// A centred message: nothing to show, and why.
    Message(Vec<Line<'static>>),
    /// A table.
    Table {
        /// The rows.
        grid: Grid,
        /// The highlighted row.
        selected: Option<usize>,
        /// Where the table remembers its scroll position.
        scroll: ScrollSlot,
    },
}

use grid::Grid;

fn draw_body(area: Rect, buf: &mut Buffer, ctx: Ctx, body: Body) {
    let (top, rest) = split_context(area);
    let t = ctx.theme;
    if let Some(parts) = body.context {
        buf.set_line(top.x, top.y, &context_line(top.width, t, parts), top.width);
    }
    match body.content {
        Content::Message(lines) => empty(rest, buf, lines),
        Content::Table {
            grid,
            selected,
            scroll,
        } => {
            let shown = grid.render(rest, buf, t, selected, Some(ctx.app.scroll(scroll)));
            scroll_hint(top, buf, t, shown, grid.rows.len());
        }
    }
}

/// Render one frame off-screen and return it as text, one line per row,
/// trailing blanks trimmed.
#[cfg(test)]
pub fn render_text(ctx: Ctx, width: u16, height: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, ctx)).unwrap();
    let buf = terminal.backend().buffer();
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
        let hints = app.tab.hints();
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
/// the leading parts that fit whole in `width`.
pub fn context_line(width: u16, theme: Theme, parts: Vec<Vec<Span<'static>>>) -> Line<'static> {
    let mut line = vec![Span::raw(" ")];
    let mut used = 1;
    for (i, part) in parts.into_iter().enumerate() {
        let part_width = line_width(&part) + if i > 0 { 3 } else { 0 };
        if i > 0 && used + part_width > width {
            break;
        }
        if i > 0 {
            line.push(Span::styled(" · ", theme.faint()));
        }
        line.extend(part);
        used += part_width;
    }
    Line::from(line)
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

/// How far down an area `height` rows tall a centred message of `lines`
/// lines starts: a third of the room it leaves.
fn message_top(lines: usize, height: u16) -> u16 {
    let lines = u16::try_from(lines).unwrap_or(u16::MAX).min(height);
    (height - lines) / 3
}

/// A centred message for a tab with nothing to show.
fn empty(area: Rect, buf: &mut Buffer, lines: Vec<Line<'static>>) {
    let top = area.y + message_top(lines.len(), area.height);
    Paragraph::new(lines)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .render(Rect::new(area.x, top, area.width, area.bottom() - top), buf);
}

/// [`empty`]'s message as plain text in an area `width` × `height`: blank
/// lines down to where it starts, then each line centred as ratatui
/// centres it (and cut, where [`empty`] would wrap).
pub fn message_text(lines: &[Line], width: u16, height: u16) -> Vec<String> {
    let top = message_top(lines.len(), height);
    let mut out = vec![String::new(); usize::from(top)];
    for line in lines {
        let indent = (width / 2).saturating_sub(line_width(&line.spans) / 2);
        let text = " ".repeat(usize::from(indent)) + &plain(line, u16::MAX);
        out.push(fmt::cut(&text, usize::from(width)).trim_end().to_string());
    }
    out
}

/// A line's text without its styles, cut to `width` cells as a row that
/// wide shows it, trailing blanks trimmed.
pub fn plain(line: &Line, width: u16) -> String {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    fmt::cut(&text, usize::from(width)).trim_end().to_string()
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
