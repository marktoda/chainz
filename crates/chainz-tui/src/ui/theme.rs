//! The palette. ANSI colours for meaning (they follow the terminal's own
//! theme) and mid-greys for structure (legible on dark and light
//! backgrounds). With `NO_COLOR` set, only bold, dim and reverse remain.

use ratatui::style::{Color, Modifier, Style};

/// Styles for every role on screen.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    color: bool,
}

impl Theme {
    /// A coloured theme, or a plain one.
    pub fn new(color: bool) -> Self {
        Theme { color }
    }

    /// Coloured unless `NO_COLOR` is set to something.
    pub fn from_env() -> Self {
        Theme::new(std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()))
    }

    fn fg(self, color: Color) -> Style {
        if self.color {
            Style::new().fg(color)
        } else {
            Style::new()
        }
    }

    /// Ordinary text.
    pub fn text(self) -> Style {
        Style::new()
    }

    /// Secondary text: units, ages, hints.
    pub fn dim(self) -> Style {
        if self.color {
            Style::new().fg(Color::Indexed(245))
        } else {
            Style::new().add_modifier(Modifier::DIM)
        }
    }

    /// Structure: rules, placeholders, separators.
    pub fn faint(self) -> Style {
        if self.color {
            Style::new().fg(Color::Indexed(240))
        } else {
            Style::new().add_modifier(Modifier::DIM)
        }
    }

    /// Column headers.
    pub fn header(self) -> Style {
        self.dim().add_modifier(Modifier::BOLD)
    }

    /// The product name, the selection marker, the active endpoint.
    pub fn accent(self) -> Style {
        self.fg(Color::Cyan)
    }

    /// The tab on screen.
    pub fn tab_active(self) -> Style {
        self.accent()
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    }

    /// Healthy.
    pub fn ok(self) -> Style {
        self.fg(Color::Green)
    }

    /// Slow, rate-limited, stale.
    pub fn warn(self) -> Style {
        self.fg(Color::Yellow)
    }

    /// Lagging.
    pub fn lag(self) -> Style {
        self.fg(Color::Magenta)
    }

    /// Down or failing.
    pub fn bad(self) -> Style {
        self.fg(Color::Red)
    }

    /// The selected row: explicit foreground and background, so it reads
    /// on any terminal theme.
    pub fn selected(self) -> Style {
        if self.color {
            Style::new()
                .bg(Color::Indexed(237))
                .fg(Color::Indexed(255))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::REVERSED)
        }
    }

    /// A key name in the footer.
    pub fn key(self) -> Style {
        self.accent()
    }
}
