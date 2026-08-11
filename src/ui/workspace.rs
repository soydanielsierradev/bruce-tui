//! The workspace: Git (left), Claude Code (center), File Manager (right), and
//! a full-width Terminal below. Metrics have been replaced by the File Manager;
//! the Terminal pane is a second PTY wired in Slice 2.

use std::cell::Cell;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph},
};

use crate::config::Config;
use crate::panels::files::{FileManager, subsequence_match};
use crate::panels::git::{self, GitView};
use crate::panels::metrics;
use crate::pty::{self, PtySession, SpawnOptions, TERMINAL_SCROLLBACK};
use crate::session::Session;
use crate::ui::theme::{BorderStyle, Palette, SideWidth, Theme};

/// Modal dialog overlaid on top of the workspace. Only one dialog can be open
/// at a time. `None` means no dialog is active.
pub enum WorkspaceDialog {
    /// No dialog open — the default state.
    None,
    /// Ctrl+F fuzzy file-search overlay.
    FileSearch {
        /// The characters the user has typed so far.
        query: String,
        /// Snapshot of all project files filtered by `query` (subsequence match).
        /// Capped to [`FILE_SEARCH_MAX_RESULTS`] items for rendering performance.
        results: Vec<std::path::PathBuf>,
        /// Index into `results` of the highlighted row.
        selected: usize,
    },
}

impl Default for WorkspaceDialog {
    fn default() -> Self {
        WorkspaceDialog::None
    }
}

/// Maximum number of results shown in the Ctrl+F file-search overlay.
const FILE_SEARCH_MAX_RESULTS: usize = 200;

/// Which pane currently has keyboard focus. `Tab` cycles through them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Git,
    Claude,
    /// File manager pane (top-right, always present). Full implementation in Slice 3.
    FileManager,
    /// Terminal pane (full-width, below top row). Full implementation in Slice 2.
    Terminal,
}

/// State backing the workspace screen.
pub struct WorkspaceState {
    /// The session this workspace is running. Owns the id used to update token
    /// metrics on exit, and the name shown in the header.
    pub session: Session,
    /// Active color theme, carried over from the welcome screen.
    pub theme: Theme,
    /// Which pane has focus.
    pub focus: Panel,
    /// Whether the Git pane is shown. The Claude pane is always shown.
    pub git_enabled: bool,
    /// Show the bottom hint bar (Settings → Footer hints).
    pub show_footer: bool,
    /// Show the top title bar (Settings → Title bar).
    pub show_title: bool,
    /// Line style for the framed side panes (Settings → Border style).
    pub border_style: BorderStyle,
    /// Width of each side pane (Settings → Side width).
    pub side_width: SideWidth,
    /// Use Nerd Font file icons in the File Manager (Settings → File icons).
    pub nerd_icons: bool,
    /// Snapshot of the repository state shown in the Git pane.
    pub git: GitView,
    /// The embedded process + emulated terminal for the Claude pane.
    pub pty: Option<PtySession>,
    /// Why the PTY couldn't start, if it didn't.
    pub pty_error: Option<String>,
    /// `true` after Ctrl+b, waiting for the leader command key.
    pub leader_pending: bool,
    /// Last size the Claude PTY was synced to, to avoid resizing every frame.
    last_pty_size: Cell<(u16, u16)>,
    /// When the Git pane was last reloaded, to throttle the refresh poll.
    last_git_refresh: Instant,
    /// When this workspace opened, used to animate the "waking Claude" overlay.
    opened_at: Instant,
    /// Latches `true` once Claude has finished its initial paint, so the waking
    /// overlay is shown only while it boots and never again.
    claude_awake: Cell<bool>,

    // ── File Manager pane (Slice 3) ─────────────────────────────────────────
    /// Background-walked file list for the top-right pane.
    pub file_manager: FileManager,
    /// Error/status message shown at the bottom of the file manager pane, e.g.
    /// "Editor not found on PATH". Cleared the next time the user opens a file.
    pub file_manager_status: Option<String>,
    // ── Terminal pane (Slice 2) ──────────────────────────────────────────────
    /// Shell PTY; `None` until the pane receives focus for the first time.
    /// Spawned lazily to avoid running two ConPTY instances simultaneously on
    /// Windows before we have confirmed that works (two-PTY smoke-test gate).
    pub terminal: Option<PtySession>,
    /// Why the terminal PTY couldn't start, if it didn't.
    pub terminal_error: Option<String>,
    /// Whether the terminal pane is visible. Defaults to true; not persisted
    /// (always starts enabled; user can hide with `Ctrl+b t`).
    pub terminal_enabled: bool,
    /// Last size the terminal PTY was synced to.
    last_term_size: Cell<(u16, u16)>,
    /// Exit code of the shell if it has exited, `None` while running.
    /// Set by `tick()`; the renderer shows a "Press Enter to restart" hint.
    pub terminal_exit_code: Option<i32>,
    // ── Ctrl+F file-search overlay (Slice 4) ─────────────────────────────────
    /// The active modal dialog, if any. `WorkspaceDialog::None` when idle.
    pub dialog: WorkspaceDialog,
    // ── Project extras subpanels (under the File Manager) ────────────────────
    /// Names of every MCP server active in this project — drawn into the
    /// bottom-left subpanel under the file manager. Combination of
    /// `<project>/.mcp.json` (re-read every tick, cheap) and the cached result
    /// of `claude mcp list` in `claude_listed_mcps`.
    pub active_mcps: Vec<String>,
    /// Cached MCP names from the background `claude mcp list` run. Empty until
    /// the first shell-out completes; refreshed periodically by re-spawning
    /// the background job.
    claude_listed_mcps: Vec<String>,
    /// Handle to the in-flight `claude mcp list` background job, if any. Polled
    /// on every tick — we join it as soon as `is_finished()` is true so the
    /// subpanel populates on the very first tick after `claude` responds.
    mcp_list_job: Option<JoinHandle<Vec<String>>>,
    /// When the background `claude mcp list` job was last kicked off. Used to
    /// re-run periodically so newly added user-scoped MCPs eventually surface
    /// without reopening the workspace.
    last_mcp_list_kick: Instant,
    /// Names of the skills currently activated in this project — drawn into
    /// the bottom-right subpanel under the file manager. This is the same set
    /// the user toggles with `E` from Manage Skills, so what shows up here
    /// stays in lock-step with the user's own on/off decisions instead of
    /// listing every skill in the library.
    pub active_skills_names: Vec<String>,
    /// Snapshot of live session info (model, speed, context usage, turn count)
    /// pulled from Claude's transcript for the Session subpanel. `None` until
    /// the transcript file exists — the pane then shows a placeholder.
    pub session_info: Option<metrics::SessionInfo>,
    /// Latest payload Claude Code pushed to Bruce's status line shim: the real
    /// context window size, the subscription rate-limit windows, and the
    /// effort/fast-mode flags. `None` when the shim isn't installed or hasn't
    /// fired yet, in which case the Session pane falls back to numbers derived
    /// from the transcript.
    pub statusline: Option<crate::statusline::StatusLine>,
    /// When the project-extras lists were last refreshed.
    last_extras_refresh: Instant,
    // ─────────────────────────────────────────────────────────────────────────
}

/// How often the Git pane is re-read from disk. Claude (or the user) changes the
/// repo while a session runs, so the snapshot must refresh; once a second keeps
/// it current without re-running git status every frame.
const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// How often the project-extras lists (active MCPs from `.mcp.json`, available
/// skills) are re-read on tick. Cheap reads (one `read_dir`, one file open),
/// so a few seconds keeps the subpanels live without flickering.
const EXTRAS_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// How often `claude mcp list` is re-run in the background. The CLI is not
/// free (Node cold start + network health checks), so we only run it every
/// ~30 s — enough to pick up MCPs that were installed while Bruce was open,
/// but not so aggressively that it burns cycles.
const MCP_LIST_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Height of the terminal pane in rows when it is visible.
const TERMINAL_ROWS: u16 = 12;

/// Detect the user's preferred shell. On Windows: `%COMSPEC%` or `cmd.exe`.
/// On Unix/macOS: `$SHELL` or `/bin/sh`.
fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

/// Resolve the editor to use for opening files.
///
/// Priority: `$BRUCE_EDITOR` → `code` → `code-insiders` → `None`.
///
/// Returns `(command, is_vscode)`. `is_vscode` is `true` for `code`/`code-insiders`
/// so the caller can append `--goto file:line` instead of just the file path.
fn editor_command() -> Option<(String, bool)> {
    // Explicit override wins everything.
    if let Ok(editor) = std::env::var("BRUCE_EDITOR") {
        if !editor.is_empty() {
            return Some((editor, false));
        }
    }
    // VS Code family: check with on_path which handles `.cmd` on Windows.
    if pty::on_path("code") {
        return Some(("code".to_string(), true));
    }
    if pty::on_path("code-insiders") {
        return Some(("code-insiders".to_string(), true));
    }
    None
}

