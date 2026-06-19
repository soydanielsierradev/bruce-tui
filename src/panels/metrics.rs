//! Session token totals read from Claude Code's transcript.
//!
//! Claude writes a JSON-lines transcript per session under
//! `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`. Bruce reads it only to
//! record a session's cumulative token total (shown in the welcome session
//! list) — the live metrics pane was replaced by the File Manager.
//!
//! Transcript shape: **one line per content block**, not per message. A single
//! assistant turn spans several lines, each repeating the same `message.id` and
//! `message.usage`, so token usage is summed once per unique id.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Cumulative token usage for one Claude session.
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

/// Fork a session's transcript: copy `<old_id>.jsonl` to `<new_id>.jsonl`,
/// rewriting the embedded `sessionId` so the copy resumes as its own session.
///
/// Claude stores `"sessionId":"<id>"` on every line, matching the filename, so a
/// raw copy would leave the duplicate pointing at the original. A UUID is
/// globally unique, so replacing that exact string only rewrites session-id
/// fields — per-message `uuid`s hold different values and are untouched.
///
/// No transcript yet (a session forked before Claude wrote anything) is not an
/// error: the duplicate simply starts empty. Returns whether a file was copied.
pub fn fork_transcript(project_path: &Path, old_id: &str, new_id: &str) -> std::io::Result<bool> {
    let Some(dir) = transcript_dir(project_path) else {
        return Ok(false);
    };
    let old_path = dir.join(format!("{old_id}.jsonl"));
    if !old_path.exists() {
        return Ok(false);
    }
    let content = fs::read_to_string(&old_path)?;
    let rewritten = content.replace(old_id, new_id);
    fs::write(dir.join(format!("{new_id}.jsonl")), rewritten)?;
    Ok(true)
}

/// Total tokens recorded for a specific session's transcript, if it exists.
///
/// Because Bruce launches `claude --session-id <id>`, the transcript file is
/// named `<id>.jsonl` — so we target the exact session rather than guessing the
/// newest file. Returns `None` if the home dir is unknown or no transcript
/// exists yet (a session closed before Claude wrote anything).
pub fn session_total_tokens(project_path: &Path, session_id: &str) -> Option<u64> {
    let path = transcript_dir(project_path)?.join(format!("{session_id}.jsonl"));
    if !path.exists() {
        return None;
    }
    Some(parse_transcript(&path).total())
}

/// Sum a transcript's token usage. Malformed lines are skipped, never fatal.
pub fn parse_transcript(path: &Path) -> Metrics {
    let Ok(content) = fs::read_to_string(path) else {
        return Metrics::default();
    };

    let mut metrics = Metrics::default();
    let mut seen_ids: HashSet<String> = HashSet::new();

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

        // Token usage is repeated on every line of a turn: count once per id.
        if let Some(id) = message.get("id").and_then(|i| i.as_str()) {
            if seen_ids.insert(id.to_string()) {
                if let Some(usage) = message.get("usage") {
                    let field = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
                    metrics.input += field("input_tokens");
                    metrics.output += field("output_tokens");
                    metrics.cache_write += field("cache_creation_input_tokens");
                    metrics.cache_read += field("cache_read_input_tokens");
                }
            }
        }
    }
    metrics
}

/// Encode an absolute project path the way Claude names its transcript folder:
/// every character that isn't ASCII-alphanumeric becomes a dash. This covers
/// path separators, the Windows drive colon, **and** spaces, dots and other
/// punctuation — Claude Code changed its encoding to dash those out, so a path
/// like `…/Proyectos Personales/…` maps to `…-Proyectos-Personales-…`. Keeping
/// the space here pointed the watcher at a directory Claude never writes to,
/// which silently zeroed out the old metrics pane.
fn encode_project(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
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
    use std::time::SystemTime;

    /// A transcript matching the real format: one line per content block, the
    /// turn's id and usage repeated across its lines (here, m1's two lines).
    const SAMPLE: &str = concat!(
        r#"{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":100,"output_tokens":10,"cache_creation_input_tokens":50,"cache_read_input_tokens":5}}}"#,
        "\n",
        r#"{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":100,"output_tokens":10,"cache_creation_input_tokens":50,"cache_read_input_tokens":5}}}"#,
        "\n",
        r#"{"type":"user","message":{"role":"user"}}"#,
        "\n",
        r#"{"type":"assistant","message":{"id":"m2","usage":{"input_tokens":200,"output_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":80}}}"#,
    );

    fn parse_sample() -> Metrics {
        let mut file = tempfile();
        file.write_all(SAMPLE.as_bytes()).unwrap();
        parse_transcript(&file.path)
    }

    /// Token usage is summed once per unique message id even though m1 spans two
    /// lines, and the cumulative total is every stream added up.
    #[test]
    fn dedupes_usage_by_id() {
        let m = parse_sample();
        assert_eq!(m.input, 300);
        assert_eq!(m.output, 30);
        assert_eq!(m.cache_write, 50);
        assert_eq!(m.cache_read, 85);
        assert_eq!(m.total(), 465);
    }

    /// The transcript folder name must match Claude's encoding exactly: every
    /// non-alphanumeric character (drive colon, separators, **and spaces**)
    /// becomes a dash. A space leaking through silently zeroed the metrics pane.
    #[test]
    fn encodes_project_path_like_claude() {
        let path = Path::new(r"C:\Users\DANIEL\Desktop\Proyectos Personales\bruce-tui");
        assert_eq!(
            encode_project(path),
            "C--Users-DANIEL-Desktop-Proyectos-Personales-bruce-tui"
        );
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
