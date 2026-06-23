//! Global application state and the terminal event loop.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
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
    /// Brief animated transition while a session's Claude process boots, before
    /// revealing its workspace.
    Loading(LoadingState),
    Workspace(WorkspaceState),
}

/// The loading transition between picking a session and its workspace: holds the
/// already-built workspace (whose PTY is booting Claude in the background) and
/// the animation's start time.
struct LoadingState {
    /// The workspace to reveal once loading finishes.
    ws: WorkspaceState,
    /// Status line shown under the wordmark (e.g. "Creating a new session…").
    message: &'static str,
    /// When the transition began, driving both the spinner and the min/max wait.
    started: Instant,
}

/// Shortest time the loading screen stays up, so it reads as an intentional
/// transition rather than a flash.
const LOADING_MIN: Duration = Duration::from_millis(650);
/// Longest the loading screen waits for Claude to paint before revealing the
/// workspace anyway, so a slow or stuck start can't trap the user here.
const LOADING_MAX: Duration = Duration::from_millis(5_000);

impl LoadingState {
    fn new(ws: WorkspaceState, message: &'static str) -> Self {
        Self { ws, message, started: Instant::now() }
    }

    /// Time to reveal the workspace: past the minimum, and either Claude has
    /// started painting or we've hit the cap.
    fn ready(&self) -> bool {
        let elapsed = self.started.elapsed();
        elapsed >= LOADING_MIN && (self.ws.pty_has_output() || elapsed >= LOADING_MAX)
    }

    /// Spinner frame index, derived from elapsed time (~110 ms per frame).
    fn tick(&self) -> usize {
        (self.started.elapsed().as_millis() / 110) as usize
    }
}

/// Build the loading transition that opens `session` in a workspace, inheriting
/// the welcome screen's theme and look/panel preferences. `resume` picks how
/// Claude launches (`--resume` vs `--session-id`); `message` is the loading line.
fn open_session_loading(
    welcome: &WelcomeState,
    session: Session,
    resume: bool,
    message: &'static str,
) -> Screen {
    let ws = WorkspaceState::new(
        session,
        resume,
        welcome.theme,
        welcome.git_enabled,
        welcome.show_footer,
        welcome.show_title,
        welcome.border_style,
        welcome.side_width,
        welcome.nerd_icons,
    );
    Screen::Loading(LoadingState::new(ws, message))
}

