//! MCP server discovery for the workspace's "MCPs in this project" subpanel.
//!
//! Two sources combined:
//!
//! - `<project>/.mcp.json` — Claude Code's project-scoped MCP config (cheap
//!   to read, runs on every tick).
//! - `claude mcp list` — Claude Code's authoritative listing including user-
//!   scoped servers that the project still inherits (slower, run once at
//!   workspace open and cached).
//!
//! Results are deduplicated by name so a server present in both sources
//! appears only once. Anything that fails (no `.mcp.json`, no `claude` on
//! PATH, garbled JSON) is treated as "empty" — the subpanel will just say
//! "no MCPs configured".

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::Deserialize;

/// Shape Claude Code writes to `.mcp.json`: a single `mcpServers` object whose
/// keys are server names. Everything inside the value is irrelevant for the
/// subpanel (which only shows names).
#[derive(Debug, Deserialize)]
struct McpJson {
    #[serde(rename = "mcpServers")]
    mcp_servers: serde_json::Map<String, serde_json::Value>,
}

/// Parse `.mcp.json`-style JSON and return the server names sorted
/// case-insensitively. Anything malformed (missing key, wrong shape, parse
/// error) yields an empty vec — the file existing without a valid shape is
/// effectively "no MCPs", not a hard error.
///
/// Pure, side-effect-free — unit-testable without a real file.
pub fn parse_mcp_json(raw: &str) -> Vec<String> {
    let parsed: Result<McpJson, _> = serde_json::from_str(raw);
    let Ok(parsed) = parsed else {
        return Vec::new();
    };
    let mut names: Vec<String> = parsed.mcp_servers.into_iter().map(|(k, _)| k).collect();
    names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    names
}

