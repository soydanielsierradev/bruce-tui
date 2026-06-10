//! Welcome screen: ASCII logo, an Options block, the saved-session list,
//! a theme selector and two modal dialogs (new session / rename session).
//!
//! This module owns both the screen *state* ([`WelcomeState`]) and its
//! *rendering* ([`render`]). For step 1 the session list is hardcoded; it will
//! be replaced by real persisted sessions once the `session` module lands.

use std::cell::Cell;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph},
};

use crate::config::Config;
use crate::session::{self, Session};
use crate::ui::theme::{BorderStyle, Palette, SideWidth, Theme};
use crate::update;

/// Re-check for a new release at most this often (seconds): once a day.
const UPDATE_CHECK_TTL: i64 = 24 * 60 * 60;


/// ASCII banner rendered at the top of the welcome screen.
///
/// Generated from the word "Bruce" with the FIGlet font "Delta Corps Priest 1".
/// Lines use Unicode block glyphs and are normalised to equal width at render
/// time so the block stays aligned when centred.
const LOGO: &str = r#"
▀█████████▄     ▄████████ ███    █▄   ▄████████    ▄████████
  ███    ███   ███    ███ ███    ███ ███    ███   ███    ███
  ███    ███   ███    ███ ███    ███ ███    █▀    ███    █▀
 ▄███▄▄▄██▀   ▄███▄▄▄▄██▀ ███    ███ ███         ▄███▄▄▄
▀▀███▀▀▀██▄  ▀▀███▀▀▀▀▀   ███    ███ ███        ▀▀███▀▀▀
  ███    ██▄ ▀███████████ ███    ███ ███    █▄    ███    █▄
  ███    ███   ███    ███ ███    ███ ███    ███   ███    ███
▄█████████▀    ███    ███ ████████▀  ████████▀    ██████████
               ███    ███
"#;

/// Labels for the Options block, in display order. The index of each entry is
/// what [`WelcomeState::option_selected`] points at.
const OPTION_LABELS: [&str; 7] = [
    " ▸ Open session",
    " + New session",
    " ✎ Rename session",
    " ⧉ Duplicate session",
    " ✕ Delete session",
    " ⟳ Check for updates",
    " ⬆ Update to latest",
];

/// Which panel currently has keyboard focus. `Tab` cycles through them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Options,
    Settings,
    Documentation,
}

/// Documentation-block rows, in display order (index = `doc_selected`).
const DOC_LABELS: [&str; 2] = [" ↗ GitHub", " ≡ Keybindings"];

/// Every keybinding, shown in the Documentation → Keybindings dialog as
/// `(key, description)` rows. An empty key with a non-empty description is a
/// section header; a fully empty entry is a blank spacer.
const KEYBINDINGS: &[(&str, &str)] = &[
    ("", "Welcome"),
    ("Tab / Shift+Tab", "Move focus between blocks"),
    ("↑ / ↓", "Select a row · cycle the theme"),
    ("Enter", "Run the selected option"),
    ("N", "Jump to “New session”"),
    ("Click author", "Open the author’s GitHub"),
    ("q / Q / Esc", "Quit Bruce"),
    ("", ""),
    ("Session picker (open / rename / duplicate / delete)", ""),
    ("type", "Filter the session list"),
    ("↑ / ↓", "Move the selection"),
    ("Enter", "Confirm the action"),
    ("Y / N", "Confirm / cancel a delete"),
    ("Esc", "Close the picker"),
    ("", ""),
    ("Workspace — side pane focused (Git / Metrics)", ""),
    ("Ctrl+1 / Ctrl+2 / Ctrl+3", "Focus Git / Claude / Metrics"),
    ("Tab / Shift+Tab", "Cycle panes"),
    ("Ctrl+g / Ctrl+m", "Toggle the Git / Metrics pane"),
    ("Shift+PgUp / Shift+PgDn", "Scroll Claude’s history"),
    ("Esc", "Back to the welcome screen"),
    ("q / Q", "Quit Bruce"),
    ("", ""),
    ("Workspace — Claude focused", ""),
    ("type", "Send keystrokes to Claude"),
    ("Ctrl+1 / Ctrl+2 / Ctrl+3", "Focus Git / Claude / Metrics"),
    ("Shift+PgUp / Shift+PgDn", "Scroll Claude’s history"),
    ("Ctrl+b", "Leader — then one of:"),
    ("Ctrl+b  b", "Back to the welcome screen"),
    ("Ctrl+b  Tab", "Switch pane"),
    ("Ctrl+b  g / m", "Toggle the Git / Metrics pane"),
    ("Ctrl+b  q", "Quit Bruce"),
];

/// Settings-block rows, in display order (index = `settings_selected`). Each
/// toggles a persisted look preference; the on/off value is read live from
/// state, not from this label.
const SETTINGS_LABELS: [&str; 6] = [
    "Theme",
    "Terminal colors",
    "Border style",
    "Side width",
    "Title bar",
    "Footer hints",
];

/// Outcome of routing a key to an open dialog.
///
/// The dialog itself can't switch screens — that's the event loop's job — so a
/// confirmed "new session" is returned here for `app` to act on.
pub enum WelcomeEvent {
    /// Nothing for the caller to do.
    None,
    /// The user confirmed the new-session form; open a workspace with this name.
    CreateSession { name: String },
    /// The user picked a session to open from the Open-session picker; resume it.
    OpenSession(Session),
    /// The user asked to auto-update; run this argv after tearing down the TUI.
    RunUpdate(Vec<String>),
}

/// State of the version check, for visible feedback in the App block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// No check has run / nothing to report.
    Idle,
    /// A check is in flight (shown after the user presses "Check for updates").
    Checking,
    /// The installed version is the latest.
    UpToDate,
    /// A newer release is available.
    Available(String),
}

/// The single dialog that can be open over the welcome screen.
pub enum Dialog {
    NewSession(NewSessionDialog),
    /// Searchable session picker shared by rename, duplicate and delete.
    Picker(SessionPicker),
    /// "Update to latest" info: how to update + open the releases page.
    UpdateInfo,
    /// Scrollable list of every keybinding; the `u16` is the scroll offset.
    Keybindings(u16),
    /// Theme selector; the `usize` is the highlighted theme's index in
    /// [`Theme::ALL`]. Moving previews (and persists) the theme live.
    ThemePicker(usize),
}

/// Which action the [`SessionPicker`] performs on the chosen session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerAction {
    Open,
    Rename,
    Duplicate,
    Delete,
}

impl PickerAction {
    /// Lowercase verb for footer hints (e.g. "rename").
    fn verb(self) -> &'static str {
        match self {
            PickerAction::Open => "open",
            PickerAction::Rename => "rename",
            PickerAction::Duplicate => "duplicate",
            PickerAction::Delete => "delete",
        }
    }

    /// Title shown on the picker dialog's border.
    fn title(self) -> &'static str {
        match self {
            PickerAction::Open => " Open session ",
            PickerAction::Rename => " Rename session ",
            PickerAction::Duplicate => " Duplicate session ",
            PickerAction::Delete => " Delete session ",
        }
    }
}

/// Step the picker is on: searching/picking, then an action-specific follow-up.
#[derive(Debug, Clone)]
pub enum PickerMode {
    /// Typing in the search box and moving through the filtered results.
    Browse,
    /// Rename follow-up: editing the chosen session's name. `target` indexes
    /// into [`WelcomeState::sessions`].
    EditName { target: usize, buffer: String },
    /// Delete follow-up: confirming removal of the chosen session.
    Confirm { target: usize },
}

