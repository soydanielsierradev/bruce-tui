# Homebrew formula for Bruce.
#
# This file is the source of truth; to publish it, copy it into a tap repo named
# `homebrew-bruce` (so users run `brew install soydanielsierradev/bruce/bruce`).
# Bump `version` and the three `sha256` values on every release.
class Bruce < Formula
  desc "Terminal workspace for Claude Code"
  homepage "https://github.com/soydanielsierradev/bruce-tui"
  version "0.9.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/soydanielsierradev/bruce-tui/releases/download/v0.9.0/bruce-aarch64-apple-darwin.tar.gz"
      sha256 "3d98427e0050d9ae227186feb1d4e164730454f65ccaaf05ec3cdb697a413abe"
    end
    on_intel do
      url "https://github.com/soydanielsierradev/bruce-tui/releases/download/v0.9.0/bruce-x86_64-apple-darwin.tar.gz"
      sha256 "4af70cad8378524ef3bc57012879a800b72577eb601a712869f736ebe0a7f184"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/soydanielsierradev/bruce-tui/releases/download/v0.9.0/bruce-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "cba858045d6b3409d5aabc4ff63bee4290ee664113a7ea2f1a871c8f78e44e00"
    end
  end

  def install
    bin.install "bruce"
  end

  def caveats
    <<~EOS
      Bruce runs the Claude CLI inside its workspace. Install Claude Code and make
      sure `claude` is on your PATH: https://docs.claude.com/claude-code
    EOS
  end

  test do
    assert_match "bruce", shell_output("#{bin}/bruce --version")
  end
end
