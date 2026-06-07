#!/bin/sh
# Bruce installer for Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/soydanielsierradev/bruce-tui/main/install.sh | sh
#
# Downloads the latest release binary for your platform and installs it to
# ~/.local/bin (override with BRUCE_BIN_DIR). Windows users: grab the .zip from
# the Releases page, or use `cargo install --git`.
set -eu

REPO="soydanielsierradev/bruce-tui"
BIN_DIR="${BRUCE_BIN_DIR:-$HOME/.local/bin}"

# --- detect platform -> Rust target triple (must match release.yml) ----------
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Linux)
    case "$arch" in
      x86_64 | amd64) target="x86_64-unknown-linux-gnu" ;;
      *) echo "error: unsupported Linux arch '$arch' (only x86_64 has prebuilt binaries; try: cargo install --git https://github.com/$REPO)" >&2; exit 1 ;;
    esac ;;
  Darwin)
    case "$arch" in
      x86_64) target="x86_64-apple-darwin" ;;
      arm64 | aarch64) target="aarch64-apple-darwin" ;;
      *) echo "error: unsupported macOS arch '$arch'" >&2; exit 1 ;;
    esac ;;
  *)
    echo "error: unsupported OS '$os'. On Windows, download the .zip from https://github.com/$REPO/releases or run 'cargo install --git https://github.com/$REPO'." >&2
    exit 1 ;;
esac

asset="bruce-${target}.tar.gz"
url="https://github.com/$REPO/releases/latest/download/$asset"

# --- download + unpack -------------------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $asset ..."
if ! curl -fsSL "$url" -o "$tmp/$asset"; then
  echo "error: download failed from $url" >&2
  echo "       (has a release been published yet? check https://github.com/$REPO/releases)" >&2
  exit 1
fi

tar -xzf "$tmp/$asset" -C "$tmp"

# --- install -----------------------------------------------------------------
mkdir -p "$BIN_DIR"
install -m 0755 "$tmp/bruce" "$BIN_DIR/bruce" 2>/dev/null || {
  cp "$tmp/bruce" "$BIN_DIR/bruce"
  chmod 0755 "$BIN_DIR/bruce"
}
echo "Installed bruce to $BIN_DIR/bruce"

# --- post-install checks -----------------------------------------------------
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "note: $BIN_DIR is not on your PATH. Add it, e.g.:"
     echo "      export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

if ! command -v claude >/dev/null 2>&1; then
  echo "note: Claude Code ('claude') was not found on your PATH."
  echo "      Bruce runs the Claude CLI inside its workspace, so install it first:"
  echo "      https://docs.claude.com/claude-code"
fi

echo "Done. Run 'bruce' in any project directory to start."
