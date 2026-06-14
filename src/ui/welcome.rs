//! Welcome screen: ASCII logo, an Options block, the saved-session list,
//! a theme selector and two modal dialogs (new session / rename session).
//!
//! This module owns both the screen *state* ([`WelcomeState`]) and its
//! *rendering* ([`render`]). For step 1 the session list is hardcoded; it will
//! be replaced by real persisted sessions once the `session` module lands.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph},
};

use crate::config::{Config, claude_skills_dir};
use crate::pty::PtySession;
use crate::session::{self, Session};
use crate::skills::{
    SkillEntry, SkillLedger, SkillState,
    delete_skill, dir_skill_names, disable_skill, enable_skill, parse_frontmatter,
    relocate_into_claude, skill_state, skill_touched_since, snapshot_roots,
};
use crate::ui::theme::{BorderStyle, Palette, SideWidth, Theme};
use crate::ui::workspace::encode_key;
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
    /// The new Skills block (bottom-right of the 2×2 grid).
    Skills,
}

/// Documentation-block rows, in display order (index = `doc_selected`).
const DOC_LABELS: [&str; 2] = [" ↗ GitHub", " ≡ Keybindings"];

/// Skills-block rows, in display order (index = `skill_selected`).
const SKILL_LABELS: [&str; 2] = [" ≋ Manage skills", " + Install a skill"];

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
    /// Relaunch an existing session (resume mode) after a skill toggle in Manage.
    ReopenSession(Session),
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
    /// Install-a-skill dialog: text input + live log stream.
    SkillInstall(InstallDialog),
    /// Manage (list/toggle/delete) Bruce-tracked skills.
    SkillManage(ManageDialog),
    /// Scrollable preview of a SKILL.md file's raw contents. `max_scroll` is the
    /// largest valid scroll offset; the renderer computes it (it depends on the
    /// wrapped line count and dialog height) so the key handler can clamp.
    /// `description`/`folder_name` back the header and the enable/disable
    /// shortcuts available from the preview.
    SkillPreview {
        lines: Vec<String>,
        scroll: u16,
        entry_name: String,
        description: String,
        folder_name: String,
        max_scroll: Cell<u16>,
    },
}

// ─── Install dialog types ─────────────────────────────────────────────────────

/// Which phase the install dialog is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallPhase {
    /// Waiting for the user to type a command and press Enter.
    Idle,
    /// The install runner is live; output is streaming into `log`.
    Running,
    /// The runner finished. `ok` = exit code 0.
    Done { ok: bool },
}

/// State for the "Install a skill" dialog.
pub struct InstallDialog {
    /// The text the user is typing (the install command).
    pub command: String,
    /// Live PTY backing the running install. `None` when idle or done.
    pub pty: Option<PtySession>,
    /// Per-root folder snapshot taken before PTY spawn (for post-install diff).
    pub before: Vec<(PathBuf, HashSet<String>)>,
    /// Current lifecycle phase.
    pub phase: InstallPhase,
    /// Set when PTY spawn fails; shown in the log region in Done{ok:false} state.
    pub spawn_error: Option<String>,
    /// Post-install summary (success or failure message). Shown in the log
    /// region after the PTY exits and `pty` is set back to `None`.
    pub summary: Option<String>,
    /// Last (rows, cols) the PTY was resized to, so render only resizes on a
    /// real change. Resizing rebuilds the vt100 parser (clearing the screen),
    /// so resizing every frame would blank the live output.
    pub last_pty_size: Cell<(u16, u16)>,
    /// When the install command was spawned. Used to detect a skill the install
    /// (re)wrote even if its folder already existed, by comparing SKILL.md mtime.
    pub spawn_at: std::time::SystemTime,
}

impl InstallDialog {
    fn new() -> Self {
        Self {
            command: String::new(),
            pty: None,
            before: Vec::new(),
            phase: InstallPhase::Idle,
            spawn_error: None,
            summary: None,
            last_pty_size: Cell::new((24, 80)),
            spawn_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }
}

// ─── Manage dialog types ──────────────────────────────────────────────────────

/// Which sub-mode the manage dialog is in.
#[derive(Debug, Clone)]
pub enum ManageMode {
    /// Browsing the skill list (with optional search filter).
    Browse,
    /// Waiting for Y/N confirmation before deleting `target`.
    ConfirmDelete { target: usize },
}

/// State for the "Manage skills" dialog.
pub struct ManageDialog {
    /// The reconciled (skill, disk-state) pairs, loaded once when the dialog opens.
    pub entries: Vec<(SkillEntry, SkillState)>,
    /// Index into `entries` of the currently highlighted row (filtered view).
    pub selected: usize,
    /// Current browse/confirm mode.
    pub mode: ManageMode,
    /// True when any enable/disable/delete has been done — shows the restart banner.
    pub restart_needed: bool,
    /// Text filter that narrows the displayed list.
    pub filter: String,
    /// One-line error/status message shown at the bottom of the dialog.
    pub status_line: String,
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
    /// Selected row within the Skills block.
    pub skill_selected: usize,
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
    /// ManageDialog state stashed while SkillPreview is open so we can pop back.
    pub pending_manage: Option<ManageDialog>,
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
            skill_selected: 0,
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
            pending_manage: None,
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

    /// Cycle focus across the four panels: Options → Settings → Documentation → Skills → Options.
    pub fn focus_next(&mut self) {
        self.focus = match self.focus {
            Focus::Options => Focus::Settings,
            Focus::Settings => Focus::Documentation,
            Focus::Documentation => Focus::Skills,
            Focus::Skills => Focus::Options,
        };
    }

