//! Bruce — a terminal workspace for Claude Code.
//!
//! `main` only parses the CLI and dispatches to a subcommand. All TUI logic
//! lives in [`app`].

mod app;
mod config;
mod panels;
mod pty;
mod session;
mod ui;

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
        Some(Command::Tui) | None => app::run(),
    }
}
