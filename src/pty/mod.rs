//! Embeds a child process in a pseudo-terminal and emulates its output.
//!
//! A background thread reads the child's bytes and feeds them into a shared
//! [`vt100::Parser`] (an in-memory terminal). The UI locks that parser to draw
//! the current screen; the event loop forwards keystrokes via [`PtySession::send`].
//!
//! That same thread answers the terminal capability/status queries a real
//! terminal would (device attributes, cursor position). Without this, full TUIs
//! like Claude Code block at startup waiting for a reply that never comes.
//!
//! Shared state across the reader thread and the UI is an `Arc<Mutex<_>>`, per
//! the project's threading rule. The child is killed on drop.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::theme::Palette;

/// How to launch the child process inside the PTY.
///
/// Deliberately session-agnostic: the PTY layer only needs a working directory
/// and the extra CLI args. The caller (the workspace) decides whether those args
/// mean `--session-id <uuid>` (new) or `--resume <uuid>` (continue).
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    /// Directory to launch the child in. `None` falls back to Bruce's own cwd.
    pub cwd: Option<PathBuf>,
    /// Extra arguments appended after the program name.
    pub args: Vec<String>,
}

/// Shared, locked PTY writer (used by both the UI thread and the reader thread).
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// How many scrolled-off lines the emulator keeps so the user can page back
/// through the conversation. Bounded so a long session can't grow unbounded.
const SCROLLBACK_LINES: usize = 10_000;

/// Scrollback lines for install dialog PTY — smaller cap since it's short-lived.
const INSTALL_SCROLLBACK: usize = 200;

/// After this long without any PTY output we treat the child as idle (waiting
/// for input) and show its cursor; while output is still streaming we keep the
/// cursor hidden so it doesn't visibly fly around the pane mid-response.
const OUTPUT_IDLE: Duration = Duration::from_millis(200);

/// A running child process attached to a PTY plus its emulated screen.
pub struct PtySession {
    /// The emulated terminal, shared with the reader thread.
    parser: Arc<Mutex<vt100::Parser>>,
    /// Write half of the PTY: keystrokes and query replies go here.
    writer: SharedWriter,
    /// Master side, kept to resize the PTY.
    master: Box<dyn MasterPty + Send>,
    /// The child process, shared with the reader thread (for try_wait on EOF)
    /// and Drop (for kill). Was `Box<dyn Child>` before, now Arc<Mutex<>> so
    /// the reader thread can call try_wait after EOF without moving ownership.
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    /// When the reader thread last received any output, to tell a streaming
    /// response apart from an idle prompt (see [`PtySession::output_idle`]).
    last_output: Arc<Mutex<Instant>>,
    /// Set once the child has produced its first byte of output, so the loading
    /// transition knows Claude has started painting (see [`PtySession::has_output`]).
    got_output: Arc<AtomicBool>,
    /// Set by the reader thread after EOF; the install dialog polls this.
    exited: Arc<AtomicBool>,
    /// Exit code written by the reader thread after the child exits.
    exit_code: Arc<Mutex<Option<i32>>>,
    /// Reader thread handle; dropping it detaches the thread.
    _reader: JoinHandle<()>,
}

impl PtySession {
    /// Spawn [`spawn_command`] in a new PTY sized `rows`x`cols`, launched per
    /// `opts` (working directory + extra CLI args).
    pub fn new(rows: u16, cols: u16, opts: SpawnOptions) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let child_raw = pair.slave.spawn_command(spawn_command(opts))?;
        // The slave handle isn't needed once the child owns it.
        drop(pair.slave);

