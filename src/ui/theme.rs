//! Color themes for the Bruce TUI.
//!
//! Each [`Theme`] resolves to a full [`Palette`] of RGB colors. Themes are
//! plain `Copy` values so they can be cycled cheaply from the event loop.

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

/// One of the built-in color themes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    Hacker,
    Cyberpunk,
    Claude,
    Dracula,
    Nord,
    Light,
    Amber,
    Tokyo,
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
    /// Git status: added / new files (conventionally green).
    pub added: Color,
    /// Git status: modified files (conventionally blue).
    pub modified: Color,
    /// Git status: deleted files (conventionally red).
    pub removed: Color,
    /// Git status: renamed files (conventionally yellow).
    pub renamed: Color,
}

impl Theme {
    /// All themes in selector order.
    pub const ALL: [Theme; 8] = [
        Theme::Hacker,
        Theme::Cyberpunk,
        Theme::Claude,
        Theme::Dracula,
        Theme::Nord,
        Theme::Light,
        Theme::Amber,
        Theme::Tokyo,
    ];

    /// A short palette preview for the welcome theme selector: the theme's
    /// surface colors (background, text, accent, muted) plus one status color.
    /// These are what actually distinguish one theme from another at a glance —
    /// the git-status colors are too conventional to tell themes apart.
    pub fn swatches(self) -> [Color; 5] {
        let p = self.palette();
        [p.bg, p.fg, p.accent, p.dim, p.added]
    }

    /// Resolve this theme to its concrete color palette.
    pub fn palette(self) -> Palette {
        match self {
            // Cyberpunk: pure-black backdrop with neon pink as the primary
            // accent and electric blue for modified files — the pink/blue neon
            // pairing that reads as "cyberpunk".
            Theme::Cyberpunk => Palette {
                name: "Cyberpunk",
                bg: Color::Rgb(0x0a, 0x0a, 0x12),
                fg: Color::Rgb(0xea, 0xea, 0xff),
                accent: Color::Rgb(0xff, 0x2e, 0x97),
                dim: Color::Rgb(0x6c, 0x5c, 0x9e),
                added: Color::Rgb(0x00, 0xf0, 0xb5),
                modified: Color::Rgb(0x2d, 0x9c, 0xff),
                removed: Color::Rgb(0xff, 0x3d, 0x6e),
                renamed: Color::Rgb(0xff, 0xd1, 0x66),
            },
            // Mirrors Claude Code's own dark look so the embedded pane and the
            // surrounding Bruce chrome read as one surface: warm near-black
            // background, soft warm-grey text, and Claude's signature coral as
            // the accent. The other themes intentionally diverge.
            Theme::Claude => Palette {
                name: "Claude",
                bg: Color::Rgb(0x00, 0x00, 0x00),
                fg: Color::Rgb(0xd4, 0xd0, 0xc8),
                accent: Color::Rgb(0xd9, 0x77, 0x57),
                dim: Color::Rgb(0x73, 0x6b, 0x5e),
                added: Color::Rgb(0x4a, 0xe3, 0x7a),
                modified: Color::Rgb(0x5a, 0xa2, 0xf7),
                removed: Color::Rgb(0xf7, 0x76, 0x8e),
                renamed: Color::Rgb(0xe0, 0xaf, 0x68),
            },
            Theme::Dracula => Palette {
                name: "Dracula",
                bg: Color::Rgb(0x28, 0x2a, 0x36),
                fg: Color::Rgb(0xf8, 0xf8, 0xf2),
                accent: Color::Rgb(0xbd, 0x93, 0xf9),
                dim: Color::Rgb(0x62, 0x72, 0xa4),
                added: Color::Rgb(0x50, 0xfa, 0x7b),
                modified: Color::Rgb(0x8b, 0xe9, 0xfd),
                removed: Color::Rgb(0xff, 0x55, 0x55),
                renamed: Color::Rgb(0xf1, 0xfa, 0x8c),
            },
            Theme::Nord => Palette {
                name: "Nord",
                bg: Color::Rgb(0x2e, 0x34, 0x40),
                fg: Color::Rgb(0xd8, 0xde, 0xe9),
                accent: Color::Rgb(0x81, 0xa1, 0xc1),
                dim: Color::Rgb(0x4c, 0x56, 0x6a),
                added: Color::Rgb(0xa3, 0xbe, 0x8c),
                modified: Color::Rgb(0x88, 0xc0, 0xd0),
                removed: Color::Rgb(0xbf, 0x61, 0x6a),
                renamed: Color::Rgb(0xeb, 0xcb, 0x8b),
            },
            Theme::Light => Palette {
                name: "Light",
                bg: Color::Rgb(0xf8, 0xf8, 0xf2),
                fg: Color::Rgb(0x28, 0x2a, 0x36),
                accent: Color::Rgb(0x1f, 0x7a, 0x3d),
                dim: Color::Rgb(0x8a, 0x8f, 0x98),
                added: Color::Rgb(0x2e, 0x7d, 0x32),
                modified: Color::Rgb(0x15, 0x65, 0xc0),
                removed: Color::Rgb(0xc6, 0x28, 0x28),
                renamed: Color::Rgb(0x9a, 0x6d, 0x00),
            },
            Theme::Amber => Palette {
                name: "Amber",
                bg: Color::Rgb(0x0f, 0x0e, 0x0a),
                fg: Color::Rgb(0xe8, 0xc8, 0x88),
                accent: Color::Rgb(0xff, 0xb0, 0x00),
                dim: Color::Rgb(0x6e, 0x5a, 0x2a),
                added: Color::Rgb(0x9c, 0xcc, 0x65),
                modified: Color::Rgb(0x64, 0xb5, 0xf6),
                removed: Color::Rgb(0xe5, 0x73, 0x73),
                renamed: Color::Rgb(0xff, 0xca, 0x28),
            },
            // Tokyo Night: the popular blue/purple dark theme. Its signature is
            // the soft blue accent on a deep indigo background.
            Theme::Tokyo => Palette {
                name: "Tokyo Night",
                bg: Color::Rgb(0x1a, 0x1b, 0x26),
                fg: Color::Rgb(0xc0, 0xca, 0xf5),
                accent: Color::Rgb(0x7a, 0xa2, 0xf7),
                dim: Color::Rgb(0x56, 0x5f, 0x89),
                added: Color::Rgb(0x9e, 0xce, 0x6a),
                modified: Color::Rgb(0x7a, 0xa2, 0xf7),
                removed: Color::Rgb(0xf7, 0x76, 0x8e),
                renamed: Color::Rgb(0xe0, 0xaf, 0x68),
            },
            // The original Bruce "Dark": green-on-near-black, the classic
            // terminal-hacker look.
            Theme::Hacker => Palette {
                name: "Hackerman",
                bg: Color::Rgb(0x0d, 0x0e, 0x11),
                fg: Color::Rgb(0xc8, 0xcc, 0xd4),
                accent: Color::Rgb(0x4a, 0xe3, 0x7a),
                dim: Color::Rgb(0x5a, 0x60, 0x6e),
                added: Color::Rgb(0x4a, 0xe3, 0x7a),
                modified: Color::Rgb(0x5a, 0xa2, 0xf7),
                removed: Color::Rgb(0xf7, 0x76, 0x8e),
                renamed: Color::Rgb(0xe0, 0xaf, 0x68),
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
