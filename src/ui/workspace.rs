//! The 3-pane workspace: Git (left), Claude Code (center) and Metrics (right).
//!
//! Step 3 ships a *static* layout with placeholder content and panel focus.
//! Real wiring lands later: git2 (step 4), the embedded PTY (step 5) and the
//! token-metrics file watcher (step 6).

use std::cell::Cell;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph},
};

use crate::config::Config;
use crate::panels::git::{self, GitView};
use crate::panels::metrics::{self, MetricsWatcher};
use crate::pty::{PtySession, SpawnOptions};
use crate::session::Session;
use crate::ui::theme::{BorderStyle, Palette, SideWidth, Theme};

/// Which pane currently has keyboard focus. `Tab` cycles through them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Git,
    Claude,
    Metrics,
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
    /// Whether the Metrics pane is shown.
    pub metrics_enabled: bool,
    /// Show the bottom hint bar (Settings → Footer hints).
    pub show_footer: bool,
    /// Show the top title bar (Settings → Title bar).
    pub show_title: bool,
    /// Line style for the framed side panes (Settings → Border style).
    pub border_style: BorderStyle,
    /// Width of each side pane (Settings → Side width).
    pub side_width: SideWidth,
    /// Snapshot of the repository state shown in the Git pane.
    pub git: GitView,
    /// The embedded process + emulated terminal for the Claude pane.
    pub pty: Option<PtySession>,
    /// Why the PTY couldn't start, if it didn't.
    pub pty_error: Option<String>,
    /// Live token-usage watcher backing the Metrics pane (`None` if it couldn't
    /// start — e.g. no home directory).
    pub metrics: Option<MetricsWatcher>,
    /// `true` after Ctrl+b, waiting for the leader command key.
    pub leader_pending: bool,
    /// Last size the PTY was synced to, to avoid resizing every frame.
    last_pty_size: Cell<(u16, u16)>,
    /// When the Git pane was last reloaded, to throttle the refresh poll.
    last_git_refresh: Instant,
    /// When this workspace opened, used to animate the "waking Claude" overlay.
    opened_at: Instant,
    /// Latches `true` once Claude has finished its initial paint, so the waking
    /// overlay is shown only while it boots and never again.
    claude_awake: Cell<bool>,
}

