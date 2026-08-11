//! Live session telemetry sourced from Claude Code's status line contract.
//!
//! The transcript JSONL tells us what a session *spent*, but it never says how
//! big the context window is or how much of the subscription budget is left.
//! Claude Code does publish both — it just pushes them instead of exposing a
//! query: whatever command is configured under `statusLine` gets a JSON blob on
//! stdin on every render, carrying `context_window.context_window_size`,
//! `rate_limits.five_hour` / `.seven_day`, `effort.level` and `fast_mode`.
//!
//! So Bruce becomes that command. [`install`] points the project's
//! `statusLine` at `bruce statusline-sink`; [`run_sink`] parks each payload in
//! `<config>/bruce/statusline/<session-id>.json` and then re-runs whatever
//! command the user had configured before, forwarding its output so their own
//! status line keeps rendering untouched. The workspace reads the sidecar with
//! [`read`].
//!
//! Everything here degrades to `None` rather than failing: an un-installed
//! shim, a session that hasn't hit the API yet, or a Claude build predating a
//! field all land on the transcript-derived fallback instead of an error.

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::Result;
use serde_json::Value;

use crate::config::bruce_dir;

/// One rolling usage window from `rate_limits` (the 5-hour or the 7-day one).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateWindow {
    /// Share of the window's budget already consumed, 0-100.
    pub used_percentage: f64,
    /// Unix-epoch seconds at which the window rolls over.
    pub resets_at: i64,
}

/// The subset of Claude Code's status line payload Bruce actually renders.
///
/// Every field is optional because the payload fills in progressively: before
/// the first API response there is no `context_window`, and `rate_limits` only
/// exists for Claude.ai Pro/Max subscribers (API-key users are billed in
/// dollars, so they get no windows at all).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct StatusLine {
    /// The real context window for this session, straight from Claude Code —
    /// no guessing from the model id. This is the number the whole context
    /// readout hangs off.
    pub context_window_size: Option<u64>,
    /// Tokens currently occupying the window: `input + cache_creation +
    /// cache_read`. Output tokens are excluded — they leave the context.
    pub used_tokens: Option<u64>,
    /// Claude Code's own used-percentage, when it supplies one. Preferred over
    /// recomputing so Bruce and the native status line never disagree.
    pub used_percentage: Option<f64>,
    /// Human-facing model name (`Opus 4.7`) as Claude Code labels it.
    pub model_display_name: Option<String>,
    /// Reasoning effort: `low` | `medium` | `high` | `xhigh` | `max`. Absent
    /// on models that don't take the parameter.
    pub effort: Option<String>,
    /// Whether the session is running in fast mode.
    pub fast_mode: bool,
    /// The 5-hour rolling usage window.
    pub five_hour: Option<RateWindow>,
    /// The 7-day (weekly) usage window.
    pub seven_day: Option<RateWindow>,
}

