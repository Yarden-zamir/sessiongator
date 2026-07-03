# typed: false
# frozen_string_literal: true

class Sessiongator < Formula
  desc "Rust TUI browser for Claude Code and opencode sessions"
  homepage "https://github.com/Yarden-zamir/sessiongator"
  url "{{URL}}"
  sha256 "{{SHA256}}"
  license "MIT"
  head "https://github.com/Yarden-zamir/sessiongator.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
    pkgshare.install "scripts/sessiongator.zsh"
  end

  def caveats
    <<~EOS
      To activate the zsh session widget, add this to your .zshrc:
        source #{opt_pkgshare}/sessiongator.zsh

      Then bind it, for example:
        bindkey '^S' ai-sessions
    EOS
  end

  test do
    system "#{bin}/sessiongator", "--list"
  end
end
