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

    /// Delete this session's JSON from disk.
    ///
    /// Deliberately leaves Claude's transcript (`<id>.jsonl`) in place: removing
    /// the conversation history is destructive and recoverable only by Claude,
    /// so Bruce only forgets its own pointer to it. A missing file is not an
    /// error (already gone).
    pub fn delete(&self) -> Result<()> {
        // Bruce's own status line cache for this session *is* ours to drop —
        // otherwise the sidecar directory grows a stale file per deleted
        // session forever.
        crate::statusline::forget(&self.id);
        let path = sessions_dir()?.join(format!("{}.json", self.id));
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("deleting session {}", path.display())),
        }
    }

    /// Duplicate this session: a fresh id and `(copy)` name pointing at the same
    /// project, with the conversation forked so the copy can diverge without
    /// touching the original. The new session is persisted before being
    /// returned.
    ///
    /// The transcript fork is best-effort: if it can't be copied the duplicate
    /// still exists, just without prior history.
    pub fn duplicate(&self) -> Result<Session> {
        let now = now_epoch();
        let copy = Session {
            id: Uuid::new_v4().to_string(),
            name: format!("{} (copy)", self.name),
            project_path: self.project_path.clone(),
            branch: self.branch.clone(),
            created_at: now,
            last_used: now,
            tokens_used: self.tokens_used,
        };
        let _ = crate::panels::metrics::fork_transcript(&self.project_path, &self.id, &copy.id);
        copy.save()?;
        Ok(copy)
    }

    /// The CLI args Bruce hands to `claude` to either start or resume this
    /// session: `[--session-id, <uuid>]` for `resume=false`, `[--resume, <uuid>]`
    /// for `resume=true`.
    ///
    /// Pure and decoupled from the spawn machinery so a regression here (flag
    /// swap, missing id) is caught by unit tests instead of silently shipping
    /// "the session won't resume" to the user.
    pub fn claude_args(&self, resume: bool) -> Vec<String> {
        let flag = if resume { "--resume" } else { "--session-id" };
        vec![flag.to_string(), self.id.clone()]
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

/// Load only the sessions belonging to `project_path`, most-recently-used first.
///
/// Bruce is a workspace *per project*: opened in a directory, it shows that
/// project's sessions, not every session ever created. Paths are compared after
/// a best-effort canonicalisation so equivalent spellings (trailing slash,
/// symlinks, drive-letter case on Windows) still match.
pub fn load_for_project(project_path: &Path) -> Result<Vec<Session>> {
    let target = canonical(project_path);
    Ok(load_all()?
        .into_iter()
        .filter(|s| canonical(&s.project_path) == target)
        .collect())
}

/// Canonicalise a path for comparison, falling back to the path as-is when it
/// can't be resolved (e.g. it no longer exists).
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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
fn sessions_dir() -> Result<PathBuf> {
    Ok(crate::config::bruce_dir()?.join("sessions"))
}

/// Current time as Unix epoch seconds. Clamps a pre-epoch clock to 0 rather than
/// erroring — a wrong timestamp must never block saving a session.
fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(id: &str) -> Session {
        Session {
            id: id.to_owned(),
            name: "test".to_owned(),
            project_path: PathBuf::from("/tmp/project"),
            branch: None,
            created_at: 0,
            last_used: 0,
            tokens_used: 0,
        }
    }

    #[test]
    fn claude_args_new_session_uses_session_id_flag() {
        let s = fixture("abc-123");
        assert_eq!(
            s.claude_args(false),
            vec!["--session-id".to_string(), "abc-123".to_string()]
        );
    }

    #[test]
    fn claude_args_resume_uses_resume_flag() {
        let s = fixture("abc-123");
        assert_eq!(
            s.claude_args(true),
            vec!["--resume".to_string(), "abc-123".to_string()]
        );
    }

    #[test]
    fn claude_args_passes_id_through_verbatim() {
        // The UUID format isn't enforced here; whatever string the session
        // holds is what we hand to Claude. Guard against a future refactor
        // accidentally normalising it.
        let s = fixture("not-a-uuid-but-still-an-id");
        let args = s.claude_args(false);
        assert_eq!(args[1], "not-a-uuid-but-still-an-id");
    }
}
