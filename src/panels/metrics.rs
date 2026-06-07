//! Token-usage metrics read from Claude Code's session transcript.
//!
//! Claude writes a JSON-lines transcript per session under
//! `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`. [`parse_transcript`]
//! turns it into [`Metrics`]; [`MetricsWatcher`] keeps that live.
//!
//! Transcript shape (important for correct counting): **one line per content
//! block**, not per message. A single assistant turn spans several lines
//! (thinking, text, tool_use…), each repeating the same `message.id` and the
//! same `message.usage`. So token usage is summed once per unique id, while
//! tool-use blocks are counted per line (each is its own block).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::SystemTime;

use notify::{RecursiveMode, Watcher};

/// Claude's context-window size, used for the "% ventana" gauge.
pub const CONTEXT_LIMIT: u64 = 200_000;

/// Cumulative usage + activity for one Claude session.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Metrics {
    /// Fresh (uncached) input tokens summed across assistant turns.
    pub input: u64,
    /// Output tokens generated.
    pub output: u64,
    /// Tokens written into the prompt cache (`cache_creation_input_tokens`).
    pub cache_write: u64,
    /// Tokens served from the prompt cache (`cache_read_input_tokens`).
    pub cache_read: u64,
    /// Assistant turns (unique message ids).
    pub turns: u64,
    /// Tool-use blocks across the session.
    pub tool_calls: u64,
    /// Distinct files touched by Write/Edit/MultiEdit.
    pub files_edited: u64,
    /// Approximate lines added by Write/Edit/MultiEdit inputs.
    pub lines_added: u64,
    /// Approximate lines removed by Edit/MultiEdit inputs.
    pub lines_removed: u64,
    /// Tokens live in the context window now (the last turn's input + cache).
    pub context: u64,
    /// Model id of the most recent assistant turn (e.g. `claude-opus-4-8`).
    pub model: String,
    /// Epoch-second timestamp of the first and last transcript entries.
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    /// Per-turn output tokens with their timestamps, for the usage sparkline.
    pub series: Vec<(i64, u64)>,
}

impl Metrics {
    /// Every token the session touched, cached or not.
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }

    /// Share of input-side tokens served from cache, as a whole percent.
    pub fn cache_hit_pct(&self) -> u64 {
        let denom = self.input + self.cache_read + self.cache_write;
        if denom == 0 {
            0
        } else {
            self.cache_read * 100 / denom
        }
    }

    /// Context-window fill as a whole percent (clamped to 100).
    pub fn context_pct(&self) -> u64 {
        (self.context * 100 / CONTEXT_LIMIT).min(100)
    }

    /// Session wall-clock duration in seconds (0 until two timestamps exist).
    pub fn duration_secs(&self) -> i64 {
        match (self.first_ts, self.last_ts) {
            (Some(a), Some(b)) => (b - a).max(0),
            _ => 0,
        }
    }

    /// Estimated USD cost from list prices for the model family. This is an
    /// approximation: prices are hard-coded and change over time.
    pub fn cost_usd(&self) -> f64 {
        let (pin, pout, pcw, pcr) = model_prices(&self.model);
        (self.input as f64 * pin
            + self.output as f64 * pout
            + self.cache_write as f64 * pcw
            + self.cache_read as f64 * pcr)
            / 1_000_000.0
    }
}