impl WorkspaceState {
    /// Open a workspace running `session`, inheriting the welcome theme and the
    /// chosen optional-panel configuration.
    ///
    /// `resume` decides how Claude is launched: `false` starts a fresh
    /// conversation pinned to the session id (`--session-id`); `true` continues
    /// an existing one (`--resume`). Either way the PTY runs in the session's
    /// `project_path`, which is the directory Claude keys its transcript on.
    pub fn new(
        session: Session,
        resume: bool,
        theme: Theme,
        git_enabled: bool,
        terminal_enabled: bool,
        show_footer: bool,
        show_title: bool,
        border_style: BorderStyle,
        side_width: SideWidth,
        nerd_icons: bool,
    ) -> Self {
        let cwd = session.project_path.clone();
        let git = git::load(&cwd);

        // Pin the conversation to the session id so it can be resumed later,
        // and route its status line through Bruce so the pane gets the real
        // context window and rate-limit windows. The status line flags are
        // scoped to this process — nothing is written to the user's project.
        let mut args = session.claude_args(resume);
        args.extend(crate::statusline::spawn_args());
        let opts = SpawnOptions { cwd: Some(cwd), args };

        // Spawn the PTY with a placeholder size; the first render resizes it to
        // the real Claude-pane dimensions.
        let (pty, pty_error) = match PtySession::new(24, 80, opts) {
            Ok(pty) => (Some(pty), None),
            Err(e) => (None, Some(e.to_string())),
        };

        // Start the background walk for the file manager immediately so the
        // file list is populated by the time the user first focuses that pane.
        let cwd_fm = session.project_path.clone();

        // Kick `claude mcp list` off on a background thread so workspace open
        // never waits on it. The subpanel starts with just `.mcp.json`
        // contents; the CLI results merge in on the first tick after the
        // shell-out completes.
        let mcp_list_job = Some(crate::mcp::spawn_list_via_claude(session.project_path.clone()));
        let project_mcps = crate::mcp::read_project_mcp_json(&session.project_path);
        let active_mcps = crate::mcp::merge_mcps(project_mcps, Vec::new());
        let active_skills_names =
            crate::skills::active_skill_names_in_project(&session.project_path);
        let session_info = metrics::read_session_info(&session.project_path, &session.id);
        let session_id = session.id.clone();

        Self {
            session,
            theme,
            // Center pane (Claude) is where the user works, so focus it first.
            focus: Panel::Claude,
            git_enabled,
            show_footer,
            show_title,
            border_style,
            side_width,
            nerd_icons,
            git,
            pty,
            pty_error,
            leader_pending: false,
            last_pty_size: Cell::new((24, 80)),
            last_git_refresh: Instant::now(),
            opened_at: Instant::now(),
            claude_awake: Cell::new(false),
            // File manager: background walk starts immediately.
            file_manager: FileManager::new(cwd_fm),
            file_manager_status: None,
            // Terminal PTY is spawned lazily on first focus (Ctrl+4 / Tab to
            // the Terminal pane). This avoids running two ConPTY instances on
            // Windows simultaneously before we've confirmed that works.
            terminal: None,
            terminal_error: None,
            terminal_enabled,
            last_term_size: Cell::new((TERMINAL_ROWS, 80)),
            terminal_exit_code: None,
            dialog: WorkspaceDialog::None,
            active_mcps,
            claude_listed_mcps: Vec::new(),
            mcp_list_job,
            last_mcp_list_kick: Instant::now(),
            active_skills_names,
            session_info,
            statusline: crate::statusline::read(&session_id),
            last_extras_refresh: Instant::now(),
        }
    }

    /// Per-frame upkeep. Reloads the Git pane at most once per
    /// [`GIT_REFRESH_INTERVAL`] so commits/edits made during the session (by
    /// Claude or the user) show up without re-running git status every frame.
    /// Also polls the terminal PTY for exit so the UI can offer a respawn.
    pub fn tick(&mut self) {
        if self.last_git_refresh.elapsed() >= GIT_REFRESH_INTERVAL {
            self.git = git::load(&self.session.project_path);
            self.last_git_refresh = Instant::now();
        }
        // Poll for terminal shell exit so render_terminal_pane can show a hint.
        if self.terminal_exit_code.is_none() {
            if let Some(pty) = &self.terminal {
                if let Some(code) = pty.poll_exit() {
                    self.terminal_exit_code = Some(code);
                }
            }
        }
        // Trigger background refresh of the file list on its 30-second cadence.
        self.file_manager.tick();

        // Join the `claude mcp list` background job as soon as it's done so
        // the results merge into the subpanel on the very next frame — no
        // waiting for the next `EXTRAS_REFRESH_INTERVAL` tick.
        //
        // Empty results (panicked thread, CLI timeout, `claude` not on PATH)
        // do NOT overwrite the last good cache: a periodic refresh that fails
        // for transient reasons would otherwise blank the subpanel until the
        // next successful run, making entries flicker in and out. Real
        // disconnects surface as an empty list once the user reopens Bruce.
        if let Some(job) = &self.mcp_list_job {
            if job.is_finished() {
                if let Some(job) = self.mcp_list_job.take() {
                    let listed = job.join().unwrap_or_default();
                    if !listed.is_empty() {
                        self.claude_listed_mcps = listed;
                    }
                    let project_mcps =
                        crate::mcp::read_project_mcp_json(&self.session.project_path);
                    self.active_mcps =
                        crate::mcp::merge_mcps(project_mcps, self.claude_listed_mcps.clone());
                }
            }
        }

        // Re-kick the `claude mcp list` shell-out periodically so newly
        // installed MCPs eventually surface without reopening the workspace.
        // Only kick when no job is already in flight.
        if self.mcp_list_job.is_none()
            && self.last_mcp_list_kick.elapsed() >= MCP_LIST_REFRESH_INTERVAL
        {
            self.mcp_list_job = Some(crate::mcp::spawn_list_via_claude(
                self.session.project_path.clone(),
            ));
            self.last_mcp_list_kick = Instant::now();
        }

        // Refresh the project-extras subpanels: re-merge `.mcp.json` with the
        // cached `claude mcp list` results, re-read which skills are
        // currently activated in this project, and re-read Claude's
        // transcript so the Session pane's model / speed / context stay
        // live as the conversation advances.
        if self.last_extras_refresh.elapsed() >= EXTRAS_REFRESH_INTERVAL {
            let project_mcps = crate::mcp::read_project_mcp_json(&self.session.project_path);
            self.active_mcps = crate::mcp::merge_mcps(project_mcps, self.claude_listed_mcps.clone());
            self.active_skills_names =
                crate::skills::active_skill_names_in_project(&self.session.project_path);
            self.session_info =
                metrics::read_session_info(&self.session.project_path, &self.session.id);
            // Keep the last good payload if the sidecar isn't readable this
            // round: the sink rewrites it via rename, and a transient miss
            // shouldn't blank the context bar mid-conversation.
            if let Some(latest) = crate::statusline::read(&self.session.id) {
                self.statusline = Some(latest);
            }
            self.last_extras_refresh = Instant::now();
        }
    }

    /// Respawn the shell PTY after the previous one has exited.
    ///
    /// Called when the user presses Enter in the Terminal pane after the shell
    /// exits. Drops the old PTY, clears the exit flag, and spawns a fresh one.
    pub fn respawn_terminal(&mut self) {
        self.terminal = None;
        self.terminal_exit_code = None;
        self.terminal_error = None;
        self.ensure_terminal();
    }

    /// Persist this session's soft metrics to disk. Call when leaving the
    /// workspace (back to welcome or on quit).
    ///
    /// Reads the session's own transcript (`<id>.jsonl`) for the cumulative
    /// token total, then `touch`es to refresh `last_used` and save. Best-effort:
    /// if the transcript isn't there yet the token count is left as-is, and a
    /// failed save only leaves metrics slightly stale — the session is intact.
    pub fn persist_metrics(&mut self) {
        if let Some(total) =
            metrics::session_total_tokens(&self.session.project_path, &self.session.id)
        {
            self.session.tokens_used = total;
        }
        let _ = self.session.touch();
    }

    /// Whether Claude has started painting (its PTY produced output). Used by
    /// the loading transition to know when to reveal the workspace. With no PTY
    /// there's nothing to wait for, so this reports ready.
    pub fn pty_has_output(&self) -> bool {
        self.pty.as_ref().map_or(true, |p| p.has_output())
    }

    /// Return the PTY that currently has keyboard focus:
    /// - `Panel::Claude`   → the Claude PTY
    /// - `Panel::Terminal` → the shell PTY
    /// - anything else     → `None` (Git/FileManager don't have a PTY)
    fn focused_pty(&self) -> Option<&PtySession> {
        match self.focus {
            Panel::Claude => self.pty.as_ref(),
            Panel::Terminal => self.terminal.as_ref(),
            _ => None,
        }
    }

    /// Spawn the shell PTY for the Terminal pane if it hasn't been spawned yet.
    ///
    /// Called on first focus of the Terminal pane (lazy spawn). Returns `true`
    /// when the PTY is available (either just spawned or already running).
    pub fn ensure_terminal(&mut self) -> bool {
        if self.terminal.is_some() {
            return true;
        }
        let shell = default_shell();
        let cwd = self.session.project_path.clone();
        let (rows, cols) = self.last_term_size.get();
        match PtySession::new_command(rows, cols, &shell, &[], Some(cwd), TERMINAL_SCROLLBACK) {
            Ok(pty) => {
                self.terminal = Some(pty);
                self.terminal_error = None;
                true
            }
            Err(e) => {
                self.terminal_error = Some(e.to_string());
                false
            }
        }
    }

    /// Forward a key event to the focused PTY (no-op without a PTY).
    pub fn send_key(&self, key: &KeyEvent) {
        if let Some(pty) = self.focused_pty() {
            // Cursor/nav keys are encoded differently depending on the child's
            // cursor-key mode (DECCKM): full TUIs like Claude switch it on and
            // then expect SS3 (ESC O x) instead of CSI (ESC [ x).
            let app_cursor = pty
                .lock_parser()
                .map(|p| p.screen().application_cursor())
                .unwrap_or(false);
            if let Some(bytes) = encode_key(key, app_cursor) {
                // Typing returns the view to the live bottom, the way a real
                // terminal snaps back when you start a new command.
                pty.scroll_to_bottom();
                pty.send(&bytes);
            }
        }
    }