/// A modal session picker: a search box over the full session list, plus an
/// action-specific follow-up. Shared by rename, duplicate and delete so all
/// three behave identically — empty query shows every session.
pub struct SessionPicker {
    /// What pressing Enter on a result does.
    pub action: PickerAction,
    /// Current search text; empty matches everything.
    pub query: String,
    /// Selected row *within the filtered results*.
    pub selected: usize,
    /// Current step (browse / edit name / confirm delete).
    pub mode: PickerMode,
}

/// Modal state for creating a session: just a name. Which panels are visible is
/// decided live inside the workspace (Ctrl+g / Ctrl+m), not here.
pub struct NewSessionDialog {
    pub name: String,
}

impl NewSessionDialog {
    fn new() -> Self {
        Self {
            name: String::new(),
        }
    }
}

/// State backing the welcome screen.
pub struct WelcomeState {
    /// Project directory Bruce was opened in. Sessions are scoped to it: the
    /// list shows only this project's sessions and new ones are created here.
    pub project_path: PathBuf,
    /// This project's saved sessions, most-recently-used first.
    pub sessions: Vec<Session>,
    /// Which panel has focus.
    pub focus: Focus,
    /// Selected row within the Options block.
    pub option_selected: usize,
    /// Selected row within the Settings block.
    pub settings_selected: usize,
    /// Selected row within the Documentation block.
    pub doc_selected: usize,
    /// Screen rectangle of the clickable author link in the tagline, recorded at
    /// render time so the event loop can hit-test mouse clicks against it.
    pub name_link: Cell<Rect>,
    /// Active color theme (restored from saved preferences).
    pub theme: Theme,
    /// Whether new/opened sessions start with the Git panel shown. Restored
    /// from preferences and updated when the user toggles it in a workspace.
    pub git_enabled: bool,
    /// Whether new/opened sessions start with the Metrics panel shown.
    pub metrics_enabled: bool,
    /// Repaint the terminal fg/bg to match the theme via OSC (Settings toggle).
    pub sync_colors: bool,
    /// Show the workspace footer hint bar (Settings block toggle).
    pub show_footer: bool,
    /// Show the workspace top title bar (Settings toggle).
    pub show_title: bool,
    /// Line style for the framed side panes (Settings option).
    pub border_style: BorderStyle,
    /// Width of each side pane (Settings option).
    pub side_width: SideWidth,
    /// Result of the version check (drives the badge and the App block).
    pub update_status: UpdateStatus,
    /// In-flight background update check; consumed once it finishes.
    update_check: Option<update::Check>,
    /// Epoch seconds of the last update check (persisted, throttles the check).
    last_update_check: i64,
    /// Latest release version last seen (persisted cache).
    latest_seen: String,
    /// Open dialog, if any. When `Some`, it captures all input.
    pub dialog: Option<Dialog>,
}

impl WelcomeState {
    /// Build the initial state: restore saved preferences and load the current
    /// project's sessions.
    pub fn new() -> Self {
        // The directory Bruce was launched in defines the project.
        let project_path =
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        // Restore the user's last theme + panel choices.
        let config = Config::load();

        // Show a badge immediately from the cached version, then refresh in the
        // background if the last check is older than the TTL.
        let update_status = if !config.latest_seen.is_empty()
            && update::is_newer(&config.latest_seen, update::current())
        {
            UpdateStatus::Available(config.latest_seen.clone())
        } else {
            UpdateStatus::Idle
        };
        let update_check = (now_epoch() - config.last_update_check >= UPDATE_CHECK_TTL)
            .then(update::Check::spawn);

        Self {
            // A load failure (e.g. unreadable config dir) yields an empty list
            // rather than blocking the welcome screen.
            sessions: session::load_for_project(&project_path).unwrap_or_default(),
            project_path,
            focus: Focus::Options,
            option_selected: 0,
            settings_selected: 0,
            doc_selected: 0,
            name_link: Cell::new(Rect::ZERO),
            theme: config.theme,
            git_enabled: config.git_enabled,
            metrics_enabled: config.metrics_enabled,
            sync_colors: config.sync_colors,
            show_footer: config.show_footer,
            show_title: config.show_title,
            border_style: config.border_style,
            side_width: config.side_width,
            update_status,
            update_check,
            last_update_check: config.last_update_check,
            latest_seen: config.latest_seen,
            dialog: None,
        }
    }

    /// Poll the in-flight update check; when it finishes, update the badge and
    /// persist the result (so the next launch uses the cache within the TTL).
    /// Cheap no-op when no check is running. Call once per frame.
    pub fn poll_update_check(&mut self) {
        let Some(check) = &self.update_check else {
            return;
        };
        let Some(result) = check.poll() else {
            return; // still in flight
        };
        self.update_check = None;
        self.last_update_check = now_epoch();
        if let Some(latest) = result {
            self.latest_seen = latest.clone();
            self.update_status = if update::is_newer(&latest, update::current()) {
                UpdateStatus::Available(latest)
            } else {
                UpdateStatus::UpToDate
            };
        } else if self.update_status == UpdateStatus::Checking {
            // The fetch failed (offline / no curl); don't claim up-to-date.
            self.update_status = UpdateStatus::Idle;
        }
        self.persist_config();
    }

    /// Start a fresh update check now, ignoring the TTL (the "Check for updates"
    /// action). Shows a "Checking…" state for immediate feedback.
    pub fn start_update_check(&mut self) {
        self.update_status = UpdateStatus::Checking;
        self.update_check = Some(update::Check::spawn());
    }

    /// Handle a mouse click at screen cell `(col, row)`. Opens the author's
    /// GitHub profile when the click lands on the tagline link. Returns whether
    /// the click was on the link (so the caller knows it was handled).
    pub fn click(&self, col: u16, row: u16) -> bool {
        let r = self.name_link.get();
        let hit = r.width > 0
            && col >= r.x
            && col < r.x + r.width
            && row >= r.y
            && row < r.y + r.height;
        if hit {
            update::open_in_browser(AUTHOR_URL);
        }
        hit
    }

    /// The newer version available, if any (drives the badge).
    pub fn available_version(&self) -> Option<&str> {
        match &self.update_status {
            UpdateStatus::Available(v) => Some(v),
            _ => None,
        }
    }

    /// Persist the current preferences (theme + panel visibility) to disk.
    /// Best-effort: a failed write just means the next run falls back to the
    /// previous saved state.
    pub fn persist_config(&self) {
        let _ = Config {
            theme: self.theme,
            git_enabled: self.git_enabled,
            metrics_enabled: self.metrics_enabled,
            last_update_check: self.last_update_check,
            latest_seen: self.latest_seen.clone(),
            sync_colors: self.sync_colors,
            show_footer: self.show_footer,
            show_title: self.show_title,
            border_style: self.border_style,
            side_width: self.side_width,
        }
        .save();
    }

    /// Reload this project's session list from disk. Called when returning from a
    /// workspace so a freshly created or just-used session shows up (with
    /// up-to-date metrics) in the Open-session picker.
    pub fn reload_sessions(&mut self) {
        self.sessions = session::load_for_project(&self.project_path).unwrap_or_default();
    }

