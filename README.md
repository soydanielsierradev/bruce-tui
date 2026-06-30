<div align="center">
  <img src="img/logo.png" alt="Bruce" width="420">
  <br><br>

  ![Rust](https://img.shields.io/badge/Rust-CE412B?logo=rust&logoColor=white)
  ![ratatui](https://img.shields.io/badge/ratatui-TUI-1f6feb)
  ![crossterm](https://img.shields.io/badge/crossterm-terminal-2ea043)
  ![git2](https://img.shields.io/badge/git2-libgit2-F05133?logo=git&logoColor=white)
  ![portable--pty](https://img.shields.io/badge/portable--pty-PTY-8957e5)
  ![License: MIT](https://img.shields.io/badge/License-MIT-green)

</div>

A terminal workspace for [Claude Code](https://docs.claude.com/claude-code): a
four-pane TUI with **Git** status on the left, **Claude** running live in the
center, a **File Manager** on the right, and a **Terminal** across the bottom. It
keeps a session per project — open Bruce in a directory and pick up exactly where
you left off, with the full conversation restored.

```
┌─ git · main ──┐┌─ Claude Code ─────────┐┌─ Files ───┐
│ BRANCHES      ││                       ││ 📁 src    │
│ COMMITS       ││   (claude runs here)  ││ 📄 README │
│ WORKING TREE  ││                       ││ 🦀 main.rs│
└───────────────┘└───────────────────────┘└───────────┘
┌─ Terminal ───────────────────────────────────────────┐
│ $ run shell commands here                             │
└───────────────────────────────────────────────────────┘
```

The File Manager opens files in **VS Code** (`code`) by default — set
`$BRUCE_EDITOR` to use another editor. File icons are emoji by default and switch
to Nerd Font glyphs under **Settings → File icons** if your terminal uses a Nerd
Font.

## Requirements

Bruce launches the Claude CLI inside its workspace, so **Claude Code must be
installed and on your `PATH`** first:

```sh
claude --version   # should print a version
```

Install it from <https://docs.claude.com/claude-code> if that fails.

## Install

Pick your platform and run the command. Remember Bruce needs the `claude` CLI on
your `PATH` (see [Requirements](#requirements)).

### macOS

With [Homebrew](https://brew.sh) (works on Apple Silicon and Intel):

```sh
brew install soydanielsierradev/bruce/bruce
```

Or with the install script:

```sh
curl -fsSL https://raw.githubusercontent.com/soydanielsierradev/bruce-tui/main/install.sh | sh
```

### Linux

```sh
curl -fsSL https://raw.githubusercontent.com/soydanielsierradev/bruce-tui/main/install.sh | sh
```

The installer drops the binary in `~/.local/bin` (override with `BRUCE_BIN_DIR`)
and tells you if that directory isn't on your `PATH`.

### Arch Linux

The Linux install script above works on Arch (it ships a glibc binary), or
build from source with `cargo install --git`.

### Windows

In PowerShell:

```powershell
irm https://raw.githubusercontent.com/soydanielsierradev/bruce-tui/main/install.ps1 | iex
```

This downloads the latest release, installs it to
`%LOCALAPPDATA%\Programs\bruce` (override with `$env:BRUCE_BIN_DIR`) and adds it
to your user `PATH`. Open a new terminal afterward so the change takes effect.

### Any platform (from source)

With a [Rust toolchain](https://rustup.rs) installed:

```sh
cargo install --git https://github.com/soydanielsierradev/bruce-tui
```

## Usage

Run Bruce from inside any project directory:

```sh
bruce          # opens the workspace (same as `bruce tui`)
```

On the welcome screen you can open, create, rename, duplicate and delete
sessions, tweak the look under **Settings** (theme, file icons, border style,
layout, title and footer bars), and find the repo and this keybindings list under
**Documentation** — your choices are remembered between runs.

### Keys

**Welcome screen**

| Key | Action |
|-----|--------|
| `Tab` / `Shift+Tab` | Move focus between blocks |
| `↑` / `↓` | Select a row |
| `Enter` | Run the selected option |
| `N` | Jump to “New session” |
| Click the author’s name | Open the author’s GitHub |
| `q` / `Esc` | Quit Bruce |

**Session picker** (open / rename / duplicate / delete)

| Key | Action |
|-----|--------|
| type | Filter the session list |
| `↑` / `↓` | Move the selection |
| `Enter` | Confirm the action |
| `Y` / `N` | Confirm / cancel a delete |
| `Esc` | Close the picker |

**Workspace — any pane**

| Key | Action |
|-----|--------|
| `Ctrl+1` / `Ctrl+2` / `Ctrl+3` / `Ctrl+4` | Focus Git / Claude / Files / Terminal |
| `Ctrl+F` | Fuzzy file search → open in your editor |
| `Ctrl+T` | Toggle the Terminal pane |
| `Tab` / `Shift+Tab` | Cycle panes |
| `Esc` | Back to the welcome screen |
| `q` / `Q` | Quit (when a non-typing pane is focused) |

**Workspace — File Manager pane**

| Key | Action |
|-----|--------|
| `↑` / `↓` | Move the selection |
| `Enter` | Open a file in your editor · enter a folder |
| `←` / `Backspace` | Go up a directory |
| `.` | Toggle hidden files |

**Workspace — Claude / Terminal pane**

| Key | Action |
|-----|--------|
| type | Send keystrokes to the focused process |
| `Shift+PageUp` / `Shift+PageDown` | Scroll Claude’s history |
| `Ctrl+b` then `b` | Back to the welcome screen |
| `Ctrl+b` then `Tab` | Switch pane |
| `Ctrl+b` then `g` / `t` | Toggle the Git / Terminal pane |
| `Ctrl+b` then `q` | Quit Bruce |

**File search overlay** (`Ctrl+F`)

| Key | Action |
|-----|--------|
| type | Filter files |
| `↑` / `↓` | Move the selection |
| `Enter` | Open the selected file |
| `Esc` | Close the overlay |

Anything else typed while the Claude or Terminal pane is focused goes straight to
that process. The same list is available in-app under **Documentation →
Keybindings**.

## Environment variables

| Var | What it does |
|-----|--------------|
| `BRUCE_CMD` | Program Bruce spawns in the Claude pane. Defaults to `claude`. Useful when the CLI lives under a different name on your PATH (`claude-code`, a wrapper script, a full path to a shim, or even `pwsh` for a plain shell to dogfood the pane). Bruce skips the “claude is missing” startup warning whenever this is set, on the assumption you picked something else on purpose. |
| `BRUCE_EDITOR` | Editor the File Manager hands off to on `Enter`. Falls back to `code` / `code-insiders` if not set. |

## Build from source

```sh
git clone https://github.com/soydanielsierradev/bruce-tui
cd bruce-tui
cargo install --path .
```

## License

[MIT](LICENSE) © Daniel Sierra
