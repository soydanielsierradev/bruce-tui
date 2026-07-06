//! Bridge Bruce's palette to whatever theme Omarchy is currently using.
//!
//! Omarchy publishes its active theme at `~/.config/omarchy/current/theme/`
//! with an `alacritty.toml` that follows the standard alacritty color schema.
//! Bruce reads that file once, maps the sixteen ANSI slots plus the primary
//! bg/fg to its own [`Palette`], and caches the result so `Theme::palette()`
//! stays cheap.
//!
//! If Omarchy isn't installed (macOS/Windows, or a Linux user who doesn't run
//! it) or the file is missing/malformed, the theme falls back to
//! [`Theme::Hacker`] — the picker still lets the user select "Omarchy" but
//! silently uses the safe default instead of erroring out.
//!
//! No live refresh: switching Omarchy themes while Bruce is running is rare
//! enough that we accept "reopen Bruce to pick up the change" as the
//! trade-off for zero per-frame I/O.

use std::sync::OnceLock;

use ratatui::style::Color;
use serde::Deserialize;

use crate::ui::theme::{Palette, Theme};

/// Path fragment relative to $HOME where Omarchy pins the active theme.
const OMARCHY_ALACRITTY_TOML: &str = ".config/omarchy/current/theme/alacritty.toml";

/// Resolve Bruce's [`Palette`] from Omarchy's active theme, cached.
///
/// The cache is a `OnceLock<Option<Palette>>` shared across the whole process:
/// `None` means "we tried and failed" so we don't re-read the file every
/// frame. Callers unwrap into [`Theme::Hacker`] when the read failed.
pub fn palette() -> Palette {
    static CACHE: OnceLock<Option<Palette>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let home = crate::config::home_dir().ok()?;
            let raw = std::fs::read_to_string(home.join(OMARCHY_ALACRITTY_TOML)).ok()?;
            parse_alacritty_palette(&raw)
        })
        .as_ref()
        .copied()
        .unwrap_or_else(|| Theme::Hacker.palette())
}

/// Parse an alacritty `[colors]` palette (the shape Omarchy ships) into a
/// Bruce [`Palette`]. Pure — takes the raw TOML string, returns
/// `Some(Palette)` on the happy path or `None` on any structural mismatch
/// (missing sections, bad hex, absent keys).
///
/// Public so the tests can round-trip without touching the filesystem.
pub fn parse_alacritty_palette(raw: &str) -> Option<Palette> {
    let cfg: AlacrittyToml = toml::from_str(raw).ok()?;
    let bg = parse_hex(&cfg.colors.primary.background)?;
    let fg = parse_hex(&cfg.colors.primary.foreground)?;
    let normal = &cfg.colors.normal;
    let bright = &cfg.colors.bright;

    Some(Palette {
        // Kept literal so `Palette.name` stays `&'static str`. The active
        // Omarchy theme name (e.g. "aether") lives in `theme.name` on disk;
        // surfacing that here would force `name` to become `String`, which
        // ripples through every other theme, so we don't.
        name: "Omarchy",
        bg,
        fg,
        // Blue is the natural "highlight" slot in alacritty schemes.
        accent: parse_hex(&normal.blue)?,
        // Bright black is the darkest non-black — reads as "muted" on both
        // dark and light backgrounds.
        dim: parse_hex(&bright.black)?,
        added: parse_hex(&normal.green)?,
        // Cyan for modified (not blue) so it doesn't collide visually with
        // the blue accent — same criterion Tokyo Night uses.
        modified: parse_hex(&normal.cyan)?,
        removed: parse_hex(&normal.red)?,
        renamed: parse_hex(&normal.yellow)?,
    })
}

/// Parse a `#RRGGBB` (or `#rrggbb`) string into a `Color::Rgb`. `None` when
/// the string isn't exactly 7 chars starting with `#` or the hex triplet
/// doesn't parse.
fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim();
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

// ── TOML deserialisation shape ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AlacrittyToml {
    colors: AlacrittyColors,
}

#[derive(Debug, Deserialize)]
struct AlacrittyColors {
    primary: PrimaryColors,
    normal: AnsiColors,
    bright: AnsiColors,
}

#[derive(Debug, Deserialize)]
struct PrimaryColors {
    background: String,
    foreground: String,
}