    /// Cycle focus across the Options, Settings and Documentation panels.
    pub fn focus_next(&mut self) {
        self.focus = match self.focus {
            Focus::Options => Focus::Settings,
            Focus::Settings => Focus::Documentation,
            Focus::Documentation => Focus::Options,
        };
    }

    /// Move the selection up within the focused panel, wrapping around. In the
    /// Themes panel this cycles the active theme to the previous one.
    pub fn select_prev(&mut self) {
        match self.focus {
            Focus::Options => {
                let n = OPTION_LABELS.len();
                self.option_selected = (self.option_selected + n - 1) % n;
            }
            Focus::Settings => {
                let n = SETTINGS_LABELS.len();
                self.settings_selected = (self.settings_selected + n - 1) % n;
            }
            Focus::Documentation => {
                let n = DOC_LABELS.len();
                self.doc_selected = (self.doc_selected + n - 1) % n;
            }
        }
    }

    /// Move the selection down within the focused panel, wrapping around. In the
    /// Themes panel this cycles the active theme to the next one.
    pub fn select_next(&mut self) {
        match self.focus {
            Focus::Options => {
                let n = OPTION_LABELS.len();
                self.option_selected = (self.option_selected + 1) % n;
            }
            Focus::Settings => {
                let n = SETTINGS_LABELS.len();
                self.settings_selected = (self.settings_selected + 1) % n;
            }
            Focus::Documentation => {
                let n = DOC_LABELS.len();
                self.doc_selected = (self.doc_selected + 1) % n;
            }
        }
    }

    /// True when the "Open session" option is selected.
    pub fn on_open_session(&self) -> bool {
        self.focus == Focus::Options && self.option_selected == 0
    }

    /// Focus the Options block on the "New session" row (the `N` shortcut).
    pub fn focus_new_session(&mut self) {
        self.focus = Focus::Options;
        self.option_selected = 1;
    }

    /// True when the "New session" option is selected.
    pub fn on_new_session(&self) -> bool {
        self.focus == Focus::Options && self.option_selected == 1
    }

    /// True when the "Rename session" option is selected.
    pub fn on_rename(&self) -> bool {
        self.focus == Focus::Options && self.option_selected == 2
    }

    /// True when the "Duplicate session" option is selected.
    pub fn on_duplicate(&self) -> bool {
        self.focus == Focus::Options && self.option_selected == 3
    }

    /// True when the "Delete session" option is selected.
    pub fn on_delete(&self) -> bool {
        self.focus == Focus::Options && self.option_selected == 4
    }

    /// True when the "Check for updates" option is selected.
    pub fn on_check_updates(&self) -> bool {
        self.focus == Focus::Options && self.option_selected == 5
    }

    /// True when the "Update to latest" option is selected.
    pub fn on_update_latest(&self) -> bool {
        self.focus == Focus::Options && self.option_selected == 6
    }

    /// True when the Documentation "GitHub" row is selected.
    pub fn on_doc_github(&self) -> bool {
        self.focus == Focus::Documentation && self.doc_selected == 0
    }

    /// True when the Documentation "Keybindings" row is selected.
    pub fn on_doc_keys(&self) -> bool {
        self.focus == Focus::Documentation && self.doc_selected == 1
    }

    /// Open the project's GitHub repository in the browser.
    pub fn open_github(&self) {
        update::open_in_browser(PROJECT_URL);
    }

    /// Open the keybindings reference dialog, scrolled to the top.
    pub fn open_keybindings(&mut self) {
        self.dialog = Some(Dialog::Keybindings(0));
    }

    /// Scroll the keybindings dialog by `delta` lines, clamped to its contents.
    fn scroll_keybindings(&mut self, delta: i32) {
        if let Some(Dialog::Keybindings(off)) = &mut self.dialog {
            let max = KEYBINDINGS.len().saturating_sub(1) as i32;
            *off = (*off as i32 + delta).clamp(0, max) as u16;
        }
    }

    /// True when the Settings block has focus (gates Enter→toggle).
    pub fn on_settings(&self) -> bool {
        self.focus == Focus::Settings
    }