    /// Forward pasted text to the focused PTY as a *bracketed paste*:
    /// wrapped in `ESC[200~` … `ESC[201~`. The child then treats the whole block
    /// as one multi-line insert instead of submitting on every newline.
    pub fn send_paste(&self, text: &str) {
        if let Some(pty) = self.focused_pty() {
            let mut bytes = Vec::with_capacity(text.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            // Snap back to the live bottom, the way typing does.
            pty.scroll_to_bottom();
            pty.send(&bytes);
        }
    }

    /// Forward a run of plain typed characters to the focused PTY, translating
    /// each newline to a carriage return (the byte Enter sends). Used when a
    /// fast burst of keystrokes is coalesced but isn't a multi-line paste.
    pub fn send_typed(&self, text: &str) {
        if let Some(pty) = self.focused_pty() {
            let mut bytes = Vec::with_capacity(text.len());
            let mut tmp = [0u8; 4];
            for c in text.chars() {
                if c == '\n' {
                    bytes.push(b'\r');
                } else {
                    bytes.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                }
            }
            pty.scroll_to_bottom();
            pty.send(&bytes);
        }
    }

    /// Scroll the focused PTY pane back by one page (no-op without a PTY). The
    /// page size is the pane's current height, so it pages like a terminal.
    pub fn scroll_up(&self) {
        match self.focus {
            Panel::Claude => {
                if let Some(pty) = self.pty.as_ref() {
                    pty.scroll_up(self.last_pty_size.get().0.max(1) as usize);
                }
            }
            Panel::Terminal => {
                if let Some(pty) = self.terminal.as_ref() {
                    pty.scroll_up(self.last_term_size.get().0.max(1) as usize);
                }
            }
            _ => {}
        }
    }

    /// Scroll the focused PTY pane forward by one page (no-op without a PTY).
    pub fn scroll_down(&self) {
        match self.focus {
            Panel::Claude => {
                if let Some(pty) = self.pty.as_ref() {
                    pty.scroll_down(self.last_pty_size.get().0.max(1) as usize);
                }
            }
            Panel::Terminal => {
                if let Some(pty) = self.terminal.as_ref() {
                    pty.scroll_down(self.last_term_size.get().0.max(1) as usize);
                }
            }
            _ => {}
        }
    }

    /// Toggle the Terminal pane. If it was focused while being hidden, focus Claude.
    /// Persists the choice immediately so it survives a hard terminal close
    /// (same rationale as `toggle_git`).
    pub fn toggle_terminal(&mut self) {
        self.terminal_enabled = !self.terminal_enabled;
        if !self.terminal_enabled && self.focus == Panel::Terminal {
            self.focus = Panel::Claude;
        }
        self.persist_panels();
    }

    /// Enabled panes in traversal order. Claude is always present; FileManager
    /// is always present in the top row; Terminal is present when enabled.
    fn enabled_panels(&self) -> Vec<Panel> {
        let mut panels = Vec::with_capacity(4);
        if self.git_enabled {
            panels.push(Panel::Git);
        }
        panels.push(Panel::Claude);
        panels.push(Panel::FileManager);
        if self.terminal_enabled {
            panels.push(Panel::Terminal);
        }
        panels
    }

    /// Give keyboard focus to `panel` directly, if it's currently enabled
    /// (Claude always is). Bound to Ctrl+1/2/3/4 so the user can jump between
    /// panes directly. Focusing the Terminal pane triggers lazy PTY spawn.
    pub fn focus_panel(&mut self, panel: Panel) {
        if self.enabled_panels().contains(&panel) {
            self.focus = panel;
            // Lazily spawn the shell PTY on first focus of the Terminal pane.
            if panel == Panel::Terminal {
                self.ensure_terminal();
            }
            // Trigger a file list refresh when the user explicitly focuses the
            // FileManager pane so they see up-to-date results immediately.
            if panel == Panel::FileManager {
                self.file_manager.refresh();
            }
        }
    }

    /// Cycle focus to the next enabled pane, skipping disabled ones. The
    /// universal fallback (Tab) for terminals that don't deliver Ctrl+1/2/3/4.
    pub fn focus_next(&mut self) {
        let panels = self.enabled_panels();
        let i = panels.iter().position(|&p| p == self.focus).unwrap_or(0);
        self.focus = panels[(i + 1) % panels.len()];
        if self.focus == Panel::Terminal {
            self.ensure_terminal();
        }
    }

    /// Cycle focus to the previous enabled pane, skipping disabled ones (BackTab).
    pub fn focus_prev(&mut self) {
        let panels = self.enabled_panels();
        let i = panels.iter().position(|&p| p == self.focus).unwrap_or(0);
        self.focus = panels[(i + panels.len() - 1) % panels.len()];
        if self.focus == Panel::Terminal {
            self.ensure_terminal();
        }
    }

    /// Move the file manager selection up by one row.
    pub fn fm_prev(&mut self) {
        self.file_manager.select_prev();
    }

    /// Move the file manager selection down by one row.
    pub fn fm_next(&mut self) {
        self.file_manager.select_next();
    }

    /// Toggle dotfile visibility in the file manager and re-walk.
    pub fn fm_toggle_hidden(&mut self) {
        self.file_manager.toggle_hidden();
    }

    /// Act on the selected file-manager row: go up via "..", descend into a
    /// folder, or open a file in the editor.
    pub fn fm_enter(&mut self) {
        self.file_manager_status = None;
        if let Some(abs_path) = self.file_manager.enter() {
            self.open_in_editor(&abs_path);
        }
    }

    /// Go up one directory in the file manager.
    pub fn fm_up(&mut self) {
        self.file_manager.cd_up();
    }

    /// Open `abs_path` in the editor: `$BRUCE_EDITOR` if set, else `code` /
    /// `code-insiders` on the PATH. VS Code gets `<project> --goto <abs>:1` so
    /// it focuses the file. On Windows the editor is launched through `cmd /C`
    /// (because `code` is a `.cmd` shim std::process::Command won't resolve);
    /// Unix spawns it directly. Detached. Errors go to `file_manager_status`.
    fn open_in_editor(&mut self, abs_path: &std::path::Path) {
        let Some((editor, is_vscode)) = editor_command() else {
            self.file_manager_status =
                Some("No editor found. Set $BRUCE_EDITOR or install 'code'.".to_string());
            return;
        };

        let mut args: Vec<String> = Vec::new();
        if is_vscode {
            args.push(self.session.project_path.to_string_lossy().into_owned());
            args.push("--goto".to_string());
            args.push(format!("{}:1", abs_path.display()));
        } else {
            args.push(abs_path.to_string_lossy().into_owned());
        }

        #[cfg(windows)]
        let mut cmd = {
            let mut c = std::process::Command::new("cmd");
            c.arg("/C").arg(&editor).args(&args);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = std::process::Command::new(&editor);
            c.args(&args);
            c
        };

        if let Err(e) = cmd.spawn() {
            self.file_manager_status = Some(format!("Failed to open editor: {e}"));
        }
    }

    /// Open the Ctrl+F file-search overlay. Snapshots the current file list and
    /// computes the initial (empty-query) result set immediately.
    pub fn open_file_search(&mut self) {
        let all_files = self
            .file_manager
            .files
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        // Empty query → all files (capped).
        let results: Vec<_> = all_files
            .into_iter()
            .take(FILE_SEARCH_MAX_RESULTS)
            .collect();
        self.dialog = WorkspaceDialog::FileSearch {
            query: String::new(),
            results,
            selected: 0,
        };
    }

    /// Returns `true` if a modal dialog is currently open.
    pub fn dialog_open(&self) -> bool {
        !matches!(self.dialog, WorkspaceDialog::None)
    }

    /// Close whatever dialog is open.
    pub fn close_dialog(&mut self) {
        self.dialog = WorkspaceDialog::None;
    }

    /// Type a character into the FileSearch query, then re-filter.
    pub fn fs_push_char(&mut self, c: char) {
        if let WorkspaceDialog::FileSearch { query, results, selected } = &mut self.dialog {
            query.push(c);
            Self::fs_refilter_inner(query, results, selected, &self.file_manager.files);
        }
    }

    /// Delete the last character from the FileSearch query, then re-filter.
    pub fn fs_pop_char(&mut self) {
        if let WorkspaceDialog::FileSearch { query, results, selected } = &mut self.dialog {
            query.pop();
            Self::fs_refilter_inner(query, results, selected, &self.file_manager.files);
        }
    }

    /// Re-compute the results list from the current query.
    fn fs_refilter_inner(
        query: &str,
        results: &mut Vec<std::path::PathBuf>,
        selected: &mut usize,
        files: &std::sync::Arc<std::sync::Mutex<Vec<std::path::PathBuf>>>,
    ) {
        let guard = match files.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        *results = guard
            .iter()
            .filter(|p| {
                let s = p.to_string_lossy();
                subsequence_match(query, &s)
            })
            .take(FILE_SEARCH_MAX_RESULTS)
            .cloned()
            .collect();
        // Clamp selection to the new result count.
        let max = results.len().saturating_sub(1);
        *selected = (*selected).min(max);
    }

    /// Move selection up inside the FileSearch overlay.
    pub fn fs_prev(&mut self) {
        if let WorkspaceDialog::FileSearch { selected, .. } = &mut self.dialog {
            *selected = selected.saturating_sub(1);
        }
    }

    /// Move selection down inside the FileSearch overlay, clamped to last result.
    pub fn fs_next(&mut self) {
        if let WorkspaceDialog::FileSearch { results, selected, .. } = &mut self.dialog {
            let max = results.len().saturating_sub(1);
            if *selected < max {
                *selected += 1;
            }
        }
    }

    /// Open the currently selected file from the FileSearch overlay using the
    /// same editor dispatch as `fm_open_selected`. Closes the dialog on success.
    pub fn fs_open_selected(&mut self) {
        // Extract the absolute path from the dialog without borrowing self.
        let abs_path = if let WorkspaceDialog::FileSearch { results, selected, .. } = &self.dialog
        {
            results
                .get(*selected)
                .map(|rel| self.session.project_path.join(rel))
        } else {
            return;
        };

        let Some(abs_path) = abs_path else {
            return;
        };

        // Reuse the shared editor dispatch, then close the overlay.
        self.file_manager_status = None;
        self.open_in_editor(&abs_path);
        self.dialog = WorkspaceDialog::None;
    }

    /// Toggle the Git pane. If it was focused while being hidden, focus Claude.
    pub fn toggle_git(&mut self) {
        self.git_enabled = !self.git_enabled;
        if !self.git_enabled && self.focus == Panel::Git {
            self.focus = Panel::Claude;
        }
        self.persist_panels();
    }

    /// Save the panel-visibility choice to disk *immediately*, so it survives
    /// even when the terminal is closed without a clean quit (which kills the
    /// process before the exit-time save runs). Loads the existing config first
    /// so the theme and update-check cache (last_update_check / latest_seen) are
    /// preserved rather than reset.
    fn persist_panels(&self) {
        let mut config = Config::load();
        config.git_enabled = self.git_enabled;
        config.terminal_enabled = self.terminal_enabled;
        let _ = config.save();
    }
}

/// Draw the full workspace screen into `frame`.
pub fn render(frame: &mut Frame, state: &WorkspaceState) {
    let pal = state.theme.palette();
    let area = frame.area();

    // Paint the whole background first.
    frame.render_widget(Block::default().style(Style::default().bg(pal.bg)), area);

    // Outer vertical layout:
    //   [0] title bar    (0 or 1 row)
    //   [1] top_row      (Git? | Claude | FileManager)
    //   [2] terminal_row (TERMINAL_ROWS when enabled, 0 when hidden)
    //   [3] footer bar   (0 or 1 row)
    let title_height = if state.show_title { 1 } else { 0 };
    let footer_height = if state.show_footer { 1 } else { 0 };
    let terminal_height = if state.terminal_enabled { TERMINAL_ROWS } else { 0 };
    let rows = Layout::vertical([
        Constraint::Length(title_height),    // title bar
        Constraint::Min(3),                  // top_row: Git? + Claude + FileManager
        Constraint::Length(terminal_height), // terminal_row: shell PTY
        Constraint::Length(footer_height),   // footer hints
    ])
    .split(area);

    if state.show_title {
        render_title(frame, rows[0], state, &pal);
    }

    // Horizontal columns within top_row:
    //   Git (optional, side%) | Claude (remainder) | FileManager (always, side%)
    // side_panes counts only toggleable side panes (Git); FileManager always
    // takes its own `side` slice.
    let side = state.side_width.percent();
    let side_panes = state.git_enabled as u16;
    let claude_width = 100 - side * side_panes - side;

    let mut constraints = Vec::with_capacity(3);
    let mut order: Vec<Panel> = Vec::with_capacity(3);
    if state.git_enabled {
        constraints.push(Constraint::Percentage(side));
        order.push(Panel::Git);
    }
    constraints.push(Constraint::Percentage(claude_width));
    order.push(Panel::Claude);
    // FileManager is always present in the top row.
    constraints.push(Constraint::Percentage(side));
    order.push(Panel::FileManager);

    let cols = Layout::horizontal(constraints).split(rows[1]);
    for (col, panel) in cols.iter().zip(order) {
        let focused = state.focus == panel;
        match panel {
            Panel::Git => render_git_pane(frame, *col, &pal, focused, &state.git, state.border_style),
            Panel::Claude => render_claude_pane(frame, *col, &pal, focused, state),
            Panel::FileManager => render_file_manager_column(frame, *col, &pal, focused, state),
            // Terminal is rendered in rows[2]; not part of the top_row split.
            Panel::Terminal => {}
        }
    }

    // Render the terminal pane when it's enabled (non-zero height).
    if state.terminal_enabled && rows[2].height > 0 {
        render_terminal_pane(frame, rows[2], &pal, state.focus == Panel::Terminal, state);
    }

    if state.show_footer {
        render_footer(frame, rows[3], &pal, state);
    }

    // Modal overlays are always rendered last so they appear on top.
    if !matches!(state.dialog, WorkspaceDialog::None) {
        render_workspace_dialog(frame, area, &pal, state);
    }
}

/// Build a pane's surrounding block. With borders on it's a rounded box whose
/// edge is the accent color when focused; with borders off (Settings → Borders)
/// it drops the lines entirely and keeps just the title, padding the content
/// down one row so it doesn't sit under the title.
fn pane_block<'a>(
    pal: &Palette,
    title: &'a str,
    focused: bool,
    bordered: bool,
    border_type: BorderType,
) -> Block<'a> {
    let mut block = Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(pal.bg));
    if bordered {
        let edge = if focused { pal.accent } else { pal.dim };
        block = block
            .borders(Borders::ALL)
            .border_type(border_type)
            .border_style(Style::default().fg(edge));
    } else {
        // Borderless (the Claude pane): pad two cells left/right so it breathes a
        // bit more from the framed side panes, and one top/bottom to line its
        // content up with their bordered inner area.
        block = block.padding(Padding::new(2, 2, 1, 1));
    }
    block
}

