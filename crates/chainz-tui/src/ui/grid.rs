//! An aligned table that degrades gracefully: columns size to their
//! content, the least important ones drop out as the terminal narrows, and
//! the selection stays in view.

use std::cell::Cell as Scroll;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::theme::Theme;
use crate::fmt;

/// Space between columns.
const GAP: u16 = 2;
/// The selection marker's column.
const MARGIN: u16 = 1;

/// One column.
#[derive(Debug, Clone)]
pub struct Col {
    /// Header text.
    pub title: String,
    /// Right-align (numbers).
    pub right: bool,
    /// When to drop it on a narrow terminal: 0 never, higher sooner.
    pub drop: u8,
    /// Widest it may grow; longer cells are cut with `…`.
    pub max: u16,
}

impl Col {
    /// A left-aligned column that is never dropped.
    pub fn new(title: impl Into<String>) -> Self {
        Col {
            title: title.into(),
            right: false,
            drop: 0,
            max: 40,
        }
    }

    /// Right-align it.
    pub fn right(mut self) -> Self {
        self.right = true;
        self
    }

    /// Drop it on narrow terminals; higher goes first.
    pub fn drop(mut self, order: u8) -> Self {
        self.drop = order;
        self
    }

    /// Cap its width.
    pub fn max(mut self, max: u16) -> Self {
        self.max = max;
        self
    }
}

/// One cell: styled text.
#[derive(Debug, Clone, Default)]
pub struct Cell {
    spans: Vec<Span<'static>>,
}

impl Cell {
    /// An empty cell.
    pub fn empty() -> Self {
        Cell::default()
    }

    /// A cell of one style.
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Cell {
            spans: vec![Span::styled(text.into(), style)],
        }
    }

    /// Append more text in another style.
    pub fn then(mut self, text: impl Into<String>, style: Style) -> Self {
        self.spans.push(Span::styled(text.into(), style));
        self
    }

    fn width(&self) -> u16 {
        let cells: usize = self.spans.iter().map(|s| fmt::width(&s.content)).sum();
        u16::try_from(cells).unwrap_or(u16::MAX)
    }

    /// The spans, cut to `max` cells with a trailing `…` if needed.
    fn fitted(&self, max: u16) -> Vec<Span<'static>> {
        if self.width() <= max {
            return self.spans.clone();
        }
        let mut left = usize::from(max);
        let mut out = Vec::new();
        for span in &self.spans {
            if left == 0 {
                break;
            }
            let text = fmt::truncate(&span.content, left);
            left -= fmt::width(&text).min(left);
            out.push(Span::styled(text, span.style));
        }
        out
    }
}

/// A table: columns plus rows of cells.
#[derive(Debug, Clone, Default)]
pub struct Grid {
    /// The columns.
    pub cols: Vec<Col>,
    /// The rows; each has one cell per column.
    pub rows: Vec<Vec<Cell>>,
}

impl Grid {
    /// A grid with these columns and no rows.
    pub fn new(cols: Vec<Col>) -> Self {
        Grid {
            cols,
            rows: Vec::new(),
        }
    }

    /// The width of each column that fits in `available` cells; `None` for
    /// the ones dropped.
    pub fn widths(&self, available: u16) -> Vec<Option<u16>> {
        let mut widths: Vec<Option<u16>> = self
            .cols
            .iter()
            .enumerate()
            .map(|(i, col)| {
                let content = self
                    .rows
                    .iter()
                    .filter_map(|row| row.get(i))
                    .map(Cell::width)
                    .max()
                    .unwrap_or(0);
                let title = u16::try_from(fmt::width(&col.title)).unwrap_or(u16::MAX);
                Some(content.max(title).min(col.max.max(title)))
            })
            .collect();
        let total = |widths: &[Option<u16>]| {
            let shown = widths.iter().flatten();
            let gaps = GAP * u16::try_from(shown.clone().count().saturating_sub(1)).unwrap_or(0);
            MARGIN + shown.sum::<u16>() + gaps
        };
        while total(&widths) > available {
            let next = (0..widths.len())
                .filter(|i| widths[*i].is_some() && self.cols[*i].drop > 0)
                .max_by_key(|i| (self.cols[*i].drop, *i));
            match next {
                Some(i) => widths[i] = None,
                None => break,
            }
        }
        widths
    }