    /// The Settings rows as (label, current value text), in display order.
    /// The Theme row shows the active theme's name; booleans read as on/off;
    /// multi-value options show their current label.
    pub fn settings_rows(&self) -> [(&'static str, String); 6] {
        let on_off = |b: bool| if b { "on" } else { "off" }.to_string();
        [
            (SETTINGS_LABELS[0], self.theme.palette().name.to_string()),
            (SETTINGS_LABELS[1], on_off(self.sync_colors)),
            (SETTINGS_LABELS[2], self.border_style.label().to_string()),
            (SETTINGS_LABELS[3], self.side_width.label().to_string()),
            (SETTINGS_LABELS[4], on_off(self.show_title)),
            (SETTINGS_LABELS[5], on_off(self.show_footer)),
        ]
    }

    /// True when the Settings block's "Theme" row is selected (it opens the
    /// theme picker on Enter instead of toggling like the other rows).
    pub fn on_settings_theme(&self) -> bool {
        self.focus == Focus::Settings && self.settings_selected == 0
    }

    /// Advance the selected setting (toggle a boolean, cycle a multi-value) and
    /// persist immediately. The Theme row (index 0) is handled separately via the
    /// picker, so it's a no-op here.
    pub fn toggle_setting(&mut self) {
        match self.settings_selected {
            1 => self.sync_colors = !self.sync_colors,
            2 => self.border_style = self.border_style.next(),
            3 => self.side_width = self.side_width.next(),
            4 => self.show_title = !self.show_title,
            5 => self.show_footer = !self.show_footer,
            _ => {}
        }
        self.persist_config();
    }

    /// Open the theme picker, highlighting the active theme.
    pub fn open_theme_picker(&mut self) {
        let idx = Theme::ALL.iter().position(|&t| t == self.theme).unwrap_or(0);
        self.dialog = Some(Dialog::ThemePicker(idx));
    }

    /// Move the theme picker by `delta`, wrapping, and apply+persist the previewed
    /// theme live (like the old Themes block did).
    fn move_theme_picker(&mut self, delta: i32) {
        let n = Theme::ALL.len() as i32;
        let new_idx = if let Some(Dialog::ThemePicker(idx)) = &mut self.dialog {
            *idx = (*idx as i32 + delta).rem_euclid(n) as usize;
            Some(*idx)
        } else {
            None
        };
        if let Some(idx) = new_idx {
            self.theme = Theme::ALL[idx];
            self.persist_config();
        }
    }

    /// Open the "Update to latest" info dialog.
    pub fn open_update_info(&mut self) {
        self.dialog = Some(Dialog::UpdateInfo);
    }

    /// Open the searchable session picker for `action` (rename / duplicate /
    /// delete). Starts in browse mode with an empty query, so every session is
    /// shown until the user types.
    pub fn open_picker(&mut self, action: PickerAction) {
        self.dialog = Some(Dialog::Picker(SessionPicker {
            action,
            query: String::new(),
            selected: 0,
            mode: PickerMode::Browse,
        }));
    }

    /// Open the new-session form.
    pub fn open_new_session(&mut self) {
        self.dialog = Some(Dialog::NewSession(NewSessionDialog::new()));
    }

    /// Route a key press to the open dialog. No-op if none is open.
    pub fn dialog_key(&mut self, code: KeyCode) -> WelcomeEvent {
        match self.dialog {
            Some(Dialog::NewSession(_)) => self.new_session_key(code),
            Some(Dialog::Picker(_)) => self.picker_key(code),
            Some(Dialog::UpdateInfo) => {
                match code {
                    KeyCode::Char('o') | KeyCode::Char('O') => {
                        update::open_in_browser(&update::releases_url());
                        WelcomeEvent::None
                    }
                    KeyCode::Char('u') | KeyCode::Char('U') => {
                        // Auto-update only for methods where it's safe; otherwise
                        // the dialog already shows the command to run by hand.
                        match update::InstallMethod::detect().auto_update_argv() {
                            Some(argv) => {
                                self.dialog = None;
                                WelcomeEvent::RunUpdate(argv)
                            }
                            None => WelcomeEvent::None,
                        }
                    }
                    KeyCode::Esc | KeyCode::Enter => {
                        self.dialog = None;
                        WelcomeEvent::None
                    }
                    _ => WelcomeEvent::None,
                }
            }
            Some(Dialog::Keybindings(_)) => {
                match code {
                    KeyCode::Up => self.scroll_keybindings(-1),
                    KeyCode::Down => self.scroll_keybindings(1),
                    KeyCode::Esc | KeyCode::Enter => self.dialog = None,
                    _ => {}
                }
                WelcomeEvent::None
            }
            Some(Dialog::ThemePicker(_)) => {
                match code {
                    KeyCode::Up => self.move_theme_picker(-1),
                    KeyCode::Down => self.move_theme_picker(1),
                    KeyCode::Esc | KeyCode::Enter => self.dialog = None,
                    _ => {}
                }
                WelcomeEvent::None
            }
            None => WelcomeEvent::None,
        }
    }

    /// Session-picker key handling for all four actions.
    ///
    /// Browse: characters/backspace edit the query (resetting the cursor),
    /// Up/Down move within the filtered results, Enter acts on the selection
    /// (open → resume it, rename → edit name, duplicate → fork now, delete →
    /// confirm), Esc closes. Returns [`WelcomeEvent::OpenSession`] when the user
    /// picks a session to open; otherwise [`WelcomeEvent::None`].
    fn picker_key(&mut self, code: KeyCode) -> WelcomeEvent {
        // Snapshot the picker state so terminal actions (which mutate the session
        // list and reload) don't fight the borrow on `self.dialog`.
        let Some(Dialog::Picker(p)) = &self.dialog else {
            return WelcomeEvent::None;
        };
        let action = p.action;
        let selected = p.selected;
        let mode = p.mode.clone();

        match mode {
            PickerMode::Browse => {
                let results = filter_sessions(&self.sessions, &p.query.clone());
                match code {
                    KeyCode::Up => self.picker_set_selected(selected.saturating_sub(1)),
                    KeyCode::Down => {
                        let max = results.len().saturating_sub(1);
                        self.picker_set_selected((selected + 1).min(max));
                    }
                    KeyCode::Char(c) => self.picker_edit_query(Some(c)),
                    KeyCode::Backspace => self.picker_edit_query(None),
                    KeyCode::Esc => self.dialog = None,
                    KeyCode::Enter => {
                        let Some(&target) = results.get(selected) else {
                            return WelcomeEvent::None;
                        };
                        match action {
                            PickerAction::Open => {
                                let session = self.sessions.get(target).cloned();
                                self.dialog = None;
                                if let Some(s) = session {
                                    return WelcomeEvent::OpenSession(s);
                                }
                            }
                            PickerAction::Rename => {
                                let name = self
                                    .sessions
                                    .get(target)
                                    .map(|s| s.name.clone())
                                    .unwrap_or_default();
                                self.picker_set_mode(PickerMode::EditName {
                                    target,
                                    buffer: name,
                                });
                            }
                            PickerAction::Duplicate => {
                                if let Some(s) = self.sessions.get(target) {
                                    let _ = s.duplicate();
                                }
                                self.dialog = None;
                                self.reload_sessions();
                            }
                            PickerAction::Delete => {
                                self.picker_set_mode(PickerMode::Confirm { target });
                            }
                        }
                    }
                    _ => {}
                }
            }
            PickerMode::EditName { target, mut buffer } => match code {
                KeyCode::Char(c) => {
                    buffer.push(c);
                    self.picker_set_mode(PickerMode::EditName { target, buffer });
                }
                KeyCode::Backspace => {
                    buffer.pop();
                    self.picker_set_mode(PickerMode::EditName { target, buffer });
                }
                KeyCode::Esc => self.picker_set_mode(PickerMode::Browse),
                KeyCode::Enter => {
                    let name = buffer.trim().to_string();
                    if !name.is_empty() {
                        if let Some(s) = self.sessions.get_mut(target) {
                            s.name = name;
                            // Persist the rename so it survives a restart.
                            let _ = s.save();
                        }
                    }
                    self.dialog = None;
                    self.reload_sessions();
                }
                _ => {}
            },
            PickerMode::Confirm { target } => match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    if let Some(s) = self.sessions.get(target) {
                        let _ = s.delete();
                    }
                    self.dialog = None;
                    self.reload_sessions();
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.picker_set_mode(PickerMode::Browse);
                }
                _ => {}
            },
        }
        WelcomeEvent::None
    }

    /// Set the picker's selected result (no-op if the picker isn't open).
    fn picker_set_selected(&mut self, selected: usize) {
        if let Some(Dialog::Picker(p)) = self.dialog.as_mut() {
            p.selected = selected;
        }
    }

    /// Set the picker's mode (no-op if the picker isn't open).
    fn picker_set_mode(&mut self, mode: PickerMode) {
        if let Some(Dialog::Picker(p)) = self.dialog.as_mut() {
            p.mode = mode;
        }
    }

    /// Edit the picker's query: push `Some(c)` or pop on `None`, resetting the
    /// result cursor to the top so the selection stays valid as matches change.
    fn picker_edit_query(&mut self, push: Option<char>) {
        if let Some(Dialog::Picker(p)) = self.dialog.as_mut() {
            match push {
                Some(c) => p.query.push(c),
                None => {
                    p.query.pop();
                }
            }
            p.selected = 0;
        }
    }

    /// New-session form key handling. Returns [`WelcomeEvent::CreateSession`]
    /// when the user confirms with a non-empty name.
    fn new_session_key(&mut self, code: KeyCode) -> WelcomeEvent {
        // Confirm: read the name out (ending the borrow) before clearing.
        if code == KeyCode::Enter {
            let name = if let Some(Dialog::NewSession(d)) = &self.dialog {
                let trimmed = d.name.trim().to_string();
                (!trimmed.is_empty()).then_some(trimmed)
            } else {
                None
            };
            if let Some(name) = name {
                self.dialog = None;
                return WelcomeEvent::CreateSession { name };
            }
            return WelcomeEvent::None;
        }

        if code == KeyCode::Esc {
            self.dialog = None;
            return WelcomeEvent::None;
        }

        if let Some(Dialog::NewSession(d)) = self.dialog.as_mut() {
            match code {
                KeyCode::Char(c) => d.name.push(c),
                KeyCode::Backspace => {
                    d.name.pop();
                }
                _ => {}
            }
        }
        WelcomeEvent::None
    }
}