/// How long Claude's PTY output must stay quiet before we consider it "awake"
/// and reveal the pane. Long enough to span the bursts and pauses of its boot,
/// so the waking overlay stays up until Claude is really at its prompt.
const WAKE_SETTLE: Duration = Duration::from_millis(700);
/// Hard cap on the waking overlay, so the pane reveals even if Claude keeps
/// emitting output and never goes quiet.
const WAKE_MAX: Duration = Duration::from_secs(10);

/// Pixel-style spinner frames for the "waking Claude" overlay.
const WAKE_SPINNER: [&str; 8] = [
    "▰ ▱ ▱ ▱ ▱",
    "▱ ▰ ▱ ▱ ▱",
    "▱ ▱ ▰ ▱ ▱",
    "▱ ▱ ▱ ▰ ▱",
    "▱ ▱ ▱ ▱ ▰",
    "▱ ▱ ▱ ▰ ▱",
    "▱ ▱ ▰ ▱ ▱",
    "▱ ▰ ▱ ▱ ▱",
];

/// Centered "Bruce is waking Claude up…" message plus a pixel spinner, shown in
/// the Claude pane while its process finishes its initial paint. `opened_at`
/// drives the spinner frame.
fn render_waking(frame: &mut Frame, area: Rect, pal: &Palette, opened_at: Instant) {
    if area.height == 0 {
        return;
    }
    let mid = area.y + area.height.saturating_sub(2) / 2;
    frame.render_widget(
        Paragraph::new("Bruce is waking Claude up…")
            .alignment(Alignment::Center)
            .style(Style::default().fg(pal.fg).bg(pal.bg).add_modifier(Modifier::BOLD)),
        Rect { x: area.x, y: mid, width: area.width, height: 1 },
    );
    let spin_y = mid + 2;
    if spin_y < area.y + area.height {
        let tick = (opened_at.elapsed().as_millis() / 110) as usize;
        frame.render_widget(
            Paragraph::new(WAKE_SPINNER[tick % WAKE_SPINNER.len()])
                .alignment(Alignment::Center)
                .style(Style::default().fg(pal.accent).add_modifier(Modifier::BOLD)),
            Rect { x: area.x, y: spin_y, width: area.width, height: 1 },
        );
    }
}

/// Render the Claude pane: the embedded terminal's emulated screen.
fn render_claude_pane(frame: &mut Frame, area: Rect, pal: &Palette, focused: bool, state: &WorkspaceState) {
    let block = pane_block(pal, " Claude Code ", focused, false, BorderType::Rounded);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(pty) = &state.pty else {
        let msg = state
            .pty_error
            .as_deref()
            .unwrap_or("PTY not available on this platform.");
        simple_body(frame, inner, pal, msg);
        return;
    };

    // Keep the PTY and emulator sized to the visible area.
    let size = (inner.height, inner.width);
    if size.0 > 0 && size.1 > 0 && state.last_pty_size.get() != size {
        pty.resize(size.0, size.1);
        state.last_pty_size.set(size);
    }

    // While Claude is still booting, cover its half-painted screen with a waking
    // overlay until it has produced output *and* gone quiet for WAKE_SETTLE —
    // long enough to mean "finished its initial paint", not just a mid-boot
    // pause. A hard cap (WAKE_MAX) guarantees the pane reveals even if Claude
    // never fully settles. Once revealed it latches, so it never returns.
    if !state.claude_awake.get() {
        let booted = pty.has_output() && pty.output_quiet(WAKE_SETTLE);
        if booted || state.opened_at.elapsed() >= WAKE_MAX {
            state.claude_awake.set(true);
        } else {
            render_waking(frame, inner, pal, state.opened_at);
            return;
        }
    }

    if let Some(parser) = pty.lock_parser() {
        let screen = parser.screen();
        frame.render_widget(
            Paragraph::new(pty_screen_lines(screen, pal)).style(Style::default().bg(pal.bg)),
            inner,
        );
        // Show the child's cursor only while the pane is focused, the child
        // isn't hiding it, we're at the live bottom (not paging through history),
        // and output has gone idle. The idle check is the key one: it keeps the
        // cursor visible while you type a prompt but hides it while a response
        // streams, so it no longer flies across the pane mid-generation.
        if focused
            && !screen.hide_cursor()
            && screen.scrollback() == 0
            && pty.output_idle()
        {
            let (row, col) = screen.cursor_position();
            frame.set_cursor_position((inner.x + col, inner.y + row));
        }
    }
}

/// Convert the emulated screen into one styled line per row.
///
/// Delegates to `crate::pty::pty_screen_lines` which now owns this logic so
/// the install dialog can reuse the same render pipeline without pulling in
/// workspace internals.
fn pty_screen_lines<'a>(screen: &vt100::Screen, pal: &Palette) -> Vec<Line<'a>> {
    crate::pty::pty_screen_lines(screen, pal)
}

/// Colour for a working-tree status mark, by change type. Theme-aware: each
/// theme defines its own harmonised green/blue/red/yellow.
fn mark_color(mark: char, pal: &Palette) -> Color {
    match mark {
        'A' => pal.added,    // added / new in the index
        'M' => pal.modified, // modified
        'D' => pal.removed,  // deleted
        'R' => pal.renamed,  // renamed
        '?' => pal.dim,      // untracked
        _ => pal.fg,
    }
}

/// Render the Git pane: branches, recent commits and the working tree as titled
/// sections, plus a pinned stats footer (ahead/behind/staged/unstaged).
fn render_git_pane(frame: &mut Frame, area: Rect, pal: &Palette, focused: bool, view: &GitView, border_style: BorderStyle) {
    let title = match view {
        GitView::Repo(info) => format!(" git · {} ", info.branch),
        _ => " git ".to_string(),
    };
    let block = pane_block(pal, title.as_str(), focused, border_style.bordered(), border_style.border_type());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let info = match view {
        GitView::Repo(info) => info,
        GitView::NotARepo => return simple_body(frame, inner, pal, "Not a git repository."),
        GitView::Error(msg) => return simple_body(frame, inner, pal, &format!("git error: {msg}")),
    };

    // Split into a body (the three sections) and a pinned stats footer.
    let parts = Layout::vertical([Constraint::Min(3), Constraint::Length(5)]).split(inner);
    let width = parts[0].width as usize;
    let mut lines: Vec<Line> = Vec::new();

    section_header(&mut lines, pal, "branches", width);
    if info.branches.is_empty() {
        lines.push(dim_line(pal, " —"));
    } else {
        for b in &info.branches {
            let color = if b.is_head { pal.accent } else { pal.fg };
            let dot = if b.is_head { "●" } else { "○" };
            lines.push(Line::from(vec![
                Span::styled(format!(" {dot} "), Style::default().fg(color)),
                Span::styled(
                    b.name.as_str(),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ]));
            let sub = match &b.upstream {
                Some(u) => format!("     ↑{} ↓{} {}", u.ahead, u.behind, u.remote),
                None => "     local".to_string(),
            };
            lines.push(dim_line(pal, &sub));
        }
    }

    block_separator(&mut lines, pal, width);
    section_header(&mut lines, pal, "commits recientes", width);
    if info.commits.is_empty() {
        lines.push(dim_line(pal, " no commits yet"));
    } else {
        for c in &info.commits {
            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", c.short), Style::default().fg(pal.accent)),
                Span::styled(c.summary.as_str(), Style::default().fg(pal.dim)),
            ]));
        }
    }

    block_separator(&mut lines, pal, width);
    section_header(&mut lines, pal, "working tree", width);
    if info.files.is_empty() {
        lines.push(dim_line(pal, " working tree clean"));
    } else {
        for f in &info.files {
            let color = mark_color(f.mark, pal);
            // The mark's colour encodes the change *type* (A green, M blue,
            // D red, R yellow). A filled chip means staged (in the index); an
            // outline means an unstaged work-tree change.
            let mark = if f.staged {
                Span::styled(
                    format!(" {} ", f.mark),
                    Style::default()
                        .fg(pal.bg)
                        .bg(color)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    format!(" {} ", f.mark),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )
            };
            lines.push(Line::from(vec![
                mark,
                Span::raw(" "),
                // The path carries the same type colour as the mark, so the
                // whole working-tree row reads green/blue/red at a glance.
                Span::styled(f.path.as_str(), Style::default().fg(color)),
            ]));
        }
        if info.extra_files > 0 {
            lines.push(dim_line(pal, &format!(" …{} more", info.extra_files)));
        }
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(pal.bg)),
        parts[0],
    );
    render_git_footer(frame, parts[1], pal, info);
}