        let child: Arc<Mutex<Box<dyn Child + Send + Sync>>> =
            Arc::new(Mutex::new(child_raw));
        let child_for_exit = Arc::clone(&child);

        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer()?));
        let mut reader = pair.master.try_clone_reader()?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_LINES)));
        let parser_for_thread = Arc::clone(&parser);
        let writer_for_thread = Arc::clone(&writer);
        let last_output = Arc::new(Mutex::new(Instant::now()));
        let last_output_for_thread = Arc::clone(&last_output);
        let got_output = Arc::new(AtomicBool::new(false));
        let got_output_for_thread = Arc::clone(&got_output);
        let exited = Arc::new(AtomicBool::new(false));
        let exited_for_thread = Arc::clone(&exited);
        let exit_code: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        let exit_code_for_thread = Arc::clone(&exit_code);

        let reader_handle = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break, // EOF or read error: child is gone.
                    Ok(n) => {
                        let chunk = &buf[..n];
                        if let Ok(mut parser) = parser_for_thread.lock() {
                            parser.process(chunk);
                        }
                        // Mark this as live output so the UI hides the cursor
                        // while a response streams, and flag that Claude has begun
                        // painting (for the loading transition).
                        got_output_for_thread.store(true, Ordering::Relaxed);
                        if let Ok(mut t) = last_output_for_thread.lock() {
                            *t = Instant::now();
                        }
                        // Answer queries *after* processing, so the cursor
                        // position we report is current.
                        respond_to_queries(chunk, &parser_for_thread, &writer_for_thread);
                    }
                }
            }
            // Child exited (EOF). Read its exit code via try_wait; fall back to
            // -1 if the wait fails or the child was killed by a signal (no code).
            let code = child_for_exit
                .lock()
                .ok()
                .and_then(|mut c| c.try_wait().ok().flatten())
                .map(|s| s.exit_code() as i32)
                .unwrap_or(-1);
            if let Ok(mut g) = exit_code_for_thread.lock() {
                *g = Some(code);
            }
            exited_for_thread.store(true, Ordering::SeqCst);
        });

        Ok(Self {
            parser,
            writer,
            master: pair.master,
            child,
            last_output,
            got_output,
            exited,
            exit_code,
            _reader: reader_handle,
        })
    }

    /// Spawn an arbitrary command in a new PTY sized `rows`x`cols`.
    ///
    /// Unlike [`PtySession::new`], this does NOT read `BRUCE_CMD` and does NOT
    /// add CI/NO_COLOR env vars — the caller controls what runs. Used by the
    /// install dialog to give interactive installers (e.g. `npx skills add`) a
    /// real TTY so they can render ANSI menus and accept keyboard input.
    pub fn new_command(
        rows: u16,
        cols: u16,
        program: &str,
        args: &[&str],
        cwd: Option<PathBuf>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(program);
        for arg in args {
            cmd.arg(arg);
        }
        // Propagate parent env so PATH resolution works.
        for (key, value) in std::env::vars() {
            cmd.env(key, value);
        }
        cmd.env("TERM", "xterm-256color");
        let cwd = cwd.or_else(|| std::env::current_dir().ok());
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }

        let child_raw = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let child: Arc<Mutex<Box<dyn Child + Send + Sync>>> =
            Arc::new(Mutex::new(child_raw));
        let child_for_exit = Arc::clone(&child);

        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer()?));
        let mut reader = pair.master.try_clone_reader()?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, INSTALL_SCROLLBACK)));
        let parser_for_thread = Arc::clone(&parser);
        let writer_for_thread = Arc::clone(&writer);
        let last_output = Arc::new(Mutex::new(Instant::now()));
        let last_output_for_thread = Arc::clone(&last_output);
        let got_output = Arc::new(AtomicBool::new(false));
        let got_output_for_thread = Arc::clone(&got_output);
        let exited = Arc::new(AtomicBool::new(false));
        let exited_for_thread = Arc::clone(&exited);
        let exit_code: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        let exit_code_for_thread = Arc::clone(&exit_code);

        let reader_handle = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        if let Ok(mut parser) = parser_for_thread.lock() {
                            parser.process(chunk);
                        }
                        got_output_for_thread.store(true, Ordering::Relaxed);
                        if let Ok(mut t) = last_output_for_thread.lock() {
                            *t = Instant::now();
                        }
                        respond_to_queries(chunk, &parser_for_thread, &writer_for_thread);
                    }
                }
            }
            let code = child_for_exit
                .lock()
                .ok()
                .and_then(|mut c| c.try_wait().ok().flatten())
                .map(|s| s.exit_code() as i32)
                .unwrap_or(-1);
            if let Ok(mut g) = exit_code_for_thread.lock() {
                *g = Some(code);
            }
            exited_for_thread.store(true, Ordering::SeqCst);
        });

        Ok(Self {
            parser,
            writer,
            master: pair.master,
            child,
            last_output,
            got_output,
            exited,
            exit_code,
            _reader: reader_handle,
        })
    }

    /// Lock the emulated terminal for reading (returns `None` if poisoned).
    pub fn lock_parser(&self) -> Option<MutexGuard<'_, vt100::Parser>> {
        self.parser.lock().ok()
    }

    /// Forward raw input bytes to the child.
    pub fn send(&self, data: &[u8]) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(data);
            let _ = writer.flush();
        }
    }

    /// Resize both the PTY and the emulator to `rows`x`cols`.
    ///
    /// vt100's `Parser` has no in-place resize, so we swap in a fresh one of the
    /// new size; the child repaints after the PTY's resize (SIGWINCH).
    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut parser) = self.parser.lock() {
            *parser = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
        }
    }

    /// Scroll the emulated screen back by `lines` (toward older output). The
    /// offset is clamped to the available scrollback by vt100, so over-scrolling
    /// simply stops at the top.
    pub fn scroll_up(&self, lines: usize) {
        if let Ok(mut parser) = self.parser.lock() {
            let offset = parser.screen().scrollback();
            parser.screen_mut().set_scrollback(offset + lines);
        }
    }

    /// Scroll the emulated screen forward by `lines` (toward newer output),
    /// stopping at the live bottom (offset 0).
    pub fn scroll_down(&self, lines: usize) {
        if let Ok(mut parser) = self.parser.lock() {
            let offset = parser.screen().scrollback();
            parser.screen_mut().set_scrollback(offset.saturating_sub(lines));
        }
    }

    /// Jump back to the live bottom of the output (cancel any scrollback).
    pub fn scroll_to_bottom(&self) {
        if let Ok(mut parser) = self.parser.lock() {
            parser.screen_mut().set_scrollback(0);
        }
    }

    /// True when the child hasn't produced output for at least `dur` — i.e. its
    /// output has gone quiet. The threshold is the caller's: a short one tells a
    /// streaming response apart from an idle prompt; a longer one tells "still
    /// booting" apart from "finished its initial paint".
    pub fn output_quiet(&self, dur: Duration) -> bool {
        self.last_output
            .lock()
            .map(|t| t.elapsed() >= dur)
            .unwrap_or(true)
    }

    /// True when the child hasn't produced output for at least [`OUTPUT_IDLE`],
    /// i.e. it's waiting for input rather than streaming a response. Drives
    /// whether the UI shows the child's cursor.
    pub fn output_idle(&self) -> bool {
        self.output_quiet(OUTPUT_IDLE)
    }

    /// True once the child has produced any output — i.e. Claude has started
    /// painting. Used to end the loading transition as soon as it's ready.
    pub fn has_output(&self) -> bool {
        self.got_output.load(Ordering::Relaxed)
    }

    /// Returns the child's exit code once it has exited, `None` if still running.
    ///
    /// The reader thread sets this after EOF; the install dialog polls it each tick
    /// to detect command completion without blocking the UI.
    pub fn exit_status(&self) -> Option<i32> {
        if self.exited.load(Ordering::SeqCst) {
            self.exit_code.lock().ok().and_then(|g| *g)
        } else {
            None
        }
    }

    /// Kill the child process best-effort (does not block).
    ///
    /// Used by the Esc handler in the install dialog to cancel a running install.
    pub fn kill(&self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Best-effort: stop the child so it doesn't outlive the workspace.
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
    }
}