/// Draw the full welcome screen into `frame`.
pub fn render(frame: &mut Frame, state: &WelcomeState) {
    let pal = state.theme.palette();
    let area = frame.area();

    // Paint the whole background first.
    frame.render_widget(Block::default().style(Style::default().bg(pal.bg)), area);

    // Vertical sections: top margin (badge), logo, tagline, a spacer, the three
    // blocks (Options, Settings, Themes) side by side, and the footer.
    let chunks = Layout::vertical([
        Constraint::Length(2),  // top margin (breathing room above the banner)
        Constraint::Length(10), // logo (9-line Delta Corps Priest 1 banner)
        Constraint::Length(1),  // tagline
        Constraint::Length(1),  // spacer between tagline and the blocks
        Constraint::Min(8),     // blocks row
        Constraint::Length(1),  // footer hints
    ])
    .split(area);

    // One row, three equal blocks: Options, Settings, Documentation.
    let cols = Layout::horizontal([
        Constraint::Percentage(34),
        Constraint::Percentage(33),
        Constraint::Percentage(33),
    ])
    .split(chunks[4]);

    render_badge(frame, chunks[0], state);
    render_logo(frame, chunks[1], state);
    render_tagline(frame, chunks[2], state);
    render_options(frame, cols[0], state);
    render_settings(frame, cols[1], state);
    render_documentation(frame, cols[2], state);
    render_footer(frame, chunks[5], state);

    // Dialogs are modal overlays. Dim everything behind them first so the screen
    // recedes and focus lands on the modal (a terminal can't truly blur, so this
    // mutes the background to the theme's dim color), then draw the modal crisp.
    if state.dialog.is_some() {
        dim_behind_dialog(frame, area, &pal);
    }
    match &state.dialog {
        Some(Dialog::NewSession(d)) => render_new_session_dialog(frame, area, &pal, d),
        Some(Dialog::Picker(p)) => render_picker_dialog(frame, area, &pal, state, p),
        Some(Dialog::UpdateInfo) => render_update_info_dialog(frame, area, &pal, state),
        Some(Dialog::Keybindings(off)) => render_keybindings_dialog(frame, area, &pal, *off),
        Some(Dialog::ThemePicker(idx)) => render_theme_picker_dialog(frame, area, &pal, *idx),
        None => {}
    }
}

/// Badge above the logo: shown only when a newer release is available.
fn render_badge(frame: &mut Frame, area: Rect, state: &WelcomeState) {
    let Some(latest) = state.available_version() else {
        return;
    };
    let pal = state.theme.palette();
    let text = format!(" ⬆ v{latest} available — Options ▸ Update to latest ");
    let badge = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default()
            .fg(pal.bg)
            .bg(pal.accent)
            .add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Center)
    .style(Style::default().bg(pal.bg));
    frame.render_widget(badge, area);
}

/// The Settings block: persisted look toggles (terminal color sync, footer/title
/// bars, border style, side width). Each row shows its current value.
fn render_settings(frame: &mut Frame, area: Rect, state: &WelcomeState) {
    let pal = state.theme.palette();
    let focused = state.focus == Focus::Settings;

    // Interleave blank spacers between rows so they breathe like the Themes block.
    let mut items: Vec<ListItem> = Vec::new();
    for (label, value) in state.settings_rows() {
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!(" {label}  "),
                Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(value, Style::default().fg(pal.accent).add_modifier(Modifier::BOLD)),
        ])));
        items.push(ListItem::new(Line::raw("")));
    }

    let block = panel_block(&pal, " Settings ", focused);
    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style(&pal));

    let mut list_state = ListState::default();
    // Rows sit at even indices (each is followed by a spacer).
    list_state.select(focused.then_some(state.settings_selected * 2));
    frame.render_stateful_widget(list, area, &mut list_state);
}

/// The Documentation block: a link to the project repo and the keybindings ref.
fn render_documentation(frame: &mut Frame, area: Rect, state: &WelcomeState) {
    let pal = state.theme.palette();
    let focused = state.focus == Focus::Documentation;

    let mut items: Vec<ListItem> = Vec::new();
    for label in DOC_LABELS {
        items.push(ListItem::new(Line::from(Span::styled(
            label,
            Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
        ))));
        items.push(ListItem::new(Line::raw("")));
    }

    let block = panel_block(&pal, " Documentation ", focused);
    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style(&pal));

    let mut list_state = ListState::default();
    list_state.select(focused.then_some(state.doc_selected * 2));
    frame.render_stateful_widget(list, area, &mut list_state);
}

/// Draw the keybindings reference: a scrollable, sectioned list of every key.
fn render_keybindings_dialog(frame: &mut Frame, screen: Rect, pal: &Palette, offset: u16) {
    let area = centered_rect(74, 82, screen);
    frame.render_widget(Clear, area);
    let block = dialog_block(pal, " Keybindings ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = keybinding_lines(pal);
    let height = inner.height as usize;
    // Clamp the scroll so the last page can't scroll past the end into blank.
    let off = (offset as usize).min(lines.len().saturating_sub(height));
    let visible: Vec<Line> = lines.into_iter().skip(off).take(height).collect();
    frame.render_widget(
        Paragraph::new(visible).style(Style::default().bg(pal.bg)),
        inner,
    );
}

/// Build one styled line per [`KEYBINDINGS`] entry: section headers in accent,
/// rows as a left-aligned key plus a dim description, blanks as empty lines.
fn keybinding_lines(pal: &Palette) -> Vec<Line<'static>> {
    KEYBINDINGS
        .iter()
        .map(|(key, desc)| match (key.is_empty(), desc.is_empty()) {
            (true, true) => Line::raw(""),
            // A section header: either ("", title) or (title, "").
            (true, false) => Line::from(Span::styled(
                format!("  {desc}"),
                Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
            )),
            (false, true) => Line::from(Span::styled(
                format!("  {key}"),
                Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
            )),
            (false, false) => Line::from(vec![
                Span::styled(
                    format!("  {key:<26}"),
                    Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
                ),
                Span::styled((*desc).to_string(), Style::default().fg(pal.dim)),
            ]),
        })
        .collect()
}

