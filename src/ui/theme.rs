//! Color themes for the Bruce TUI.
//!
//! Each [`Theme`] resolves to a full [`Palette`] of RGB colors. Themes are
//! plain `Copy` values so they can be cycled cheaply from the event loop.

use ratatui::style::Color;

/// One of the five built-in color themes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Dracula,
    Nord,
    Light,
    Amber,
}

/// A resolved set of colors for rendering a single theme.
pub struct Palette {
    /// Human-readable theme name, shown in the theme selector.
    pub name: &'static str,
    /// Window background.
    pub bg: Color,
    /// Primary foreground / body text.
    pub fg: Color,
    /// Accent color for highlights, borders and the active selection.
    pub accent: Color,
    /// Muted color for secondary text (dates, hints, metadata).
    pub dim: Color,
}

impl Theme {
    /// All themes in selector order.
    pub const ALL: [Theme; 5] = [
        Theme::Dark,
        Theme::Dracula,
        Theme::Nord,
        Theme::Light,
        Theme::Amber,
    ];

    /// Resolve this theme to its concrete color palette.
    pub fn palette(self) -> Palette {
        match self {
            Theme::Dark => Palette {
                name: "Dark",
                bg: Color::Rgb(0x0d, 0x0e, 0x11),
                fg: Color::Rgb(0xc8, 0xcc, 0xd4),
                accent: Color::Rgb(0x4a, 0xe3, 0x7a),
                dim: Color::Rgb(0x5a, 0x60, 0x6e),
            },
            Theme::Dracula => Palette {
                name: "Dracula",
                bg: Color::Rgb(0x28, 0x2a, 0x36),
                fg: Color::Rgb(0xf8, 0xf8, 0xf2),
                accent: Color::Rgb(0xbd, 0x93, 0xf9),
                dim: Color::Rgb(0x62, 0x72, 0xa4),
            },
            Theme::Nord => Palette {
                name: "Nord",
                bg: Color::Rgb(0x2e, 0x34, 0x40),
                fg: Color::Rgb(0xd8, 0xde, 0xe9),
                accent: Color::Rgb(0x81, 0xa1, 0xc1),
                dim: Color::Rgb(0x4c, 0x56, 0x6a),
            },
            Theme::Light => Palette {
                name: "Light",
                bg: Color::Rgb(0xf8, 0xf8, 0xf2),
                fg: Color::Rgb(0x28, 0x2a, 0x36),
                accent: Color::Rgb(0x1f, 0x7a, 0x3d),
                dim: Color::Rgb(0x8a, 0x8f, 0x98),
            },
            Theme::Amber => Palette {
                name: "Amber",
                bg: Color::Rgb(0x0f, 0x0e, 0x0a),
                fg: Color::Rgb(0xe8, 0xc8, 0x88),
                accent: Color::Rgb(0xff, 0xb0, 0x00),
                dim: Color::Rgb(0x6e, 0x5a, 0x2a),
            },
        }
    }

    /// Next theme in selector order, wrapping around.
    pub fn next(self) -> Theme {
        let idx = Self::ALL.iter().position(|&t| t == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    /// Previous theme in selector order, wrapping around.
    pub fn prev(self) -> Theme {
        let idx = Self::ALL.iter().position(|&t| t == self).unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}