    /// Move the selection up within the focused panel, wrapping around.
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
            Focus::Skills => {
                let n = SKILL_LABELS.len();
                self.skill_selected = (self.skill_selected + n - 1) % n;
            }
        }
    }

    /// Move the selection down within the focused panel, wrapping around.
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
            Focus::Skills => {
                let n = SKILL_LABELS.len();
                self.skill_selected = (self.skill_selected + 1) % n;
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

    /// True when Skills block "Manage skills" row is selected.
    pub fn on_skills_manage(&self) -> bool {
        self.focus == Focus::Skills && self.skill_selected == 0
    }

    /// True when Skills block "Install a skill" row is selected.
    pub fn on_skills_install(&self) -> bool {
        self.focus == Focus::Skills && self.skill_selected == 1
    }

    /// Open the install dialog (Skills → "Install a skill").
    pub fn open_install_dialog(&mut self) {
        self.dialog = Some(Dialog::SkillInstall(InstallDialog::new()));
    }

    /// Open the manage dialog, loading and reconciling the ledger immediately.
    pub fn open_manage_dialog(&mut self) {
        let mut ledger = match SkillLedger::load() {
            Ok(l) => l,
            Err(e) => {
                // Silently fall back to an empty ledger if load fails.
                eprintln!("[bruce] failed to load skill ledger: {e}");
                return;
            }
        };
        // Reconcile drops any entry whose folder is gone (ADR-5: once per open).
        let _ = ledger.reconcile();
        let skills_root = claude_skills_dir().unwrap_or_default();
        let entries: Vec<(SkillEntry, SkillState)> = ledger
            .entries()
            .iter()
            .map(|e| {
                let state = skill_state(&skills_root.join(&e.folder_name));
                (e.clone(), state)
            })
            .collect();
        self.dialog = Some(Dialog::SkillManage(ManageDialog {
            entries,
            selected: 0,
            mode: ManageMode::Browse,
            restart_needed: false,
            filter: String::new(),
            status_line: String::new(),
        }));
    }

    /// Poll the running PTY install for exit (called from the 50ms timer branch).
    ///
    /// When the PTY exits: diffs the skills directory, auto-disables and
    /// registers any new skills, then transitions the dialog to Done.
    pub fn tick_install_runner(&mut self) {
        // Check PTY exit without mutating dialog yet.
        let pty_exited = if let Some(Dialog::SkillInstall(d)) = &self.dialog {
            d.pty.as_ref().and_then(|p| p.poll_exit()).is_some()
        } else {
            return;
        };

        // Guard against re-firing post-install if already in Done.
        let already_done = if let Some(Dialog::SkillInstall(d)) = &self.dialog {
            matches!(d.phase, InstallPhase::Done { .. })
        } else {
            return;
        };

        if already_done || !pty_exited {
            return;
        }

        // Read exit code and before snapshot.
        let (exit_code, before, spawn_at) = if let Some(Dialog::SkillInstall(d)) = &self.dialog {
            let code = d.pty.as_ref()
                .and_then(|p| p.poll_exit())
                .unwrap_or(-1);
            (code, d.before.clone(), d.spawn_at)
        } else {
            return;
        };

        // Drop PTY handle before post-install work.
        if let Some(Dialog::SkillInstall(d)) = &mut self.dialog {
            d.pty = None;
        }

        if exit_code == 0 {
            let claude_root = claude_skills_dir().unwrap_or_default();
            let mut ledger = match SkillLedger::load() {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[bruce] ledger load failed post-install: {e}");
                    if let Some(Dialog::SkillInstall(d)) = &mut self.dialog {
                        d.summary = Some(format!(
                            "Warning: could not load ledger: {e}\nInstall succeeded (skill not registered)."
                        ));
                        d.phase = InstallPhase::Done { ok: true };
                    }
                    return;
                }
            };
            let ledgered: HashSet<String> =
                ledger.entries().iter().map(|e| e.folder_name.clone()).collect();

            // Detect the skill(s) the command installed across every watched root
            // (npx skills installs into ~/.agents/skills, not ~/.claude/skills): a
            // brand-new folder, OR an existing folder whose SKILL.md was rewritten
            // during this run (so a re-install of an already-present skill still
            // counts). Skip anything Bruce already manages.
            let mut new_skills: Vec<(PathBuf, String)> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for (root, before_set) in &before {
                let after = dir_skill_names(root).unwrap_or_default();
                for folder in &after {
                    if ledgered.contains(folder) || seen.contains(folder) {
                        continue;
                    }
                    let dir = root.join(folder);
                    if !before_set.contains(folder) || skill_touched_since(&dir, spawn_at) {
                        seen.insert(folder.clone());
                        new_skills.push((dir, folder.clone()));
                    }
                }
            }

            let install_cmd = if let Some(Dialog::SkillInstall(d)) = &self.dialog {
                d.command.clone()
            } else {
                String::new()
            };

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            let mut post_lines: Vec<String> = Vec::new();
            let mut registered_names: Vec<String> = Vec::new();
            for (src_dir, folder) in &new_skills {
                // Bring it into ~/.claude/skills if the command dropped it
                // elsewhere (e.g. ~/.agents/skills) — the only dir Claude reads.
                let skill_dir = if src_dir.starts_with(&claude_root) {
                    src_dir.clone()
                } else {
                    match relocate_into_claude(src_dir, folder) {
                        Ok(dest) => {
                            post_lines.push(format!("Moved {folder} into ~/.claude/skills."));
                            dest
                        }
                        Err(e) => {
                            post_lines.push(format!("Warning: could not move {folder}: {e}"));
                            continue;
                        }
                    }
                };

                let skill_md = skill_dir.join("SKILL.md");
                let skill_md_disabled = skill_dir.join("SKILL.md.disabled");
                if !skill_md.exists() && !skill_md_disabled.exists() {
                    post_lines.push(format!("Warning: {folder} has no SKILL.md — skipped."));
                    continue;
                }

                // Auto-disable: rename SKILL.md → SKILL.md.disabled.
                if skill_md.exists() {
                    if let Err(e) = disable_skill(&skill_dir) {
                        post_lines.push(format!("Warning: could not disable {folder}: {e}"));
                    }
                }

                let (name, description) = parse_frontmatter(&skill_dir);
                let display = name.clone();
                let entry = SkillEntry {
                    name,
                    folder_name: folder.clone(),
                    description,
                    installed_at: now,
                    install_command: install_cmd.clone(),
                };
                if let Err(e) = ledger.add(entry) {
                    post_lines.push(format!("Warning: could not register {folder}: {e}"));
                } else {
                    registered_names.push(display);
                }
            }

            if !registered_names.is_empty() {
                // Persist the ledger so the Manage dialog (which reads from disk)
                // shows the freshly installed, disabled skills.
                if let Err(e) = ledger.save() {
                    post_lines.push(format!("Warning: could not save the skills ledger: {e}"));
                }
                let names = registered_names
                    .iter()
                    .map(|n| format!("\"{n}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                post_lines.push(format!(
                    "Bruce installed {names} — disabled by default. Enable it in Manage."
                ));
            } else if new_skills.is_empty() {
                // Exit 0 but nothing new in any watched root — be honest.
                post_lines.push(
                    "Command finished (exit 0) but no new skill appeared in ~/.claude/skills or ~/.agents/skills."
                        .to_string(),
                );
                post_lines.push(
                    "Nothing was registered — check the command actually installs a skill (npx skills needs -y)."
                        .to_string(),
                );
            } else {
                post_lines.push(
                    "Command finished, but the new skill(s) could not be registered — see the warnings above."
                        .to_string(),
                );
            }

            if let Some(Dialog::SkillInstall(d)) = &mut self.dialog {
                d.summary = Some(post_lines.join("\n"));
                d.phase = InstallPhase::Done { ok: true };
            }
        } else {
            if let Some(Dialog::SkillInstall(d)) = &mut self.dialog {
                d.summary = Some(format!("Install failed (exit {exit_code})."));
                d.phase = InstallPhase::Done { ok: false };
            }
        }
    }

    /// Route a key press to the open dialog. No-op if none is open.
    ///
    /// The signature accepts a full `&KeyEvent` so the install dialog can
    /// receive modifiers (e.g. Ctrl+C) for PTY stdin forwarding. All other
    /// dialogs extract `key.code` and remain behaviorally unchanged.
    pub fn dialog_key(&mut self, key: &KeyEvent) -> WelcomeEvent {
        let code = key.code;
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
            Some(Dialog::SkillInstall(_)) => self.install_dialog_key(key),
            Some(Dialog::SkillManage(_)) => self.manage_dialog_key(code),
            Some(Dialog::SkillPreview { .. }) => self.preview_key(code),
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

    // ─── Install dialog key handler ────────────────────────────────────────────

    fn install_dialog_key(&mut self, key: &KeyEvent) -> WelcomeEvent {
        let code = key.code;
        let phase = if let Some(Dialog::SkillInstall(d)) = &self.dialog {
            d.phase.clone()
        } else {
            return WelcomeEvent::None;
        };

        match phase {
            InstallPhase::Idle => match code {
                KeyCode::Esc => {
                    self.dialog = None;
                }
                KeyCode::Enter => {
                    // Snapshot command, validate non-empty, then spawn PTY.
                    let cmd = if let Some(Dialog::SkillInstall(d)) = &self.dialog {
                        let trimmed = d.command.trim().to_string();
                        if trimmed.is_empty() { return WelcomeEvent::None; }
                        trimmed
                    } else {
                        return WelcomeEvent::None;
                    };

                    // Snapshot watched roots BEFORE spawn (ADR-3), and record the
                    // spawn time so an install that rewrites an already-present
                    // skill is still detected (via SKILL.md mtime).
                    let before = snapshot_roots();
                    let spawn_at = std::time::SystemTime::now();

                    // Platform shell dispatch — handles .cmd shims and builtins.
                    #[cfg(windows)]
                    let (program, args_vec) = ("cmd", vec!["/C", cmd.as_str()]);
                    #[cfg(not(windows))]
                    let (program, args_vec) = ("sh", vec!["-c", cmd.as_str()]);

                    match PtySession::new_command(24, 80, program, &args_vec, None) {
                        Ok(pty) => {
                            if let Some(Dialog::SkillInstall(d)) = &mut self.dialog {
                                d.pty = Some(pty);
                                d.before = before;
                                d.spawn_at = spawn_at;
                                d.phase = InstallPhase::Running;
                            }
                        }
                        Err(e) => {
                            if let Some(Dialog::SkillInstall(d)) = &mut self.dialog {
                                d.spawn_error = Some(format!("Error: {e}"));
                                d.phase = InstallPhase::Done { ok: false };
                            }
                        }
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(Dialog::SkillInstall(d)) = &mut self.dialog {
                        d.command.push(c);
                    }
                }
                KeyCode::Backspace => {
                    if let Some(Dialog::SkillInstall(d)) = &mut self.dialog {
                        d.command.pop();
                    }
                }
                _ => {}
            },
            InstallPhase::Running => match code {
                KeyCode::Esc => {
                    // Kill the child and cancel the install.
                    if let Some(Dialog::SkillInstall(d)) = &mut self.dialog {
                        if let Some(pty) = &d.pty {
                            pty.kill();
                        }
                        d.pty = None;
                        d.phase = InstallPhase::Done { ok: false };
                        d.summary = Some("Canceled.".to_string());
                    }
                }
                _ => {
                    // Forward all other keys to the PTY stdin.
                    if let Some(Dialog::SkillInstall(d)) = &self.dialog {
                        if let Some(pty) = &d.pty {
                            let app_cursor = pty
                                .lock_parser()
                                .map(|p| p.screen().application_cursor())
                                .unwrap_or(false);
                            if let Some(bytes) = encode_key(key, app_cursor) {
                                pty.send(&bytes);
                            }
                        }
                    }
                }
            },
            InstallPhase::Done { .. } => match code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.dialog = None;
                }
                _ => {}
            },
        }
        WelcomeEvent::None
    }

    // ─── Manage dialog key handler ─────────────────────────────────────────────

    fn manage_dialog_key(&mut self, code: KeyCode) -> WelcomeEvent {
        let mode = if let Some(Dialog::SkillManage(d)) = &self.dialog {
            d.mode.clone()
        } else {
            return WelcomeEvent::None;
        };

        match mode {
            ManageMode::Browse => {
                let filtered = self.manage_filtered_indices();
                let n = filtered.len();
                match code {
                    KeyCode::Esc => {
                        self.dialog = None;
                        return WelcomeEvent::None;
                    }
                    KeyCode::Up => {
                        if let Some(Dialog::SkillManage(d)) = &mut self.dialog {
                            if d.selected > 0 { d.selected -= 1; }
                        }
                    }
                    KeyCode::Down => {
                        if let Some(Dialog::SkillManage(d)) = &mut self.dialog {
                            if n > 0 { d.selected = (d.selected + 1).min(n - 1); }
                        }
                    }
                    KeyCode::Char(c) if c != 'e' && c != 'E' && c != 'd' && c != 'D'
                        && c != 'x' && c != 'X' && c != 'r' && c != 'R' => {
                        // Filter input: accumulate characters.
                        if let Some(Dialog::SkillManage(d)) = &mut self.dialog {
                            d.filter.push(c);
                            d.selected = 0;
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(Dialog::SkillManage(d)) = &mut self.dialog {
                            d.filter.pop();
                            d.selected = 0;
                        }
                    }
                    KeyCode::Enter => {
                        // Enter on a row opens the preview dialog.
                        let entry_idx = filtered.get(
                            if let Some(Dialog::SkillManage(d)) = &self.dialog {
                                d.selected
                            } else { return WelcomeEvent::None; }
                        ).copied();
                        if let Some(idx) = entry_idx {
                            self.open_skill_preview(idx);
                        }
                    }
                    KeyCode::Char('e') | KeyCode::Char('E') => {
                        let sel_idx = if let Some(Dialog::SkillManage(d)) = &self.dialog {
                            filtered.get(d.selected).copied()
                        } else { None };
                        if let Some(idx) = sel_idx {
                            self.manage_enable(idx);
                        }
                    }
                    KeyCode::Char('d') | KeyCode::Char('D') => {
                        let sel_idx = if let Some(Dialog::SkillManage(d)) = &self.dialog {
                            filtered.get(d.selected).copied()
                        } else { None };
                        if let Some(idx) = sel_idx {
                            self.manage_disable(idx);
                        }
                    }
                    KeyCode::Char('x') | KeyCode::Char('X') => {
                        let sel_idx = if let Some(Dialog::SkillManage(d)) = &self.dialog {
                            filtered.get(d.selected).copied()
                        } else { None };
                        if let Some(idx) = sel_idx {
                            if let Some(Dialog::SkillManage(d)) = &mut self.dialog {
                                d.mode = ManageMode::ConfirmDelete { target: idx };
                            }
                        }
                    }
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        // Reopen the current session from the manage dialog.
                        if let Some(session) = self.sessions.first().cloned() {
                            self.dialog = None;
                            return WelcomeEvent::ReopenSession(session);
                        }
                    }
                    _ => {}
                }
            }
            ManageMode::ConfirmDelete { target } => match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.manage_delete(target);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    if let Some(Dialog::SkillManage(d)) = &mut self.dialog {
                        d.mode = ManageMode::Browse;
                    }
                }
                _ => {}
            },
        }
        WelcomeEvent::None
    }

    /// Returns the filtered indices into ManageDialog.entries based on the current filter.
    fn manage_filtered_indices(&self) -> Vec<usize> {
        if let Some(Dialog::SkillManage(d)) = &self.dialog {
            let q = d.filter.trim().to_lowercase();
            d.entries
                .iter()
                .enumerate()
                .filter(|(_, (e, _))| {
                    q.is_empty()
                        || e.name.to_lowercase().contains(&q)
                        || e.description.to_lowercase().contains(&q)
                })
                .map(|(i, _)| i)
                .collect()
        } else {
            Vec::new()
        }
    }

    fn manage_enable(&mut self, idx: usize) {
        let skill_dir = if let Some(Dialog::SkillManage(d)) = &self.dialog {
            d.entries.get(idx).map(|(e, _)| {
                claude_skills_dir().unwrap_or_default().join(&e.folder_name)
            })
        } else {
            None
        };
        if let Some(dir) = skill_dir {
            match enable_skill(&dir) {
                Ok(()) => {
                    if let Some(Dialog::SkillManage(d)) = &mut self.dialog {
                        if let Some((_, state)) = d.entries.get_mut(idx) {
                            *state = skill_state(&dir);
                        }
                        d.restart_needed = true;
                        d.status_line.clear();
                    }
                }
                Err(e) => {
                    if let Some(Dialog::SkillManage(d)) = &mut self.dialog {
                        d.status_line = format!("Error: {e}");
                    }
                }
            }
        }
    }

    fn manage_disable(&mut self, idx: usize) {
        let skill_dir = if let Some(Dialog::SkillManage(d)) = &self.dialog {
            d.entries.get(idx).map(|(e, _)| {
                claude_skills_dir().unwrap_or_default().join(&e.folder_name)
            })
        } else {
            None
        };
        if let Some(dir) = skill_dir {
            match disable_skill(&dir) {
                Ok(()) => {
                    if let Some(Dialog::SkillManage(d)) = &mut self.dialog {
                        if let Some((_, state)) = d.entries.get_mut(idx) {
                            *state = skill_state(&dir);
                        }
                        d.restart_needed = true;
                        d.status_line.clear();
                    }
                }
                Err(e) => {
                    if let Some(Dialog::SkillManage(d)) = &mut self.dialog {
                        d.status_line = format!("Error: {e}");
                    }
                }
            }
        }
    }

    fn manage_delete(&mut self, idx: usize) {
        // Take a snapshot of the entry to delete (avoids borrowing self.dialog).
        let entry_clone = if let Some(Dialog::SkillManage(d)) = &self.dialog {
            d.entries.get(idx).map(|(e, _)| e.clone())
        } else {
            None
        };
        if let Some(entry) = entry_clone {
            let mut ledger = match SkillLedger::load() {
                Ok(l) => l,
                Err(e) => {
                    if let Some(Dialog::SkillManage(d)) = &mut self.dialog {
                        d.status_line = format!("Error loading ledger: {e}");
                        d.mode = ManageMode::Browse;
                    }
                    return;
                }
            };
            match delete_skill(&entry, &mut ledger) {
                Ok(()) => {
                    if let Some(Dialog::SkillManage(d)) = &mut self.dialog {
                        d.entries.remove(idx);
                        d.selected = d.selected.min(d.entries.len().saturating_sub(1));
                        d.mode = ManageMode::Browse;
                        d.restart_needed = true;
                        d.status_line.clear();
                    }
                }
                Err(e) => {
                    if let Some(Dialog::SkillManage(d)) = &mut self.dialog {
                        d.status_line = format!("Error: {e}");
                        d.mode = ManageMode::Browse;
                    }
                }
            }
        }
    }

    // ─── Skill preview key handler ─────────────────────────────────────────────

    fn open_skill_preview(&mut self, idx: usize) {
        let (entry, state_val) = if let Some(Dialog::SkillManage(d)) = &self.dialog {
            match d.entries.get(idx) {
                Some(pair) => pair.clone(),
                None => return,
            }
        } else {
            return;
        };

        let skill_dir = claude_skills_dir().unwrap_or_default().join(&entry.folder_name);
        let path = if state_val == SkillState::Disabled {
            skill_dir.join("SKILL.md.disabled")
        } else {
            skill_dir.join("SKILL.md")
        };

        let lines: Vec<String> = match std::fs::read_to_string(&path) {
            Ok(content) => content.lines().map(|l| l.to_string()).collect(),
            Err(e) => vec![format!("Error reading skill file: {e}")],
        };

        // Stash the current ManageDialog before replacing dialog with preview.
        if let Some(Dialog::SkillManage(d)) = self.dialog.take() {
            self.pending_manage = Some(d);
        }
        self.dialog = Some(Dialog::SkillPreview {
            lines,
            scroll: 0,
            entry_name: entry.name.clone(),
            description: entry.description.clone(),
            folder_name: entry.folder_name.clone(),
            max_scroll: Cell::new(0),
        });
    }

    fn preview_key(&mut self, code: KeyCode) -> WelcomeEvent {
        // How far down the content can scroll, set by the last render. Clamping
        // to it stops the offset inflating past the end (which would make the
        // following Up/PageUp presses appear to do nothing).
        let max = if let Some(Dialog::SkillPreview { max_scroll, .. }) = &self.dialog {
            max_scroll.get()
        } else {
            0
        };
        match code {
            KeyCode::Up => {
                if let Some(Dialog::SkillPreview { scroll, .. }) = &mut self.dialog {
                    *scroll = scroll.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(Dialog::SkillPreview { scroll, .. }) = &mut self.dialog {
                    *scroll = scroll.saturating_add(1).min(max);
                }
            }
            KeyCode::PageUp => {
                if let Some(Dialog::SkillPreview { scroll, .. }) = &mut self.dialog {
                    *scroll = scroll.saturating_sub(10);
                }
            }
            KeyCode::PageDown => {
                if let Some(Dialog::SkillPreview { scroll, .. }) = &mut self.dialog {
                    *scroll = scroll.saturating_add(10).min(max);
                }
            }
            KeyCode::Char('e') | KeyCode::Char('E') => self.preview_set_enabled(true),
            KeyCode::Char('d') | KeyCode::Char('D') => self.preview_set_enabled(false),
            KeyCode::Esc | KeyCode::Enter => {
                // Pop back to ManageDialog.
                self.dialog = None;
                if let Some(manage) = self.pending_manage.take() {
                    self.dialog = Some(Dialog::SkillManage(manage));
                }
            }
            _ => {}
        }
        WelcomeEvent::None
    }

    /// Enable or disable the skill being previewed, and keep the stashed Manage
    /// list's marker in sync so it's correct when the user pops back.
    fn preview_set_enabled(&mut self, enable: bool) {
        let folder = if let Some(Dialog::SkillPreview { folder_name, .. }) = &self.dialog {
            folder_name.clone()
        } else {
            return;
        };
        let dir = claude_skills_dir().unwrap_or_default().join(&folder);
        let result = if enable { enable_skill(&dir) } else { disable_skill(&dir) };
        if result.is_err() {
            return;
        }
        // Reflect the new on-disk state in the stashed Manage list + flag restart.
        if let Some(manage) = self.pending_manage.as_mut() {
            for (entry, state) in manage.entries.iter_mut() {
                if entry.folder_name == folder {
                    *state = skill_state(&dir);
                }
            }
            manage.restart_needed = true;
        }
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

    // 2×2 grid: row 1 = Options | Settings, row 2 = Documentation | Skills.
    let rows = Layout::vertical([
        Constraint::Percentage(50),
        Constraint::Percentage(50),
    ])
    .split(chunks[4]);

    let top = Layout::horizontal([
        Constraint::Percentage(50),
        Constraint::Percentage(50),
    ])
    .split(rows[0]);

    let bottom = Layout::horizontal([
        Constraint::Percentage(50),
        Constraint::Percentage(50),
    ])
    .split(rows[1]);

    render_badge(frame, chunks[0], state);
    render_logo(frame, chunks[1], state);
    render_tagline(frame, chunks[2], state);
    render_options(frame, top[0], state);
    render_settings(frame, top[1], state);
    render_documentation(frame, bottom[0], state);
    render_skills(frame, bottom[1], state);
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
        Some(Dialog::SkillInstall(d)) => render_install_dialog(frame, area, &pal, d),
        Some(Dialog::SkillManage(d)) => render_manage_dialog(frame, area, &pal, state, d),
        Some(Dialog::SkillPreview { lines, scroll, entry_name, description, max_scroll, .. }) => {
            render_preview_dialog(frame, area, &pal, lines, *scroll, entry_name, description, max_scroll)
        }
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

/// The Skills block: "Manage skills" and "Install a skill" rows.
fn render_skills(frame: &mut Frame, area: Rect, state: &WelcomeState) {
    let pal = state.theme.palette();
    let focused = state.focus == Focus::Skills;

    let mut items: Vec<ListItem> = Vec::new();
    for label in SKILL_LABELS {
        items.push(ListItem::new(Line::from(Span::styled(
            label,
            Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
        ))));
        items.push(ListItem::new(Line::raw("")));
    }

    let block = panel_block(&pal, " Skills ", focused);
    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style(&pal));

    let mut list_state = ListState::default();
    list_state.select(focused.then_some(state.skill_selected * 2));
    frame.render_stateful_widget(list, area, &mut list_state);
}

/// Install-a-skill dialog: command input + live log + disclaimer.
fn render_install_dialog(frame: &mut Frame, screen: Rect, pal: &Palette, d: &InstallDialog) {
    let area = centered_rect(70, 80, screen);
    frame.render_widget(Clear, area);
    let block = dialog_block(pal, " Install a skill ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Layout: disclaimer (1), command input (2), PTY/log region (rest).
    // The separator row was removed to give the PTY screen an extra line.
    let sections = Layout::vertical([
        Constraint::Length(1), // disclaimer
        Constraint::Length(2), // command input
        Constraint::Min(1),    // PTY / status region
    ])
    .split(inner);

    // Disclaimer — always visible.
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "  Command runs in your shell — verify before running.",
            Style::default().fg(pal.accent),
        )))
        .style(Style::default().bg(pal.bg)),
        sections[0],
    );

    // Command input with cursor.
    let cursor = if d.phase == InstallPhase::Idle { "▏" } else { "" };
    let input_lines = vec![
        Line::from(Span::styled(
            "  Command",
            Style::default().fg(pal.dim),
        )),
        Line::from(Span::styled(
            format!("  {}{}", d.command, cursor),
            Style::default()
                .fg(pal.bg)
                .bg(pal.accent)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    frame.render_widget(
        Paragraph::new(input_lines).style(Style::default().bg(pal.bg)),
        sections[1],
    );

    // Placeholder hint in Idle + empty.
    if d.phase == InstallPhase::Idle && d.command.is_empty() {
        let hint = Paragraph::new(Line::from(Span::styled(
            "  e.g. npx skills add <name>",
            Style::default().fg(pal.dim),
        )))
        .style(Style::default().bg(pal.bg));
        let hint_area = Rect { y: sections[1].y + 1, height: 1, ..sections[1] };
        frame.render_widget(hint, hint_area);
    }

    // PTY / status region (sections[2]).
    let log_rect = sections[2];

    match &d.pty {
        Some(pty) => {
            // Resize the PTY to the dialog rect ONLY when it actually changes.
            // resize() rebuilds the vt100 parser (clearing the screen), so doing
            // it every frame would wipe the live menu/output before it's seen.
            let size = (log_rect.height, log_rect.width);
            if size.0 > 0 && size.1 > 0 && d.last_pty_size.get() != size {
                pty.resize(size.0, size.1);
                d.last_pty_size.set(size);
            }
            if let Some(parser) = pty.lock_parser() {
                let screen_data = parser.screen();
                frame.render_widget(
                    Paragraph::new(crate::pty::pty_screen_lines(screen_data, pal))
                        .style(Style::default().bg(pal.bg)),
                    log_rect,
                );
            }
        }
        None => {
            // Idle (no PTY yet), Done after spawn_error, or Done with summary.
            let text = d.summary.as_deref().or(d.spawn_error.as_deref());
            if let Some(msg) = text {
                let lines: Vec<Line> = msg.lines().map(|line| {
                    let color = if line.starts_with("Install succeeded")
                        || line.starts_with("Bruce installed")
                    {
                        pal.accent
                    } else if line.starts_with("Install failed")
                        || line.starts_with("Error")
                        || line.starts_with("Warning")
                    {
                        pal.removed
                    } else {
                        pal.fg
                    };
                    Line::from(Span::styled(
                        format!("  {line}"),
                        Style::default().fg(color),
                    ))
                }).collect();
                frame.render_widget(
                    Paragraph::new(lines).style(Style::default().bg(pal.bg)),
                    log_rect,
                );
            }
        }
    }
}

/// Manage-skills dialog: filtered list with ●/○/! markers.
/// Build a one-line "key label" command footer for the bottom of a modal.
fn modal_hint_line(pal: &Palette, hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
    for (key, label) in hints {
        spans.push(Span::styled((*key).to_string(), Style::default().fg(pal.accent)));
        spans.push(Span::styled(format!(" {label}   "), Style::default().fg(pal.dim)));
    }
    Line::from(spans)
}

fn render_manage_dialog(
    frame: &mut Frame,
    screen: Rect,
    pal: &Palette,
    _state: &WelcomeState,
    d: &ManageDialog,
) {
    let area = centered_rect(70, 80, screen);
    frame.render_widget(Clear, area);

    let title = match &d.mode {
        ManageMode::Browse => " Manage skills ",
        ManageMode::ConfirmDelete { .. } => " Confirm delete ",
    };
    let block = dialog_block(pal, title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match &d.mode {
        ManageMode::Browse => {
            // Layout: filter (2), restart banner (1 if needed), list (rest),
            // status (1), command footer (1).
            let banner_h = if d.restart_needed { 1u16 } else { 0 };
            let sections = Layout::vertical([
                Constraint::Length(2),        // filter
                Constraint::Length(banner_h), // restart banner (0 when not needed)
                Constraint::Min(1),           // skill list
                Constraint::Length(1),        // status line
                Constraint::Length(1),        // command footer
            ])
            .split(inner);

            // Filter input.
            let filter_lines = vec![
                Line::from(Span::styled("  Filter", Style::default().fg(pal.dim))),
                Line::from(Span::styled(
                    format!("  {}▏", d.filter),
                    Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
                )),
            ];
            frame.render_widget(
                Paragraph::new(filter_lines).style(Style::default().bg(pal.bg)),
                sections[0],
            );

            // Restart banner.
            if d.restart_needed && banner_h > 0 {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "  Restart the Claude session to load changes.",
                        Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
                    )))
                    .style(Style::default().bg(pal.bg)),
                    sections[1],
                );
            }

            // Filtered skill list.
            let q = d.filter.trim().to_lowercase();
            let filtered: Vec<usize> = d
                .entries
                .iter()
                .enumerate()
                .filter(|(_, (e, _))| {
                    q.is_empty()
                        || e.name.to_lowercase().contains(&q)
                        || e.description.to_lowercase().contains(&q)
                })
                .map(|(i, _)| i)
                .collect();

            if filtered.is_empty() {
                let msg = if d.entries.is_empty() {
                    "  No skills installed yet."
                } else {
                    "  No matching skills."
                };
                frame.render_widget(
                    Paragraph::new(Line::styled(msg, Style::default().fg(pal.dim)))
                        .style(Style::default().bg(pal.bg)),
                    sections[2],
                );
            } else {
                let items: Vec<ListItem> = filtered
                    .iter()
                    .filter_map(|&i| d.entries.get(i))
                    .map(|(entry, st)| {
                        let (marker, marker_color) = match st {
                            SkillState::Enabled => ("●", pal.accent),
                            SkillState::Disabled => ("○", pal.dim),
                            SkillState::Broken => ("!", pal.removed),
                        };
                        ListItem::new(Line::from(vec![
                            Span::styled(
                                format!(" {marker} "),
                                Style::default().fg(marker_color),
                            ),
                            Span::styled(
                                format!("{:<20}", entry.name),
                                Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                entry.description.chars().take(30).collect::<String>(),
                                Style::default().fg(pal.dim),
                            ),
                        ]))
                    })
                    .collect();

                let list = List::new(items).highlight_style(highlight_style(pal));
                let mut list_state = ListState::default();
                list_state.select(Some(d.selected.min(filtered.len().saturating_sub(1))));
                frame.render_stateful_widget(list, sections[2], &mut list_state);
            }

            // Status line.
            if !d.status_line.is_empty() {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        format!("  {}", d.status_line),
                        Style::default().fg(pal.removed),
                    )))
                    .style(Style::default().bg(pal.bg)),
                    sections[3],
                );
            }

            // Command footer, inside the modal.
            frame.render_widget(
                Paragraph::new(modal_hint_line(
                    pal,
                    &[
                        ("↑↓", "select"),
                        ("E", "enable"),
                        ("D", "disable"),
                        ("X", "delete"),
                        ("Enter", "preview"),
                        ("R", "reopen"),
                        ("Esc", "close"),
                    ],
                ))
                .style(Style::default().bg(pal.bg)),
                sections[4],
            );
        }
        ManageMode::ConfirmDelete { target } => {
            let name = d
                .entries
                .get(*target)
                .map(|(e, _)| e.name.as_str())
                .unwrap_or("?");
            let lines = vec![
                Line::from(Span::styled(
                    "  Delete this skill?",
                    Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
                )),
                Line::raw(""),
                Line::from(Span::styled(
                    format!("  {name}"),
                    Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
                )),
                Line::raw(""),
                Line::from(Span::styled(
                    "  This removes the skill folder and its ledger entry.",
                    Style::default().fg(pal.dim),
                )),
            ];
            let footer_h: u16 = if inner.height > 0 { 1 } else { 0 };
            let body = Rect { height: inner.height.saturating_sub(footer_h), ..inner };
            frame.render_widget(
                Paragraph::new(lines).style(Style::default().bg(pal.bg)),
                body,
            );
            if footer_h > 0 {
                let footer_rect = Rect {
                    y: inner.y + inner.height - footer_h,
                    height: footer_h,
                    ..inner
                };
                frame.render_widget(
                    Paragraph::new(modal_hint_line(pal, &[("Y", "delete"), ("N/Esc", "cancel")]))
                        .style(Style::default().bg(pal.bg)),
                    footer_rect,
                );
            }
        }
    }
}