/// Reply to the capability/status queries a TUI sends at startup. ConPTY (and
/// real terminals) route these to the host and feed our reply back to the child.
fn respond_to_queries(chunk: &[u8], parser: &Arc<Mutex<vt100::Parser>>, writer: &SharedWriter) {
    let contains = |needle: &[u8]| chunk.windows(needle.len()).any(|w| w == needle);

    let mut reply: Vec<u8> = Vec::new();

    // Primary Device Attributes (ESC[c / ESC[0c): claim to be a VT102.
    if contains(b"\x1b[c") || contains(b"\x1b[0c") {
        reply.extend_from_slice(b"\x1b[?1;2c");
    }
    // Secondary Device Attributes (ESC[>c / ESC[>0c): xterm-like response.
    if contains(b"\x1b[>c") || contains(b"\x1b[>0c") {
        reply.extend_from_slice(b"\x1b[>0;276;0c");
    }
    // Device Status Report — terminal OK (ESC[5n -> ESC[0n).
    if contains(b"\x1b[5n") {
        reply.extend_from_slice(b"\x1b[0n");
    }
    // Device Status Report — cursor position (ESC[6n -> ESC[row;colR).
    if contains(b"\x1b[6n") {
        let (row, col) = parser
            .lock()
            .ok()
            .map(|p| p.screen().cursor_position())
            .unwrap_or((0, 0));
        reply.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
    }

    if !reply.is_empty() {
        if let Ok(mut writer) = writer.lock() {
            let _ = writer.write_all(&reply);
            let _ = writer.flush();
        }
    }
}

