# sessiongator

Rust TUI browser for Claude Code and opencode sessions.

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

```sh
source "$(brew --prefix)/share/sessiongator/sessiongator.zsh"
bindkey '^S' ai-sessions
```

The wrapper writes selections through `GATOR_OUTPUT`; otherwise `sessiongator` prints selections to stdout.
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