/// The pinned ahead/behind/staged/unstaged stats row at the bottom of Git.
fn render_git_footer(frame: &mut Frame, area: Rect, pal: &Palette, info: &git::GitInfo) {
    let width = area.width as usize;
    let lines = vec![
        Line::from(Span::styled(
            "─".repeat(width),
            Style::default().fg(pal.dim),
        )),
        stat_row(pal, width, "ahead", info.ahead, pal.added),
        stat_row(pal, width, "behind", info.behind, pal.renamed),
        stat_row(pal, width, "staged", info.staged, pal.added),
        stat_row(pal, width, "unstaged", info.unstaged, pal.removed),
    ];
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(pal.bg)),
        area,
    );
}

/// A right-aligned "label … value" stats row.
fn stat_row<'a>(pal: &Palette, width: usize, label: &str, value: usize, color: Color) -> Line<'a> {
    let value = value.to_string();
    let pad = width.saturating_sub(1 + label.len() + value.len());
    Line::from(vec![
        Span::styled(format!(" {label}"), Style::default().fg(pal.dim)),
        Span::raw(" ".repeat(pad)),
        Span::styled(value, Style::default().fg(color).add_modifier(Modifier::BOLD)),
    ])
}

/// Push an uppercase section title plus a *dashed* underline rule. The dashed
/// rule (title↔content) is deliberately distinct from the solid rule that
/// separates whole blocks (see [`block_separator`]).
fn section_header<'a>(lines: &mut Vec<Line<'a>>, pal: &Palette, title: &str, width: usize) {
    lines.push(Line::from(Span::styled(
        format!(" {}", title.to_uppercase()),
        Style::default().fg(pal.dim).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        "┄".repeat(width),
        Style::default().fg(pal.dim),
    )));
}

/// Push a solid rule separating one block from the next.
fn block_separator<'a>(lines: &mut Vec<Line<'a>>, pal: &Palette, width: usize) {
    lines.push(Line::from(Span::styled(
        "─".repeat(width),
        Style::default().fg(pal.dim),
    )));
}

/// A single dim line of text (owned, so it outlives any borrowed snapshot).
fn dim_line<'a>(pal: &Palette, text: &str) -> Line<'a> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(pal.dim),
    ))
}

/// Render a one-line body (used for the not-a-repo / error states).
fn simple_body(frame: &mut Frame, area: Rect, pal: &Palette, text: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(pal.dim),
        )))
        .style(Style::default().bg(pal.bg)),
        area,
    );
}

/// Render the File Manager pane: a scrollable list of project-relative file paths.
///
/// - The selected row is highlighted in accent colour.
/// - A dim status line at the very bottom shows error messages (e.g. editor
///   not found) or the dotfile toggle hint.
/// - The list scrolls to keep the selection in view; `clamp_scroll` is called
///   here (the only place with the real visible height) so `selected` and
///   `scroll_offset` are always consistent.
/// Drive the entire right-hand column. Layout, top to bottom:
///
/// - **Files** (flex — takes whatever the fixed rows leave).
/// - **MCPs + Skills** side by side (`EXTRAS_ROW_HEIGHT`).
/// - **Session** (`SESSION_ROW_HEIGHT`) — model, speed, context bar, turns.
/// - **Usage** (`USAGE_ROW_HEIGHT`) — cumulative tokens + estimated cost.
///
/// The focus marker stays with the file manager; the three bottom rows are
/// info-only and not navigable.
///
/// When the column is too short for the whole stack to be useful (under
/// [`EXTRAS_MIN_HEIGHT`] rows) the extras collapse and only the file manager
/// is rendered so we don't paint 2-row panels that no one can read.
fn render_file_manager_column(
    frame: &mut Frame,
    area: Rect,
    pal: &Palette,
    focused: bool,
    state: &WorkspaceState,
) {
    /// Fixed height for the MCP + Skills row. 9 rows leaves 7 for content
    /// after the frame, so 3-4 items breathe instead of crowding the
    /// pane's edges. The Files pane loses the same amount from its flex
    /// share, matching the "shrink Files a bit to un-squash the extras"
    /// intent.
    const EXTRAS_ROW_HEIGHT: u16 = 9;
    /// Session pane: six content rows (model, context, bar, 5h, week, tokens)
    /// plus the frame. Folding the old Usage pane in here bought back five
    /// rows for the file manager while showing strictly more.
    const SESSION_ROW_HEIGHT: u16 = 8;
    /// Threshold below which the whole extras stack collapses. Sum of the
    /// two fixed rows plus a floor for the file manager (~6 rows). Under this
    /// the panes stack too tight to read.
    const EXTRAS_MIN_HEIGHT: u16 = EXTRAS_ROW_HEIGHT + SESSION_ROW_HEIGHT + 6;

    if area.height < EXTRAS_MIN_HEIGHT {
        render_file_manager_pane(frame, area, pal, focused, state);
        return;
    }

    let rows = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(EXTRAS_ROW_HEIGHT),
        Constraint::Length(SESSION_ROW_HEIGHT),
    ])
    .split(area);
    render_file_manager_pane(frame, rows[0], pal, focused, state);
    render_project_extras_pane(frame, rows[1], pal, state);
    render_session_pane(frame, rows[2], pal, state);
}

/// The bottom half of the right column: MCPs (left) and active skills (right),
/// each in its own framed pane. Pure rendering — no focus, no input handling.
fn render_project_extras_pane(
    frame: &mut Frame,
    area: Rect,
    pal: &Palette,
    state: &WorkspaceState,
) {
    let halves = Layout::horizontal([
        Constraint::Percentage(50),
        Constraint::Percentage(50),
    ])
    .split(area);

    // MCPs: pass the set of names `claude mcp list` reported as connected
    // so each entry gets a green check or the neutral bullet. Skills don't
    // need the distinction — they're either activated in this project or
    // not listed at all.
    render_extras_list(
        frame,
        halves[0],
        pal,
        state,
        " MCPs ",
        &state.active_mcps,
        "(no MCPs)",
        Some(&state.claude_listed_mcps),
    );
    render_extras_list(
        frame,
        halves[1],
        pal,
        state,
        " Skills ",
        &state.active_skills_names,
        "(no skills)",
        None,
    );
}

/// Render one of the bottom subpanels: a framed block with a one-line-per-name
/// list, or a dimmed "(no X)" placeholder when the list is empty.
///
/// When `connected` is `Some`, each entry present in that set is drawn with
/// a green check (`✔`) to distinguish live MCP servers from configured-but-
/// unreachable ones; entries missing from the set fall back to the neutral
/// bullet. Pass `None` for lists where every item is inherently "on" (e.g.
/// skills — if they're listed, they're activated).
fn render_extras_list(
    frame: &mut Frame,
    area: Rect,
    pal: &Palette,
    state: &WorkspaceState,
    title: &str,
    items: &[String],
    empty_hint: &str,
    connected: Option<&[String]>,
) {
    let block = pane_block(
        pal,
        title,
        false,
        state.border_style.bordered(),
        state.border_style.border_type(),
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {empty_hint}"),
                Style::default().fg(pal.dim).add_modifier(Modifier::ITALIC),
            )))
            .style(Style::default().bg(pal.bg)),
            Rect { x: inner.x, y: inner.y, width: inner.width, height: 1 },
        );
        return;
    }

    let visible = inner.height as usize;
    for (i, name) in items.iter().take(visible).enumerate() {
        let y = inner.y + i as u16;
        let is_connected = connected
            .map(|set| set.iter().any(|c| c == name))
            .unwrap_or(false);
        let (glyph, glyph_style) = if is_connected {
            (
                "✔",
                Style::default().fg(pal.added).add_modifier(Modifier::BOLD),
            )
        } else {
            ("•", Style::default().fg(pal.dim))
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled(glyph, glyph_style),
                Span::styled(format!(" {name}"), Style::default().fg(pal.fg)),
            ]))
            .style(Style::default().bg(pal.bg)),
            Rect { x: inner.x, y, width: inner.width, height: 1 },
        );
    }

    // If there are more entries than fit, show a "+N more" footer on the last
    // line instead of silently truncating.
    if items.len() > visible && visible >= 2 {
        let remaining = items.len() - (visible - 1);
        let y = inner.y + (visible as u16) - 1;
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" … +{remaining} more"),
                Style::default().fg(pal.dim).add_modifier(Modifier::ITALIC),
            )))
            .style(Style::default().bg(pal.bg)),
            Rect { x: inner.x, y, width: inner.width, height: 1 },
        );
    }
}

