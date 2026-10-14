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

Open the picker with a search already typed:

```sh
sessiongator -q 'rate limiter'
```

## CLI

Every picker action also works without the TUI:

```sh
sessiongator list                          # all sessions, newest first (alias: ls)
sessiongator list rate limiter --here      # title/path/model match, current project only
sessiongator search 'connection pool' -n 5 # also searches transcripts, like the picker (alias: s)
sessiongator show e4255e84 --grep migration
sessiongator resume 'rate limiter'         # runs the tool in the session directory (alias: r)
sessiongator resume e4255e84 --here --print
sessiongator path e4255e84
sessiongator delete e4255e84               # to the sessiongator trash (alias: rm)
sessiongator undo                          # revert the newest convert or delete
sessiongator history
```

A session reference is an id, a unique id prefix, `tool:id`, or text that matches one session title, path, or model. Without a reference, `show`, `resume`, and `path` use the newest session in the current project. A reference that matches several sessions fails and lists the candidates.

Filters for `list` and `search`: `-t/--tool` (repeatable), `-p/--here`, `-d/--dir`, `-s/--sort updated|created|msgs|path`, `-n/--limit`, and `--json` for one JSON object per line.

Shell completions: `sessiongator completions zsh` (also `bash`, `fish`, `elvish`, `powershell`).

The interactive picker defaults to `auto`, following the macOS appearance like navgator. Override it for terminals with a different background:

```sh
sessiongator --theme dark
SESSIONGATOR_THEME=light sessiongator
```

Supported themes are `auto`, `light`, and `dark`. On non-macOS systems, `auto` currently uses the light palette.

## Config

Sessiongator reads `~/.config/sessiongator/config.toml` (or `$SESSIONGATOR_CONFIG`), creating a starter file on first run. Print the JSON Schema with `sessiongator config-schema`.

```toml
[ui]
theme = "auto"

[keybindings.global]
"ctrl+s" = "cycle-sort"

[keybindings.list]
"ctrl+k" = "move-up"
```

Theme precedence is `--theme`, then `SESSIONGATOR_THEME`, then the config file, then `auto`.

Each key chord maps to an action, and `"none"` disables a default so the key reaches the search input instead. Contexts are `global`, `list`, and `transcript`; a binding in `list` or `transcript` wins over the same chord in `global`.

Actions: `cancel`, `resume`, `resume-here`, `show-path`, `convert`, `copy-id`, `toggle-search`, `cycle-sort`, `move-up`, `move-down`, `move-left`, `move-right`, `move-home`, `move-end`, `page-up`, `page-down`, `scroll-top`, `scroll-bottom`.

The keys footer always reflects the active bindings.

Dry-run a native conversion. The source tool comes from the session, and the target defaults to opencode for Claude sessions, else claude:

```sh
sessiongator convert e4255e84 --dry-run
sessiongator convert 'rate limiter' --to codex --mark-source    # note in the original: where it went, how to resume
sessiongator convert 'rate limiter' --to codex --delete-source  # original to the trash
sessiongator convert --from claude --to opencode --id <session-id> --dry-run --plan-json
```

`sessiongator undo` removes the note or restores the original; `undo --target` also removes the converted copy. opencode targets must be OpenCode 2 databases.

`sessiongator compat` shows how well each installed tool version is tested. `verified` versions passed a full pass with real clients and models, and versions between two verified ones (`bracketed`) count as verified. `probe-passed` (daily CI, no model calls) and newer versions of a known major version convert with a warning. Other target versions stop with exit code 3 unless you pass `--allow-unsupported-version`. Pass `--backup` to copy the opencode database before a write.

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