/// Parse a status line payload. Returns `None` only when the input isn't JSON
/// at all — a well-formed payload missing every field of interest still yields
/// an (empty) `StatusLine`, because "connected but nothing to say yet" is a
/// real state we want to distinguish from "no sidecar".
pub fn parse(json: &str) -> Option<StatusLine> {
    let value: Value = serde_json::from_str(json).ok()?;
    let mut out = StatusLine::default();

    if let Some(cw) = value.get("context_window").and_then(Value::as_object) {
        out.context_window_size = cw.get("context_window_size").and_then(Value::as_u64);
        out.used_percentage = cw.get("used_percentage").and_then(Value::as_f64);
        // `total_input_tokens` is already the input-only sum Claude Code uses
        // for its own percentage. Fall back to adding up `current_usage` for
        // builds that predate the combined field.
        out.used_tokens = cw
            .get("total_input_tokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                let usage = cw.get("current_usage").and_then(Value::as_object)?;
                let field = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
                Some(
                    field("input_tokens")
                        + field("cache_creation_input_tokens")
                        + field("cache_read_input_tokens"),
                )
            });
    }

    out.model_display_name = value
        .get("model")
        .and_then(|m| m.get("display_name"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    out.effort = value
        .get("effort")
        .and_then(|e| e.get("level"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    out.fast_mode = value
        .get("fast_mode")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if let Some(limits) = value.get("rate_limits") {
        out.five_hour = parse_window(limits.get("five_hour"));
        out.seven_day = parse_window(limits.get("seven_day"));
    }

    Some(out)
}

fn parse_window(value: Option<&Value>) -> Option<RateWindow> {
    let obj = value?.as_object()?;
    Some(RateWindow {
        used_percentage: obj.get("used_percentage").and_then(Value::as_f64)?,
        resets_at: obj.get("resets_at").and_then(Value::as_i64).unwrap_or(0),
    })
}

/// Directory holding one sidecar per session plus the delegate record.
fn sidecar_dir() -> Result<PathBuf> {
    Ok(bruce_dir()?.join("statusline"))
}

/// Where a given session's latest payload is parked.
fn sidecar_path(session_id: &str) -> Result<PathBuf> {
    Ok(sidecar_dir()?.join(format!("{session_id}.json")))
}

/// Latest status line payload for a session, or `None` when the shim hasn't
/// been installed, hasn't fired yet, or wrote something unparseable.
pub fn read(session_id: &str) -> Option<StatusLine> {
    let path = sidecar_path(session_id).ok()?;
    let raw = fs::read_to_string(path).ok()?;
    parse(&raw)
}

/// Drop a session's sidecar. Called when a session is deleted so the directory
/// doesn't accumulate a file per session forever.
pub fn forget(session_id: &str) {
    if let Ok(path) = sidecar_path(session_id) {
        let _ = fs::remove_file(path);
    }
}

/// Where the user's pre-existing status line command is remembered so the sink
/// can keep rendering it.
fn delegate_path() -> Result<PathBuf> {
    Ok(sidecar_dir()?.join("delegate.json"))
}

/// Marker that identifies a `statusLine.command` as Bruce's own shim.
const SINK_MARKER: &str = "statusline-sink";

/// Whether a configured `statusLine.command` is Bruce's own shim. Guards the
/// delegate record: without it, a chain that pointed back at Bruce would fork
/// a bruce process on every single status line render.
fn is_bruce_shim(command: &str) -> bool {
    command.contains(SINK_MARKER)
}

/// Extra `claude` arguments that route this session's status line through
/// Bruce, for the caller to append when spawning the PTY.
///
/// Uses `--settings` rather than writing a settings file, and that choice is
/// deliberate: command-line settings outrank local, project and user scopes,
/// and they live and die with the process Bruce spawned. Nothing lands in the
/// user's repository, nothing needs cleaning up, and a crashed Bruce can't
/// strand a project pointing at a status line command that no longer exists.
///
/// The status line the user already had is recorded first so [`run_sink`] can
/// keep rendering it — Bruce observes the payload, it doesn't take the row.
///
/// Returns an empty vec when the binary path can't be resolved: the workspace
/// then falls back to transcript-derived numbers instead of failing to open.
pub fn spawn_args() -> Vec<String> {
    let Ok(exe) = std::env::current_exe() else {
        return Vec::new();
    };
    let command = format!("{} {SINK_MARKER}", shell_quote(&exe.to_string_lossy()));

    // Whatever Claude Code would otherwise have rendered is what the sink has
    // to re-run. `--settings` shadows every scope, so the user's own command
    // is the one we're displacing regardless of where they configured it.
    match global_statusline_command() {
        Some(existing) => {
            let _ = record_delegate(&existing);
        }
        None => {
            let _ = clear_delegate();
        }
    }

    let settings = serde_json::json!({
        "statusLine": {
            "type": "command",
            "command": command,
            // Reset countdowns and rate-limit windows drift while the session
            // sits idle, so ask for a periodic re-render rather than relying
            // purely on conversation events.
            "refreshInterval": 10,
        }
    });
    vec!["--settings".to_string(), settings.to_string()]
}

/// The `statusLine.command` configured in `~/.claude/settings.json`, if any.
fn global_statusline_command() -> Option<String> {
    let home = crate::config::home_dir().ok()?;
    let raw = fs::read_to_string(home.join(".claude").join("settings.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("statusLine")?
        .get("command")?
        .as_str()
        .map(str::to_owned)
}

fn record_delegate(command: &str) -> Result<()> {
    // Guard against chaining into ourselves, which would fork bruce processes
    // recursively on every status line render.
    if is_bruce_shim(command) {
        return clear_delegate();
    }
    let path = delegate_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::json!({ "command": command });
    fs::write(&path, serde_json::to_string_pretty(&body)?)?;
    Ok(())
}

fn clear_delegate() -> Result<()> {
    if let Ok(path) = delegate_path() {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

fn read_delegate() -> Option<String> {
    let raw = fs::read_to_string(delegate_path().ok()?).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value.get("command")?.as_str().map(str::to_owned)
}

/// Entry point for `bruce statusline-sink`, invoked by Claude Code, not by the
/// user: read the payload from stdin, park it under the session's id, then run
/// the user's original command with the same payload and forward its output.
///
/// Errors are deliberately swallowed after the write: a broken delegate must
/// not take down the status line, and Claude Code renders whatever reaches
/// stdout regardless.
pub fn run_sink() -> Result<()> {
    let mut payload = String::new();
    std::io::stdin().read_to_string(&mut payload)?;

    if let Some(session_id) = serde_json::from_str::<Value>(&payload)
        .ok()
        .and_then(|v| v.get("session_id").and_then(Value::as_str).map(str::to_owned))
        && !session_id.is_empty()
    {
        let _ = park(&session_id, &payload);
    }

    if let Some(command) = read_delegate() {
        let _ = run_delegate(&command, &payload);
    }
    Ok(())
}

/// Write the payload to the session's sidecar via a temp file + rename, so the
/// workspace never reads a half-written file mid-render.
fn park(session_id: &str, payload: &str) -> Result<()> {
    let path = sidecar_path(session_id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, payload)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Run the user's original status line command with the payload on stdin and
/// relay its stdout, so their status line renders exactly as before.
fn run_delegate(command: &str, payload: &str) -> Result<()> {
    let mut child = shell(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes());
    }
    let output = child.wait_with_output()?;
    std::io::stdout().write_all(&output.stdout)?;
    Ok(())
}

/// Build a shell invocation for `command`. Claude Code runs `statusLine`
/// commands through a shell, so the delegate has to be run the same way for
/// pipes and quoting inside it to behave.
fn shell(command: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    }
}

/// Quote a path for the shell that will run the status line command. Bruce can
/// live under a path with spaces (`~/Proyectos Personales/...`), so an unquoted
/// command would split into the wrong argv.
fn shell_quote(path: &str) -> String {
    if cfg!(windows) {
        return format!("\"{path}\"");
    }
    format!("'{}'", path.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload shape documented in Claude Code's status line reference.
    const SAMPLE: &str = r#"{
        "session_id": "abc-123",
        "model": { "display_name": "Opus 4.7" },
        "context_window": {
            "total_input_tokens": 15500,
            "total_output_tokens": 1200,
            "context_window_size": 1000000,
            "used_percentage": 1.55,
            "remaining_percentage": 98.45,
            "current_usage": {
                "input_tokens": 500,
                "output_tokens": 1200,
                "cache_creation_input_tokens": 13000,
                "cache_read_input_tokens": 2000
            }
        },
        "exceeds_200k_tokens": false,
        "fast_mode": true,
        "effort": { "level": "high" },
        "rate_limits": {
            "five_hour": { "used_percentage": 23.5, "resets_at": 1738425600 },
            "seven_day": { "used_percentage": 41.2, "resets_at": 1738857600 }
        }
    }"#;

    #[test]
    fn parse_reads_every_field_bruce_renders() {
        let s = parse(SAMPLE).expect("sample payload parses");
        assert_eq!(s.context_window_size, Some(1_000_000));
        assert_eq!(s.used_tokens, Some(15_500));
        assert_eq!(s.used_percentage, Some(1.55));
        assert_eq!(s.model_display_name.as_deref(), Some("Opus 4.7"));
        assert_eq!(s.effort.as_deref(), Some("high"));
        assert!(s.fast_mode);
        assert_eq!(
            s.five_hour,
            Some(RateWindow { used_percentage: 23.5, resets_at: 1738425600 })
        );
        assert_eq!(
            s.seven_day,
            Some(RateWindow { used_percentage: 41.2, resets_at: 1738857600 })
        );
    }

    #[test]
    fn used_tokens_falls_back_to_summing_current_usage() {
        // Older Claude Code builds emit `current_usage` without the combined
        // `total_input_tokens`; the input-only sum has to match anyway.
        let raw = r#"{
            "context_window": {
                "context_window_size": 200000,
                "current_usage": {
                    "input_tokens": 500,
                    "output_tokens": 9999,
                    "cache_creation_input_tokens": 13000,
                    "cache_read_input_tokens": 2000
                }
            }
        }"#;
        let s = parse(raw).expect("payload parses");
        // 500 + 13000 + 2000 — output_tokens must NOT be counted.
        assert_eq!(s.used_tokens, Some(15_500));
    }

    #[test]
    fn parse_tolerates_a_payload_before_the_first_api_call() {
        // No context_window, no rate_limits: a real state, not an error.
        let s = parse(r#"{"session_id":"x","model":{"display_name":"Sonnet 4.6"}}"#)
            .expect("minimal payload parses");
        assert_eq!(s.context_window_size, None);
        assert_eq!(s.used_tokens, None);
        assert_eq!(s.five_hour, None);
        assert!(!s.fast_mode);
        assert_eq!(s.model_display_name.as_deref(), Some("Sonnet 4.6"));
    }

    #[test]
    fn parse_tolerates_null_current_usage_after_compact() {
        let raw = r#"{"context_window":{"context_window_size":200000,"current_usage":null}}"#;
        let s = parse(raw).expect("payload parses");
        assert_eq!(s.context_window_size, Some(200_000));
        assert_eq!(s.used_tokens, None);
    }

    #[test]
    fn parse_rejects_non_json() {
        assert!(parse("not json at all").is_none());
    }

    #[test]
    fn parse_omits_rate_limits_for_api_key_users() {
        // API-key users are billed in dollars and get no subscription windows.
        let s = parse(r#"{"context_window":{"context_window_size":200000},"rate_limits":null}"#)
            .expect("payload parses");
        assert_eq!(s.five_hour, None);
        assert_eq!(s.seven_day, None);
    }

    /// A second install must recognise its own command and leave the recorded
    /// delegate alone. Getting this wrong makes the sink delegate to itself,
    /// so every status line render forks another bruce process.
    #[test]
    fn is_bruce_shim_recognises_the_command_install_writes() {
        let generated = format!("{} {SINK_MARKER}", shell_quote("/home/a b/bruce"));
        assert!(is_bruce_shim(&generated));
        assert!(is_bruce_shim("/usr/local/bin/bruce statusline-sink"));
    }

    /// Uninstall keys off the same predicate, so anything that isn't ours has
    /// to read as foreign — including status lines that merely mention bruce.
    #[test]
    fn is_bruce_shim_leaves_foreign_commands_alone() {
        assert!(!is_bruce_shim("~/.claude/statusline.sh"));
        assert!(!is_bruce_shim("jq -r '.model.display_name'"));
        assert!(!is_bruce_shim("/usr/local/bin/bruce --version"));
    }

    /// `--settings` takes a JSON *string*, so a malformed one would make
    /// `claude` refuse to start — this is the session's whole PTY on the line.
    #[test]
    fn spawn_args_emit_a_settings_flag_claude_can_parse() {
        let args = spawn_args();
        assert_eq!(args.len(), 2, "expected --settings plus its payload");
        assert_eq!(args[0], "--settings");

        let parsed: Value = serde_json::from_str(&args[1]).expect("payload is valid JSON");
        let line = parsed.get("statusLine").expect("carries a statusLine");
        assert_eq!(line.get("type").and_then(Value::as_str), Some("command"));
        assert_eq!(line.get("refreshInterval").and_then(Value::as_u64), Some(10));

        let command = line.get("command").and_then(Value::as_str).expect("a command");
        assert!(is_bruce_shim(command), "command must be recognisably ours: {command}");
    }

    #[test]
    fn shell_quote_survives_paths_with_spaces_and_quotes() {
        if cfg!(windows) {
            return;
        }
        assert_eq!(shell_quote("/home/a b/bruce"), "'/home/a b/bruce'");
        assert_eq!(shell_quote("/home/o'brien/bruce"), r"'/home/o'\''brien/bruce'");
    }

}