/// Scrollable raw SKILL.md preview dialog.
/// Word-wrap one logical line to `width` columns, returning the visual lines.
/// A word longer than `width` is hard-split so nothing is ever cut off-screen.
fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    let push_word = |word: &str, out: &mut Vec<String>, cur: &mut String, cur_len: &mut usize| {
        let wlen = word.chars().count();
        if *cur_len > 0 && *cur_len + 1 + wlen > width {
            out.push(std::mem::take(cur));
            *cur_len = 0;
        }
        if wlen > width {
            // Word doesn't fit on a line by itself: hard-split it.
            for ch in word.chars() {
                if *cur_len == width {
                    out.push(std::mem::take(cur));
                    *cur_len = 0;
                }
                cur.push(ch);
                *cur_len += 1;
            }
        } else {
            if *cur_len > 0 {
                cur.push(' ');
                *cur_len += 1;
            }
            cur.push_str(word);
            *cur_len += wlen;
        }
    };
    for word in line.split(' ') {
        push_word(word, &mut out, &mut cur, &mut cur_len);
    }
    out.push(cur);
    out
}

fn render_preview_dialog(
    frame: &mut Frame,
    screen: Rect,
    pal: &Palette,
    lines: &[String],
    scroll: u16,
    entry_name: &str,
    description: &str,
    max_scroll: &Cell<u16>,
) {
    let area = centered_rect(74, 85, screen);
    frame.render_widget(Clear, area);
    let title = format!(" {} — SKILL.md ", entry_name);
    let block = dialog_block(pal, &title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let width = inner.width as usize;

    // Header: the skill name and its description, word-wrapped, with a rule.
    let mut header: Vec<Line> = Vec::new();
    header.push(Line::from(Span::styled(
        entry_name.to_string(),
        Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
    )));
    for dl in wrap_line(description, width) {
        header.push(Line::from(Span::styled(dl, Style::default().fg(pal.dim))));
    }
    header.push(Line::from(Span::styled(
        "─".repeat(width),
        Style::default().fg(pal.dim),
    )));
    let header_h = (header.len() as u16).min(inner.height);
    let header_rect = Rect { height: header_h, ..inner };
    frame.render_widget(
        Paragraph::new(header).style(Style::default().bg(pal.bg)),
        header_rect,
    );

    // Reserve the bottom row for the command footer; the body scrolls between
    // the header and it.
    let footer_h: u16 = if inner.height > header_h { 1 } else { 0 };
    let content_rect = Rect {
        y: inner.y + header_h,
        height: inner.height.saturating_sub(header_h).saturating_sub(footer_h),
        ..inner
    };
    let wrapped: Vec<String> = lines.iter().flat_map(|l| wrap_line(l, width)).collect();
    let view_h = content_rect.height as usize;
    // Publish the largest valid offset so the key handler can clamp to it.
    let max = wrapped.len().saturating_sub(view_h);
    max_scroll.set(max as u16);
    let off = (scroll as usize).min(max);
    let visible: Vec<Line> = wrapped[off..]
        .iter()
        .take(view_h)
        .map(|l| Line::from(Span::styled(l.clone(), Style::default().fg(pal.fg))))
        .collect();
    frame.render_widget(
        Paragraph::new(visible).style(Style::default().bg(pal.bg)),
        content_rect,
    );

    // Command footer, inside the modal.
    if footer_h > 0 {
        let footer_rect = Rect {
            y: inner.y + inner.height - footer_h,
            height: footer_h,
            ..inner
        };
        frame.render_widget(
            Paragraph::new(modal_hint_line(
                pal,
                &[
                    ("↑↓/PgUp/PgDn", "scroll"),
                    ("E", "enable"),
                    ("D", "disable"),
                    ("Esc", "back"),
                ],
            ))
            .style(Style::default().bg(pal.bg)),
            footer_rect,
        );
    }
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
        Some(Dialog::SkillInstall(d)) => match d.phase {
            InstallPhase::Idle => Line::from(vec![
                Span::styled("  type command   ", Style::default().fg(pal.dim)),
                Span::styled("Enter", Style::default().fg(pal.accent)),
                Span::styled(" run   ", Style::default().fg(pal.dim)),
                Span::styled("Esc", Style::default().fg(pal.accent)),
                Span::styled(" cancel", Style::default().fg(pal.dim)),
            ]),
            InstallPhase::Running => Line::from(vec![
                Span::styled("  ↑↓", Style::default().fg(pal.accent)),
                Span::styled(" scroll log   ", Style::default().fg(pal.dim)),
                Span::styled("waiting for install…", Style::default().fg(pal.dim)),
            ]),
            InstallPhase::Done { .. } => Line::from(vec![
                Span::styled("  ↑↓", Style::default().fg(pal.accent)),
                Span::styled(" scroll   ", Style::default().fg(pal.dim)),
                Span::styled("Esc", Style::default().fg(pal.accent)),
                Span::styled(" close", Style::default().fg(pal.dim)),
            ]),
        },
        // Manage and Preview show their command hints inside the modal itself.
        Some(Dialog::SkillManage(_)) => Line::raw(""),
        Some(Dialog::SkillPreview { .. }) => Line::raw(""),
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
