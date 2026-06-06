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
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Shared, locked PTY writer (used by both the UI thread and the reader thread).
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// A running child process attached to a PTY plus its emulated screen.
pub struct PtySession {
    /// The emulated terminal, shared with the reader thread.
    parser: Arc<Mutex<vt100::Parser>>,
    /// Write half of the PTY: keystrokes and query replies go here.
    writer: SharedWriter,
    /// Master side, kept to resize the PTY.
    master: Box<dyn MasterPty + Send>,
    /// The child process, killed on drop.
    child: Box<dyn Child + Send + Sync>,
    /// Reader thread handle; dropping it detaches the thread.
    _reader: JoinHandle<()>,
}

impl PtySession {
    /// Spawn [`spawn_command`] in a new PTY sized `rows`x`cols`.
    pub fn new(rows: u16, cols: u16) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let child = pair.slave.spawn_command(spawn_command())?;
        // The slave handle isn't needed once the child owns it.
        drop(pair.slave);

        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer()?));
        let mut reader = pair.master.try_clone_reader()?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let parser_for_thread = Arc::clone(&parser);
        let writer_for_thread = Arc::clone(&writer);

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
                        // Answer queries *after* processing, so the cursor
                        // position we report is current.
                        respond_to_queries(chunk, &parser_for_thread, &writer_for_thread);
                    }
                }
            }
        });

        Ok(Self {
            parser,
            writer,
            master: pair.master,
            child,
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
            *parser = vt100::Parser::new(rows, cols, 0);
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Best-effort: stop the child so it doesn't outlive the workspace.
        let _ = self.child.kill();
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

/// The command run inside the PTY: Claude Code.
///
/// `CreateProcess` / exec resolve `claude` via PATH (so this needs the parent
/// environment, propagated below). Override with the `BRUCE_CMD` env var to run
/// something else, e.g. `BRUCE_CMD=pwsh` for a plain shell. If `claude` is only
/// installed as a `.cmd`/`.ps1` shim (not an `.exe`) on some Windows setups,
/// point BRUCE_CMD at the shim's full path.
fn spawn_command() -> CommandBuilder {
    let program = match std::env::var("BRUCE_CMD") {
        Ok(p) if !p.trim().is_empty() => p,
        _ => "claude".to_string(),
    };
    let mut cmd = CommandBuilder::new(program);

    // portable-pty doesn't inherit the parent environment, so propagate it —
    // otherwise PATH is empty and `claude` can't be resolved.
    for (key, value) in std::env::vars() {
        cmd.env(key, value);
    }
    // Advertise a capable terminal so the child emits colour sequences.
    cmd.env("TERM", "xterm-256color");
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    cmd
}
