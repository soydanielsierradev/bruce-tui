//! Welcome screen: ASCII logo, an Options block, the saved-session list,
//! a theme selector and two modal dialogs (new session / rename session).
//!
//! This module owns both the screen *state* ([`WelcomeState`]) and its
//! *rendering* ([`render`]). For step 1 the session list is hardcoded; it will
//! be replaced by real persisted sessions once the `session` module lands.

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph},
};

use crate::ui::theme::{Palette, Theme};

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
const OPTION_LABELS: [&str; 2] = [" + New session", " ✎ Rename session"];

/// Which panel currently has keyboard focus. `Tab` toggles between the two.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Options,
    Sessions,
}

/// Lightweight summary of a saved session, shown as one list row.
///
/// This is intentionally *not* the persisted `Session` struct — it only holds
/// what the welcome list needs to display.
pub struct SessionSummary {
    pub name: String,
    pub branch: String,
    pub last_used: String,
    pub tokens: u64,
}

/// Outcome of routing a key to an open dialog.
///
/// The dialog itself can't switch screens — that's the event loop's job — so a
/// confirmed "new session" is returned here for `app` to act on.
pub enum WelcomeEvent {
    /// Nothing for the caller to do.
    None,
    /// The user confirmed the new-session form; open a workspace with this name.
    CreateSession { name: String },
}

/// The single dialog that can be open over the welcome screen.
pub enum Dialog {
    Rename(RenameDialog),
    NewSession(NewSessionDialog),
}

/// Modal state for renaming a session.
///
/// `buffer` is the edit mode toggle: `None` means the user is browsing the
/// session list; `Some(text)` means they are typing a new name for the row at
/// `selected`.
pub struct RenameDialog {
    /// Row in the session table the dialog is pointing at.
    pub selected: usize,
    /// `Some` while editing — holds the in-progress name.
    pub buffer: Option<String>,
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
    /// Saved sessions, in display order.
    pub sessions: Vec<SessionSummary>,
    /// Which panel has focus.
    pub focus: Focus,
    /// Selected row within the Options block.
    pub option_selected: usize,
    /// Selected row within the Sessions block.
    pub session_selected: usize,
    /// Active color theme.
    pub theme: Theme,
    /// Open dialog, if any. When `Some`, it captures all input.
    pub dialog: Option<Dialog>,
}

impl WelcomeState {
    /// Build the initial state with hardcoded sample sessions.
    pub fn new() -> Self {
        let sessions = vec![
            SessionSummary {
                name: "bruce".into(),
                branch: "main".into(),
                last_used: "2026-06-04".into(),
                tokens: 47_832,
            },
            SessionSummary {
                name: "oauth-service".into(),
                branch: "feature/oauth".into(),
                last_used: "2026-06-02".into(),
                tokens: 128_410,
            },
            SessionSummary {
                name: "tui-experiments".into(),
                branch: "spike/ratatui".into(),
                last_used: "2026-05-29".into(),
                tokens: 9_204,
            },
        ];
        Self {
            sessions,
            focus: Focus::Options,
            option_selected: 0,
            session_selected: 0,
            theme: Theme::Dark,
            dialog: None,
        }
    }

    /// Toggle focus between the Options and Sessions panels.
    pub fn focus_next(&mut self) {
        self.focus = match self.focus {
            Focus::Options => Focus::Sessions,
            Focus::Sessions => Focus::Options,
        };
    }

