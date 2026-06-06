//! Token-usage metrics read from Claude Code's session transcript.
//!
//! Claude writes a JSON-lines transcript per session under
//! `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`. Each assistant turn
//! carries a `usage` block; [`parse_transcript`] sums it into [`Metrics`].
//!
//! A live refresh (file watcher) lands in [`watch`]; this module's parsing is
//! pure so it can be unit-reasoned and reused by the watcher thread.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::SystemTime;

use notify::{RecursiveMode, Watcher};

/// Cumulative token usage for one Claude session.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Metrics {
    /// Fresh (uncached) input tokens summed across assistant turns.
    pub input: u64,
    /// Output tokens generated.
    pub output: u64,
    /// Tokens written into the prompt cache (`cache_creation_input_tokens`).
    pub cache_write: u64,
    /// Tokens served from the prompt cache (`cache_read_input_tokens`).
    pub cache_read: u64,
    /// Number of assistant turns counted (unique message ids).
    pub messages: u64,
}

impl Metrics {
    /// Every token the session touched, cached or not.
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }
}

/// The `~/.claude/projects/<encoded-cwd>` directory for a project, if the home
/// directory is known. Existence is not checked here.
pub fn transcript_dir(project_path: &Path) -> Option<PathBuf> {
    let home = home_dir()?;
    Some(
        home.join(".claude")
            .join("projects")
            .join(encode_project(project_path)),
    )
}

/// The newest `*.jsonl` in `dir` (Claude's currently active session), if any.
pub fn newest_transcript(dir: &Path) -> Option<PathBuf> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
            newest = Some((modified, path));
        }
    }
    newest.map(|(_, path)| path)
}

/// Sum the token usage of every assistant turn in a transcript file.
///
/// Claude writes several lines per turn (one per content block), each repeating
/// the same `message.id` and the same `message.usage`. We dedupe by id so each
/// turn is counted once. Malformed lines are skipped, never fatal.
pub fn parse_transcript(path: &Path) -> Metrics {
    let Ok(content) = fs::read_to_string(path) else {
        return Metrics::default();
    };

    let mut metrics = Metrics::default();
    let mut seen: HashSet<String> = HashSet::new();

    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };

        // Dedupe by message id: the same turn spans multiple lines.
        if let Some(id) = message.get("id").and_then(|i| i.as_str()) {
            if !seen.insert(id.to_string()) {
                continue;
            }
        }

        let Some(usage) = message.get("usage") else {
            continue;
        };
        let field = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);

        metrics.input += field("input_tokens");
        metrics.output += field("output_tokens");
        metrics.cache_write += field("cache_creation_input_tokens");
        metrics.cache_read += field("cache_read_input_tokens");
        metrics.messages += 1;
    }

    metrics
}

/// A live view of the active session's metrics, refreshed by a file watcher.
///
/// Holds the `notify` watcher and a worker thread that re-parses the newest
/// transcript whenever Claude writes to the project's transcript directory.
/// Shared state is an `Arc<Mutex<_>>`, per the project's threading rule; the
/// watcher and thread are dropped (and stop) with this struct.
pub struct MetricsWatcher {
    /// Latest parsed metrics, updated by the worker thread.
    metrics: Arc<Mutex<Metrics>>,
    /// Kept alive so the OS watch stays registered; dropping it ends watching.
    _watcher: notify::RecommendedWatcher,
    /// Worker thread handle; the channel closing ends its loop.
    _thread: JoinHandle<()>,
}

impl MetricsWatcher {
    /// Start watching `project_path`'s transcript directory for token usage.
    ///
    /// Seeds with the current totals, then refreshes on every filesystem event.
    /// Returns `None` if the home directory or the watch can't be established.
    pub fn new(project_path: &Path) -> Option<Self> {
        let dir = transcript_dir(project_path)?;
        // Claude may not have created the directory yet (it spawns alongside the
        // watcher). Create it so the watch registers and catches the first write.
        let _ = fs::create_dir_all(&dir);

        let seed = newest_transcript(&dir)
            .map(|file| parse_transcript(&file))
            .unwrap_or_default();
        let metrics = Arc::new(Mutex::new(seed));

        let (tx, rx) = mpsc::channel();
        let mut watcher =
            notify::recommended_watcher(move |res| { let _ = tx.send(res); }).ok()?;
        watcher.watch(&dir, RecursiveMode::NonRecursive).ok()?;

        let metrics_for_thread = Arc::clone(&metrics);
        let thread = std::thread::spawn(move || {
            // Each event means Claude touched the directory; re-read the newest
            // transcript and replace the totals. Errors just skip that tick.
            for event in rx {
                if event.is_err() {
                    continue;
                }
                if let Some(file) = newest_transcript(&dir) {
                    let fresh = parse_transcript(&file);
                    if let Ok(mut current) = metrics_for_thread.lock() {
                        *current = fresh;
                    }
                }
            }
        });

        Some(Self {
            metrics,
            _watcher: watcher,
            _thread: thread,
        })
    }

    /// A copy of the latest metrics (cheap; the struct is small and `Clone`).
    pub fn snapshot(&self) -> Metrics {
        self.metrics
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default()
    }
}

/// Encode an absolute project path the way Claude names its transcript folder:
/// every path separator (and the drive colon on Windows) becomes a dash.
fn encode_project(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| match c {
            ':' | '\\' | '/' => '-',
            other => other,
        })
        .collect()
}

/// The user's home directory, from the platform's standard env var. Avoids a
/// dependency just for this one lookup.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}
