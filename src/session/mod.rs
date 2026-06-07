//! Persisted sessions: the metadata Bruce stores so a conversation can be
//! resumed after the machine is shut down.
//!
//! The key design decision: Bruce does **not** store the conversation history.
//! Claude Code already writes every message to
//! `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`. Bruce only owns the
//! session *id* (a UUID it generates and passes to `claude --session-id`), so
//! later it can relaunch `claude --resume <id>` and let Claude rebuild the full
//! transcript itself.
//!
//! What Bruce persists, therefore, is light: identity (`id`, `name`), where to
//! relaunch (`project_path`, `branch`) and soft metrics (`last_used`,
//! `tokens_used`). Identity is written the moment a session is created — never
//! deferred to a clean exit — so a session survives a crash, a power cut, or a
//! suspended laptop. Metrics are refreshed on exit; if that refresh is lost the
//! session is still fully recoverable.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A saved session, serialised one-per-file as `<id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// UUID Bruce generates and hands to `claude --session-id` / `--resume`.
    /// Also the file stem of both this JSON and Claude's own transcript.
    pub id: String,
    /// User-facing display name.
    pub name: String,
    /// Working directory `claude` is launched in (the project being worked on).
    pub project_path: PathBuf,
    /// Git branch at creation time, shown in the session list. Optional because
    /// the project may not be a git repository.
    pub branch: Option<String>,
    /// Creation time, Unix epoch seconds. Set once, never changed.
    pub created_at: i64,
    /// Last time the session was opened, Unix epoch seconds. Refreshed on use.
    pub last_used: i64,
    /// Cumulative tokens used, read from Claude's transcript. Soft metric.
    pub tokens_used: u64,
}

impl Session {
    /// Build a brand-new session with a fresh UUID and `created_at == last_used`
    /// set to now. Does not touch disk — call [`Session::save`] to persist.
    pub fn new(name: String, project_path: PathBuf, branch: Option<String>) -> Self {
        let now = now_epoch();
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            project_path,
            branch,
            created_at: now,
            last_used: now,
            tokens_used: 0,
        }
    }

    /// Write the session to `<sessions_dir>/<id>.json`, creating the directory
    /// if needed. Pretty-printed so the files are human-readable.
    pub fn save(&self) -> Result<()> {
        let dir = sessions_dir()?;
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating sessions dir {}", dir.display()))?;
        let path = dir.join(format!("{}.json", self.id));
        let json = serde_json::to_string_pretty(self).context("serialising session")?;
        fs::write(&path, json).with_context(|| format!("writing session {}", path.display()))?;
        Ok(())
    }

    /// Mark the session as used now and persist. Call when opening a session.
    pub fn touch(&mut self) -> Result<()> {
        self.last_used = now_epoch();
        self.save()
    }
}

/// Load every valid session from disk, most-recently-used first.
///
/// A single corrupt or unreadable file is skipped rather than failing the whole
/// load — one bad JSON must not lock the user out of every other session.
pub fn load_all() -> Result<Vec<Session>> {
    let dir = sessions_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    let entries =
        fs::read_dir(&dir).with_context(|| format!("reading sessions dir {}", dir.display()))?;
    for entry in entries {
        let path = match entry {
            Ok(e) => e.path(),
            Err(_) => continue,
        };
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match load_file(&path) {
            Ok(session) => sessions.push(session),
            Err(_) => continue, // skip corrupt files, keep the rest usable
        }
    }

    sessions.sort_by(|a, b| b.last_used.cmp(&a.last_used));
    Ok(sessions)
}

/// Parse a single session file.
fn load_file(path: &Path) -> Result<Session> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("reading session {}", path.display()))?;
    let session: Session =
        serde_json::from_str(&raw).with_context(|| format!("parsing session {}", path.display()))?;
    Ok(session)
}

/// Directory holding session files: `<config>/bruce/sessions`.
///
/// Config base resolution, in order: `XDG_CONFIG_HOME`, then `HOME/.config`,
/// then `USERPROFILE/.config` (Windows). No OS-exclusive APIs, so it builds and
/// runs the same on macOS, Linux and the developer's Windows box.
fn sessions_dir() -> Result<PathBuf> {
    Ok(config_base()?.join("bruce").join("sessions"))
}

/// Resolve the platform config base directory.
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

/// Current time as Unix epoch seconds. Clamps a pre-epoch clock to 0 rather than
/// erroring — a wrong timestamp must never block saving a session.
fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
