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

/// A snapshot of the live session state for the workspace Session pane, read
/// out of Claude's transcript: which model is answering, its speed tier, how
/// much context the last turn carried, how that figure has moved turn over
/// turn, and the cumulative token totals.
///
/// This is the fallback source. When Bruce's status line shim is live,
/// [`crate::statusline`] supplies the same ground truth first-hand — including
/// the one thing the transcript can't tell us, the actual window size. All
/// fields are best-effort: an empty or `None` value means the transcript
/// doesn't carry that datum yet.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SessionInfo {
    /// Raw model id from the last assistant entry (e.g. `claude-opus-4-7`).
    pub model: Option<String>,
    /// Service speed tier from the last usage block (`standard` / `fast`).
    pub speed: Option<String>,
    /// Total tokens sitting in Claude's context on the last turn — the sum of
    /// `input_tokens`, `cache_creation_input_tokens`, and
    /// `cache_read_input_tokens`. This is what Claude actually loaded, not
    /// the cumulative session total, so it tracks the "how close am I to
    /// auto-compact?" question.
    pub last_context_tokens: u64,
    /// Context size after each assistant turn, oldest first. Feeds the
    /// "≈N turns to compact" projection — a single current value tells you
    /// where you are, but the slope tells you when to stop.
    pub context_series: Vec<u64>,
    /// Cumulative token usage across every assistant turn, behind the Session
    /// pane's `Tokens` row. Computed in the same transcript walk so the pane
    /// never costs a second read of the file.
    pub totals: Metrics,
}

/// Snapshot of session info from `<id>.jsonl`. Returns `None` when the
/// transcript file isn't there yet — the subpanel then shows a placeholder.
pub fn read_session_info(project_path: &Path, session_id: &str) -> Option<SessionInfo> {
    let path = transcript_dir(project_path)?.join(format!("{session_id}.jsonl"));
    if !path.exists() {
        return None;
    }
    Some(parse_session_info(&path))
}

/// Walk the transcript once and pull out everything the Session pane needs:
/// the last assistant entry's model, speed and prompt-side token count, the
/// per-turn context series behind the compact projection, and the cumulative
/// totals. Pure — takes a path so tests can point at a fixture without a real
/// Claude session on disk.
pub fn parse_session_info(path: &Path) -> SessionInfo {
    let Ok(content) = fs::read_to_string(path) else {
        return SessionInfo::default();
    };

    let mut info = SessionInfo::default();
    let mut seen_ids: HashSet<String> = HashSet::new();

    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ty = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if ty != "assistant" {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };

        // Overwrite unconditionally: we want the *last* assistant turn's
        // model / speed / context size, so the final iteration wins.
        if let Some(model) = message.get("model").and_then(|m| m.as_str()) {
            info.model = Some(model.to_string());
        }
        if let Some(usage) = message.get("usage") {
            let field = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
            // "Context on this turn" is what Claude had to load: fresh input
            // plus everything served from cache. Output tokens don't count
            // — they leave the context, they don't fill it.
            info.last_context_tokens = field("input_tokens")
                + field("cache_creation_input_tokens")
                + field("cache_read_input_tokens");
            if let Some(speed) = usage.get("speed").and_then(|s| s.as_str()) {
                info.speed = Some(speed.to_string());
            }

            // Cumulative totals for the Usage subpanel — dedupe by message
            // id because a single turn spans multiple transcript lines that
            // all repeat the same usage block.
            if let Some(id) = message.get("id").and_then(|i| i.as_str()) {
                if seen_ids.insert(id.to_string()) {
                    // One sample per real turn, for the same reason: the
                    // repeated lines would otherwise flatten the slope with
                    // dozens of zero-delta points.
                    info.context_series.push(info.last_context_tokens);
                    info.totals.input += field("input_tokens");
                    info.totals.output += field("output_tokens");
                    info.totals.cache_write += field("cache_creation_input_tokens");
                    info.totals.cache_read += field("cache_read_input_tokens");
                }
            }
        }
    }
    info
}

/// Per-family Claude pricing in **USD per million tokens**, as of 2026-07.
///
/// These figures track the public Anthropic API pricing page. Cache-read is
/// the "hit" rate (a fraction of input); cache-write matches or slightly
/// exceeds input for the write path. Values are approximations — the Usage
/// subpanel always shows `~$X.XX` to signal that this is an estimate, not
/// what the user will actually be billed.
///
/// Fields:
///
/// - `.0` input (fresh, uncached)
/// - `.1` output
/// - `.2` cache write
/// - `.3` cache read
#[derive(Debug, Clone, Copy)]
struct Pricing {
    input_per_mtok: f64,
    output_per_mtok: f64,
    cache_write_per_mtok: f64,
    cache_read_per_mtok: f64,
}

