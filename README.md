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

| Platform | Command |
|----------|---------|
| Linux / macOS | `curl -fsSL https://raw.githubusercontent.com/soydanielsierradev/bruce-tui/main/install.sh \| sh` |
| Any (with Rust) | `cargo install --git https://github.com/soydanielsierradev/bruce-tui` |
| Windows | Download the `.zip` from [Releases](https://github.com/soydanielsierradev/bruce-tui/releases) and put `bruce.exe` on your `PATH` |

> Homebrew and AUR packages are planned once the first release is published.

The `curl` installer drops the binary in `~/.local/bin` (override with
`BRUCE_BIN_DIR`). If that directory isn't on your `PATH`, the script tells you.

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

See [LICENSE](LICENSE) if present; otherwise all rights reserved by the author.