#[derive(Debug, Deserialize)]
struct AnsiColors {
    #[allow(dead_code)]
    black: String,
    red: String,
    green: String,
    yellow: String,
    blue: String,
    #[allow(dead_code)]
    magenta: String,
    cyan: String,
    #[allow(dead_code)]
    white: String,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The real palette Omarchy ships for the "aether" theme, checked in as
    /// the reference fixture so a future refactor can't silently change how
    /// we map alacritty → Bruce.
    const AETHER_TOML: &str = "\
[colors]
[colors.primary]
background = \"#121212\"
foreground = \"#ffffff\"

[colors.normal]
black   = \"#121212\"
red     = \"#d75033\"
green   = \"#b99b67\"
yellow  = \"#a57d41\"
blue    = \"#7d99c0\"
magenta = \"#dc4e2f\"
cyan    = \"#88a1c5\"
white   = \"#ffffff\"

[colors.bright]
black   = \"#9e9e9e\"
red     = \"#ff704a\"
green   = \"#cdaf7c\"
yellow  = \"#c5944a\"
blue    = \"#92aed3\"
magenta = \"#ff6e46\"
cyan    = \"#9eb6d8\"
white   = \"#ffffff\"
";

    #[test]
    fn parse_alacritty_palette_reads_aether_fixture() {
        let p = parse_alacritty_palette(AETHER_TOML).expect("parse");
        assert_eq!(p.name, "Omarchy");
        assert_eq!(p.bg, Color::Rgb(0x12, 0x12, 0x12));
        assert_eq!(p.fg, Color::Rgb(0xff, 0xff, 0xff));
        // Accent = normal.blue, modified = normal.cyan — must not collapse to
        // the same colour or the git modified column looks like the accent.
        assert_eq!(p.accent, Color::Rgb(0x7d, 0x99, 0xc0));
        assert_eq!(p.modified, Color::Rgb(0x88, 0xa1, 0xc5));
        assert_ne!(p.accent, p.modified);
        // dim = bright.black.
        assert_eq!(p.dim, Color::Rgb(0x9e, 0x9e, 0x9e));
        // Git status trio.
        assert_eq!(p.added, Color::Rgb(0xb9, 0x9b, 0x67));
        assert_eq!(p.removed, Color::Rgb(0xd7, 0x50, 0x33));
        assert_eq!(p.renamed, Color::Rgb(0xa5, 0x7d, 0x41));
    }

    #[test]
    fn parse_alacritty_palette_returns_none_on_missing_sections() {
        // Missing colors.bright — the deserialiser must reject the shape.
        let raw = "\
[colors]
[colors.primary]
background = \"#000000\"
foreground = \"#ffffff\"
[colors.normal]
black = \"#000000\"
red = \"#ff0000\"
green = \"#00ff00\"
yellow = \"#ffff00\"
blue = \"#0000ff\"
magenta = \"#ff00ff\"
cyan = \"#00ffff\"
white = \"#ffffff\"
";
        assert!(parse_alacritty_palette(raw).is_none());
    }

    #[test]
    fn parse_alacritty_palette_returns_none_on_bad_hex() {
        // Same as aether but `normal.blue` is malformed — the whole parse
        // fails so we fall back to the safe default at the call site.
        let raw = AETHER_TOML.replace("blue    = \"#7d99c0\"", "blue    = \"not-hex\"");
        assert!(parse_alacritty_palette(&raw).is_none());
    }

    /// Snapshot of the "aurora" (or similarly bright-blue) Omarchy theme with
    /// the extra `[colors.cursor]` table that real Omarchy configs ship. Bruce
    /// only cares about `primary/normal/bright`; the presence of unrelated
    /// sections must not break the parse.
    #[test]
    fn parse_alacritty_palette_tolerates_extra_cursor_section() {
        let raw = "\
[colors]
[colors.primary]
background = \"#040400\"
foreground = \"#e8e6cf\"

[colors.normal]
black   = \"#040400\"
red     = \"#c55c43\"
green   = \"#72a747\"
yellow  = \"#a5a92f\"
blue    = \"#368bd6\"
magenta = \"#b568b5\"
cyan    = \"#00b5b2\"
white   = \"#e8e6cf\"

[colors.bright]
black   = \"#64655e\"
red     = \"#f57d5c\"
green   = \"#91d14a\"
yellow  = \"#cad20f\"
blue    = \"#57aeff\"
magenta = \"#e286e8\"
cyan    = \"#31deda\"
white   = \"#f2efb5\"

[colors.cursor]
text   = \"#040400\"
cursor = \"#e8e6cf\"
";
        let p = parse_alacritty_palette(raw).expect("parse");
        assert_eq!(p.accent, Color::Rgb(0x36, 0x8b, 0xd6));
    }

    #[test]
    fn parse_hex_accepts_only_seven_char_hash_prefixed_values() {
        assert_eq!(parse_hex("#7d99c0"), Some(Color::Rgb(0x7d, 0x99, 0xc0)));
        assert_eq!(parse_hex("  #ffffff  "), Some(Color::Rgb(0xff, 0xff, 0xff)));
        assert_eq!(parse_hex("7d99c0"), None, "missing hash");
        assert_eq!(parse_hex("#7d99"), None, "too short");
        assert_eq!(parse_hex("#zzzzzz"), None, "not hex");
    }
}
