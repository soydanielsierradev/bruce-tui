//! Bruce — a terminal workspace for Claude Code.
//!
//! `main` only parses the CLI and dispatches to a subcommand. All TUI logic
//! lives in [`app`].

// Bruce shouldn't need to reach into raw pointers anywhere. Pin that as a
// crate-wide build-time check so a future refactor can't sneak unsafe in
// without an explicit per-block override.
#![deny(unsafe_code)]

mod app;
mod config;
mod mcp;
mod panels;
mod pty;
mod session;
mod skills;
mod ui;
mod update;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Top-level CLI definition.
#[derive(Parser)]
#[command(name = "bruce", version, about = "TUI workspace for Claude Code")]
struct Cli {
    /// Subcommand to run. When omitted, Bruce launches the TUI directly, so
    /// `bruce` and `bruce tui` are equivalent.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Launch the TUI workspace.
    Tui,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // No subcommand defaults to the TUI — running `bruce` just opens it.
    match cli.command {
        Some(Command::Tui) | None => {
            warn_if_claude_missing();
            app::run()
        }
    }
}

/// Warn (before entering the TUI) if the `claude` CLI isn't on the PATH.
///
/// Bruce runs Claude Code inside its workspace, so without it the center pane is
/// dead. The warning prints to stderr — visible before the alternate screen
/// opens — and waits for Enter so it isn't missed, but lets the user continue.
fn warn_if_claude_missing() {
    use std::io::Write;

    if !pty::claude_missing() {
        return;
    }
    let mut err = std::io::stderr();
    let _ = writeln!(err, "\n⚠  Bruce could not find the `claude` CLI on your PATH.");
    let _ = writeln!(err, "   Bruce runs Claude Code inside its workspace, so the");
    let _ = writeln!(err, "   center pane won't work until it's installed:");
    let _ = writeln!(err, "   https://docs.claude.com/claude-code\n");
    let _ = write!(err, "   Press Enter to continue anyway, or Ctrl+C to quit... ");
    let _ = err.flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}
