//! The 3-pane workspace: Git (left), Claude Code (center) and Metrics (right).
//!
//! Step 3 ships a *static* layout with placeholder content and panel focus.
//! Real wiring lands later: git2 (step 4), the embedded PTY (step 5) and the
//! token-metrics file watcher (step 6).

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};

use crate::panels::git::{self, GitView};
use crate::panels::metrics::MetricsWatcher;
use crate::pty::PtySession;
use crate::ui::theme::{Palette, Theme};

/// Which pane currently has keyboard focus. `Tab` cycles through them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Git,
    Claude,
    Metrics,
}

/// State backing the workspace screen.
pub struct WorkspaceState {
    /// Name of the session this workspace was opened for.
    pub session_name: String,
    /// Active color theme, carried over from the welcome screen.
    pub theme: Theme,
    /// Which pane has focus.
    pub focus: Panel,
    /// Whether the Git pane is shown. The Claude pane is always shown.
    pub git_enabled: bool,
    /// Whether the Metrics pane is shown.
    pub metrics_enabled: bool,
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
}

impl WorkspaceState {
    /// Open a workspace for `session_name`, inheriting the welcome theme and
    /// the chosen optional-panel configuration.
    pub fn new(session_name: String, theme: Theme, git_enabled: bool, metrics_enabled: bool) -> Self {
        // Sessions don't persist a project path yet, so read the repo Bruce is
        // running in. Once the session module lands, pass the session's path.
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let git = git::load(&cwd);
        // Watch Claude's transcript for this project to surface live token usage.
        let metrics = MetricsWatcher::new(&cwd);

        // Spawn the PTY with a placeholder size; the first render resizes it to
        // the real Claude-pane dimensions.
        let (pty, pty_error) = match PtySession::new(24, 80) {
            Ok(pty) => (Some(pty), None),
            Err(e) => (None, Some(e.to_string())),
        };

        Self {
            session_name,
            theme,
            // Center pane (Claude) is where the user works, so focus it first.
            focus: Panel::Claude,
            git_enabled,
            metrics_enabled,
            git,
            pty,
            pty_error,
            metrics,
            leader_pending: false,
            last_pty_size: Cell::new((24, 80)),
        }
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
                pty.send(&bytes);
            }
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

    /// Cycle focus to the next enabled pane, skipping disabled ones.
    pub fn focus_next(&mut self) {
        let panels = self.enabled_panels();
        let i = panels.iter().position(|&p| p == self.focus).unwrap_or(0);
        self.focus = panels[(i + 1) % panels.len()];
    }

    /// Cycle focus to the previous enabled pane, skipping disabled ones.
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
    }

    /// Toggle the Metrics pane. If it was focused while being hidden, focus Claude.
    pub fn toggle_metrics(&mut self) {
        self.metrics_enabled = !self.metrics_enabled;
        if !self.metrics_enabled && self.focus == Panel::Metrics {
            self.focus = Panel::Claude;
        }
    }
}

/// Draw the full workspace screen into `frame`.
pub fn render(frame: &mut Frame, state: &WorkspaceState) {
    let pal = state.theme.palette();
    let area = frame.area();

    // Paint the whole background first.
    frame.render_widget(Block::default().style(Style::default().bg(pal.bg)), area);

    // Vertical sections: title bar, the three panes, footer.
    let rows = Layout::vertical([
        Constraint::Length(1), // title bar
        Constraint::Min(3),    // panes
        Constraint::Length(1), // footer hints
    ])
    .split(area);

    render_title(frame, rows[0], state, &pal);

    // Columns adapt to which panes are enabled: each side pane takes 25%, the
    // Claude pane absorbs the rest (50% with both, 75% with one, 100% alone).
    let side_panes = state.git_enabled as u16 + state.metrics_enabled as u16;
    let claude_width = 100 - 25 * side_panes;

    let mut constraints = Vec::with_capacity(3);
    let mut order = Vec::with_capacity(3);
    if state.git_enabled {
        constraints.push(Constraint::Percentage(25));
        order.push(Panel::Git);
    }
    constraints.push(Constraint::Percentage(claude_width));
    order.push(Panel::Claude);
    if state.metrics_enabled {
        constraints.push(Constraint::Percentage(25));
        order.push(Panel::Metrics);
    }

    let cols = Layout::horizontal(constraints).split(rows[1]);
    for (col, panel) in cols.iter().zip(order) {
        let focused = state.focus == panel;
        match panel {
            Panel::Git => render_git_pane(frame, *col, &pal, focused, &state.git),
            Panel::Claude => render_claude_pane(frame, *col, &pal, focused, state),
            Panel::Metrics => render_metrics_pane(frame, *col, &pal, focused, state),
        }
    }

    render_footer(frame, rows[2], &pal, state);
}

/// Render the Claude pane: the embedded terminal's emulated screen.
fn render_claude_pane(frame: &mut Frame, area: Rect, pal: &Palette, focused: bool, state: &WorkspaceState) {
    let border_color = if focused { pal.accent } else { pal.dim };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            " Claude Code ",
            Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(pal.bg));
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

    if let Some(parser) = pty.lock_parser() {
        let screen = parser.screen();
        frame.render_widget(
            Paragraph::new(pty_screen_lines(screen, pal)).style(Style::default().bg(pal.bg)),
            inner,
        );
        // Show the child's cursor only while the pane is focused.
        if focused && !screen.hide_cursor() {
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
fn render_git_pane(frame: &mut Frame, area: Rect, pal: &Palette, focused: bool, view: &GitView) {
    let title = match view {
        GitView::Repo(info) => format!(" git · {} ", info.branch),
        _ => " git ".to_string(),
    };
    let border_color = if focused { pal.accent } else { pal.dim };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title.as_str(),
            Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(pal.bg));
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
    let border_color = if focused { pal.accent } else { pal.dim };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            " Metrics ",
            Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(pal.bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(watcher) = &state.metrics else {
        return simple_body(frame, inner, pal, "Token metrics unavailable.");
    };
    let m = watcher.snapshot();
    let width = inner.width as usize;

    let mut lines: Vec<Line> = Vec::new();
    // Headline total, then the breakdown.
    lines.push(Line::from(vec![
        Span::styled(" tokens", Style::default().fg(pal.dim)),
        Span::raw(" ".repeat(width.saturating_sub(1 + 6 + fmt_count(m.total()).len()))),
        Span::styled(
            fmt_count(m.total()),
            Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        "┄".repeat(width),
        Style::default().fg(pal.dim),
    )));
    metric_row(&mut lines, pal, width, "input", m.input, pal.fg);
    metric_row(&mut lines, pal, width, "output", m.output, pal.added);
    metric_row(&mut lines, pal, width, "cache write", m.cache_write, pal.renamed);
    metric_row(&mut lines, pal, width, "cache read", m.cache_read, pal.modified);
    lines.push(Line::raw(""));
    metric_row(&mut lines, pal, width, "turns", m.messages, pal.dim);

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
            state.session_name.clone(),
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
        // Typing flows to Claude; control keys live behind the leader.
        Line::from(vec![
            txt("  typing → Claude    "),
            key("Ctrl+b"),
            txt(" leader (then b/Tab/g/m/q)"),
        ])
    } else {
        // Side pane focused: direct navigation.
        Line::from(vec![
            key("  Tab"),
            txt(" switch   "),
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