/// The Session pane: one dense readout of everything that decides whether the
/// user can keep going — what's answering, how much context is left, how much
/// of the subscription budget is spent, and what the conversation cost.
///
/// - **Model** — friendly name, plus reasoning effort and a `fast` marker.
/// - **Context** — tokens still free, the share of the window they represent,
///   a proportional bar, and how many turns fit before auto-compact. The
///   projection is the actionable half: a percentage says where you are, the
///   turn count says when to wrap up.
/// - **5h / Week** — the rolling subscription usage windows, each with a
///   countdown to its reset. Present only for Claude.ai subscribers.
/// - **Tokens** — cumulative input (`↑`), output (`↓`) and cached (`⚡`).
/// - **Cost** — only when `ANTHROPIC_API_KEY` is set. Subscription users pay in
///   rate-limit budget, so a dollar figure there would be the wrong currency.
///
/// Window size, effort and the rate-limit windows come from Claude Code's own
/// status line payload via [`crate::statusline`]. When that isn't available
/// (shim not installed, or no API response yet) the pane falls back to the
/// transcript and [`metrics::context_cap_fallback`], so it degrades instead of
/// going blank.
fn render_session_pane(frame: &mut Frame, area: Rect, pal: &Palette, state: &WorkspaceState) {
    let block = pane_block(
        pal,
        " Session ",
        false,
        state.border_style.bordered(),
        state.border_style.border_type(),
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let status = state.statusline.as_ref();
    let info = state.session_info.as_ref();
    if status.is_none() && info.is_none() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " waiting for first response…",
                Style::default().fg(pal.dim).add_modifier(Modifier::ITALIC),
            )))
            .style(Style::default().bg(pal.bg)),
            Rect { x: inner.x, y: inner.y, width: inner.width, height: 1 },
        );
        return;
    }

    // Width of the label column: the longest label (" Context") plus a gap.
    const LABEL_WIDTH: u16 = 9;
    let label_style = Style::default().fg(pal.dim);
    let value_style = Style::default().fg(pal.fg);
    let value_width = inner.width.saturating_sub(LABEL_WIDTH);

    // Rows are collected first and painted after, because how many there are
    // depends on what the payload actually carries — an API-key user has no
    // rate-limit windows, a fresh session has no token totals.
    let mut rows: Vec<(&str, Line)> = Vec::new();

    // ── Model ───────────────────────────────────────────────────────────────
    // Claude Code's own display name wins: it's the label the user picked in
    // `/model`, so it can't drift the way a locally-derived one would.
    let model_name = status
        .and_then(|s| s.model_display_name.clone())
        .or_else(|| info.and_then(|i| i.model.as_deref().map(metrics::model_display)))
        .unwrap_or_else(|| "—".to_string());
    let mut model_spans = vec![Span::styled(model_name, value_style)];
    // Effort and fast mode ride along as suffix badges instead of taking rows
    // of their own — they're one word each, and a full row for one word is
    // space this pane doesn't have. Speed from the transcript is the fallback
    // when the status line shim hasn't reported an effort level.
    if let Some(effort) = status.and_then(|s| s.effort.as_deref()) {
        model_spans.push(Span::styled(
            format!(" · {effort}"),
            Style::default().fg(pal.dim),
        ));
    } else if let Some(speed) = info.and_then(|i| i.speed.as_deref()) {
        model_spans.push(Span::styled(
            format!(" · {speed}"),
            Style::default().fg(pal.dim),
        ));
    }
    if status.map(|s| s.fast_mode).unwrap_or(false) {
        model_spans.push(Span::styled(" · fast", Style::default().fg(pal.accent)));
    }
    rows.push((" Model", Line::from(model_spans)));

    // ── Context ─────────────────────────────────────────────────────────────
    let observed = info.map(|i| i.last_context_tokens).unwrap_or(0);
    let cap = status
        .and_then(|s| s.context_window_size)
        .unwrap_or_else(|| metrics::context_cap_fallback(observed));
    let used = status.and_then(|s| s.used_tokens).unwrap_or(observed).min(cap);
    let free = cap.saturating_sub(used);
    let pct_free = if cap == 0 {
        0
    } else {
        ((free as f64 / cap as f64) * 100.0).round() as u64
    };
    rows.push((
        " Context",
        Line::from(vec![
            Span::styled(
                format!("{} free ", fmt_tokens(free)),
                context_style(pal, pct_free),
            ),
            Span::styled(format!("· {pct_free}%"), Style::default().fg(pal.dim)),
        ]),
    ));

    // Bar and turn projection share a row, the bar taking whatever the
    // projection leaves — so a narrow pane degrades to just the bar rather
    // than wrapping into a mess.
    let projection = info
        .and_then(|i| metrics::context_growth_per_turn(&i.context_series))
        .and_then(|growth| metrics::turns_to_compact(used, cap, growth))
        .map(|turns| format!(" ≈{turns} turns"));
    let projection_width = projection
        .as_ref()
        .map(|p| p.chars().count() as u16)
        .unwrap_or(0);
    let mut bar_row = bar_spans(
        value_width.saturating_sub(projection_width + 1),
        pct_free as f64,
        context_colour(pal, pct_free),
        pal.dim,
    );
    if let Some(text) = projection {
        bar_row.push(Span::styled(text, Style::default().fg(pal.dim)));
    }
    rows.push(("", Line::from(bar_row)));

    // ── Rate limits ─────────────────────────────────────────────────────────
    // What a subscription actually spends. Absent for API-key users, and
    // absent until the session's first API response.
    let now = unix_now();
    if let Some(window) = status.and_then(|s| s.five_hour) {
        rows.push((" 5h", limit_line(pal, window, value_width, now)));
    }
    if let Some(window) = status.and_then(|s| s.seven_day) {
        rows.push((" Week", limit_line(pal, window, value_width, now)));
    }

    // ── Tokens (and cost, when tokens map to a bill) ────────────────────────
    if let Some(i) = info {
        let cached = i.totals.cache_read + i.totals.cache_write;
        rows.push((
            " Tokens",
            Line::from(vec![
                Span::styled(format!("{}↑ ", fmt_tokens(i.totals.input)), value_style),
                Span::styled(format!("{}↓ ", fmt_tokens(i.totals.output)), value_style),
                Span::styled(
                    format!("{}⚡", fmt_tokens(cached)),
                    Style::default().fg(pal.dim),
                ),
            ]),
        ));

        if metrics::cost_applies() {
            let cost = metrics::estimated_cost(&i.totals, i.model.as_deref());
            rows.push((
                " Cost",
                Line::from(Span::styled(
                    format!("~${cost:.2}"),
                    Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
                )),
            ));
        }
    }

    // ── Paint ───────────────────────────────────────────────────────────────
    for (i, (label, value)) in rows.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        if !label.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(*label, label_style)))
                    .style(Style::default().bg(pal.bg)),
                Rect { x: inner.x, y, width: LABEL_WIDTH.min(inner.width), height: 1 },
            );
        }
        if value_width > 0 {
            frame.render_widget(
                Paragraph::new(value.clone()).style(Style::default().bg(pal.bg)),
                Rect { x: inner.x + LABEL_WIDTH, y, width: value_width, height: 1 },
            );
        }
    }
}

/// Colour for a context readout: green while there's room, accent as it fills,
/// and the palette's red once auto-compact is close.
fn context_colour(pal: &Palette, pct_free: u64) -> Color {
    if pct_free >= 40 {
        pal.added
    } else if pct_free >= 15 {
        pal.accent
    } else {
        pal.removed
    }
}

/// Same health scale as [`context_colour`], bolded for the numeric readout.
fn context_style(pal: &Palette, pct_free: u64) -> Style {
    Style::default()
        .fg(context_colour(pal, pct_free))
        .add_modifier(Modifier::BOLD)
}

/// Colour for a rate-limit readout. The sense is inverted from context: this
/// percentage is what's already *spent*, so high is the bad end.
fn limit_colour(pal: &Palette, pct_used: f64) -> Color {
    if pct_used >= 85.0 {
        pal.removed
    } else if pct_used >= 60.0 {
        pal.renamed
    } else {
        pal.added
    }
}

/// One rate-limit row: proportional bar, percentage spent, and how long until
/// the window rolls over.
fn limit_line(
    pal: &Palette,
    window: crate::statusline::RateWindow,
    width: u16,
    now: i64,
) -> Line<'static> {
    let suffix = format!(
        " {:.0}% · {}",
        window.used_percentage,
        fmt_countdown(window.resets_at - now)
    );
    let bar_width = width.saturating_sub(suffix.chars().count() as u16 + 1);
    let mut spans = bar_spans(
        bar_width,
        window.used_percentage,
        limit_colour(pal, window.used_percentage),
        pal.dim,
    );
    spans.push(Span::styled(suffix, Style::default().fg(pal.dim)));
    Line::from(spans)
}

/// Spans for a one-row proportional bar `width` cells wide, `pct` of them
/// filled. Yields an empty vec when there's no room, so callers can splice the
/// result into a line without a width check of their own.
fn bar_spans(width: u16, pct: f64, fill: Color, empty: Color) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let filled = (((width as f64) * pct.clamp(0.0, 100.0) / 100.0).round() as u16).min(width);
    let mut spans = Vec::new();
    if filled > 0 {
        spans.push(Span::styled(
            "█".repeat(filled as usize),
            Style::default().fg(fill),
        ));
    }
    if width > filled {
        spans.push(Span::styled(
            "░".repeat((width - filled) as usize),
            Style::default().fg(empty),
        ));
    }
    spans
}

/// Seconds since the Unix epoch; 0 if the system clock predates it.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Compact countdown to a rate-limit reset: `3d 4h`, `2h 14m`, `12m`, `now`.
/// Deliberately terse — it shares a narrow row with a bar and a percentage.
fn fmt_countdown(seconds: i64) -> String {
    if seconds <= 0 {
        return "now".to_string();
    }
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    if days > 0 {
        return format!("{days}d {}h", hours % 24);
    }
    if hours > 0 {
        return format!("{hours}h {}m", minutes % 60);
    }
    format!("{minutes}m")
}

/// Format a token count for the Session pane: three digits max plus a unit
/// suffix so it fits in a narrow value column. `1_234` → `1.2K`,
/// `152_010` → `152K`, `1_200_000` → `1.2M`.
fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    if n < 10_000 {
        // One decimal in the sub-10K band so 1_234 doesn't collapse to "1K",
        // which loses too much precision at the low end.
        return format!("{:.1}K", n as f64 / 1_000.0);
    }
    if n < 1_000_000 {
        return format!("{}K", n / 1_000);
    }
    // Extended context windows put a million tokens on screen; "1000K" reads
    // worse than "1.0M".
    format!("{:.1}M", n as f64 / 1_000_000.0)
}

