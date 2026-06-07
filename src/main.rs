//! Bruce — a terminal workspace for Claude Code.
//!
//! `main` only parses the CLI and dispatches to a subcommand. All TUI logic
//! lives in [`app`].

mod app;
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
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Launch the TUI workspace.
    Tui,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Tui => app::run(),
    }
}
