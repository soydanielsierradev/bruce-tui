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

use crate::ui::theme::Theme;

/// Persisted user preferences. Defaults match Bruce's first-run look: the Hacker
/// theme with every panel visible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Last active color theme.
    pub theme: Theme,
    /// Whether the Git panel is shown.
    pub git_enabled: bool,
    /// Whether the Metrics panel is shown.
    pub metrics_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::Hacker,
            git_enabled: true,
            metrics_enabled: true,
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

/// Read an environment variable as a non-empty path.
fn env_path(key: &str) -> Option<PathBuf> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}
