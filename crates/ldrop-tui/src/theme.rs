// SPDX-License-Identifier: GPL-3.0-or-later
//! Colors and symbols (SPEC §11.3): the terminal's own 16 ANSI colors by default, `NO_COLOR`
//! honored, an optional custom accent, and ASCII when the locale isn't UTF-8.

use ldrop_core::config::{TuiColors, TuiConfig};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;

/// Box drawing for terminals without UTF-8.
const ASCII_BORDER: border::Set = border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

/// Glyphs that have an ASCII stand-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Symbols {
    pub pointer: &'static str,
    pub sep: &'static str,
    pub ellipsis: &'static str,
    pub bar_full: &'static str,
    pub bar_empty: &'static str,
    pub incoming: &'static str,
    pub outgoing: &'static str,
    pub up_down: &'static str,
    pub arrow: &'static str,
    pub dash: &'static str,
}

const UNICODE: Symbols = Symbols {
    pointer: "▸",
    sep: "·",
    ellipsis: "…",
    bar_full: "█",
    bar_empty: "░",
    incoming: "↓",
    outgoing: "↑",
    up_down: "↑↓",
    arrow: "→",
    dash: "—",
};

const ASCII: Symbols = Symbols {
    pointer: ">",
    sep: "-",
    ellipsis: "...",
    bar_full: "#",
    bar_empty: ".",
    incoming: "v",
    outgoing: "^",
    up_down: "up/down",
    arrow: "->",
    dash: "-",
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// False with `NO_COLOR`: only bold, dim and reverse are used.
    pub color: bool,
    pub accent: Color,
    pub ascii: bool,
}

impl Theme {
    pub fn from_env(config: &TuiConfig) -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
            .unwrap_or_default();
        Theme::new(config, !no_color, !locale_is_utf8(&locale))
    }

    pub fn new(config: &TuiConfig, color: bool, ascii: bool) -> Self {
        let accent = match config.colors {
            TuiColors::Terminal => Color::Cyan,
            TuiColors::Custom => config.custom.accent.parse().unwrap_or(Color::Cyan),
        };
        Theme { color, accent, ascii }
    }

    pub fn symbols(&self) -> &'static Symbols {
        if self.ascii { &ASCII } else { &UNICODE }
    }

    pub fn border(&self) -> border::Set<'static> {
        if self.ascii { ASCII_BORDER } else { border::PLAIN }
    }

    fn fg(&self, color: Color, fallback: Modifier) -> Style {
        if self.color { Style::new().fg(color) } else { Style::new().add_modifier(fallback) }
    }

    pub fn accent(&self) -> Style {
        self.fg(self.accent, Modifier::BOLD)
    }

    pub fn title(&self) -> Style {
        self.accent().add_modifier(Modifier::BOLD)
    }

    pub fn selected(&self) -> Style {
        Style::new().add_modifier(Modifier::REVERSED)
    }

    pub fn dim(&self) -> Style {
        self.fg(Color::DarkGray, Modifier::DIM)
    }

    pub fn ok(&self) -> Style {
        self.fg(Color::Green, Modifier::BOLD)
    }

    pub fn warn(&self) -> Style {
        self.fg(Color::Yellow, Modifier::BOLD)
    }

    pub fn error(&self) -> Style {
        self.fg(Color::Red, Modifier::BOLD)
    }

    pub fn bold(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }
}

fn locale_is_utf8(locale: &str) -> bool {
    let l = locale.to_ascii_lowercase();
    l.contains("utf-8") || l.contains("utf8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_detection() {
        assert!(locale_is_utf8("en_US.UTF-8"));
        assert!(locale_is_utf8("de_DE.utf8"));
        assert!(!locale_is_utf8("C"));
        assert!(!locale_is_utf8("POSIX"));
        assert!(!locale_is_utf8(""));
    }

    #[test]
    fn custom_accent() {
        let mut config = TuiConfig::default();
        assert_eq!(Theme::new(&config, true, false).accent, Color::Cyan);
        config.colors = TuiColors::Custom;
        config.custom.accent = "magenta".into();
        assert_eq!(Theme::new(&config, true, false).accent, Color::Magenta);
        config.custom.accent = "#ff8800".into();
        assert_eq!(Theme::new(&config, true, false).accent, Color::Rgb(0xff, 0x88, 0));
        config.custom.accent = "no such color".into();
        assert_eq!(Theme::new(&config, true, false).accent, Color::Cyan);
    }
}
