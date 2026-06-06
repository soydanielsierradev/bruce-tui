//! Global application state and the terminal event loop.

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::style::Color;

use crate::ui::theme::Theme;
use crate::ui::welcome::{self, WelcomeEvent, WelcomeState};
use crate::ui::workspace::{self, Panel, WorkspaceState};

/// Which screen is currently active.
///
/// `WelcomeState` is kept *outside* this enum (alive for the whole loop) so
/// returning from the workspace preserves the welcome selection and theme.
enum Screen {
    Welcome,
    Workspace(WorkspaceState),
}

/// Entry point for the `bruce tui` subcommand.
///
/// Sets up the terminal (raw mode + alternate screen), runs the event loop,
/// and always restores the terminal afterwards — even on error.
pub fn run() -> Result<()> {
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal);
    // Hand the terminal back with its own colours restored before leaving the
    // alternate screen, so the user's normal prompt isn't left recoloured.
    reset_terminal_colors();
    ratatui::restore();
    result
}

/// Tell the host terminal to recolour its *default* foreground/background to the
/// theme via OSC 10/11. Unlike painting cells, this also covers the window
/// padding around the character grid, so the whole window matches the theme
/// instead of framing Bruce in the terminal's own background colour.
fn apply_terminal_colors(theme: Theme) {
    let pal = theme.palette();
    let mut out = std::io::stdout();
    if let Some((r, g, b)) = rgb(pal.bg) {
        let _ = write!(out, "\x1b]11;#{r:02x}{g:02x}{b:02x}\x07");
    }
    if let Some((r, g, b)) = rgb(pal.fg) {
        let _ = write!(out, "\x1b]10;#{r:02x}{g:02x}{b:02x}\x07");
    }
    let _ = out.flush();
}

/// Restore the terminal's default foreground/background (OSC 110/111).
fn reset_terminal_colors() {
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]110\x07\x1b]111\x07");
    let _ = out.flush();
}

/// Extract RGB from a palette colour. All built-in themes use `Color::Rgb`, so
/// non-RGB colours (which OSC can't express as a hex triplet) are simply skipped.
fn rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

/// Draw / input loop. Returns when the user quits.
fn run_loop(terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    let mut welcome = WelcomeState::new();
    let mut screen = Screen::Welcome;
    // Last theme pushed to the terminal via OSC, so we only re-emit on change.
    let mut applied_theme: Option<Theme> = None;

    loop {
        // The active theme lives on whichever screen is showing; the workspace
        // carries a snapshot of the welcome theme it was opened with.
        let current_theme = match &screen {
            Screen::Welcome => welcome.theme,
            Screen::Workspace(ws) => ws.theme,
        };
        if applied_theme != Some(current_theme) {
            apply_terminal_colors(current_theme);
            applied_theme = Some(current_theme);
        }

        terminal.draw(|frame| match &screen {
            Screen::Welcome => welcome::render(frame, &welcome),
            Screen::Workspace(ws) => workspace::render(frame, ws),
        })?;

        // Poll instead of blocking: the reader thread feeds PTY output into the
        // emulator, so we must redraw on a timer even when no key is pressed.
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };

        // On Windows, key events fire for both press and release; only act on
        // presses so every keystroke registers exactly once.
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // Pending screen change, applied after the match so we never reassign
        // `screen` while it is borrowed.
        let mut transition: Option<Screen> = None;

        match &mut screen {
            Screen::Welcome => {
                // While a dialog is open it captures every key — including 'q'
                // and Esc — so nothing leaks to the underlying navigation. A
                // confirmed new-session form comes back as an event to act on.
                if welcome.dialog.is_some() {
                    if let WelcomeEvent::CreateSession { name } = welcome.dialog_key(key.code) {
                        // New sessions start with every pane visible; the user
                        // toggles them live with Ctrl+g / Ctrl+m.
                        transition = Some(Screen::Workspace(WorkspaceState::new(
                            name,
                            welcome.theme,
                            true,
                            true,
                        )));
                    }
                } else {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => break,
                        KeyCode::Tab | KeyCode::BackTab => welcome.focus_next(),
                        KeyCode::Up => welcome.select_prev(),
                        KeyCode::Down => welcome.select_next(),
                        KeyCode::Left => welcome.theme = welcome.theme.prev(),
                        KeyCode::Right => welcome.theme = welcome.theme.next(),
                        KeyCode::Char('n') | KeyCode::Char('N') => welcome.focus_new_session(),
                        KeyCode::Enter => {
                            if welcome.on_new_session() {
                                welcome.open_new_session();
                            } else if welcome.on_rename() {
                                welcome.open_rename();
                            } else if let Some(s) =
                                welcome.sessions.get(welcome.session_selected)
                            {
                                // Existing sessions enable both panes until the
                                // session module persists per-session config.
                                transition = Some(Screen::Workspace(WorkspaceState::new(
                                    s.name.clone(),
                                    welcome.theme,
                                    true,
                                    true,
                                )));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Screen::Workspace(ws) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let claude_focused = ws.focus == Panel::Claude && ws.pty.is_some();

                if claude_focused {
                    if ws.leader_pending {
                        // Second key of the Ctrl+b chord: a Bruce command.
                        ws.leader_pending = false;
                        match key.code {
                            KeyCode::Char('b') => transition = Some(Screen::Welcome),
                            KeyCode::Tab => ws.focus_next(),
                            KeyCode::BackTab => ws.focus_prev(),
                            KeyCode::Char('g') => ws.toggle_git(),
                            KeyCode::Char('m') => ws.toggle_metrics(),
                            KeyCode::Char('q') => break,
                            _ => {} // unknown command: swallow it
                        }
                    } else if ctrl && matches!(key.code, KeyCode::Char('b')) {
                        ws.leader_pending = true;
                    } else {
                        // Everything else is the user typing into Claude.
                        ws.send_key(&key);
                    }
                } else {
                    // A side pane has focus: navigate Bruce directly.
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') => break,
                        KeyCode::Esc => transition = Some(Screen::Welcome),
                        KeyCode::Tab => ws.focus_next(),
                        KeyCode::BackTab => ws.focus_prev(),
                        KeyCode::Char('g') if ctrl => ws.toggle_git(),
                        KeyCode::Char('m') if ctrl => ws.toggle_metrics(),
                        _ => {}
                    }
                }
            }
        }

        if let Some(next) = transition {
            screen = next;
        }
    }

    Ok(())
}
