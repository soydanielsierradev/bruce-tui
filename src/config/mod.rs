//! User preferences that persist across runs.
//!
//! Bruce remembers the last configuration the user chose — the active theme and
//! which optional panels are shown — so reopening it doesn't reset everything to
//! defaults. Preferences are global (one config for all projects), stored as
//! `<config>/bruce/config.json`.
//!
//! This module also owns the platform config-directory resolution shared with
//! the session store.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::ui::theme::{BorderStyle, SideWidth, Theme};

/// Persisted user preferences. Defaults match Bruce's first-run look: the Hacker
/// theme with every panel visible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Last active color theme.
    pub theme: Theme,
    /// Whether the Git panel is shown.
    #[serde(default = "default_true")]
    pub git_enabled: bool,
    /// Whether the bottom Terminal pane is shown. The user can toggle it with
    /// `Ctrl+T`; persisting the last choice keeps that decision across runs
    /// instead of always defaulting to on. Defaults on so first-run users see
    /// the pane and discover it exists.
    #[serde(default = "default_true")]
    pub terminal_enabled: bool,
    /// Unix-epoch seconds of the last update check (0 = never). Throttles the
    /// network check to roughly once a day.
    #[serde(default)]
    pub last_update_check: i64,
    /// Latest release version last seen (empty = unknown). Lets the badge show
    /// immediately on startup from cache, before any new network check returns.
    #[serde(default)]
    pub latest_seen: String,
    /// Repaint the terminal's default fg/bg to match the theme (via OSC). Off
    /// leaves the host terminal's own colors. Defaults on.
    #[serde(default = "default_true")]
    pub sync_colors: bool,
    /// Show the one-line hint bar at the bottom of the workspace. Defaults on.
    #[serde(default = "default_true")]
    pub show_footer: bool,
    /// Show the top title bar (app name + session). Defaults on.
    #[serde(default = "default_true")]
    pub show_title: bool,
    /// Line style for the framed side panes. Defaults rounded.
    #[serde(default)]
    pub border_style: BorderStyle,
    /// Width of each side pane. Defaults normal (25%).
    #[serde(default)]
    pub side_width: SideWidth,
    /// Use Nerd Font file icons in the file manager (needs a Nerd Font set in
    /// the terminal). Off = emoji icons, which render anywhere. Defaults off.
    #[serde(default)]
    pub nerd_icons: bool,
}

/// Serde default for the boolean preferences added after the first release, so
/// an older `config.json` missing these keys loads them as enabled (the prior
/// always-on behaviour) instead of `false`.
fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::Hacker,
            git_enabled: true,
            terminal_enabled: true,
            last_update_check: 0,
            latest_seen: String::new(),
            sync_colors: true,
            show_footer: true,
            show_title: true,
            border_style: BorderStyle::default(),
            side_width: SideWidth::default(),
            nerd_icons: false,
        }
    }
}

impl Config {
    /// Load preferences from disk, falling back to defaults when the file is
    /// missing or unreadable/corrupt — a bad config must never block startup.
    pub fn load() -> Self {
        let Ok(path) = config_path() else {
            return Self::default();
        };
        let Ok(raw) = fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    /// Persist preferences to `<config>/bruce/config.json`, creating the
    /// directory if needed.
    pub fn save(&self) -> Result<()> {
        let dir = bruce_dir()?;
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating config dir {}", dir.display()))?;
        let path = dir.join("config.json");
        let json = serde_json::to_string_pretty(self).context("serialising config")?;
        fs::write(&path, json).with_context(|| format!("writing config {}", path.display()))?;
        Ok(())
    }
}

/// Full path to the config file.
fn config_path() -> Result<PathBuf> {
    Ok(bruce_dir()?.join("config.json"))
}

/// Bruce's data directory: `<config-base>/bruce`. Shared by config and sessions.
pub fn bruce_dir() -> Result<PathBuf> {
    Ok(config_base()?.join("bruce"))
}

/// Resolve the platform config base directory, in order: `XDG_CONFIG_HOME`,
/// then `HOME/.config`, then `USERPROFILE/.config` (Windows). No OS-exclusive
/// APIs, so it builds and runs the same on macOS, Linux and Windows.
fn config_base() -> Result<PathBuf> {
    if let Some(xdg) = env_path("XDG_CONFIG_HOME") {
        return Ok(xdg);
    }
    let home = env_path("HOME")
        .or_else(|| env_path("USERPROFILE"))
        .context("no XDG_CONFIG_HOME, HOME or USERPROFILE set; cannot locate config dir")?;
    Ok(home.join(".config"))
}

/// Resolve the user's home directory via `HOME` (Unix/macOS) or `USERPROFILE`
/// (Windows) — no platform-exclusive APIs, no `dirs` crate (REQ-8, REQ-9).
pub fn home_dir() -> Result<PathBuf> {
    env_path("HOME")
        .or_else(|| env_path("USERPROFILE"))
        .context("no HOME or USERPROFILE set; cannot locate the home directory")
}

/// Resolve the Claude CLI skills directory: `~/.claude/skills` — the only place
/// Claude reads skills from.
pub fn claude_skills_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude").join("skills"))
}

/// Resolve the cross-agent skills directory: `~/.agents/skills`. Tools like
/// `npx skills` install here (and notoriously fail to symlink into
/// `~/.claude/skills`), so Bruce watches it and relocates new skills.
pub fn agents_skills_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".agents").join("skills"))
}

/// Read an environment variable as a non-empty path.
pub(crate) fn env_path(key: &str) -> Option<PathBuf> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}