/// Draw the "Update to latest" info dialog: current/latest version and the
/// per-channel update commands, with a key to open the releases page.
fn render_update_info_dialog(frame: &mut Frame, screen: Rect, pal: &Palette, state: &WelcomeState) {
    let area = centered_rect(70, 70, screen);
    frame.render_widget(Clear, area);

    let block = dialog_block(pal, " Update to latest ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let status = match &state.update_status {
        UpdateStatus::Available(v) => {
            format!("  v{} installed · v{} available", update::current(), v)
        }
        UpdateStatus::UpToDate => format!("  v{} installed · up to date", update::current()),
        _ => format!("  v{} installed", update::current()),
    };

    let method = update::InstallMethod::detect();
    let auto = method.auto_update_argv().is_some();

    let mut lines = vec![
        Line::from(Span::styled(
            status,
            Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  Installed via: ", Style::default().fg(pal.dim)),
            Span::styled(
                method.label(),
                Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled("  Update command:", Style::default().fg(pal.dim))),
        Line::from(Span::styled(
            format!("  {}", method.command()),
            Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
    ];

    // Action hints depend on whether we can run the update ourselves.
    if auto {
        lines.push(Line::from(vec![
            Span::styled("  Press ", Style::default().fg(pal.dim)),
            Span::styled("U", Style::default().fg(pal.accent).add_modifier(Modifier::BOLD)),
            Span::styled(" to update now, ", Style::default().fg(pal.dim)),
            Span::styled("O", Style::default().fg(pal.accent).add_modifier(Modifier::BOLD)),
            Span::styled(" for releases, ", Style::default().fg(pal.dim)),
            Span::styled("Esc", Style::default().fg(pal.accent).add_modifier(Modifier::BOLD)),
            Span::styled(" to close.", Style::default().fg(pal.dim)),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "  Run the command above to update.",
            Style::default().fg(pal.dim),
        )));
        lines.push(Line::from(vec![
            Span::styled("  Press ", Style::default().fg(pal.dim)),
            Span::styled("O", Style::default().fg(pal.accent).add_modifier(Modifier::BOLD)),
            Span::styled(" for releases, ", Style::default().fg(pal.dim)),
            Span::styled("Esc", Style::default().fg(pal.accent).add_modifier(Modifier::BOLD)),
            Span::styled(" to close.", Style::default().fg(pal.dim)),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(pal.bg)),
        inner,
    );
}

/// Pixel-style spinner: a single lit block bouncing back and forth on a track.
const SPINNER: [&str; 8] = [
    "▰ ▱ ▱ ▱ ▱",
    "▱ ▰ ▱ ▱ ▱",
    "▱ ▱ ▰ ▱ ▱",
    "▱ ▱ ▱ ▰ ▱",
    "▱ ▱ ▱ ▱ ▰",
    "▱ ▱ ▱ ▰ ▱",
    "▱ ▱ ▰ ▱ ▱",
    "▱ ▰ ▱ ▱ ▱",
];

/// Full-screen loading transition shown between the welcome and workspace
/// screens while the chosen session's Claude process boots: a solid theme
/// background, the Bruce wordmark centered, a status line and a pixel spinner.
/// `tick` advances the spinner frame; the caller derives it from elapsed time.
pub fn render_loading(frame: &mut Frame, theme: Theme, message: &str, tick: usize) {
    let pal = theme.palette();
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(pal.bg)), area);

    // Pad the banner to a uniform width so centered alignment shifts the whole
    // block instead of staggering rows (same approach as render_logo).
    let logo_lines: Vec<&str> = LOGO.trim_matches('\n').lines().collect();
    let logo_h = logo_lines.len() as u16;
    let width = logo_lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let banner = logo_lines
        .iter()
        .map(|l| format!("{:<width$}", l, width = width))
        .collect::<Vec<_>>()
        .join("\n");

    // Vertically center the stack: logo, blank, message, blank, spinner.
    let stack_h = logo_h + 4;
    let top = area.y + area.height.saturating_sub(stack_h) / 2;

    frame.render_widget(
        Paragraph::new(banner)
            .alignment(Alignment::Center)
            .style(Style::default().fg(pal.accent).bg(pal.bg)),
        Rect { x: area.x, y: top, width: area.width, height: logo_h },
    );
    frame.render_widget(
        Paragraph::new(message.to_string())
            .alignment(Alignment::Center)
            .style(Style::default().fg(pal.fg).add_modifier(Modifier::BOLD)),
        Rect { x: area.x, y: top + logo_h + 1, width: area.width, height: 1 },
    );
    frame.render_widget(
        Paragraph::new(SPINNER[tick % SPINNER.len()])
            .alignment(Alignment::Center)
            .style(Style::default().fg(pal.accent).add_modifier(Modifier::BOLD)),
        Rect { x: area.x, y: top + logo_h + 3, width: area.width, height: 1 },
    );
}

fn render_logo(frame: &mut Frame, area: Rect, state: &WelcomeState) {
    let pal = state.theme.palette();

    // Pad every line to the widest one so centred alignment shifts the whole
    // block uniformly instead of staggering each row by its own length.
    let lines: Vec<&str> = LOGO.trim_matches('\n').lines().collect();
    let width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let banner = lines
        .iter()
        .map(|l| format!("{:<width$}", l, width = width))
        .collect::<Vec<_>>()
        .join("\n");

    let logo = Paragraph::new(banner)
        .alignment(Alignment::Center)
        .style(Style::default().fg(pal.accent).bg(pal.bg));
    frame.render_widget(logo, area);

    // Project version sitting to the RIGHT of the wordmark, at mid-height, in
    // the same accent color — a small gap past the banner's right edge.
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    let banner_w = width as u16;
    let left_pad = area.width.saturating_sub(banner_w) / 2;
    let banner_right = area.x + left_pad + banner_w;
    let gap = 2; // breathing room from the glyphs
    let start = banner_right.saturating_add(gap);
    let area_right = area.x + area.width;
    if start < area_right {
        let mid_line = area.y + (lines.len() as u16) / 2;
        let tag_rect = Rect {
            x: start,
            y: mid_line,
            width: area_right - start,
            height: 1,
        };
        let tag = Paragraph::new(version)
            .alignment(Alignment::Left)
            .style(Style::default().fg(pal.accent).bg(pal.bg));
        frame.render_widget(tag, tag_rect);
    }
}

fn render_options(frame: &mut Frame, area: Rect, state: &WelcomeState) {
    let pal = state.theme.palette();
    let focused = state.focus == Focus::Options;

    // Rows are interleaved with blank spacers so they breathe with the same
    // rhythm as the Themes block.
    let mut items: Vec<ListItem> = Vec::new();
    for (i, label) in OPTION_LABELS.iter().enumerate() {
        let mut spans = vec![Span::styled(
            *label,
            Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
        )];
        // Inline update feedback on the two update rows (the old App block's job).
        if i == 5 {
            if let Some(s) = update_status_suffix(&state.update_status) {
                spans.push(Span::styled(format!("  {s}"), Style::default().fg(pal.dim)));
            }
        } else if i == 6 {
            spans.push(Span::styled(
                format!("  v{}", update::current()),
                Style::default().fg(pal.dim),
            ));
        }
        items.push(ListItem::new(Line::from(spans)));
        items.push(ListItem::new(Line::raw("")));
    }

    let block = panel_block(&pal, " Options ", focused);
    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style(&pal));

    let mut list_state = ListState::default();
    // Rows sit at even indices (each is followed by a spacer).
    list_state.select(focused.then_some(state.option_selected * 2));
    frame.render_stateful_widget(list, area, &mut list_state);
}

/// Short inline label for the update check's current state, or `None` when idle.
fn update_status_suffix(status: &UpdateStatus) -> Option<String> {
    match status {
        UpdateStatus::Checking => Some("checking…".to_string()),
        UpdateStatus::UpToDate => Some("up to date".to_string()),
        UpdateStatus::Available(v) => Some(format!("v{v} available")),
        UpdateStatus::Idle => None,
    }
}

/// Author of the project; the tagline links their name to this GitHub profile.
pub const AUTHOR_NAME: &str = "Daniel Sierra";
pub const AUTHOR_URL: &str = "https://github.com/soydanielsierradev";
/// The project's GitHub repository (Documentation → GitHub).
pub const PROJECT_URL: &str = "https://github.com/soydanielsierradev/bruce-tui";

/// Centered tagline under the wordmark: a credit line whose author name is an
/// accented, underlined link. Records the name's on-screen rectangle in
/// `state.name_link` so the event loop can open the profile on a mouse click.
fn render_tagline(frame: &mut Frame, area: Rect, state: &WelcomeState) {
    let pal = state.theme.palette();
    let prefix = "A terminal workspace for Claude Code · Developed by ";
    let line = Line::from(vec![
        Span::styled(prefix, Style::default().fg(pal.dim)),
        Span::styled(
            AUTHOR_NAME,
            Style::default()
                .fg(pal.accent)
                .add_modifier(Modifier::UNDERLINED),
        ),
    ]);

    // Work out where the name lands so clicks can be hit-tested. The line is
    // centered, so the run starts at the centered left edge plus the prefix.
    let text_w = (prefix.chars().count() + AUTHOR_NAME.chars().count()) as u16;
    let left = area.x + area.width.saturating_sub(text_w) / 2;
    let name_x = left + prefix.chars().count() as u16;
    state.name_link.set(Rect {
        x: name_x,
        y: area.y,
        width: AUTHOR_NAME.chars().count() as u16,
        height: 1,
    });

    frame.render_widget(
        Paragraph::new(line)
            .alignment(Alignment::Center)
            .style(Style::default().bg(pal.bg)),
        area,
    );
}

/// One formatted session row for the picker: a bullet, name, branch, relative
/// last-used time and token count.
fn session_row<'a>(state: &WelcomeState, s: &'a Session) -> Line<'a> {
    let pal = state.theme.palette();
    let branch = s.branch.as_deref().unwrap_or("—");
    Line::from(vec![
        Span::styled(" ● ", Style::default().fg(pal.accent)),
        Span::styled(
            format!("{:<18}", s.name),
            Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{:<20}", branch), Style::default().fg(pal.accent)),
        Span::styled(format!("{:<10}", fmt_relative(s.last_used)), Style::default().fg(pal.dim)),
        Span::styled(
            format!("{:>9} tok", fmt_tokens(s.tokens_used)),
            Style::default().fg(pal.dim),
        ),
    ])
}

/// Render the theme picker dialog: one row per theme with its name and a strip
/// of color swatches previewing its palette. `selected` is the highlighted
/// theme's index in [`Theme::ALL`]; moving it previews the theme live.
fn render_theme_picker_dialog(frame: &mut Frame, screen: Rect, pal: &Palette, selected: usize) {
    let area = centered_rect(46, 70, screen);
    frame.render_widget(Clear, area);
    let block = dialog_block(pal, " Theme ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut items: Vec<ListItem> = Vec::new();
    for theme in Theme::ALL {
        let mut spans = vec![Span::styled(
            format!("  {:<12}", theme.palette().name),
            Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
        )];
        // Color swatches: each palette color as a solid block, joined into one
        // continuous bar so the row reads as a single theme strip.
        for color in theme.swatches() {
            spans.push(Span::styled("███", Style::default().fg(color)));
        }
        items.push(ListItem::new(Line::from(spans)));
        // Blank spacer so themes don't stack into one solid grid of color.
        items.push(ListItem::new(Line::raw("")));
    }

    let list = List::new(items).highlight_style(highlight_style(pal));
    let mut list_state = ListState::default();
    // Rows sit at even indices (each is followed by a spacer).
    list_state.select(Some(selected * 2));
    frame.render_stateful_widget(list, inner, &mut list_state);
}

fn render_footer(frame: &mut Frame, area: Rect, state: &WelcomeState) {
    let pal = state.theme.palette();

    // Hints depend on whether (and which) modal dialog is open.
    let hints = match &state.dialog {
        Some(Dialog::Picker(p)) => match &p.mode {
            PickerMode::EditName { .. } => Line::from(vec![
                Span::styled("  type", Style::default().fg(pal.accent)),
                Span::styled(" new name   ", Style::default().fg(pal.dim)),
                Span::styled("Enter", Style::default().fg(pal.accent)),
                Span::styled(" save   ", Style::default().fg(pal.dim)),
                Span::styled("Esc", Style::default().fg(pal.accent)),
                Span::styled(" back", Style::default().fg(pal.dim)),
            ]),
            PickerMode::Confirm { .. } => Line::from(vec![
                Span::styled("  Y", Style::default().fg(pal.accent)),
                Span::styled(" delete   ", Style::default().fg(pal.dim)),
                Span::styled("N", Style::default().fg(pal.accent)),
                Span::styled("/", Style::default().fg(pal.dim)),
                Span::styled("Esc", Style::default().fg(pal.accent)),
                Span::styled(" cancel", Style::default().fg(pal.dim)),
            ]),
            PickerMode::Browse => Line::from(vec![
                Span::styled("  type", Style::default().fg(pal.accent)),
                Span::styled(" to search   ", Style::default().fg(pal.dim)),
                Span::styled("↑↓", Style::default().fg(pal.accent)),
                Span::styled(" pick   ", Style::default().fg(pal.dim)),
                Span::styled("Enter", Style::default().fg(pal.accent)),
                Span::styled(format!(" {}   ", p.action.verb()), Style::default().fg(pal.dim)),
                Span::styled("Esc", Style::default().fg(pal.accent)),
                Span::styled(" close", Style::default().fg(pal.dim)),
            ]),
        },
        Some(Dialog::NewSession(_)) => Line::from(vec![
            Span::styled("  type", Style::default().fg(pal.accent)),
            Span::styled(" a name   ", Style::default().fg(pal.dim)),
            Span::styled("Enter", Style::default().fg(pal.accent)),
            Span::styled(" create   ", Style::default().fg(pal.dim)),
            Span::styled("Esc", Style::default().fg(pal.accent)),
            Span::styled(" cancel", Style::default().fg(pal.dim)),
        ]),
        Some(Dialog::UpdateInfo) => Line::from(vec![
            Span::styled("  U", Style::default().fg(pal.accent)),
            Span::styled(" update now   ", Style::default().fg(pal.dim)),
            Span::styled("O", Style::default().fg(pal.accent)),
            Span::styled(" releases   ", Style::default().fg(pal.dim)),
            Span::styled("Esc", Style::default().fg(pal.accent)),
            Span::styled(" close", Style::default().fg(pal.dim)),
        ]),
        Some(Dialog::Keybindings(_)) => Line::from(vec![
            Span::styled("  ↑↓", Style::default().fg(pal.accent)),
            Span::styled(" scroll   ", Style::default().fg(pal.dim)),
            Span::styled("Esc", Style::default().fg(pal.accent)),
            Span::styled(" close", Style::default().fg(pal.dim)),
        ]),
        Some(Dialog::ThemePicker(_)) => Line::from(vec![
            Span::styled("  ↑↓", Style::default().fg(pal.accent)),
            Span::styled(" preview   ", Style::default().fg(pal.dim)),
            Span::styled("Enter/Esc", Style::default().fg(pal.accent)),
            Span::styled(" done", Style::default().fg(pal.dim)),
        ]),
        None => Line::from(vec![
            Span::styled("  ↑↓", Style::default().fg(pal.accent)),
            Span::styled(" select   ", Style::default().fg(pal.dim)),
            Span::styled("Tab", Style::default().fg(pal.accent)),
            Span::styled(" switch panel   ", Style::default().fg(pal.dim)),
            Span::styled("Enter", Style::default().fg(pal.accent)),
            Span::styled(" open   ", Style::default().fg(pal.dim)),
            Span::styled("Q", Style::default().fg(pal.accent)),
            Span::styled(" quit", Style::default().fg(pal.dim)),
        ]),
    };

    frame.render_widget(
        Paragraph::new(hints).style(Style::default().bg(pal.bg)),
        area,
    );
}

/// Draw the modal session picker (rename / duplicate / delete) over `screen`.
///
/// Browse mode shows a search box above the filtered session list. The
/// follow-up modes replace the body with a name editor (rename) or a
/// confirmation prompt (delete).
fn render_picker_dialog(
    frame: &mut Frame,
    screen: Rect,
    pal: &Palette,
    state: &WelcomeState,
    picker: &SessionPicker,
) {
    let area = centered_rect(60, 60, screen);
    frame.render_widget(Clear, area);

    let block = dialog_block(pal, picker.action.title());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match &picker.mode {
        PickerMode::Browse => render_picker_browse(frame, inner, pal, state, picker),
        PickerMode::EditName { buffer, .. } => render_picker_edit_name(frame, inner, pal, buffer),
        PickerMode::Confirm { target } => {
            render_picker_confirm(frame, inner, pal, state, *target)
        }
    }
}

/// Browse step: a search box on top, the filtered session list below.
fn render_picker_browse(
    frame: &mut Frame,
    inner: Rect,
    pal: &Palette,
    state: &WelcomeState,
    picker: &SessionPicker,
) {
    let rows = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(inner);

    // Search box: a labelled input with a block cursor.
    let search = Line::from(vec![
        Span::styled(" Search ", Style::default().fg(pal.dim)),
        Span::styled(
            format!("{}▏", picker.query),
            Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(search).style(Style::default().bg(pal.bg)),
        rows[0],
    );

    // Filtered results, or a hint when nothing matches.
    let matches = filter_sessions(&state.sessions, &picker.query);
    if matches.is_empty() {
        let empty = if state.sessions.is_empty() {
            "  No sessions yet."
        } else {
            "  No matches."
        };
        frame.render_widget(
            Paragraph::new(Line::styled(empty, Style::default().fg(pal.dim)))
                .style(Style::default().bg(pal.bg)),
            rows[1],
        );
        return;
    }

    let items: Vec<ListItem> = matches
        .iter()
        .filter_map(|&i| state.sessions.get(i))
        .map(|s| ListItem::new(session_row(state, s)))
        .collect();

    let list = List::new(items).highlight_style(highlight_style(pal));
    let mut list_state = ListState::default();
    list_state.select(Some(picker.selected.min(matches.len().saturating_sub(1))));
    frame.render_stateful_widget(list, rows[1], &mut list_state);
}

/// Rename follow-up: an inline editor for the chosen session's new name.
fn render_picker_edit_name(frame: &mut Frame, inner: Rect, pal: &Palette, buffer: &str) {
    let lines = vec![
        Line::from(Span::styled(
            "  New name",
            Style::default().fg(pal.dim),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            format!("  {buffer}▏"),
            Style::default()
                .fg(pal.bg)
                .bg(pal.accent)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(pal.bg)),
        inner,
    );
}

/// Delete follow-up: confirm removal of the chosen session.
fn render_picker_confirm(
    frame: &mut Frame,
    inner: Rect,
    pal: &Palette,
    state: &WelcomeState,
    target: usize,
) {
    let name = state
        .sessions
        .get(target)
        .map(|s| s.name.clone())
        .unwrap_or_default();

    let lines = vec![
        Line::from(Span::styled(
            "  Delete this session?",
            Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            format!("  {name}"),
            Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  The Claude conversation is kept on disk.",
            Style::default().fg(pal.dim),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(pal.bg)),
        inner,
    );
}

/// Draw the modal new-session form centred over `screen`.
fn render_new_session_dialog(frame: &mut Frame, screen: Rect, pal: &Palette, dialog: &NewSessionDialog) {
    let area = centered_rect(50, 25, screen);
    frame.render_widget(Clear, area);

    let block = dialog_block(pal, " New session ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::from(Span::styled(
            "  Session name",
            Style::default().fg(pal.dim),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            format!("  {}▏", dialog.name),
            Style::default()
                .fg(pal.bg)
                .bg(pal.accent)
                .add_modifier(Modifier::BOLD),
        )),
    ];

    let form = Paragraph::new(lines).style(Style::default().bg(pal.bg));
    frame.render_widget(form, inner);
}

/// A bordered panel whose border colour signals focus.
fn panel_block(pal: &Palette, title: &str, focused: bool) -> Block<'static> {
    // The active block reads by color alone: accent border + title when focused,
    // dim when not.
    let color = if focused { pal.accent } else { pal.dim };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))
        // A blank top row so the first item isn't glued to the border.
        .padding(Padding::top(1))
        .style(Style::default().bg(pal.bg))
}

/// Mute every cell on screen to the theme's dim color, used as a backdrop behind
/// modal dialogs. The modal is drawn crisp on top afterwards, so only the
/// background recedes — the terminal equivalent of dimming/blurring behind a
/// dialog. Faded styling is also stripped so nothing in the background stays bold.
fn dim_behind_dialog(frame: &mut Frame, area: Rect, pal: &Palette) {
    let buf = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(Style::default().fg(pal.dim).bg(pal.bg));
            }
        }
    }
}