/// List prices in USD per 1M tokens `(input, output, cache_write, cache_read)`,
/// by model family. Approximate; defaults to Sonnet pricing for unknown models.
fn model_prices(model: &str) -> (f64, f64, f64, f64) {
    let m = model.to_lowercase();
    if m.contains("opus") {
        (15.0, 75.0, 18.75, 1.5)
    } else if m.contains("haiku") {
        (1.0, 5.0, 1.25, 0.1)
    } else {
        (3.0, 15.0, 3.75, 0.3)
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

/// Total tokens recorded for a specific session's transcript, if it exists.
///
/// Because Bruce launches `claude --session-id <id>`, the transcript file is
/// named `<id>.jsonl` — so we can target the exact session rather than guessing
/// the newest file. Returns `None` if the home dir is unknown or no transcript
/// exists yet (a session closed before Claude wrote anything).
pub fn session_total_tokens(project_path: &Path, session_id: &str) -> Option<u64> {
    let path = transcript_dir(project_path)?.join(format!("{session_id}.jsonl"));
    if !path.exists() {
        return None;
    }
    Some(parse_transcript(&path).total())
}

/// Parse a transcript into [`Metrics`]. Malformed lines are skipped, never fatal.
pub fn parse_transcript(path: &Path) -> Metrics {
    let Ok(content) = fs::read_to_string(path) else {
        return Metrics::default();
    };

    let mut metrics = Metrics::default();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut files: HashSet<String> = HashSet::new();

    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        // Every entry carries a timestamp; track the session span.
        let line_ts = value
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_iso_epoch);
        if let Some(ts) = line_ts {
            metrics.first_ts = Some(metrics.first_ts.map_or(ts, |f| f.min(ts)));
            metrics.last_ts = Some(metrics.last_ts.map_or(ts, |l| l.max(ts)));
        }

        if value.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };

        if let Some(model) = message.get("model").and_then(|m| m.as_str()) {
            metrics.model = model.to_string();
        }

        // Token usage is repeated on every line of a turn: count once per id.
        if let Some(id) = message.get("id").and_then(|i| i.as_str()) {
            if seen_ids.insert(id.to_string()) {
                if let Some(usage) = message.get("usage") {
                    let field =
                        |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
                    let input = field("input_tokens");
                    let output = field("output_tokens");
                    let cache_write = field("cache_creation_input_tokens");
                    let cache_read = field("cache_read_input_tokens");

                    metrics.input += input;
                    metrics.output += output;
                    metrics.cache_write += cache_write;
                    metrics.cache_read += cache_read;
                    metrics.turns += 1;
                    // The most recent turn defines the live context size.
                    metrics.context = input + cache_read + cache_write;
                    metrics
                        .series
                        .push((line_ts.unwrap_or(0), output));
                }
            }
        }

        // Tool-use blocks live one-per-line, so count them on every line.
        if let Some(blocks) = message.get("content").and_then(|c| c.as_array()) {
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                    continue;
                }
                metrics.tool_calls += 1;
                count_edit(block, &mut metrics, &mut files);
            }
        }
    }

    metrics.files_edited = files.len() as u64;
    metrics
}

/// Account a Write/Edit/MultiEdit tool call: record the file and approximate
/// the lines it adds/removes. Other tools are ignored.
fn count_edit(block: &serde_json::Value, metrics: &mut Metrics, files: &mut HashSet<String>) {
    let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let Some(input) = block.get("input") else {
        return;
    };
    let str_field = |v: &serde_json::Value, key: &str| {
        v.get(key).and_then(|x| x.as_str()).map(str::to_string)
    };

    if let Some(path) = str_field(input, "file_path") {
        files.insert(path);
    }

    match name {
        "Write" => {
            metrics.lines_added += line_count(input.get("content").and_then(|c| c.as_str()));
        }
        "Edit" => {
            metrics.lines_added += line_count(input.get("new_string").and_then(|c| c.as_str()));
            metrics.lines_removed += line_count(input.get("old_string").and_then(|c| c.as_str()));
        }
        "MultiEdit" => {
            if let Some(edits) = input.get("edits").and_then(|e| e.as_array()) {
                for edit in edits {
                    metrics.lines_added +=
                        line_count(edit.get("new_string").and_then(|c| c.as_str()));
                    metrics.lines_removed +=
                        line_count(edit.get("old_string").and_then(|c| c.as_str()));
                }
            }
        }
        _ => {}
    }
}

