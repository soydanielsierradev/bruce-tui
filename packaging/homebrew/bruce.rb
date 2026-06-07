# Homebrew formula for Bruce.
#
# This file is the source of truth; to publish it, copy it into a tap repo named
# `homebrew-bruce` (so users run `brew install soydanielsierradev/bruce/bruce`).
# Bump `version` and the three `sha256` values on every release.
class Bruce < Formula
  desc "Terminal workspace for Claude Code"
  homepage "https://github.com/soydanielsierradev/bruce-tui"
  version "0.8.0"
  # TODO: add a LICENSE file to the repo, then declare it here, e.g. license "MIT".

  on_macos do
    on_arm do
      url "https://github.com/soydanielsierradev/bruce-tui/releases/download/v0.8.0/bruce-aarch64-apple-darwin.tar.gz"
      sha256 "150f7dcdbf87b7d91cd373029e06d5c050b3c6d34db4f336425caaf0820c3e56"
    end
    on_intel do
      url "https://github.com/soydanielsierradev/bruce-tui/releases/download/v0.8.0/bruce-x86_64-apple-darwin.tar.gz"
      sha256 "3ce2126802b4291631657f2783c426238929528f1d8f398bd040dfc6c5791fe6"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/soydanielsierradev/bruce-tui/releases/download/v0.8.0/bruce-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "c6d3a9ce2baa97cadb72ec474b073c98418f3792bcfa95d244797688f18161e5"
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
