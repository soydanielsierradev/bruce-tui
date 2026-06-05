//! Global application state and the terminal event loop.

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::ui::welcome::{self, WelcomeEvent, WelcomeState};
use crate::ui::workspace::{self, WorkspaceState};

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
    ratatui::restore();
    result
}

/// Draw / input loop. Returns when the user quits.
fn run_loop(terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    let mut welcome = WelcomeState::new();
    let mut screen = Screen::Welcome;

    loop {
        terminal.draw(|frame| match &screen {
            Screen::Welcome => welcome::render(frame, &welcome),
            Screen::Workspace(ws) => workspace::render(frame, ws),
        })?;

        // `event::read` blocks until the next terminal event.
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

        if let Some(next) = transition {
            screen = next;
        }
    }

    Ok(())
}
