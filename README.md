# Bruce — a terminal workspace for Claude Code

Bruce is a TUI that wraps [Claude Code](https://docs.claude.com/claude-code) in a
three-pane workspace: **Git** status on the left, **Claude** running live in the
center, and token **Metrics** on the right. It keeps a session per project — open
Bruce in a directory, pick up exactly where you left off, with the full
conversation restored.

```
┌─ git · main ──┐┌─ Claude Code ─────────┐┌─ Metrics ─┐
│ BRANCHES      ││                       ││ tokens    │
│ COMMITS       ││   (claude runs here)  ││ context % │
│ WORKING TREE  ││                       ││ cost      │
└───────────────┘└───────────────────────┘└───────────┘
```

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

```sh
curl -fsSL https://raw.githubusercontent.com/soydanielsierradev/bruce-tui/main/install.sh | sh
```

Works on both Apple Silicon and Intel. Homebrew (once the tap is published):

```sh
brew install soydanielsierradev/bruce/bruce
```

### Linux

```sh
curl -fsSL https://raw.githubusercontent.com/soydanielsierradev/bruce-tui/main/install.sh | sh
```

The installer drops the binary in `~/.local/bin` (override with `BRUCE_BIN_DIR`)
and tells you if that directory isn't on your `PATH`.

### Arch Linux

Once published to the AUR:

```sh
paru -S bruce-bin     # or: yay -S bruce-bin
```

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

On the welcome screen you can create, resume, rename, duplicate and delete
sessions, and pick a theme — your choices are remembered between runs.

### Keys

| Key | Action |
|-----|--------|
| `Tab` | Switch focused pane |
| `Ctrl+b` then `g` / `m` | Toggle the Git / Metrics pane |
| `Ctrl+b` then `b` | Back to the welcome screen |
| `Ctrl+b` then `q` | Quit |

Everything else typed while the Claude pane is focused goes straight to Claude.

## Build from source

```sh
git clone https://github.com/soydanielsierradev/bruce-tui
cd bruce-tui
cargo install --path .
```

## License

[MIT](LICENSE) © Daniel Sierra