/// Number of lines in an optional string (0 if absent or empty).
fn line_count(text: Option<&str>) -> u64 {
    match text {
        Some(t) if !t.is_empty() => t.lines().count() as u64,
        _ => 0,
    }
}

/// Parse an ISO-8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SS…Z`) to epoch seconds.
/// Avoids a date-crate dependency; the transcript's format is fixed.
fn parse_iso_epoch(s: &str) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let num = |range: std::ops::Range<usize>| s.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, min, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + min * 60 + sec)
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A transcript matching the real format: one line per content block, the
    /// turn's id and usage repeated across its lines (here, m1's two lines).
    const SAMPLE: &str = concat!(
        r#"{"type":"assistant","timestamp":"2026-06-05T11:00:00.000Z","message":{"id":"m1","model":"claude-opus-4-8","content":[{"type":"thinking","thinking":"x"}],"usage":{"input_tokens":100,"output_tokens":10,"cache_creation_input_tokens":50,"cache_read_input_tokens":5}}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2026-06-05T11:00:01.000Z","message":{"id":"m1","model":"claude-opus-4-8","content":[{"type":"tool_use","name":"Write","input":{"file_path":"a.rs","content":"l1\nl2\nl3"}}],"usage":{"input_tokens":100,"output_tokens":10,"cache_creation_input_tokens":50,"cache_read_input_tokens":5}}}"#,
        "\n",
        r#"{"type":"user","timestamp":"2026-06-05T11:00:02.000Z","message":{"role":"user","content":[{"type":"tool_result"}]}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2026-06-05T11:05:00.000Z","message":{"id":"m2","model":"claude-opus-4-8","content":[{"type":"tool_use","name":"Edit","input":{"file_path":"b.rs","old_string":"x\ny","new_string":"x\ny\nz"}}],"usage":{"input_tokens":200,"output_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":80}}}"#,
    );

    fn parse_sample() -> Metrics {
        let mut file = tempfile();
        file.write_all(SAMPLE.as_bytes()).unwrap();
        parse_transcript(&file.path)
    }

    /// Usage is summed once per unique message id even though m1 spans two lines.
    #[test]
    fn dedupes_usage_by_id() {
        let m = parse_sample();
        assert_eq!(m.turns, 2);
        assert_eq!(m.input, 300);
        assert_eq!(m.output, 30);
        assert_eq!(m.cache_write, 50);
        assert_eq!(m.cache_read, 85);
    }

    /// Tool-use blocks are counted per line, and the last turn sets the context.
    #[test]
    fn counts_tools_files_and_context() {
        let m = parse_sample();
        assert_eq!(m.tool_calls, 2); // Write + Edit
        assert_eq!(m.files_edited, 2); // a.rs, b.rs
        assert_eq!(m.lines_added, 6); // Write 3 + Edit new 3
        assert_eq!(m.lines_removed, 2); // Edit old 2
        assert_eq!(m.context, 280); // m2: 200 + 80 + 0
        assert_eq!(m.model, "claude-opus-4-8");
    }

    /// Duration is the span between first and last timestamps.
    #[test]
    fn computes_duration() {
        assert_eq!(parse_sample().duration_secs(), 300);
    }

    #[test]
    fn iso_epoch_is_monotonic() {
        let a = parse_iso_epoch("2026-06-05T11:00:00.000Z").unwrap();
        let b = parse_iso_epoch("2026-06-05T11:05:00.000Z").unwrap();
        assert_eq!(b - a, 300);
    }

    /// Minimal temp file (avoids a dev-dependency); cleaned up on drop.
    struct TempFile {
        path: PathBuf,
    }
    impl TempFile {
        fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            fs::write(&self.path, bytes)
        }
    }
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }
    fn tempfile() -> TempFile {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        path.push(format!("bruce-metrics-test-{nanos}.jsonl"));
        TempFile { path }
    }
}