/// A bordered block for a modal dialog (always accent-bordered).
fn dialog_block<'a>(pal: &Palette, title: &'a str) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(pal.accent))
        .title(Span::styled(
            title,
            Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(pal.bg))
}

/// Shared highlight style for the selected row of a focused list.
fn highlight_style(pal: &Palette) -> Style {
    Style::default()
        .bg(pal.accent)
        .fg(pal.bg)
        .add_modifier(Modifier::BOLD)
}

/// Compute a rectangle centred inside `area`, sized as a percentage of it.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([Constraint::Percentage(percent_y)])
        .flex(Flex::Center)
        .split(area);
    Layout::horizontal([Constraint::Percentage(percent_x)])
        .flex(Flex::Center)
        .split(vertical[0])[0]
}

/// Current time as Unix epoch seconds (0 on a pre-epoch clock).
fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Indices into `sessions` whose name or branch contains `query`
/// (case-insensitive). An empty query matches every session.
fn filter_sessions(sessions: &[Session], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            q.is_empty()
                || s.name.to_lowercase().contains(&q)
                || s
                    .branch
                    .as_deref()
                    .is_some_and(|b| b.to_lowercase().contains(&q))
        })
        .map(|(i, _)| i)
        .collect()
}

/// Format a Unix-epoch timestamp as a short relative time (e.g. `2h ago`,
/// `3d ago`). A zero/invalid timestamp renders as a dash; future or just-now
/// times read as `just now`.
fn fmt_relative(epoch: i64) -> String {
    if epoch <= 0 {
        return "—".to_string();
    }
    let diff = now_epoch() - epoch;
    match diff {
        d if d < 60 => "just now".to_string(),
        d if d < 3_600 => format!("{}m ago", d / 60),
        d if d < 86_400 => format!("{}h ago", d / 3_600),
        d if d < 7 * 86_400 => format!("{}d ago", d / 86_400),
        d if d < 30 * 86_400 => format!("{}w ago", d / (7 * 86_400)),
        d => format!("{}mo ago", d / (30 * 86_400)),
    }
}

/// Format a token count with thousands separators (e.g. `47_832` -> `47,832`).
fn fmt_tokens(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}
