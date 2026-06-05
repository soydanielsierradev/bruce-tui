//! The 3-pane workspace: Git (left), Claude Code (center) and Metrics (right).
//!
//! Step 3 ships a *static* layout with placeholder content and panel focus.
//! Real wiring lands later: git2 (step 4), the embedded PTY (step 5) and the
//! token-metrics file watcher (step 6).

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};

use crate::panels::git::{self, GitView};
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
}

impl WorkspaceState {
    /// Open a workspace for `session_name`, inheriting the welcome theme and
    /// the chosen optional-panel configuration.
    pub fn new(session_name: String, theme: Theme, git_enabled: bool, metrics_enabled: bool) -> Self {
        // Sessions don't persist a project path yet, so read the repo Bruce is
        // running in. Once the session module lands, pass the session's path.
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let git = git::load(&cwd);

        Self {
            session_name,
            theme,
            // Center pane (Claude) is where the user works, so focus it first.
            focus: Panel::Claude,
            git_enabled,
            metrics_enabled,
            git,
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
        let (title, body) = match panel {
            Panel::Git => (" Git ", git_lines(&pal, &state.git)),
            Panel::Claude => (" Claude Code ", claude_placeholder(&pal)),
            Panel::Metrics => (" Metrics ", metrics_placeholder(&pal)),
        };
        render_pane(frame, *col, &pal, title, state.focus == panel, &body);
    }

    render_footer(frame, rows[2], &pal);
}

/// Build the Git pane body from a repository snapshot.
fn git_lines<'a>(pal: &Palette, view: &'a GitView) -> Vec<Line<'a>> {
    match view {
        GitView::NotARepo => vec![Line::from(Span::styled(
            "Not a git repository.",
            Style::default().fg(pal.dim),
        ))],
        GitView::Error(msg) => vec![Line::from(Span::styled(
            format!("git error: {msg}"),
            Style::default().fg(pal.dim),
        ))],
        GitView::Repo(info) => {
            let mut lines = vec![
                Line::from(Span::styled(
                    format!(" ⎇ {}", info.branch),
                    Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
                )),
                Line::raw(""),
            ];

            if info.files.is_empty() {
                lines.push(Line::from(Span::styled(
                    " working tree clean",
                    Style::default().fg(pal.dim),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    " Changes",
                    Style::default().fg(pal.dim).add_modifier(Modifier::BOLD),
                )));
                for f in &info.files {
                    // Staged changes read in accent, unstaged/untracked in fg.
                    let mark_color = if f.staged { pal.accent } else { pal.fg };
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!(" {} ", f.mark),
                            Style::default().fg(mark_color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(f.path.as_str(), Style::default().fg(pal.fg)),
                    ]));
                }
                if info.extra_files > 0 {
                    lines.push(Line::from(Span::styled(
                        format!(" …{} more", info.extra_files),
                        Style::default().fg(pal.dim),
                    )));
                }
            }

            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                " Recent",
                Style::default().fg(pal.dim).add_modifier(Modifier::BOLD),
            )));
            if info.commits.is_empty() {
                lines.push(Line::from(Span::styled(
                    " no commits yet",
                    Style::default().fg(pal.dim),
                )));
            } else {
                for c in &info.commits {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!(" {} ", c.short),
                            Style::default().fg(pal.accent),
                        ),
                        Span::styled(c.summary.as_str(), Style::default().fg(pal.fg)),
                    ]));
                }
            }

            lines
        }
    }
}

/// Placeholder body for the Claude pane (embedded PTY lands in step 5).
fn claude_placeholder<'a>(pal: &Palette) -> Vec<Line<'a>> {
    vec![
        Line::from(Span::styled(
            "The Claude Code process will",
            Style::default().fg(pal.dim),
        )),
        Line::from(Span::styled(
            "be embedded here in a PTY",
            Style::default().fg(pal.dim),
        )),
        Line::from(Span::styled("(step 5).", Style::default().fg(pal.dim))),
    ]
}

/// Placeholder body for the Metrics pane (file watcher lands in step 6).
fn metrics_placeholder<'a>(pal: &Palette) -> Vec<Line<'a>> {
    vec![
        Line::from(Span::styled("tokens: —", Style::default().fg(pal.accent))),
        Line::raw(""),
        Line::from(Span::styled(
            "Live token usage via a file",
            Style::default().fg(pal.dim),
        )),
        Line::from(Span::styled("watcher (step 6).", Style::default().fg(pal.dim))),
    ]
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

/// One bordered pane whose border colour signals focus.
fn render_pane(
    frame: &mut Frame,
    area: Rect,
    pal: &Palette,
    title: &str,
    focused: bool,
    body: &[Line],
) {
    let border_color = if focused { pal.accent } else { pal.dim };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(pal.bg));

    let paragraph = Paragraph::new(body.to_vec())
        .block(block)
        .alignment(Alignment::Left)
        .style(Style::default().bg(pal.bg));
    frame.render_widget(paragraph, area);
}

fn render_footer(frame: &mut Frame, area: Rect, pal: &Palette) {
    let hints = Line::from(vec![
        Span::styled("  Tab", Style::default().fg(pal.accent)),
        Span::styled(" switch   ", Style::default().fg(pal.dim)),
        Span::styled("^g", Style::default().fg(pal.accent)),
        Span::styled(" git   ", Style::default().fg(pal.dim)),
        Span::styled("^m", Style::default().fg(pal.accent)),
        Span::styled(" metrics   ", Style::default().fg(pal.dim)),
        Span::styled("Esc", Style::default().fg(pal.accent)),
        Span::styled(" back   ", Style::default().fg(pal.dim)),
        Span::styled("Q", Style::default().fg(pal.accent)),
        Span::styled(" quit", Style::default().fg(pal.dim)),
    ]);
    frame.render_widget(
        Paragraph::new(hints).style(Style::default().bg(pal.bg)),
        area,
    );
}