    /// Move the selection up within the focused panel, wrapping around.
    pub fn select_prev(&mut self) {
        match self.focus {
            Focus::Options => {
                let n = OPTION_LABELS.len();
                self.option_selected = (self.option_selected + n - 1) % n;
            }
            Focus::Sessions => {
                let n = self.sessions.len();
                if n > 0 {
                    self.session_selected = (self.session_selected + n - 1) % n;
                }
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
            Focus::Sessions => {
                let n = self.sessions.len();
                if n > 0 {
                    self.session_selected = (self.session_selected + 1) % n;
                }
            }
        }
    }

    /// Focus the Options block on the "New session" row (the `N` shortcut).
    pub fn focus_new_session(&mut self) {
        self.focus = Focus::Options;
        self.option_selected = 0;
    }

    /// True when the "New session" option is selected.
    pub fn on_new_session(&self) -> bool {
        self.focus == Focus::Options && self.option_selected == 0
    }

    /// True when the "Rename session" option is selected.
    pub fn on_rename(&self) -> bool {
        self.focus == Focus::Options && self.option_selected == 1
    }

    /// Open the new-session form.
    pub fn open_new_session(&mut self) {
        self.dialog = Some(Dialog::NewSession(NewSessionDialog::new()));
    }

    /// Open the rename dialog, unless there are no sessions to rename.
    pub fn open_rename(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.dialog = Some(Dialog::Rename(RenameDialog {
            selected: 0,
            buffer: None,
        }));
    }

    /// Route a key press to the open dialog. No-op if none is open.
    pub fn dialog_key(&mut self, code: KeyCode) -> WelcomeEvent {
        match self.dialog {
            Some(Dialog::Rename(_)) => {
                self.rename_key(code);
                WelcomeEvent::None
            }
            Some(Dialog::NewSession(_)) => self.new_session_key(code),
            None => WelcomeEvent::None,
        }
    }

    /// Rename dialog key handling.
    ///
    /// Browse mode (`buffer == None`): Up/Down move, Enter starts editing, Esc
    /// closes. Edit mode (`buffer == Some`): characters/backspace edit, Enter
    /// commits the new name (empty is ignored), Esc cancels the edit.
    fn rename_key(&mut self, code: KeyCode) {
        let n = self.sessions.len();
        let Some(Dialog::Rename(dlg)) = self.dialog.as_mut() else {
            return;
        };

        if dlg.buffer.is_some() {
            match code {
                KeyCode::Char(c) => {
                    if let Some(buf) = dlg.buffer.as_mut() {
                        buf.push(c);
                    }
                }
                KeyCode::Backspace => {
                    if let Some(buf) = dlg.buffer.as_mut() {
                        buf.pop();
                    }
                }
                KeyCode::Esc => dlg.buffer = None,
                KeyCode::Enter => {
                    let new_name = dlg
                        .buffer
                        .take()
                        .map(|b| b.trim().to_string())
                        .unwrap_or_default();
                    let idx = dlg.selected;
                    if !new_name.is_empty() {
                        if let Some(s) = self.sessions.get_mut(idx) {
                            s.name = new_name;
                        }
                    }
                }
                _ => {}
            }
        } else {
            match code {
                KeyCode::Up if n > 0 => dlg.selected = (dlg.selected + n - 1) % n,
                KeyCode::Down if n > 0 => dlg.selected = (dlg.selected + 1) % n,
                KeyCode::Enter => {
                    let cur = self
                        .sessions
                        .get(dlg.selected)
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    if let Some(Dialog::Rename(dlg)) = self.dialog.as_mut() {
                        dlg.buffer = Some(cur);
                    }
                }
                KeyCode::Esc => self.dialog = None,
                _ => {}
            }
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

    // Vertical sections: top margin, logo, Options block, session list,
    // theme bar, footer.
    let chunks = Layout::vertical([
        Constraint::Length(2),  // top margin (breathing room above the banner)
        Constraint::Length(10), // logo (9-line Delta Corps Priest 1 banner)
        Constraint::Length(4),  // options block (2 rows + borders)
        Constraint::Min(5),     // session list
        Constraint::Length(3),  // theme selector
        Constraint::Length(1),  // footer hints
    ])
    .split(area);

    render_logo(frame, chunks[1], state);
    render_options(frame, chunks[2], state);
    render_sessions(frame, chunks[3], state);
    render_theme_bar(frame, chunks[4], state);
    render_footer(frame, chunks[5], state);

    // Dialogs are modal overlays drawn on top of everything.
    match &state.dialog {
        Some(Dialog::Rename(d)) => render_rename_dialog(frame, area, &pal, state, d),
        Some(Dialog::NewSession(d)) => render_new_session_dialog(frame, area, &pal, d),
        None => {}
    }
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
}

fn render_options(frame: &mut Frame, area: Rect, state: &WelcomeState) {
    let pal = state.theme.palette();
    let focused = state.focus == Focus::Options;

    let items: Vec<ListItem> = OPTION_LABELS
        .iter()
        .map(|label| {
            ListItem::new(Line::from(Span::styled(
                *label,
                Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
            )))
        })
        .collect();

    let block = panel_block(&pal, " Options ", focused);
    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style(&pal));

    let mut list_state = ListState::default();
    // Only show the cursor when this panel has focus.
    list_state.select(focused.then_some(state.option_selected));
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn render_sessions(frame: &mut Frame, area: Rect, state: &WelcomeState) {
    let pal = state.theme.palette();
    let focused = state.focus == Focus::Sessions;

    let items: Vec<ListItem> = state
        .sessions
        .iter()
        .map(|s| ListItem::new(session_row(state, s)))
        .collect();

    let block = panel_block(&pal, " Sessions ", focused);
    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style(&pal));

    let mut list_state = ListState::default();
    list_state.select(focused.then_some(state.session_selected));
    frame.render_stateful_widget(list, area, &mut list_state);
}

/// One formatted session row: name, branch, last-used date and token count.
fn session_row<'a>(state: &WelcomeState, s: &'a SessionSummary) -> Line<'a> {
    let pal = state.theme.palette();
    Line::from(vec![
        Span::styled(
            format!(" {:<18}", s.name),
            Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{:<22}", s.branch), Style::default().fg(pal.accent)),
        Span::styled(format!("{:<12}", s.last_used), Style::default().fg(pal.dim)),
        Span::styled(
            format!("{:>10} tok", fmt_tokens(s.tokens)),
            Style::default().fg(pal.dim),
        ),
    ])
}

fn render_theme_bar(frame: &mut Frame, area: Rect, state: &WelcomeState) {
    let pal = state.theme.palette();

    let mut spans = vec![Span::styled(" Theme: ", Style::default().fg(pal.dim))];
    for theme in Theme::ALL {
        let selected = theme == state.theme;
        let style = if selected {
            Style::default()
                .fg(pal.bg)
                .bg(pal.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(pal.dim)
        };
        spans.push(Span::styled(format!(" {} ", theme.palette().name), style));
        spans.push(Span::raw(" "));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(pal.dim))
        .style(Style::default().bg(pal.bg));

    let bar = Paragraph::new(Line::from(spans))
        .block(block)
        .style(Style::default().bg(pal.bg));
    frame.render_widget(bar, area);
}

fn render_footer(frame: &mut Frame, area: Rect, state: &WelcomeState) {
    let pal = state.theme.palette();

    // Hints depend on whether (and which) modal dialog is open.
    let hints = match &state.dialog {
        Some(Dialog::Rename(d)) if d.buffer.is_some() => Line::from(vec![
            Span::styled("  type", Style::default().fg(pal.accent)),
            Span::styled(" new name   ", Style::default().fg(pal.dim)),
            Span::styled("Enter", Style::default().fg(pal.accent)),
            Span::styled(" save   ", Style::default().fg(pal.dim)),
            Span::styled("Esc", Style::default().fg(pal.accent)),
            Span::styled(" cancel", Style::default().fg(pal.dim)),
        ]),
        Some(Dialog::Rename(_)) => Line::from(vec![
            Span::styled("  ↑↓", Style::default().fg(pal.accent)),
            Span::styled(" pick   ", Style::default().fg(pal.dim)),
            Span::styled("Enter", Style::default().fg(pal.accent)),
            Span::styled(" rename   ", Style::default().fg(pal.dim)),
            Span::styled("Esc", Style::default().fg(pal.accent)),
            Span::styled(" close", Style::default().fg(pal.dim)),
        ]),
        Some(Dialog::NewSession(_)) => Line::from(vec![
            Span::styled("  type", Style::default().fg(pal.accent)),
            Span::styled(" a name   ", Style::default().fg(pal.dim)),
            Span::styled("Enter", Style::default().fg(pal.accent)),
            Span::styled(" create   ", Style::default().fg(pal.dim)),
            Span::styled("Esc", Style::default().fg(pal.accent)),
            Span::styled(" cancel", Style::default().fg(pal.dim)),
        ]),
        None => Line::from(vec![
            Span::styled("  ↑↓", Style::default().fg(pal.accent)),
            Span::styled(" select   ", Style::default().fg(pal.dim)),
            Span::styled("Tab", Style::default().fg(pal.accent)),
            Span::styled(" switch   ", Style::default().fg(pal.dim)),
            Span::styled("←→", Style::default().fg(pal.accent)),
            Span::styled(" theme   ", Style::default().fg(pal.dim)),
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

/// Draw the modal rename dialog centred over `screen`.
fn render_rename_dialog(
    frame: &mut Frame,
    screen: Rect,
    pal: &Palette,
    state: &WelcomeState,
    dialog: &RenameDialog,
) {
    let area = centered_rect(60, 60, screen);
    frame.render_widget(Clear, area);

    let block = dialog_block(pal, " Rename session ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // One row per session: name (or edit buffer for the active row) + branch.
    let items: Vec<ListItem> = state
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let editing_this = dialog.buffer.is_some() && i == dialog.selected;
            let name_span = if editing_this {
                let buf = dialog.buffer.as_deref().unwrap_or("");
                Span::styled(
                    format!(" {}▏", buf),
                    Style::default()
                        .fg(pal.bg)
                        .bg(pal.accent)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    format!(" {:<18}", s.name),
                    Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
                )
            };
            ListItem::new(Line::from(vec![
                name_span,
                Span::raw("  "),
                Span::styled(s.branch.clone(), Style::default().fg(pal.dim)),
            ]))
        })
        .collect();

    let list = List::new(items).highlight_style(highlight_style(pal));
    let mut list_state = ListState::default();
    // Don't show the row cursor while editing — the inline buffer is the cursor.
    list_state.select((dialog.buffer.is_none()).then_some(dialog.selected));
    frame.render_stateful_widget(list, inner, &mut list_state);
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
fn panel_block<'a>(pal: &Palette, title: &'a str, focused: bool) -> Block<'a> {
    let border_color = if focused { pal.accent } else { pal.dim };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default().fg(pal.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(pal.bg))
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