fn render_file_manager_pane(frame: &mut Frame, area: Rect, pal: &Palette, focused: bool, state: &WorkspaceState) {
    let border_type = state.border_style.border_type();
    let block = pane_block(pal, " Files ", focused, state.border_style.bordered(), border_type);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    let fm = &state.file_manager;

    // Row 0: the directory currently being browsed.
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", fm.current_dir_label()),
            Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
        )))
        .style(Style::default().bg(pal.bg)),
        Rect { x: inner.x, y: inner.y, width: inner.width, height: 1 },
    );

    // The list lives between the path row (top) and the status row (bottom).
    let list_top = inner.y + 1;
    let status_y = inner.y + inner.height.saturating_sub(1);
    let list_height = inner.height.saturating_sub(2) as usize;

    let count = fm.entries.len();
    let selected = fm.selected.min(count.saturating_sub(1));
    let scroll = if selected < fm.scroll_offset {
        selected
    } else {
        let bottom = fm.scroll_offset + list_height;
        if list_height > 0 && selected >= bottom {
            selected + 1 - list_height
        } else {
            fm.scroll_offset
        }
    };

    if count == 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " (empty)",
                Style::default().fg(pal.dim).add_modifier(Modifier::ITALIC),
            )))
            .style(Style::default().bg(pal.bg)),
            Rect { x: inner.x, y: list_top, width: inner.width, height: 1 },
        );
    } else {
        for (i, item) in fm.entries.iter().skip(scroll).take(list_height).enumerate() {
            let is_selected = scroll + i == selected;
            let y = list_top + i as u16;
            let icon = crate::panels::files::icon_for(item, state.nerd_icons);
            let suffix = if item.is_dir && !item.is_parent { "/" } else { "" };
            let label = format!(" {icon} {}{suffix}", item.name);
            let style = if is_selected {
                Style::default()
                    .fg(pal.bg)
                    .bg(pal.accent)
                    .add_modifier(Modifier::BOLD)
            } else if item.is_dir {
                Style::default().fg(pal.accent)
            } else {
                Style::default().fg(pal.fg)
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(label, style)))
                    .style(Style::default().bg(pal.bg)),
                Rect { x: inner.x, y, width: inner.width, height: 1 },
            );
        }
    }

    // Status / hint line ───────────────────────────────────────────────────────
    let hint_text = if let Some(status) = &state.file_manager_status {
        status.clone()
    } else if focused {
        let dot = if fm.show_hidden { "hide" } else { "show" };
        format!(" ↑↓ nav  Enter open/cd  ← up  . {dot} dotfiles")
    } else {
        String::new()
    };

    if !hint_text.is_empty() && status_y >= list_top {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint_text,
                Style::default().fg(pal.dim).add_modifier(Modifier::ITALIC),
            )))
            .style(Style::default().bg(pal.bg)),
            Rect { x: inner.x, y: status_y, width: inner.width, height: 1 },
        );
    }
}

/// Render the Terminal pane: the embedded shell's emulated screen.
///
/// Before the PTY has been spawned (before first focus) we show a short hint.
/// Once spawned we display its screen with the same pipeline as the Claude pane.
fn render_terminal_pane(frame: &mut Frame, area: Rect, pal: &Palette, focused: bool, state: &WorkspaceState) {
    let block = pane_block(pal, " Terminal ", focused, state.border_style.bordered(), state.border_style.border_type());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(pty) = &state.terminal else {
        // Shell hasn't been spawned yet — show the hint message until the user
        // focuses this pane for the first time.
        if let Some(err) = &state.terminal_error {
            simple_body(frame, inner, pal, &format!(" Shell error: {err}"));
        } else {
            simple_body(frame, inner, pal, " Press Ctrl+4 or Tab here to start a shell");
        }
        return;
    };

    // Keep the PTY and emulator sized to the visible area.
    let size = (inner.height, inner.width);
    if size.0 > 0 && size.1 > 0 && state.last_term_size.get() != size {
        pty.resize(size.0, size.1);
        state.last_term_size.set(size);
    }

    // If the shell has exited, show the last screen content plus a respawn hint.
    if let Some(code) = state.terminal_exit_code {
        if let Some(parser) = pty.lock_parser() {
            let screen = parser.screen();
            frame.render_widget(
                Paragraph::new(pty_screen_lines(screen, pal)).style(Style::default().bg(pal.bg)),
                inner,
            );
        }
        // Overlay a dim hint at the bottom of the inner rect.
        let hint_y = inner.y + inner.height.saturating_sub(1);
        if hint_y >= inner.y {
            let msg = format!(" Shell exited (code {code}). Press Enter to restart.");
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    msg,
                    Style::default().fg(pal.dim).add_modifier(Modifier::ITALIC),
                )))
                .style(Style::default().bg(pal.bg)),
                Rect { x: inner.x, y: hint_y, width: inner.width, height: 1 },
            );
        }
        return;
    }

    if let Some(parser) = pty.lock_parser() {
        let screen = parser.screen();
        frame.render_widget(
            Paragraph::new(pty_screen_lines(screen, pal)).style(Style::default().bg(pal.bg)),
            inner,
        );
        // Show the shell's cursor only while the pane is focused, the shell
        // isn't hiding it, we're at the live bottom, and output is idle.
        if focused
            && !screen.hide_cursor()
            && screen.scrollback() == 0
            && pty.output_idle()
        {
            let (row, col) = screen.cursor_position();
            frame.set_cursor_position((inner.x + col, inner.y + row));
        }
    }
}

/// Top title bar: app name + the open session's name.
fn render_title(frame: &mut Frame, area: Rect, state: &WorkspaceState, pal: &Palette) {
    let line = Line::from(vec![
        Span::styled(
            "  Bruce ",
            Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled("· ", Style::default().fg(pal.dim)),
        Span::styled(
            state.session.name.clone(),
            Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(pal.bg)),
        area,
    );
}

fn render_footer(frame: &mut Frame, area: Rect, pal: &Palette, state: &WorkspaceState) {
    let key = |k: &'static str| Span::styled(k, Style::default().fg(pal.accent));
    let txt = |t: &'static str| Span::styled(t, Style::default().fg(pal.dim));

    let hints = if state.leader_pending {
        // Mid-chord: show what the next key does.
        Line::from(vec![
            Span::styled("  Ctrl+b ", Style::default().fg(pal.accent).add_modifier(Modifier::BOLD)),
            txt("→  "),
            key("b"),
            txt(" back   "),
            key("Tab"),
            txt(" switch   "),
            key("g"),
            txt(" git   "),
            key("t"),
            txt(" terminal   "),
            key("q"),
            txt(" quit"),
        ])
    } else if state.focus == Panel::Claude && state.pty.is_some() {
        // Typing flows to Claude; control keys stay on Ctrl-chords.
        Line::from(vec![
            txt("  typing → Claude    "),
            key("Ctrl+1/2/3/4"),
            txt(" panes    "),
            key("Ctrl+F"),
            txt(" find    "),
            key("Shift+PgUp/PgDn"),
            txt(" scroll    "),
            key("Ctrl+b"),
            txt(" leader"),
        ])
    } else {
        // Side pane focused: direct navigation.
        Line::from(vec![
            key("  Ctrl+1/2/3/4"),
            txt(" panes   "),
            key("Ctrl+F"),
            txt(" find   "),
            key("Ctrl+T"),
            txt(" term   "),
            key("Esc"),
            txt(" back   "),
            key("Q"),
            txt(" quit"),
        ])
    };

    frame.render_widget(
        Paragraph::new(hints).style(Style::default().bg(pal.bg)),
        area,
    );
}

/// Compute a rectangle centred inside `area`, sized as percentages of its
/// width and height. Mirrors the same helper in `welcome.rs` so the workspace
/// can render its own dialogs without importing welcome's private API.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([Constraint::Percentage(percent_y)])
        .flex(Flex::Center)
        .split(area);
    Layout::horizontal([Constraint::Percentage(percent_x)])
        .flex(Flex::Center)
        .split(vertical[0])[0]
}

/// Build a dialog overlay block using the project's standard style: rounded
/// border in accent colour, accent title, background fill.
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

/// Render the active modal dialog overlay on top of the workspace.
///
/// Currently only `WorkspaceDialog::FileSearch` is defined. The overlay is
/// sized to 70 × 70 % of the screen and centred. The `Clear` widget erases
/// the background so the dialog appears floating rather than transparent.
fn render_workspace_dialog(frame: &mut Frame, area: Rect, pal: &Palette, state: &WorkspaceState) {
    match &state.dialog {
        WorkspaceDialog::None => {}
        WorkspaceDialog::FileSearch { query, results, selected } => {
            // Mute the workspace behind the overlay so the modal stands out,
            // the same backdrop the welcome dialogs use.
            crate::ui::welcome::dim_behind_dialog(frame, area, pal);
            render_file_search_dialog(frame, area, pal, query, results, *selected);
        }
    }
}

/// Render the Ctrl+F file-search overlay.
///
/// Layout (inside the dialog border):
/// ```
/// ┌─ File Search ───────────────────────────────┐
/// │  > {query}_                                  │   ← input line
/// │ ─────────────────────────────────────────── │   ← separator
/// │  src/app.rs                                  │   ← results list
/// │  src/main.rs  ← highlighted                 │
/// │  …                                           │
/// │ ─────────────────────────────────────────── │   ← separator
/// │  type to filter  ↑↓ select  Enter open  Esc │   ← footer hint
/// └──────────────────────────────────────────────┘
/// ```
fn render_file_search_dialog(
    frame: &mut Frame,
    area: Rect,
    pal: &Palette,
    query: &str,
    results: &[std::path::PathBuf],
    selected: usize,
) {
    let overlay = centered_rect(70, 70, area);
    frame.render_widget(Clear, overlay);
    let block = dialog_block(pal, " File Search ");
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);

    if inner.height < 4 {
        return;
    }

    // Reserve: 1 input line + 1 separator + N results + 1 separator + 1 footer.
    // Rows available for the results list:
    let reserved = 4usize; // input + sep + sep + footer
    let list_height = (inner.height as usize).saturating_sub(reserved);

    // ── Input line ────────────────────────────────────────────────────────────
    let input_text = format!(" > {}█", query);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            input_text,
            Style::default().fg(pal.fg),
        )))
        .style(Style::default().bg(pal.bg)),
        Rect { x: inner.x, y: inner.y, width: inner.width, height: 1 },
    );

    // ── Top separator ─────────────────────────────────────────────────────────
    let sep_y = inner.y + 1;
    if sep_y < inner.y + inner.height {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                Style::default().fg(pal.dim),
            )))
            .style(Style::default().bg(pal.bg)),
            Rect { x: inner.x, y: sep_y, width: inner.width, height: 1 },
        );
    }

    // ── Results list ──────────────────────────────────────────────────────────
    // Compute a scroll window so the selected entry is always visible.
    let scroll = if list_height == 0 {
        0
    } else if selected < list_height {
        0
    } else {
        selected + 1 - list_height
    };

    let list_start_y = inner.y + 2;
    for (i, path) in results.iter().skip(scroll).take(list_height).enumerate() {
        let abs_idx = scroll + i;
        let is_selected = abs_idx == selected;
        let display = path.display().to_string();
        let y = list_start_y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let style = if is_selected {
            Style::default()
                .fg(pal.bg)
                .bg(pal.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(pal.fg)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {display}"),
                style,
            )))
            .style(Style::default().bg(pal.bg)),
            Rect { x: inner.x, y, width: inner.width, height: 1 },
        );
    }

    // Show a dim hint when results are empty.
    if results.is_empty() && list_height > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " No files match",
                Style::default().fg(pal.dim).add_modifier(Modifier::ITALIC),
            )))
            .style(Style::default().bg(pal.bg)),
            Rect { x: inner.x, y: list_start_y, width: inner.width, height: 1 },
        );
    }

    // ── Bottom separator ──────────────────────────────────────────────────────
    let bot_sep_y = inner.y + 2 + list_height as u16;
    if bot_sep_y < inner.y + inner.height {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                Style::default().fg(pal.dim),
            )))
            .style(Style::default().bg(pal.bg)),
            Rect { x: inner.x, y: bot_sep_y, width: inner.width, height: 1 },
        );
    }

    // ── Footer hint ───────────────────────────────────────────────────────────
    let footer_y = inner.y + inner.height.saturating_sub(1);
    if footer_y >= inner.y && footer_y < inner.y + inner.height {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " type to filter  ↑↓ select  Enter open  Esc close",
                Style::default().fg(pal.dim).add_modifier(Modifier::ITALIC),
            )))
            .style(Style::default().bg(pal.bg)),
            Rect { x: inner.x, y: footer_y, width: inner.width, height: 1 },
        );
    }
}