/// Look up per-million-token pricing for a Claude model id. Falls back to
/// the Sonnet 4.x rate for unknown ids — that's the fleet median, so an
/// unknown model gets a reasonable estimate instead of `$0`.
fn pricing_for(model_id: &str) -> Pricing {
    let id = model_id.to_ascii_lowercase();
    // Opus 4.x: premium tier.
    if id.contains("opus") {
        return Pricing {
            input_per_mtok: 15.00,
            output_per_mtok: 75.00,
            cache_write_per_mtok: 18.75,
            cache_read_per_mtok: 1.50,
        };
    }
    // Haiku 4.x: cheapest.
    if id.contains("haiku") {
        return Pricing {
            input_per_mtok: 1.00,
            output_per_mtok: 5.00,
            cache_write_per_mtok: 1.25,
            cache_read_per_mtok: 0.10,
        };
    }
    // Fable 5 sits alongside Sonnet at the "smart" tier.
    if id.contains("fable") {
        return Pricing {
            input_per_mtok: 3.00,
            output_per_mtok: 15.00,
            cache_write_per_mtok: 3.75,
            cache_read_per_mtok: 0.30,
        };
    }
    // Sonnet 4.x and everything unmapped: median rate.
    Pricing {
        input_per_mtok: 3.00,
        output_per_mtok: 15.00,
        cache_write_per_mtok: 3.75,
        cache_read_per_mtok: 0.30,
    }
}

/// Estimated cost in USD for a session's cumulative token usage under the
/// given model's pricing. Returns 0.0 when `model_id` is `None` — no model
/// means we haven't seen an assistant turn yet, so there's nothing to price.
pub fn estimated_cost(totals: &Metrics, model_id: Option<&str>) -> f64 {
    let Some(model) = model_id else {
        return 0.0;
    };
    let p = pricing_for(model);
    let per_mtok = |tokens: u64, rate: f64| (tokens as f64 / 1_000_000.0) * rate;
    per_mtok(totals.input, p.input_per_mtok)
        + per_mtok(totals.output, p.output_per_mtok)
        + per_mtok(totals.cache_write, p.cache_write_per_mtok)
        + per_mtok(totals.cache_read, p.cache_read_per_mtok)
}

/// The standard Claude context window.
pub const STANDARD_CONTEXT_CAP: u64 = 200_000;

/// The extended window shipped by default on Opus 4.6+ for Max/Team/Enterprise
/// plans, and on Fable 5 for everyone.
pub const EXTENDED_CONTEXT_CAP: u64 = 1_000_000;

/// Share of the window at which Claude Code triggers an auto-compact. Used to
/// project how many turns are left, so the estimate lands where compaction
/// actually happens rather than at a theoretical 100 %.
pub const AUTO_COMPACT_AT: f64 = 0.92;

/// Best-effort context window size when the status line sidecar isn't
/// available — an un-installed shim, or a session that hasn't hit the API yet.
///
/// The window can't be derived from the model id: the same model runs at 200 K
/// or 1 M depending on the user's plan and `CLAUDE_CODE_DISABLE_1M_CONTEXT`.
/// What the transcript *does* prove is a floor — a session observed at 372 K
/// tokens is definitionally not on a 200 K window. So the observed peak picks
/// the tier. Guessing low is the dangerous direction: it pins the readout at
/// "0 % free" and the user stops trusting it.
///
/// Prefer [`crate::statusline::StatusLine::context_window_size`] whenever it's
/// there; this is the fallback, not the source of truth.
pub fn context_cap_fallback(observed_tokens: u64) -> u64 {
    if observed_tokens > STANDARD_CONTEXT_CAP {
        return EXTENDED_CONTEXT_CAP;
    }
    STANDARD_CONTEXT_CAP
}

/// Average per-turn context growth over the tail of a session, in tokens.
///
/// Only positive deltas count: a `/compact` or `/clear` drops the context
/// sharply, and folding those in would cancel out real growth and report a
/// flat or shrinking window. Returns `None` when there aren't two comparable
/// turns yet, or when the tail only shrank.
pub fn context_growth_per_turn(series: &[u64]) -> Option<u64> {
    /// How many recent turns to average over. Short enough to react when a
    /// session starts pulling big files in, long enough that one fat tool
    /// result doesn't dominate the estimate.
    const WINDOW: usize = 6;

    let tail = &series[series.len().saturating_sub(WINDOW)..];
    if tail.len() < 2 {
        return None;
    }
    let mut total = 0u64;
    let mut counted = 0u64;
    for pair in tail.windows(2) {
        if pair[1] > pair[0] {
            total += pair[1] - pair[0];
            counted += 1;
        }
    }
    if counted == 0 {
        return None;
    }
    Some(total / counted)
}