/// Entry point for the `bruce tui` subcommand.
///
/// Sets up the terminal (raw mode + alternate screen), runs the event loop,
/// and always restores the terminal afterwards — even on error.
pub fn run() -> Result<()> {
    let mut terminal = ratatui::init();
    // Ask the terminal to wrap pasted text in markers (ESC[200~ … ESC[201~) and
    // deliver it as a single `Event::Paste`. Without this a multi-line paste
    // arrives as one Enter keypress per line, and the Claude pane submits a
    // message on each — splitting the paste into many sends.
    set_bracketed_paste(true);
    // Ask the terminal to disambiguate key combos that legacy terminals can't
    // tell apart — most importantly Ctrl+1/2/3, which otherwise arrive as bare
    // digits so the pane-switch shortcuts silently do nothing.
    let keyboard_enhanced = set_keyboard_enhancement(true);
    let outcome = run_loop(&mut terminal);
    // Hand the terminal back with its own colours restored before leaving the
    // alternate screen, so the user's normal prompt isn't left recoloured.
    if keyboard_enhanced {
        set_keyboard_enhancement(false);
    }
    set_bracketed_paste(false);
    set_mouse_capture(false);
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

/// Enable or disable terminal mouse reporting. Used so the welcome screen can
/// hit-test clicks on the author link, while the workspace keeps native terminal
/// text selection (capture is turned off there). Best-effort: a terminal without
/// mouse support simply ignores it.
fn set_mouse_capture(on: bool) {
    let mut out = std::io::stdout();
    let _ = if on {
        crossterm::execute!(out, EnableMouseCapture)
    } else {
        crossterm::execute!(out, DisableMouseCapture)
    };
}

/// Push or pop the Kitty keyboard protocol's `DISAMBIGUATE_ESCAPE_CODES` flag.
///
/// Legacy terminals collapse several distinct combos onto the same byte —
/// `Ctrl+1`/`Ctrl+2`/`Ctrl+3` arrive as plain `1`/`2`/`3` (only `Ctrl+4` survives,
/// as the FS control code), so Bruce's pane-switch shortcuts appear dead. The
/// protocol makes the terminal report each combo distinctly.
///
/// Only the single disambiguation flag is requested — not key-release or repeat
/// reporting — so the event stream stays the same shape (still press-only). When
/// enabling, returns whether the flag was actually pushed; the caller pops it on
/// exit only when so. Guarded by [`supports_keyboard_enhancement`] so terminals
/// without support (e.g. the legacy Windows console) are left untouched.
fn set_keyboard_enhancement(on: bool) -> bool {
    use crossterm::event::{
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    };
    let mut out = std::io::stdout();
    if on {
        if !matches!(crossterm::terminal::supports_keyboard_enhancement(), Ok(true)) {
            return false;
        }
        crossterm::execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok()
    } else {
        let _ = crossterm::execute!(out, PopKeyboardEnhancementFlags);
        false
    }
}

/// Enable or disable bracketed paste mode. When on, the terminal delivers a
/// paste as a single [`Event::Paste`] instead of synthesised keystrokes, so the
/// Claude pane can forward the whole block at once. Best-effort: terminals
/// without support simply ignore it (and pasting falls back to per-key events).
fn set_bracketed_paste(on: bool) {
    let mut out = std::io::stdout();
    let _ = if on {
        crossterm::execute!(out, EnableBracketedPaste)
    } else {
        crossterm::execute!(out, DisableBracketedPaste)
    };
}

/// Longest gap tolerated between keystrokes while coalescing a burst. A paste
/// streams in with sub-millisecond gaps between the console's input batches, so
/// we keep collecting across them; a human types far slower, so after this long
/// without a key the burst is considered finished. Small enough that the wait
/// added to ordinary typing is imperceptible.
const BURST_GAP: Duration = Duration::from_millis(10);

/// Forward typing — and pastes — to the Claude pane, coalescing a burst.
///
/// `Event::Paste` covers pastes on Unix, but on Windows crossterm reads the
/// console via the WinAPI backend, which delivers a paste as a stream of
/// individual key events and never emits `Event::Paste`. So when a plain
/// character lands on the Claude pane we keep collecting keys until input goes
/// quiet (see [`BURST_GAP`]) — spanning the micro-gaps between the console's
/// batches so the *whole* paste is gathered before anything is sent:
///
/// - a multi-line run is sent as ONE bracketed paste, so it lands as a single
///   insert instead of submitting a message per line. Collecting it whole (not
///   in fragments) matters: mixing bracketed-paste chunks with directly typed
///   chunks races inside Claude's input and scrambles the result.
/// - anything else is replayed verbatim, so ordinary typing and Enter-to-submit
///   behave exactly as before.
///
/// Returns the first event read past the burst (a chord, a resize, …) so the
/// caller can process it instead of dropping it.
fn forward_typing(ws: &WorkspaceState, first: KeyEvent) -> Result<Option<Event>> {
    // The character a plain typing key inserts, or `None` if it isn't plain
    // text (a modifier chord, an arrow, a function key — which ends the burst).
    fn as_text(key: &KeyEvent) -> Option<char> {
        let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
        match key.code {
            KeyCode::Char(c) if plain => Some(c),
            KeyCode::Enter if plain => Some('\n'),
            KeyCode::Tab if plain => Some('\t'),
            _ => None,
        }
    }

    let Some(first_ch) = as_text(&first) else {
        ws.send_key(&first);
        return Ok(None);
    };

    let mut buf = String::new();
    buf.push(first_ch);
    let mut stashed = None;

    // Keep collecting until input stays quiet for BURST_GAP. A paste's bytes
    // arrive faster than that gap (so the whole block is gathered here); after
    // the last hand-typed key the wait simply elapses and we fall through.
    while event::poll(BURST_GAP)? {
        let evt = event::read()?;
        match &evt {
            // Skip key-release events (Windows fires press *and* release).
            Event::Key(k) if k.kind != KeyEventKind::Press => continue,
            Event::Key(k) => match as_text(k) {
                Some(c) => buf.push(c),
                None => {
                    stashed = Some(evt);
                    break;
                }
            },
            _ => {
                stashed = Some(evt);
                break;
            }
        }
    }

    // Treat as a paste only when there's a newline with content after it —
    // genuinely multi-line. A lone key or a single line (even one a fast typist
    // ended with Enter) is replayed so Enter still submits.
    let count = buf.chars().count();
    let multiline = buf.trim_end_matches(['\n', '\r']).contains('\n');
    if count == 1 {
        ws.send_key(&first);
    } else if multiline {
        ws.send_paste(&buf);
    } else {
        ws.send_typed(&buf);
    }
    Ok(stashed)
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
    // Last (theme, sync_colors) pushed to the terminal via OSC, so we only
    // re-emit on a real change (theme switch or the Settings toggle).
    let mut applied_colors: Option<(Theme, bool)> = None;
    // Set when the user triggers an auto-update; returned to `run` to execute.
    let mut pending_update: Option<Vec<String>> = None;

    // The welcome screen captures mouse to make the author link clickable; it's
    // turned off in the workspace so the Claude pane keeps native text selection.
    set_mouse_capture(true);

    // An event read past a coalesced typing burst (see `forward_typing`) that
    // still needs handling — processed before polling for the next one.
    let mut pending: Option<Event> = None;

    loop {
        // The active theme lives on whichever screen is showing; the workspace
        // carries a snapshot of the welcome theme it was opened with.
        let current_theme = match &screen {
            Screen::Welcome => welcome.theme,
            Screen::Loading(load) => load.ws.theme,
            Screen::Workspace(ws) => ws.theme,
        };
        // The color-sync preference is global (toggled in the welcome Settings
        // block), so the welcome state is the source of truth on both screens.
        let desired_colors = (current_theme, welcome.sync_colors);
        if applied_colors != Some(desired_colors) {
            if welcome.sync_colors {
                apply_terminal_colors(current_theme);
            } else {
                reset_terminal_colors();
            }
            applied_colors = Some(desired_colors);
        }

        // Per-frame upkeep before drawing: refresh the Git pane on its throttle
        // so changes made during the session are reflected live.
        if let Screen::Workspace(ws) = &mut screen {
            ws.tick();
        }
        // Pick up the background update check whenever it finishes (cheap no-op
        // otherwise); the welcome state lives across both screens.
        welcome.poll_update_check();

        // Reveal the workspace once its loading transition is ready (Claude has
        // painted, or the cap elapsed).
        if matches!(&screen, Screen::Loading(load) if load.ready()) {
            if let Screen::Loading(load) = std::mem::replace(&mut screen, Screen::Welcome) {
                screen = Screen::Workspace(load.ws);
            }
        }

        terminal.draw(|frame| match &screen {
            Screen::Welcome => welcome::render(frame, &welcome),
            Screen::Loading(load) => {
                welcome::render_loading(frame, load.ws.theme, load.message, load.tick())
            }
            Screen::Workspace(ws) => workspace::render(frame, ws),
        })?;

        // Poll instead of blocking: the reader thread feeds PTY output into the
        // emulator, so we must redraw on a timer even when no key is pressed.
        // A `pending` event left over from a coalesced burst jumps the queue.
        let evt = match pending.take() {
            Some(evt) => evt,
            None => {
                if !event::poll(Duration::from_millis(50))? {
                    // Timer tick: poll the install runner if one is active.
                    // This is the ONLY place where tick_install_runner is called
                    // (P2-T4: in the timer branch, not the key branch).
                    if matches!(&screen, Screen::Welcome) {
                        welcome.tick_install_runner();
                    }
                    continue;
                }
                event::read()?
            }
        };
        // A left-click on the welcome screen may land on the author link.
        if let Event::Mouse(me) = evt {
            if matches!(&screen, Screen::Welcome)
                && welcome.dialog.is_none()
                && me.kind == MouseEventKind::Down(MouseButton::Left)
            {
                welcome.click(me.column, me.row);
            }
            continue;
        }
        // A paste arrives as one event (bracketed paste). In the workspace it
        // goes straight to the focused PTY pane (Claude or Terminal) as a
        // bracketed paste so multi-line code lands as a single insert; in a
        // welcome text field we replay its printable characters so pasting a
        // name/query still works.
        if let Event::Paste(text) = evt {
            match &mut screen {
                Screen::Workspace(ws)
                    if ws.focus == Panel::Claude || ws.focus == Panel::Terminal =>
                {
                    ws.send_paste(&text)
                }
                Screen::Welcome if welcome.dialog.is_some() => {
                    for c in text.chars().filter(|c| !c.is_control()) {
                        let synthetic = KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty());
                        welcome.dialog_key(&synthetic);
                    }
                }
                _ => {}
            }
            continue;
        }
        let Event::Key(key) = evt else {
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
                    match welcome.dialog_key(&key) {
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
                        transition = Some(open_session_loading(
                            &welcome,
                            session,
                            false,
                            "Creating a new session…",
                        ));
                        }
                        WelcomeEvent::OpenSession(s) => {
                            // The user picked a session from the Open picker:
                            // resume it. Bump last_used now that it's reopened.
                            let mut session = s;
                            let _ = session.touch();
                            transition = Some(open_session_loading(
                                &welcome,
                                session,
                                true,
                                "Resuming session…",
                            ));
                        }
                        WelcomeEvent::ReopenSession(s) => {
                            // Re-launch a session after a skill change (resume mode).
                            let mut session = s;
                            let _ = session.touch();
                            transition = Some(open_session_loading(
                                &welcome,
                                session,
                                true,
                                "Restarting session…",
                            ));
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
                            if welcome.on_open_session() {
                                welcome.open_picker(welcome::PickerAction::Open);
                            } else if welcome.on_new_session() {
                                welcome.open_new_session();
                            } else if welcome.on_rename() {
                                welcome.open_picker(welcome::PickerAction::Rename);
                            } else if welcome.on_duplicate() {
                                welcome.open_picker(welcome::PickerAction::Duplicate);
                            } else if welcome.on_delete() {
                                welcome.open_picker(welcome::PickerAction::Delete);
                            } else if welcome.on_check_updates() {
                                welcome.start_update_check();
                            } else if welcome.on_update_latest() {
                                welcome.open_update_info();
                            } else if welcome.on_settings_theme() {
                                welcome.open_theme_picker();
                            } else if welcome.on_settings() {
                                welcome.toggle_setting();
                            } else if welcome.on_doc_github() {
                                welcome.open_github();
                            } else if welcome.on_doc_keys() {
                                welcome.open_keybindings();
                            } else if welcome.on_skills_manage() {
                                welcome.open_manage_dialog();
                            } else if welcome.on_skills_install() {
                                welcome.open_install_dialog();
                            }
                        }
                        _ => {}
                    }
                }
            }
            // The loading transition is non-interactive; swallow keys until it
            // promotes itself to the workspace.
            Screen::Loading(_) => {}
            Screen::Workspace(ws) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                // Scrollback pages on Shift+PageUp/PageDown (the terminal-standard
                // keys); Ctrl is accepted too since some terminals swallow one or
                // the other. The plain keys still reach Claude.
                let scroll_mod = ctrl || key.modifiers.contains(KeyModifiers::SHIFT);

                // ── Dialog intercept (highest priority) ──────────────────────
                // When the Ctrl+F file-search overlay is open, route ALL keys to
                // the dialog — nothing reaches Claude, the shell, or the panel
                // nav branches below. This must come BEFORE the pane-switch
                // and forward_typing branches so Ctrl+F is never forwarded.
                if ws.dialog_open() {
                    match key.code {
                        KeyCode::Esc => ws.close_dialog(),
                        KeyCode::Enter => ws.fs_open_selected(),
                        KeyCode::Up => ws.fs_prev(),
                        KeyCode::Down => ws.fs_next(),
                        KeyCode::Backspace => ws.fs_pop_char(),
                        KeyCode::Char(c) if !ctrl => ws.fs_push_char(c),
                        _ => {} // swallow unhandled keys while dialog is open
                    }
                    // Skip ALL other branches: dialog consumes the event.
                } else if ctrl && key.code == KeyCode::Char('f') {
                    // Ctrl+F opens the file-search overlay from ANY pane.
                    ws.open_file_search();
                } else if ctrl && key.code == KeyCode::Char('t') {
                    // Ctrl+T toggles the terminal pane from ANY pane.
                    ws.toggle_terminal();
                } else {

                // Ctrl+1/2/3/4 jump straight to a pane from anywhere — even
                // while Claude has focus — so switching never needs the leader chord.
                let pane = ctrl
                    .then(|| match key.code {
                        KeyCode::Char('1') => Some(Panel::Git),
                        KeyCode::Char('2') => Some(Panel::Claude),
                        KeyCode::Char('3') => Some(Panel::FileManager),
                        KeyCode::Char('4') => Some(Panel::Terminal),
                        _ => None,
                    })
                    .flatten();

                // A PTY pane (Claude or Terminal) has focus: typing goes to the shell.
                let pty_focused = (ws.focus == Panel::Claude && ws.pty.is_some())
                    || (ws.focus == Panel::Terminal && ws.terminal.is_some());

                if let Some(panel) = pane {
                    ws.focus_panel(panel);
                } else if pty_focused {
                    // Enter on a dead terminal shell respawns it instead of
                    // sending the keystroke to a no-longer-running process.
                    if ws.focus == Panel::Terminal
                        && ws.terminal_exit_code.is_some()
                        && key.code == KeyCode::Enter
                    {
                        ws.respawn_terminal();
                    } else if ws.leader_pending {
                        // Second key of the Ctrl+b chord: a Bruce command.
                        ws.leader_pending = false;
                        match key.code {
                            KeyCode::Char('b') => transition = Some(Screen::Welcome),
                            KeyCode::Tab => ws.focus_next(),
                            KeyCode::BackTab => ws.focus_prev(),
                            KeyCode::Char('g') => ws.toggle_git(),
                            KeyCode::Char('t') => ws.toggle_terminal(),
                            KeyCode::Char('q') => quit = true,
                            _ => {} // unknown command: swallow it
                        }
                    } else if scroll_mod && key.code == KeyCode::PageUp {
                        ws.scroll_up();
                    } else if scroll_mod && key.code == KeyCode::PageDown {
                        ws.scroll_down();
                    } else if ctrl && matches!(key.code, KeyCode::Char('b')) {
                        ws.leader_pending = true;
                    } else {
                        // Everything else is the user typing into the focused PTY.
                        // Coalesce any burst so a multi-line paste lands as one
                        // bracketed insert (Windows path where crossterm doesn't
                        // surface bracketed paste). Any event read past the burst
                        // is stashed for the next iteration.
                        pending = forward_typing(ws, key)?;
                    }
                } else if ws.focus == Panel::FileManager {
                    // File Manager pane has focus: browse the directory tree.
                    // Up/Down move the selection; Enter descends into a folder
                    // (or opens a file in the editor); Left/Backspace go up; `.`
                    // toggles dotfiles. Ctrl+1/2/3/4 handled above via `pane`.
                    match key.code {
                        KeyCode::Up => ws.fm_prev(),
                        KeyCode::Down => ws.fm_next(),
                        KeyCode::Enter => ws.fm_enter(),
                        KeyCode::Left | KeyCode::Backspace => ws.fm_up(),
                        KeyCode::Char('.') => ws.fm_toggle_hidden(),
                        KeyCode::Char('q') | KeyCode::Char('Q') => quit = true,
                        KeyCode::Esc => transition = Some(Screen::Welcome),
                        KeyCode::Tab => ws.focus_next(),
                        KeyCode::BackTab => ws.focus_prev(),
                        KeyCode::Char('g') if ctrl => ws.toggle_git(),
                        _ => {}
                    }
                } else {
                    // A non-PTY pane (Git) has focus, or the Terminal pane before
                    // first-spawn. Tab triggers spawn via focus_next → ensure_terminal.
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') => quit = true,
                        KeyCode::Esc => transition = Some(Screen::Welcome),
                        KeyCode::Tab => ws.focus_next(),
                        KeyCode::BackTab => ws.focus_prev(),
                        KeyCode::Char('g') if ctrl => ws.toggle_git(),
                        KeyCode::PageUp if scroll_mod => ws.scroll_up(),
                        KeyCode::PageDown if scroll_mod => ws.scroll_down(),
                        _ => {}
                    }
                }

                } // end of `else` block wrapping all non-dialog workspace key handling
            }
        }

        // Leaving a workspace (back to welcome or quitting): persist its soft
        // metrics, and remember the panel visibility the user ended on as the
        // new global preference.
        if quit || matches!(transition, Some(Screen::Welcome)) {
            if let Screen::Workspace(ws) = &mut screen {
                ws.persist_metrics();
                welcome.git_enabled = ws.git_enabled;
                welcome.persist_config();
            }
        }

        if quit {
            break;
        }

        if let Some(next) = transition {
            match &next {
                // Returning to the welcome screen: reload sessions so a session
                // just created or used shows up with fresh metrics, and re-arm
                // the mouse so the author link is clickable again.
                Screen::Welcome => {
                    welcome.reload_sessions();
                    set_mouse_capture(true);
                }
                // Entering a workspace (via the loading screen): release the mouse
                // so the Claude pane keeps native terminal selection.
                Screen::Loading(_) => set_mouse_capture(false),
                Screen::Workspace(_) => {}
            }
            screen = next;
        }
    }

    Ok(pending_update)
}