/// How often the Git pane is re-read from disk. Claude (or the user) changes the
/// repo while a session runs, so the snapshot must refresh; once a second keeps
/// it current without re-running git status every frame.
const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

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
        metrics_enabled: bool,
        show_footer: bool,
        show_title: bool,
        border_style: BorderStyle,
        side_width: SideWidth,
    ) -> Self {
        let cwd = session.project_path.clone();
        let git = git::load(&cwd);
        // Watch Claude's transcript for this project to surface live token usage.
        let metrics = MetricsWatcher::new(&cwd);

        // Pin the conversation to the session id so it can be resumed later.
        let flag = if resume { "--resume" } else { "--session-id" };
        let opts = SpawnOptions {
            cwd: Some(cwd),
            args: vec![flag.to_string(), session.id.clone()],
        };

        // Spawn the PTY with a placeholder size; the first render resizes it to
        // the real Claude-pane dimensions.
        let (pty, pty_error) = match PtySession::new(24, 80, opts) {
            Ok(pty) => (Some(pty), None),
            Err(e) => (None, Some(e.to_string())),
        };

        Self {
            session,
            theme,
            // Center pane (Claude) is where the user works, so focus it first.
            focus: Panel::Claude,
            git_enabled,
            metrics_enabled,
            show_footer,
            show_title,
            border_style,
            side_width,
            git,
            pty,
            pty_error,
            metrics,
            leader_pending: false,
            last_pty_size: Cell::new((24, 80)),
            last_git_refresh: Instant::now(),
            opened_at: Instant::now(),
            claude_awake: Cell::new(false),
        }
    }

    /// Per-frame upkeep. Reloads the Git pane at most once per
    /// [`GIT_REFRESH_INTERVAL`] so commits/edits made during the session (by
    /// Claude or the user) show up without re-running git status every frame.
    pub fn tick(&mut self) {
        if self.last_git_refresh.elapsed() >= GIT_REFRESH_INTERVAL {
            self.git = git::load(&self.session.project_path);
            self.last_git_refresh = Instant::now();
        }
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

    /// Forward a key event to the embedded process (no-op without a PTY).
    pub fn send_key(&self, key: &KeyEvent) {
        if let Some(pty) = self.pty.as_ref() {
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

    /// Forward pasted text to the embedded process as a *bracketed paste*:
    /// wrapped in `ESC[200~` … `ESC[201~`. The child then treats the whole block
    /// as one multi-line insert instead of submitting on every newline — which
    /// is what made a pasted snippet split into several messages. Claude Code
    /// also collapses a large paste into a "[Pasted text +N lines]" placeholder
    /// once it sees these markers, so the input stays readable.
    pub fn send_paste(&self, text: &str) {
        if let Some(pty) = self.pty.as_ref() {
            let mut bytes = Vec::with_capacity(text.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            // Snap back to the live bottom, the way typing does.
            pty.scroll_to_bottom();
            pty.send(&bytes);
        }
    }

    /// Forward a run of plain typed characters to the child, translating each
    /// newline to a carriage return (the byte Enter sends). Used when a fast
    /// burst of keystrokes is coalesced but isn't a multi-line paste, so it
    /// behaves exactly as if the keys had been typed one by one.
    pub fn send_typed(&self, text: &str) {
        if let Some(pty) = self.pty.as_ref() {
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

    /// Scroll the Claude pane back by one page (no-op without a PTY). The page
    /// size is the pane's current height, so it pages like a terminal.
    pub fn scroll_up(&self) {
        if let Some(pty) = self.pty.as_ref() {
            pty.scroll_up(self.last_pty_size.get().0.max(1) as usize);
        }
    }

    /// Scroll the Claude pane forward by one page (no-op without a PTY).
    pub fn scroll_down(&self) {
        if let Some(pty) = self.pty.as_ref() {
            pty.scroll_down(self.last_pty_size.get().0.max(1) as usize);
        }
    }

    /// Enabled panes in left-to-right order. Claude is always present.
    fn enabled_panels(&self) -> Vec<Panel> {
        let mut panels = Vec::with_capacity(3);
        if self.git_enabled {
            panels.push(Panel::Git);
        }
        panels.push(Panel::Claude);
        if self.metrics_enabled {
            panels.push(Panel::Metrics);
        }
        panels
    }

    /// Give keyboard focus to `panel` directly, if it's currently enabled
    /// (Claude always is). Bound to Ctrl+1/2/3 so the user can jump between
    /// panes directly — where the terminal delivers those keys.
    pub fn focus_panel(&mut self, panel: Panel) {
        if self.enabled_panels().contains(&panel) {
            self.focus = panel;
        }
    }

    /// Cycle focus to the next enabled pane, skipping disabled ones. The
    /// universal fallback (Tab) for terminals that don't deliver Ctrl+1/2/3.
    pub fn focus_next(&mut self) {
        let panels = self.enabled_panels();
        let i = panels.iter().position(|&p| p == self.focus).unwrap_or(0);
        self.focus = panels[(i + 1) % panels.len()];
    }

    /// Cycle focus to the previous enabled pane, skipping disabled ones (BackTab).
    pub fn focus_prev(&mut self) {
        let panels = self.enabled_panels();
        let i = panels.iter().position(|&p| p == self.focus).unwrap_or(0);
        self.focus = panels[(i + panels.len() - 1) % panels.len()];
    }

    /// Toggle the Git pane. If it was focused while being hidden, focus Claude.
    pub fn toggle_git(&mut self) {
        self.git_enabled = !self.git_enabled;
        if !self.git_enabled && self.focus == Panel::Git {
            self.focus = Panel::Claude;
        }
        self.persist_panels();
    }

    /// Toggle the Metrics pane. If it was focused while being hidden, focus Claude.
    pub fn toggle_metrics(&mut self) {
        self.metrics_enabled = !self.metrics_enabled;
        if !self.metrics_enabled && self.focus == Panel::Metrics {
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
        config.metrics_enabled = self.metrics_enabled;
        let _ = config.save();
    }
}

/// Draw the full workspace screen into `frame`.
pub fn render(frame: &mut Frame, state: &WorkspaceState) {
    let pal = state.theme.palette();
    let area = frame.area();

    // Paint the whole background first.
    frame.render_widget(Block::default().style(Style::default().bg(pal.bg)), area);

    // Vertical sections: title bar, the three panes, footer. The title and
    // footer rows collapse to zero height when hidden (Settings → Title bar /
    // Footer hints), giving those lines back to the panes.
    let title_height = if state.show_title { 1 } else { 0 };
    let footer_height = if state.show_footer { 1 } else { 0 };
    let rows = Layout::vertical([
        Constraint::Length(title_height),  // title bar
        Constraint::Min(3),                // panes
        Constraint::Length(footer_height), // footer hints
    ])
    .split(area);

    if state.show_title {
        render_title(frame, rows[0], state, &pal);
    }

    // Columns adapt to which panes are enabled: each side pane takes the
    // configured width (Settings → Side width), Claude absorbs the rest.
    let side = state.side_width.percent();
    let side_panes = state.git_enabled as u16 + state.metrics_enabled as u16;
    let claude_width = 100 - side * side_panes;

    let mut constraints = Vec::with_capacity(3);
    let mut order = Vec::with_capacity(3);
    if state.git_enabled {
        constraints.push(Constraint::Percentage(side));
        order.push(Panel::Git);
    }
    constraints.push(Constraint::Percentage(claude_width));
    order.push(Panel::Claude);
    if state.metrics_enabled {
        constraints.push(Constraint::Percentage(side));
        order.push(Panel::Metrics);
    }

    let cols = Layout::horizontal(constraints).split(rows[1]);
    for (col, panel) in cols.iter().zip(order) {
        let focused = state.focus == panel;
        // Only the side panes (Git, Metrics) are framed; the Claude pane stays
        // borderless so the embedded terminal blends in, keeping the same spacing
        // via padding.
        match panel {
            Panel::Git => render_git_pane(frame, *col, &pal, focused, &state.git, state.border_style.border_type()),
            Panel::Claude => render_claude_pane(frame, *col, &pal, focused, state),
            Panel::Metrics => render_metrics_pane(frame, *col, &pal, focused, state),
        }
    }

    if state.show_footer {
        render_footer(frame, rows[2], &pal, state);
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
/// Cells that use the terminal *default* colour resolve to the active theme's
/// `fg`/`bg` (not `Color::Reset`), so the embedded child blends into the rest
/// of the workspace instead of showing the host terminal's own background.
/// Colours the child picks explicitly (diff green/red, accents) pass through
/// untouched — they carry meaning we must not override.
fn pty_screen_lines<'a>(screen: &vt100::Screen, pal: &Palette) -> Vec<Line<'a>> {
    let (rows, cols) = screen.size();
    let mut lines = Vec::with_capacity(rows as usize);
    for row in 0..rows {
        let mut spans = Vec::with_capacity(cols as usize);
        for col in 0..cols {
            let (text, style) = match screen.cell(row, col) {
                Some(cell) => {
                    let raw = cell.contents();
                    // A blank cell yields an empty string; render it as a space.
                    let content = if raw.is_empty() {
                        " ".to_string()
                    } else {
                        raw.to_string()
                    };
                    let mut style = Style::default()
                        .fg(conv_color(cell.fgcolor(), pal.fg))
                        .bg(conv_color(cell.bgcolor(), pal.bg));
                    if cell.bold() {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if cell.italic() {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    if cell.underline() {
                        style = style.add_modifier(Modifier::UNDERLINED);
                    }
                    if cell.inverse() {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                    (content, style)
                }
                None => (
                    " ".to_string(),
                    Style::default().fg(pal.fg).bg(pal.bg),
                ),
            };
            spans.push(Span::styled(text, style));
        }
        lines.push(Line::from(spans));
    }
    lines
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

/// Map a vt100 colour to a ratatui colour. The terminal *default* colour
/// resolves to `fallback` (the theme's fg or bg) so embedded output shares the
/// workspace background instead of falling back to the host terminal.
fn conv_color(color: vt100::Color, fallback: Color) -> Color {
    match color {
        vt100::Color::Default => fallback,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Render the Git pane: branches, recent commits and the working tree as titled
/// sections, plus a pinned stats footer (ahead/behind/staged/unstaged).
fn render_git_pane(frame: &mut Frame, area: Rect, pal: &Palette, focused: bool, view: &GitView, border_type: BorderType) {
    let title = match view {
        GitView::Repo(info) => format!(" git · {} ", info.branch),
        _ => " git ".to_string(),
    };
    let block = pane_block(pal, title.as_str(), focused, true, border_type);
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

/// Render the Metrics pane: live token usage parsed from Claude's transcript.
fn render_metrics_pane(frame: &mut Frame, area: Rect, pal: &Palette, focused: bool, state: &WorkspaceState) {
    let block = pane_block(pal, " Metrics ", focused, true, state.border_style.border_type());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(watcher) = &state.metrics else {
        return simple_body(frame, inner, pal, "Token metrics unavailable.");
    };
    let m = watcher.snapshot();
    let width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();

    // ── tokens · session ──
    section_header(&mut lines, pal, "tokens · sesión", width);
    big_number(&mut lines, &fmt_count(m.total()), pal.accent);
    lines.push(dim_line(pal, &format!(" {} turnos", m.turns)));
    lines.push(Line::raw(""));

    // ── active context window ──
    section_header(&mut lines, pal, "contexto activo", width);
    big_number(&mut lines, &fmt_count(m.context), pal.modified);
    let ctx_pct = m.context_pct();
    bar_line(&mut lines, pal, width, ctx_pct, pal.modified);
    metric_row_str(&mut lines, pal, width, "% ventana", &format!("{ctx_pct}%"), pal.modified);
    lines.push(Line::raw(""));

    // ── breakdown ──
    section_header(&mut lines, pal, "desglose", width);
    metric_row(&mut lines, pal, width, "input", m.input, pal.fg);
    metric_row(&mut lines, pal, width, "output", m.output, pal.added);
    metric_row_str(&mut lines, pal, width, "cache hits", &format!("{}%", m.cache_hit_pct()), pal.renamed);
    metric_row(&mut lines, pal, width, "tool calls", m.tool_calls, pal.accent);
    lines.push(Line::raw(""));

    // ── estimated cost (approximate: hard-coded list prices) ──
    section_header(&mut lines, pal, "costo · est.", width);
    big_number(&mut lines, &format!("${:.2}", m.cost_usd()), pal.renamed);
    lines.push(dim_line(pal, &format!(" {}", model_short(&m.model))));
    lines.push(Line::raw(""));

    // ── active MCP servers (those exercised this session) ──
    section_header(&mut lines, pal, "mcp · activos", width);
    if m.mcp_servers.is_empty() {
        lines.push(dim_line(pal, " —"));
    } else {
        for server in &m.mcp_servers {
            lines.push(Line::from(vec![
                Span::styled(" ● ", Style::default().fg(pal.added)),
                Span::styled(server.clone(), Style::default().fg(pal.fg)),
            ]));
        }
    }
    lines.push(Line::raw(""));

    // ── current session ──
    section_header(&mut lines, pal, "sesión", width);
    metric_row_str(&mut lines, pal, width, "duración", &fmt_duration(m.duration_secs()), pal.added);
    metric_row(&mut lines, pal, width, "archivos", m.files_edited, pal.modified);
    metric_row_str(&mut lines, pal, width, "líneas +", &format!("+{}", m.lines_added), pal.added);
    metric_row_str(&mut lines, pal, width, "líneas −", &format!("−{}", m.lines_removed), pal.removed);

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(pal.bg)),
        inner,
    );
}

/// Push a right-aligned "label … value" row, the value formatted compactly.
fn metric_row<'a>(lines: &mut Vec<Line<'a>>, pal: &Palette, width: usize, label: &str, value: u64, color: Color) {
    let value = fmt_count(value);
    let pad = width.saturating_sub(1 + label.len() + value.len());
    lines.push(Line::from(vec![
        Span::styled(format!(" {label}"), Style::default().fg(pal.dim)),
        Span::raw(" ".repeat(pad)),
        Span::styled(value, Style::default().fg(color).add_modifier(Modifier::BOLD)),
    ]));
}

/// Push a right-aligned "label … value" row where the value is a ready string.
fn metric_row_str<'a>(lines: &mut Vec<Line<'a>>, pal: &Palette, width: usize, label: &str, value: &str, color: Color) {
    let pad = width.saturating_sub(1 + label.chars().count() + value.chars().count());
    lines.push(Line::from(vec![
        Span::styled(format!(" {label}"), Style::default().fg(pal.dim)),
        Span::raw(" ".repeat(pad)),
        Span::styled(value.to_string(), Style::default().fg(color).add_modifier(Modifier::BOLD)),
    ]));
}

/// Push a prominent (bold, coloured) headline number.
fn big_number<'a>(lines: &mut Vec<Line<'a>>, text: &str, color: Color) {
    lines.push(Line::from(Span::styled(
        format!(" {text}"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
}

/// Push a horizontal fill bar: `filled` blocks coloured, the rest dim.
fn bar_line<'a>(lines: &mut Vec<Line<'a>>, pal: &Palette, width: usize, pct: u64, color: Color) {
    let cells = width.saturating_sub(2);
    let filled = (cells as u64 * pct / 100) as usize;
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("█".repeat(filled), Style::default().fg(color)),
        Span::styled("░".repeat(cells.saturating_sub(filled)), Style::default().fg(pal.dim)),
    ]));
}

/// Human-friendly elapsed time: `2h 5m`, `5m 12s`, `42s`.
fn fmt_duration(secs: i64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// Drop the `claude-` prefix from a model id for a compact label.
fn model_short(model: &str) -> String {
    model.strip_prefix("claude-").unwrap_or(model).to_string()
}

/// Compact human count: 1234 -> "1.2k", 1500000 -> "1.5M".
fn fmt_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
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
            key("m"),
            txt(" metrics   "),
            key("q"),
            txt(" quit"),
        ])
    } else if state.focus == Panel::Claude && state.pty.is_some() {
        // Typing flows to Claude; control keys stay on Ctrl-chords.
        Line::from(vec![
            txt("  typing → Claude    "),
            key("Ctrl+1/2/3"),
            txt(" panes    "),
            key("Shift+PgUp/PgDn"),
            txt(" scroll    "),
            key("Ctrl+b"),
            txt(" leader (Tab/b/g/m/q)"),
        ])
    } else {
        // Side pane focused: direct navigation.
        Line::from(vec![
            key("  Ctrl+1/2/3"),
            txt(" / "),
            key("Tab"),
            txt(" panes   "),
            key("^g"),
            txt(" git   "),
            key("^m"),
            txt(" metrics   "),
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

/// Encode a key event as the bytes a terminal would send to the child.
///
/// Covers the common cases (text, control chars, Enter/Tab/Esc/Backspace and
/// the arrow/navigation keys); anything else is dropped. `app_cursor` is the
/// child's DECCKM state: when on, cursor keys use SS3 (`ESC O x`) instead of
/// CSI (`ESC [ x`) — the tilde keys (PageUp/Down, Delete) are unaffected.
fn encode_key(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
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