/// How many more turns fit before auto-compact, given the current fill, the
/// window size, and the recent per-turn growth.
///
/// `None` means the question doesn't apply yet (no growth measured, or an
/// empty window). `Some(0)` means the threshold is already behind us.
pub fn turns_to_compact(used: u64, cap: u64, growth_per_turn: u64) -> Option<u64> {
    if cap == 0 || growth_per_turn == 0 {
        return None;
    }
    let threshold = (cap as f64 * AUTO_COMPACT_AT) as u64;
    Some(threshold.saturating_sub(used) / growth_per_turn)
}

/// Whether Bruce should show a dollar estimate at all.
///
/// Subscription users (Pro/Max) pay in rate-limit budget, not per token — a
/// `~$4.20` readout there is the wrong currency and quietly misleads. An
/// `ANTHROPIC_API_KEY` in the environment is the signal that tokens really do
/// map to a bill.
pub fn cost_applies() -> bool {
    std::env::var("ANTHROPIC_API_KEY")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Friendly display for a Claude model id: `claude-opus-4-7` → `Opus 4.7`,
/// `claude-haiku-4-5-20251001` → `Haiku 4.5`. Falls back to the raw id
/// when the shape doesn't match — that way an unknown model is still
/// surfaced verbatim instead of hidden.
pub fn model_display(model_id: &str) -> String {
    let lower = model_id.to_ascii_lowercase();
    let families = ["opus", "sonnet", "haiku", "fable"];
    for family in families {
        if let Some(idx) = lower.find(family) {
            // Take the two version components right after the family name
            // (e.g. "4" and "7" from "opus-4-7-20251001"). Anything past
            // those (dated snapshots) drops off — the family + M.N is
            // what a human recognises.
            let tail = &lower[idx + family.len()..];
            let parts: Vec<&str> = tail
                .split('-')
                .filter(|p| !p.is_empty())
                .collect();
            let version: Vec<&str> = parts.iter().take(2).copied().collect();
            let capitalised = capitalise(family);
            if version.is_empty() {
                return capitalised;
            }
            return format!("{capitalised} {}", version.join("."));
        }
    }
    model_id.to_string()
}

fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
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

    /// `parse_session_info` picks the last assistant entry's model/speed/
    /// context total and counts user turns. Two assistant entries and two
    /// user entries here — the last assistant is `m2` on Sonnet 4.6, so
    /// that's what survives.
    #[test]
    fn parse_session_info_picks_the_last_assistant_turn() {
        let raw = concat!(
            r#"{"type":"user","message":{"role":"user"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m1","model":"claude-opus-4-7","usage":{"input_tokens":100,"output_tokens":200,"cache_creation_input_tokens":50,"cache_read_input_tokens":5000,"speed":"standard"}}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m2","model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":900,"cache_creation_input_tokens":2000,"cache_read_input_tokens":150000,"speed":"fast"}}}"#,
        );
        let mut f = tempfile();
        f.write_all(raw.as_bytes()).unwrap();
        let info = parse_session_info(&f.path);
        assert_eq!(info.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(info.speed.as_deref(), Some("fast"));
        // 10 + 2000 + 150000 = 152010. Output (900) doesn't count.
        assert_eq!(info.last_context_tokens, 152_010);
    }

    /// Empty transcript file: `SessionInfo::default()`. Doesn't panic on
    /// missing fields — the subpanel just renders a placeholder.
    #[test]
    fn parse_session_info_returns_default_on_empty_file() {
        let mut f = tempfile();
        f.write_all(b"").unwrap();
        let info = parse_session_info(&f.path);
        assert!(info.model.is_none());
    }

    #[test]
    fn model_display_maps_known_families() {
        assert_eq!(model_display("claude-opus-4-7"), "Opus 4.7");
        assert_eq!(model_display("claude-sonnet-4-6"), "Sonnet 4.6");
        assert_eq!(model_display("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_display("claude-fable-5"), "Fable 5");
    }

    #[test]
    fn model_display_falls_back_to_raw_on_unknown_id() {
        // No known family in the string — surface it verbatim rather than
        // hiding it, so a new model at least shows *something* real.
        assert_eq!(model_display("mystery-model-9000"), "mystery-model-9000");
    }

    /// Session totals are computed in the same walk as the info block: the
    /// same fixture that hits `m1` twice (two lines, same id) must NOT
    /// double-count the tokens, mirroring `parse_transcript`.
    #[test]
    fn parse_session_info_totals_dedupe_by_id() {
        let raw = concat!(
            r#"{"type":"assistant","message":{"id":"m1","model":"claude-opus-4-7","usage":{"input_tokens":100,"output_tokens":10,"cache_creation_input_tokens":50,"cache_read_input_tokens":5}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m1","model":"claude-opus-4-7","usage":{"input_tokens":100,"output_tokens":10,"cache_creation_input_tokens":50,"cache_read_input_tokens":5}}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m2","model":"claude-opus-4-7","usage":{"input_tokens":200,"output_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":80}}}"#,
        );
        let mut f = tempfile();
        f.write_all(raw.as_bytes()).unwrap();
        let info = parse_session_info(&f.path);
        // m1 counted once (100+200), same as parse_transcript.
        assert_eq!(info.totals.input, 300);
        assert_eq!(info.totals.output, 30);
        assert_eq!(info.totals.cache_write, 50);
        assert_eq!(info.totals.cache_read, 85);
    }

    /// Opus is priced higher than Sonnet — same token totals must yield a
    /// bigger estimated bill under Opus. Guards against accidentally
    /// swapping the tiers.
    #[test]
    fn estimated_cost_opus_beats_sonnet_for_same_tokens() {
        let totals = Metrics {
            input: 1_000_000,
            output: 1_000_000,
            cache_write: 0,
            cache_read: 0,
        };
        let opus = estimated_cost(&totals, Some("claude-opus-4-7"));
        let sonnet = estimated_cost(&totals, Some("claude-sonnet-4-6"));
        // Opus: 15 + 75 = 90 USD; Sonnet: 3 + 15 = 18 USD. Opus > Sonnet.
        assert!(opus > sonnet, "opus {opus} should exceed sonnet {sonnet}");
        assert!((opus - 90.0).abs() < 0.01);
        assert!((sonnet - 18.0).abs() < 0.01);
    }

    /// No model recorded yet = no responses yet = nothing to price. Must be
    /// exactly zero so the subpanel shows `~$0.00` (a placeholder), never
    /// e.g. the Sonnet rate applied silently.
    #[test]
    fn estimated_cost_returns_zero_without_model() {
        let totals = Metrics { input: 5_000, output: 5_000, ..Default::default() };
        assert_eq!(estimated_cost(&totals, None), 0.0);
    }

    #[test]
    fn context_cap_fallback_starts_at_the_standard_window() {
        assert_eq!(context_cap_fallback(0), STANDARD_CONTEXT_CAP);
        assert_eq!(context_cap_fallback(150_000), STANDARD_CONTEXT_CAP);
        assert_eq!(context_cap_fallback(STANDARD_CONTEXT_CAP), STANDARD_CONTEXT_CAP);
    }

    /// Regression: a real Opus 4.7 session in this very repo was observed at
    /// 372,206 context tokens (and manually compacted at 372,967). The old
    /// hardcoded 200 K cap clamped that to zero free, so the Session pane read
    /// "0K free · 0%" in red for the rest of the session. Anything above the
    /// standard window is proof of the extended one.
    #[test]
    fn context_cap_fallback_promotes_past_the_standard_window() {
        assert_eq!(context_cap_fallback(372_206), EXTENDED_CONTEXT_CAP);
        assert_eq!(context_cap_fallback(STANDARD_CONTEXT_CAP + 1), EXTENDED_CONTEXT_CAP);
    }

    #[test]
    fn context_growth_averages_only_the_turns_that_grew() {
        // +10K, +10K, then a compact drops it to 5K. The drop must not be
        // averaged in, or the slope reads as flat and the projection lies.
        let series = [10_000, 20_000, 30_000, 5_000];
        assert_eq!(context_growth_per_turn(&series), Some(10_000));
    }

    #[test]
    fn context_growth_needs_two_comparable_turns() {
        assert_eq!(context_growth_per_turn(&[]), None);
        assert_eq!(context_growth_per_turn(&[10_000]), None);
        // Only shrank: nothing to extrapolate from.
        assert_eq!(context_growth_per_turn(&[30_000, 10_000]), None);
    }

    #[test]
    fn turns_to_compact_counts_down_to_the_auto_compact_threshold() {
        // 92 % of 200K is 184K. With 100K used and 10K per turn: 8 turns left.
        assert_eq!(turns_to_compact(100_000, 200_000, 10_000), Some(8));
        // Already past the threshold: zero, not a negative or a panic.
        assert_eq!(turns_to_compact(190_000, 200_000, 10_000), Some(0));
        // No measurable growth or no window: the question doesn't apply.
        assert_eq!(turns_to_compact(100_000, 200_000, 0), None);
        assert_eq!(turns_to_compact(100_000, 0, 10_000), None);
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