/// Encode a key event as the bytes a terminal would send to the child.
///
/// Covers the common cases (text, control chars, Enter/Tab/Esc/Backspace and
/// the arrow/navigation keys); anything else is dropped. `app_cursor` is the
/// child's DECCKM state: when on, cursor keys use SS3 (`ESC O x`) instead of
/// CSI (`ESC [ x`) — the tilde keys (PageUp/Down, Delete) are unaffected.
pub fn encode_key(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let bytes = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Ctrl+A..Ctrl+Z (and a few neighbours) map to 0x01..0x1a.
                let upper = c.to_ascii_uppercase();
                if upper.is_ascii_alphabetic() {
                    vec![(upper as u8) - b'A' + 1]
                } else {
                    let mut b = [0u8; 4];
                    c.encode_utf8(&mut b).as_bytes().to_vec()
                }
            } else {
                let mut b = [0u8; 4];
                c.encode_utf8(&mut b).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => cursor_seq(app_cursor, b'A'),
        KeyCode::Down => cursor_seq(app_cursor, b'B'),
        KeyCode::Right => cursor_seq(app_cursor, b'C'),
        KeyCode::Left => cursor_seq(app_cursor, b'D'),
        KeyCode::Home => cursor_seq(app_cursor, b'H'),
        KeyCode::End => cursor_seq(app_cursor, b'F'),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => return None,
    };
    Some(bytes)
}

/// A cursor/nav key sequence ending in `final_byte`. DECCKM selects the prefix:
/// `ESC O` in application mode, `ESC [` in normal mode.
fn cursor_seq(app_cursor: bool, final_byte: u8) -> Vec<u8> {
    let prefix: &[u8] = if app_cursor { b"\x1bO" } else { b"\x1b[" };
    let mut seq = prefix.to_vec();
    seq.push(final_byte);
    seq
}

// ── Unit tests for the key encoder ────────────────────────────────────────────

#[cfg(test)]
mod encode_key_tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn plain_ascii_char_is_passed_through() {
        assert_eq!(encode_key(&key(KeyCode::Char('a')), false), Some(b"a".to_vec()));
        assert_eq!(encode_key(&key(KeyCode::Char('Z')), false), Some(b"Z".to_vec()));
    }

    #[test]
    fn ctrl_alpha_maps_to_control_byte() {
        // Ctrl+A → 0x01, Ctrl+Z → 0x1a — independent of letter case.
        assert_eq!(encode_key(&ctrl(KeyCode::Char('a')), false), Some(vec![0x01]));
        assert_eq!(encode_key(&ctrl(KeyCode::Char('A')), false), Some(vec![0x01]));
        assert_eq!(encode_key(&ctrl(KeyCode::Char('z')), false), Some(vec![0x1a]));
    }

    #[test]
    fn ctrl_non_alpha_falls_back_to_utf8() {
        // Ctrl+1 isn't a real control sequence; we let the digit through so
        // apps that read modified key events still get the printable byte.
        assert_eq!(encode_key(&ctrl(KeyCode::Char('1')), false), Some(b"1".to_vec()));
    }

    #[test]
    fn multibyte_utf8_char_is_encoded_in_utf8() {
        // 'ñ' is two bytes in UTF-8.
        assert_eq!(
            encode_key(&key(KeyCode::Char('ñ')), false),
            Some("ñ".as_bytes().to_vec())
        );
    }

    #[test]
    fn enter_tab_backtab_backspace_escape() {
        assert_eq!(encode_key(&key(KeyCode::Enter), false), Some(vec![b'\r']));
        assert_eq!(encode_key(&key(KeyCode::Tab), false), Some(vec![b'\t']));
        assert_eq!(encode_key(&key(KeyCode::BackTab), false), Some(b"\x1b[Z".to_vec()));
        assert_eq!(encode_key(&key(KeyCode::Backspace), false), Some(vec![0x7f]));
        assert_eq!(encode_key(&key(KeyCode::Esc), false), Some(vec![0x1b]));
    }

    #[test]
    fn cursor_keys_normal_mode() {
        assert_eq!(encode_key(&key(KeyCode::Up), false), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode_key(&key(KeyCode::Down), false), Some(b"\x1b[B".to_vec()));
        assert_eq!(encode_key(&key(KeyCode::Right), false), Some(b"\x1b[C".to_vec()));
        assert_eq!(encode_key(&key(KeyCode::Left), false), Some(b"\x1b[D".to_vec()));
        assert_eq!(encode_key(&key(KeyCode::Home), false), Some(b"\x1b[H".to_vec()));
        assert_eq!(encode_key(&key(KeyCode::End), false), Some(b"\x1b[F".to_vec()));
    }

    #[test]
    fn cursor_keys_application_mode_use_ss3_prefix() {
        // DECCKM on: `ESC O <letter>` instead of `ESC [ <letter>`.
        assert_eq!(encode_key(&key(KeyCode::Up), true), Some(b"\x1bOA".to_vec()));
        assert_eq!(encode_key(&key(KeyCode::Down), true), Some(b"\x1bOB".to_vec()));
        assert_eq!(encode_key(&key(KeyCode::Right), true), Some(b"\x1bOC".to_vec()));
        assert_eq!(encode_key(&key(KeyCode::Left), true), Some(b"\x1bOD".to_vec()));
        assert_eq!(encode_key(&key(KeyCode::Home), true), Some(b"\x1bOH".to_vec()));
        assert_eq!(encode_key(&key(KeyCode::End), true), Some(b"\x1bOF".to_vec()));
    }

    #[test]
    fn tilde_nav_keys_unaffected_by_app_cursor() {
        // PageUp/PageDown/Delete are CSI ~ sequences in both modes.
        for app_cursor in [false, true] {
            assert_eq!(
                encode_key(&key(KeyCode::PageUp), app_cursor),
                Some(b"\x1b[5~".to_vec())
            );
            assert_eq!(
                encode_key(&key(KeyCode::PageDown), app_cursor),
                Some(b"\x1b[6~".to_vec())
            );
            assert_eq!(
                encode_key(&key(KeyCode::Delete), app_cursor),
                Some(b"\x1b[3~".to_vec())
            );
        }
    }

    #[test]
    fn unsupported_key_returns_none() {
        // F-keys and similar aren't translated — caller can choose to ignore.
        assert_eq!(encode_key(&key(KeyCode::F(1)), false), None);
        assert_eq!(encode_key(&key(KeyCode::Null), false), None);
    }
}

#[cfg(test)]
mod session_pane_tests {
    use super::*;
    use crate::ui::theme::Theme;

    fn pal() -> Palette {
        Theme::Hacker.palette()
    }

    #[test]
    fn fmt_tokens_scales_from_raw_to_millions() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_234), "1.2K");
        assert_eq!(fmt_tokens(152_010), "152K");
        // Extended windows put seven figures on screen; "1000K" reads worse.
        assert_eq!(fmt_tokens(1_000_000), "1.0M");
        assert_eq!(fmt_tokens(1_400_000), "1.4M");
    }

    #[test]
    fn fmt_countdown_picks_the_coarsest_useful_unit() {
        assert_eq!(fmt_countdown(0), "now");
        assert_eq!(fmt_countdown(-500), "now");
        assert_eq!(fmt_countdown(12 * 60), "12m");
        assert_eq!(fmt_countdown(2 * 3600 + 14 * 60), "2h 14m");
        assert_eq!(fmt_countdown(3 * 86_400 + 4 * 3600), "3d 4h");
    }

    #[test]
    fn bar_spans_fills_proportionally_and_never_overflows() {
        let p = pal();
        let render = |width: u16, pct: f64| -> String {
            bar_spans(width, pct, p.added, p.dim)
                .iter()
                .map(|s| s.content.to_string())
                .collect()
        };
        assert_eq!(render(10, 0.0), "░░░░░░░░░░");
        assert_eq!(render(10, 50.0), "█████░░░░░");
        assert_eq!(render(10, 100.0), "██████████");
        // Out-of-range percentages are clamped, not allowed to overrun the row.
        assert_eq!(render(10, 250.0).chars().count(), 10);
        assert_eq!(render(10, -20.0).chars().count(), 10);
        // No room means no spans, so callers can splice unconditionally.
        assert!(bar_spans(0, 50.0, p.added, p.dim).is_empty());
    }

    /// Context and rate limits read the percentage in opposite directions:
    /// a high *free* context is healthy, a high *used* limit is not.
    #[test]
    fn context_and_limit_colours_move_in_opposite_directions() {
        let p = pal();
        assert_eq!(context_colour(&p, 80), p.added);
        assert_eq!(context_colour(&p, 20), p.accent);
        assert_eq!(context_colour(&p, 5), p.removed);

        assert_eq!(limit_colour(&p, 10.0), p.added);
        assert_eq!(limit_colour(&p, 70.0), p.renamed);
        assert_eq!(limit_colour(&p, 95.0), p.removed);
    }

    #[test]
    fn limit_line_keeps_the_percentage_and_reset_inside_the_row() {
        let p = pal();
        let window = crate::statusline::RateWindow {
            used_percentage: 41.2,
            resets_at: 1_000_000 + 2 * 3600,
        };
        let line = limit_line(&p, window, 30, 1_000_000);
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.ends_with(" 41% · 2h 0m"), "got {text:?}");
        // The bar has to fit in what the suffix leaves, never wrap the row.
        assert!(text.chars().count() <= 30, "row overflowed: {text:?}");
    }
}