    fn line(&self, widths: &[Option<u16>], cells: &[Cell], marker: Span<'static>) -> Line<'static> {
        let mut spans = vec![marker];
        let mut first = true;
        for (i, width) in widths.iter().enumerate() {
            let Some(width) = *width else { continue };
            if !first {
                spans.push(Span::raw(" ".repeat(usize::from(GAP))));
            }
            first = false;
            let cell = cells.get(i).cloned().unwrap_or_default();
            let pad = " ".repeat(usize::from(width.saturating_sub(cell.width())));
            let fitted = cell.fitted(width);
            if self.cols[i].right {
                spans.push(Span::raw(pad));
                spans.extend(fitted);
            } else {
                spans.extend(fitted);
                spans.push(Span::raw(pad));
            }
        }
        Line::from(spans)
    }

    /// Draw the header and as many rows as fit. `selected` highlights a
    /// row; `scroll` remembers the first visible row between frames.
    ///
    /// Returns the 1-based range of rows shown when some did not fit.
    pub fn render(
        &self,
        area: Rect,
        buf: &mut Buffer,
        theme: Theme,
        selected: Option<usize>,
        scroll: Option<&Scroll<usize>>,
    ) -> Option<(usize, usize)> {
        if area.height == 0 || area.width == 0 {
            return None;
        }
        let widths = self.widths(area.width);
        let header: Vec<Cell> = self
            .cols
            .iter()
            .map(|c| Cell::new(c.title.clone(), theme.header()))
            .collect();
        buf.set_line(
            area.x,
            area.y,
            &self.line(&widths, &header, Span::raw(" ")),
            area.width,
        );
        let height = usize::from(area.height - 1);
        if height == 0 {
            return None;
        }
        let mut offset = scroll.map_or(0, Scroll::get);
        if let Some(selected) = selected {
            if selected < offset {
                offset = selected;
            } else if selected >= offset + height {
                offset = selected + 1 - height;
            }
        }
        offset = offset.min(self.rows.len().saturating_sub(height));
        if let Some(scroll) = scroll {
            scroll.set(offset);
        }
        for (row, cells) in self.rows.iter().enumerate().skip(offset).take(height) {
            let y = area.y + 1 + u16::try_from(row - offset).unwrap_or(0);
            let marker = if selected == Some(row) {
                buf.set_style(Rect::new(area.x, y, area.width, 1), theme.selected());
                Span::styled("▸", theme.accent())
            } else {
                Span::raw(" ")
            };
            buf.set_line(area.x, y, &self.line(&widths, cells, marker), area.width);
        }
        (self.rows.len() > height).then(|| (offset + 1, (offset + height).min(self.rows.len())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> Grid {
        let mut grid = Grid::new(vec![
            Col::new("CHAIN"),
            Col::new("ID").drop(2),
            Col::new("HEAD").right(),
            Col::new("RPC").drop(1).max(6),
        ]);
        let plain = Style::new();
        grid.rows = vec![
            vec![
                Cell::new("eth", plain),
                Cell::new("1", plain),
                Cell::new("23,481,220", plain),
                Cell::new("infura", plain),
            ],
            vec![
                Cell::new("zora", plain),
                Cell::new("7777777", plain),
                Cell::new("9", plain),
                Cell::new("publicnode", plain),
            ],
        ];
        grid
    }

    fn text(grid: &Grid, width: u16, height: u16, selected: Option<usize>) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        grid.render(area, &mut buf, Theme::new(false), selected, None);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn aligns_truncates_and_marks_the_selection() {
        assert_eq!(
            text(&grid(), 40, 3, Some(1)),
            [
                " CHAIN  ID             HEAD  RPC",
                " eth    1        23,481,220  infura",
                "▸zora   7777777           9  publi…",
            ]
        );
    }

    #[test]
    fn drops_columns_by_priority_when_narrow() {
        // ID (drop 2) goes before RPC (drop 1); the rest never go.
        assert_eq!(grid().widths(40), [Some(5), Some(7), Some(10), Some(6)]);
        assert_eq!(grid().widths(30), [Some(5), None, Some(10), Some(6)]);
        assert_eq!(grid().widths(20), [Some(5), None, Some(10), None]);
        assert_eq!(grid().widths(4), [Some(5), None, Some(10), None]);
        assert_eq!(text(&grid(), 20, 2, None)[1], " eth    23,481,220");
    }

    #[test]
    fn scrolls_to_keep_the_selection_visible() {
        let mut grid = Grid::new(vec![Col::new("N")]);
        grid.rows = (0..10)
            .map(|i| vec![Cell::new(i.to_string(), Style::new())])
            .collect();
        let scroll = Scroll::new(0);
        let area = Rect::new(0, 0, 10, 4);
        let draw = |selected| {
            let mut buf = Buffer::empty(area);
            grid.render(
                area,
                &mut buf,
                Theme::new(false),
                Some(selected),
                Some(&scroll),
            );
            (1..4)
                .map(|y| buf[(1, y)].symbol().to_string())
                .collect::<String>()
        };
        assert_eq!(draw(0), "012");
        assert_eq!(draw(4), "234");
        // Moving back up scrolls one row at a time, not a page.
        assert_eq!(draw(3), "234");
        assert_eq!(draw(1), "123");
        assert_eq!(draw(9), "789");
    }
}
