# sessiongator

🐊 Rust TUI browser for Claude Code, opencode, Codex, and Copilot sessions.

Nerd Font is recommended for all gator-family CLIs so built-in icons render correctly.

## Install

Homebrew tap: https://github.com/Yarden-zamir/homebrew-tap

```sh
brew install yarden-zamir/tap/sessiongator
```

## Run

```sh
sessiongator
```

List sessions without the TUI:

```sh
sessiongator --list
```

Dry-run a native conversion by session id:

```sh
sessiongator convert --from claude --to opencode --id <session-id> --dry-run --plan-json
```

The converter checks known supported tool versions before writing. Live opencode writes create a database backup by default; Claude writes use an atomic JSONL file write.

## Zsh Widget

Choose one setup path.

Homebrew manages both the binary and wrapper:

```zsh
brew install yarden-zamir/tap/sessiongator
source "$(brew --prefix sessiongator)/share/sessiongator/sessiongator.zsh"
bindkey '^S' ai-sessions
```

Alternatively, [gh-source](https://github.com/Yarden-zamir/gh-source) clones the repository, builds a missing local release binary, and sources the local wrapper:

```zsh
gh_source Yarden-zamir/sessiongator/scripts/sessiongator.zsh \
  --skip-build-if-present target/release/sessiongator \
  --build cargo build --release
bindkey '^S' ai-sessions
```

The wrapper prefers `$SESSIONGATOR_BIN`, then adjacent local release and debug builds, then `sessiongator` on `PATH`.
The wrapper writes selections through `GATOR_OUTPUT`; otherwise `sessiongator` prints selections to stdout. Press `Ctrl+Enter` to resume without changing the current directory.
Inside the TUI, press `Ctrl+T` on a selected session to place a dry-run `sessiongator convert ...` command in your shell prompt.

## Build

```sh
cargo build --release
```

## Check

```sh
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## License

MIT
