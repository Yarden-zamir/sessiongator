# sessiongator

Rust TUI browser for Claude Code and opencode sessions.

## Install

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

## Zsh Widget

```sh
source "$(brew --prefix)/share/sessiongator/sessiongator.zsh"
bindkey '^S' ai-sessions
```

The wrapper writes selections through `GATOR_OUTPUT`; otherwise `sessiongator` prints selections to stdout.

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