/// Map a vt100 colour to a ratatui colour. The terminal *default* colour
/// resolves to `fallback` (the theme's fg or bg) so embedded output shares the
/// workspace background instead of falling back to the host terminal.
pub fn conv_color(color: vt100::Color, fallback: Color) -> Color {
    match color {
        vt100::Color::Default => fallback,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Convert the emulated screen into one styled line per row.
///
/// Cells that use the terminal *default* colour resolve to the active theme's
/// `fg`/`bg` (not `Color::Reset`), so the embedded child blends into the rest
/// of the workspace instead of showing the host terminal's own background.
/// Colours the child picks explicitly (diff green/red, accents) pass through
/// untouched — they carry meaning we must not override.
pub fn pty_screen_lines<'a>(screen: &vt100::Screen, pal: &Palette) -> Vec<Line<'a>> {
    let (rows, cols) = screen.size();
    let mut lines = Vec::with_capacity(rows as usize);
    for row in 0..rows {
        let mut spans = Vec::with_capacity(cols as usize);
        for col in 0..cols {
            let (text, style) = match screen.cell(row, col) {
                Some(cell) => {
                    let raw = cell.contents();
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

/// The command run inside the PTY: Claude Code, launched per `opts`.
///
/// `CreateProcess` / exec resolve `claude` via PATH (so this needs the parent
/// environment, propagated below). Override with the `BRUCE_CMD` env var to run
/// something else, e.g. `BRUCE_CMD=pwsh` for a plain shell. If `claude` is only
/// installed as a `.cmd`/`.ps1` shim (not an `.exe`) on some Windows setups,
/// point BRUCE_CMD at the shim's full path.
///
/// `opts.args` (e.g. `--session-id <uuid>` or `--resume <uuid>`) are appended so
/// the spawned conversation can be created with, or resumed from, a known id.
/// `opts.cwd` is the project directory; Claude writes its transcript keyed off
/// this path, so resuming must use the same cwd it was created with.
fn spawn_command(opts: SpawnOptions) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(program());
    for arg in &opts.args {
        cmd.arg(arg);
    }

    // portable-pty doesn't inherit the parent environment, so propagate it —
    // otherwise PATH is empty and `claude` can't be resolved.
    for (key, value) in std::env::vars() {
        cmd.env(key, value);
    }
    // Advertise a capable terminal so the child emits colour sequences.
    cmd.env("TERM", "xterm-256color");

    // Launch in the session's project directory; fall back to Bruce's own cwd.
    let cwd = opts
        .cwd
        .or_else(|| std::env::current_dir().ok());
    if let Some(cwd) = cwd {
        cmd.cwd(cwd);
    }
    cmd
}

/// The program Bruce spawns in the Claude pane: `claude`, or the `BRUCE_CMD`
/// override when set (e.g. `pwsh` for a plain shell).
pub fn program() -> String {
    match std::env::var("BRUCE_CMD") {
        Ok(p) if !p.trim().is_empty() => p,
        _ => "claude".to_string(),
    }
}

/// True when Bruce will spawn the default `claude` CLI but it isn't on `PATH`.
///
/// Used for a friendly startup warning. Returns false when `BRUCE_CMD` overrides
/// the program (the user picked something else on purpose) so we don't nag.
pub fn claude_missing() -> bool {
    program() == "claude" && !on_path("claude")
}

/// Best-effort `which`: is an executable named `program` reachable via `PATH`?
///
/// Pure lookup (no process spawned). On Windows it also tries the `PATHEXT`
/// extensions (so a `claude.cmd`/`claude.exe` shim is found); on Unix it checks
/// for the bare name.
fn on_path(program: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string())
            .split(';')
            .map(|s| s.to_string())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&paths) {
        if dir.join(program).is_file() {
            return true;
        }
        for ext in &exts {
            if !ext.is_empty() && dir.join(format!("{program}{ext}")).is_file() {
                return true;
            }
        }
    }
    false
}