/// Read `<project>/.mcp.json` and return the server names. Missing file or
/// unreadable bytes yield an empty vec; this is the normal case for projects
/// that don't ship project-scoped MCP servers.
pub fn read_project_mcp_json(project_path: &Path) -> Vec<String> {
    let path = project_path.join(".mcp.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    parse_mcp_json(&raw)
}

/// How long we wait for `claude mcp list` to return before giving up. The CLI
/// usually answers in well under a second, but a cold Node start plus health
/// checks against many servers can push it past a few seconds — the previous
/// 3 s cap fired often enough on real machines to make the subpanel blink
/// between "populated" and "empty" as the periodic refresh timed out. Since
/// this runs on a background thread and never blocks the UI, a generous
/// timeout is free — 10 s covers realistic slow paths without letting a truly
/// stuck CLI accumulate zombies.
const CLAUDE_MCP_TIMEOUT: Duration = Duration::from_secs(10);

/// Ask the `claude` CLI for every MCP server it sees from `project_path`
/// (project-scoped + user-scoped). Empty vec if `claude` isn't on PATH, if it
/// errors, or if its output doesn't match the expected shape.
///
/// The CLI's stable output is one server per line, of the form
/// `<name>: <command>`. We trim each line at the first colon and take the
/// left side. Newer or future `claude` versions may add columns; this stays
/// forwards-compatible by ignoring everything past the first colon.
pub fn list_via_claude(project_path: &Path) -> Vec<String> {
    // Spawn with a small timeout via std + child kill. Bare `Command::output()`
    // would block forever if the CLI hangs.
    let mut child = match Command::new("claude")
        .args(["mcp", "list"])
        .current_dir(project_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let start = std::time::Instant::now();
    let output = loop {
        match child.try_wait() {
            Ok(Some(_)) => break child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() >= CLAUDE_MCP_TIMEOUT {
                    let _ = child.kill();
                    return Vec::new();
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return Vec::new(),
        }
    };

    let Some(output) = output else { return Vec::new() };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_claude_list(&stdout)
}

/// Spawn `claude mcp list` on a background thread so workspace open never
/// blocks waiting for the CLI. Returns a join handle that yields the parsed
/// server list when the process completes (or an empty vec on timeout /
/// failure — same semantics as [`list_via_claude`]).
///
/// The caller polls with `is_finished()` on the returned handle and joins it
/// once ready. Consumers must own the [`PathBuf`] so the thread can move it
/// without keeping a lifetime tied to the caller.
pub fn spawn_list_via_claude(project_path: PathBuf) -> JoinHandle<Vec<String>> {
    thread::spawn(move || list_via_claude(&project_path))
}

/// Parse the line-oriented output of `claude mcp list`.
///
/// As of Claude Code 0.16-ish the real format is:
///
/// ```text
/// <name>: <command-and-args> - <status icon and text>
/// ```
///
/// where `<name>` can contain colons (notably the `plugin:<plugin>:<server>`
/// namespacing the plugin system uses for MCPs it provides), so we split on
/// `": "` (colon followed by space) — the actual delimiter Claude writes
/// between name and command — instead of the bare colon.
///
/// We also filter by status: only servers reporting `✔ Connected` make it
/// into the list, so disconnected / unauthenticated entries don't surface as
/// "active" in the subpanel.
///
/// Pure, testable without invoking `claude`.
pub fn parse_claude_list(raw: &str) -> Vec<String> {
    let mut names: Vec<String> = raw
        .lines()
        .filter_map(parse_claude_list_line)
        .collect();
    names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    names.dedup();
    names
}

/// Parse a single line of `claude mcp list` output. Returns the display name
/// when the line describes a connected server, or `None` otherwise (blank
/// line, header, malformed, or not-yet-connected server).
fn parse_claude_list_line(line: &str) -> Option<String> {
    // Split the status suffix off the right end. The line shape is
    // "name: command - status"; the final " - " is the delimiter. Use
    // rsplit so a command containing " - " (uncommon but possible) doesn't
    // confuse us.
    let (head, status) = line.rsplit_once(" - ")?;
    if !status.contains("Connected") {
        return None;
    }

    // Now split "name: command" using ": " — Claude uses colon+space as the
    // real delimiter, so colons inside the name (e.g. `plugin:engram:engram`)
    // don't break the split.
    let (raw_name, _command) = head.split_once(": ")?;
    let raw_name = raw_name.trim();
    if raw_name.is_empty() {
        return None;
    }

    // Plugin-provided MCPs are namespaced as `plugin:<plugin>:<server>`.
    // Show just the meaningful tail so the subpanel reads naturally — what
    // the user wants to see is "engram", not "plugin".
    let display = if let Some(rest) = raw_name.strip_prefix("plugin:") {
        rest.rsplit(':').next().unwrap_or(rest)
    } else {
        raw_name
    };
    Some(display.to_string())
}

/// Merge two name lists and return a deduplicated, sorted vec.
///
/// Pure, testable.
pub fn merge_mcps(project_scoped: Vec<String>, claude_listed: Vec<String>) -> Vec<String> {
    let mut all: Vec<String> = project_scoped;
    all.extend(claude_listed);
    all.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    all.dedup();
    all
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mcp_json_returns_server_names_sorted() {
        let raw = r#"{
            "mcpServers": {
                "zed": {"command": "z"},
                "alpha": {"command": "a"},
                "Mid": {"command": "m"}
            }
        }"#;
        assert_eq!(parse_mcp_json(raw), vec!["alpha", "Mid", "zed"]);
    }

    #[test]
    fn parse_mcp_json_handles_empty_servers_map() {
        let raw = r#"{"mcpServers": {}}"#;
        assert!(parse_mcp_json(raw).is_empty());
    }

    #[test]
    fn parse_mcp_json_returns_empty_for_garbage() {
        assert!(parse_mcp_json("not json").is_empty());
        assert!(parse_mcp_json("").is_empty());
        // Wrong shape: no mcpServers key.
        assert!(parse_mcp_json(r#"{"foo": []}"#).is_empty());
    }

    #[test]
    fn read_project_mcp_json_returns_empty_when_missing() {
        let tmp = std::env::temp_dir().join(format!(
            "bruce-mcp-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        // No `.mcp.json` written.
        assert!(read_project_mcp_json(&tmp).is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parse_claude_list_extracts_connected_servers() {
        let raw = "Checking MCP server health…\n\n\
github: gh-cli --remote - ✔ Connected\n\
filesystem: node fs-server.js - ✔ Connected\n\
postgres: pg-mcp - ✔ Connected\n";
        assert_eq!(
            parse_claude_list(raw),
            vec!["filesystem", "github", "postgres"]
        );
    }

    #[test]
    fn parse_claude_list_skips_unauthenticated_servers() {
        // Real-world: Google Drive shows up here with "Needs authentication"
        // when the user hasn't completed OAuth. They don't think of it as
        // "active", so it must not appear in the subpanel.
        let raw = "\
claude.ai Google Drive: https://drivemcp.googleapis.com/mcp/v1 - ! Needs authentication\n\
github: gh-cli - ✔ Connected\n";
        assert_eq!(parse_claude_list(raw), vec!["github"]);
    }

    #[test]
    fn parse_claude_list_unwraps_plugin_namespaced_names() {
        // Plugin-provided MCPs come through as "plugin:<plugin>:<server>" —
        // the colon-separated prefix would have made the old parser show
        // "plugin" instead of the real server name.
        let raw = "plugin:engram:engram: engram mcp --tools=agent - ✔ Connected\n";
        assert_eq!(parse_claude_list(raw), vec!["engram"]);
    }

    #[test]
    fn parse_claude_list_handles_realistic_mixed_output() {
        // Exactly the shape `claude mcp list` produced for the v0.16.1 dogfood:
        // a header line, a blank line, an unauthenticated entry, and a
        // plugin-namespaced connected one. Only the connected one survives,
        // with its display name unwrapped.
        let raw = "Checking MCP server health…\n\n\
claude.ai Google Drive: https://drivemcp.googleapis.com/mcp/v1 - ! Needs authentication\n\
plugin:engram:engram: engram mcp --tools=agent - ✔ Connected\n";
        assert_eq!(parse_claude_list(raw), vec!["engram"]);
    }

    #[test]
    fn parse_claude_list_skips_blank_and_malformed_lines() {
        let raw = "\n\
no-status-line\n\
no-colon-line - ✔ Connected\n\
   : empty-name - ✔ Connected\n\
realone: foo - ✔ Connected\n";
        assert_eq!(parse_claude_list(raw), vec!["realone"]);
    }

    #[test]
    fn parse_claude_list_dedupes_repeated_names() {
        let raw = "foo: bar - ✔ Connected\nfoo: bar - ✔ Connected\n";
        assert_eq!(parse_claude_list(raw), vec!["foo"]);
    }

    #[test]
    fn merge_mcps_dedupes_and_sorts_case_insensitively() {
        let merged = merge_mcps(
            vec!["github".to_string(), "Filesystem".to_string()],
            vec!["github".to_string(), "postgres".to_string()],
        );
        assert_eq!(merged, vec!["Filesystem", "github", "postgres"]);
    }
}
