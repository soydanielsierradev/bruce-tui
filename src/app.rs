//! Global application state and the terminal event loop.

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::style::Color;

use crate::session::Session;
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
    let outcome = run_loop(&mut terminal);
    // Hand the terminal back with its own colours restored before leaving the
    // alternate screen, so the user's normal prompt isn't left recoloured.
    reset_terminal_colors();
    ratatui::restore();

    // If the user asked to auto-update, run it now — outside the TUI, so the
    // package manager's output is visible — then exit (the new binary is picked
    // up on the next launch).
    if let Some(argv) = outcome? {
        run_update(&argv);
    }
    Ok(())
}

/// Run the resolved update command with inherited stdio so the user sees the
/// package manager's progress, then print whether to restart.
fn run_update(argv: &[String]) {
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    println!("\nUpdating Bruce: {}\n", argv.join(" "));
    match std::process::Command::new(program).args(args).status() {
        Ok(status) if status.success() => {
            println!("\nDone. Restart `bruce` to use the new version.");
        }
        Ok(status) => {
            eprintln!("\nUpdate command exited with {status}. Try running it manually.");
        }
        Err(e) => {
            eprintln!("\nCouldn't run the update command ({e}). Run it manually.");
        }
    }
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

/// Draw / input loop. Returns when the user quits; `Some(argv)` means the user
/// asked to auto-update, to be run after the TUI is torn down.
fn run_loop(terminal: &mut ratatui::DefaultTerminal) -> Result<Option<Vec<String>>> {
    let mut welcome = WelcomeState::new();
    let mut screen = Screen::Welcome;
    // Last theme pushed to the terminal via OSC, so we only re-emit on change.
    let mut applied_theme: Option<Theme> = None;
    // Set when the user triggers an auto-update; returned to `run` to execute.
    let mut pending_update: Option<Vec<String>> = None;

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

        // Per-frame upkeep before drawing: refresh the Git pane on its throttle
        // so changes made during the session are reflected live.
        if let Screen::Workspace(ws) = &mut screen {
            ws.tick();
        }
        // Pick up the background update check whenever it finishes (cheap no-op
        // otherwise); the welcome state lives across both screens.
        welcome.poll_update_check();

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
        // Set instead of breaking mid-match, so leaving a workspace can persist
        // its metrics at one central point before the loop exits.
        let mut quit = false;

        match &mut screen {
            Screen::Welcome => {
                // While a dialog is open it captures every key — including 'q'
                // and Esc — so nothing leaks to the underlying navigation. A
                // confirmed new-session form comes back as an event to act on.
                if welcome.dialog.is_some() {
                    match welcome.dialog_key(key.code) {
                        WelcomeEvent::RunUpdate(argv) => {
                            // Tear down the TUI and run the update from `run`.
                            pending_update = Some(argv);
                            break;
                        }
                        WelcomeEvent::CreateSession { name } => {
                        // A new session is persisted the moment it's created
                        // (write-on-create), so it survives a crash or power cut
                        // even before any clean exit. Launch fresh (--session-id)
                        // pinned to the new id so it can be resumed later.
                        // Sessions are scoped to the project Bruce was opened
                        // in; reuse the welcome's project path so creation and
                        // the filtered list agree on one source of truth.
                        let cwd = welcome.project_path.clone();
                        // Tag the session with the repo's current branch so the
                        // welcome list shows it (None outside a repo / detached).
                        let branch = crate::panels::git::current_branch_name(&cwd);
                        let session = Session::new(name, cwd, branch);
                        // Best-effort persist: if the disk write fails the
                        // workspace still opens, only this run won't be resumable.
                        let _ = session.save();
                        // Open with the user's saved panel preferences; they can
                        // still toggle them live with Ctrl+g / Ctrl+m.
                        transition = Some(Screen::Workspace(WorkspaceState::new(
                            session,
                            false,
                            welcome.theme,
                            welcome.git_enabled,
                            welcome.metrics_enabled,
                        )));
                        }
                        WelcomeEvent::None => {}
                    }
                } else {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => break,
                        KeyCode::Tab | KeyCode::BackTab => welcome.focus_next(),
                        KeyCode::Up => welcome.select_prev(),
                        KeyCode::Down => welcome.select_next(),
                        KeyCode::Char('n') | KeyCode::Char('N') => welcome.focus_new_session(),
                        KeyCode::Enter => {
                            if welcome.on_new_session() {
                                welcome.open_new_session();
                            } else if welcome.on_rename() {
                                welcome.open_picker(welcome::PickerAction::Rename);
                            } else if welcome.on_duplicate() {
                                welcome.open_picker(welcome::PickerAction::Duplicate);
                            } else if welcome.on_delete() {
                                welcome.open_picker(welcome::PickerAction::Delete);
                            } else if welcome.on_app_check() {
                                welcome.start_update_check();
                            } else if welcome.on_app_update() {
                                welcome.open_update_info();
                            } else if welcome.on_session() {
                                if let Some(s) =
                                    welcome.sessions.get(welcome.session_selected)
                                {
                                    // Resume the persisted conversation: launch
                                    // `claude --resume <id>` in its project dir so
                                    // Claude rebuilds the full transcript. Bump
                                    // last_used now that it's being opened.
                                    let mut session = s.clone();
                                    let _ = session.touch();
                                    transition = Some(Screen::Workspace(WorkspaceState::new(
                                        session,
                                        true,
                                        welcome.theme,
                                        welcome.git_enabled,
                                        welcome.metrics_enabled,
                                    )));
                                }
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
                            KeyCode::Char('q') => quit = true,
                            _ => {} // unknown command: swallow it
                        }
                    } else if ctrl && matches!(key.code, KeyCode::Char('b')) {
                        ws.leader_pending = true;
                    } else if key.modifiers.contains(KeyModifiers::SHIFT)
                        && key.code == KeyCode::PageUp
                    {
                        // Shift+PageUp/PageDown page through the scrollback, the
                        // terminal-standard keys — so they don't reach Claude.
                        ws.scroll_up();
                    } else if key.modifiers.contains(KeyModifiers::SHIFT)
                        && key.code == KeyCode::PageDown
                    {
                        ws.scroll_down();
                    } else {
                        // Everything else is the user typing into Claude.
                        ws.send_key(&key);
                    }
                } else {
                    // A side pane has focus: navigate Bruce directly.
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') => quit = true,
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

        // Leaving a workspace (back to welcome or quitting): persist its soft
        // metrics, and remember the panel visibility the user ended on as the
        // new global preference.
        if quit || matches!(transition, Some(Screen::Welcome)) {
            if let Screen::Workspace(ws) = &mut screen {
                ws.persist_metrics();
                welcome.git_enabled = ws.git_enabled;
                welcome.metrics_enabled = ws.metrics_enabled;
                welcome.persist_config();
            }
        }

        if quit {
            break;
        }

        if let Some(next) = transition {
            // Returning to the welcome screen: reload sessions so a session just
            // created or used shows up with fresh last_used / token metrics.
            if matches!(next, Screen::Welcome) {
                welcome.reload_sessions();
            }
            screen = next;
        }
    }

    Ok(pending_update)
}
