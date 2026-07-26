# AGENTS.md

## Project Shape

- This repo is the standalone Rust CLI/TUI binary `sessiongator`; generic terminal/tooling helpers belong in the `gator` dependency, not here.
- `src/main.rs` dispatches `--list`, interactive TUI, and `convert`; `src/session.rs` owns the TUI event loop and shell selection lines.
- `src/sources/` adapters are read-only summary/transcript readers for Claude Code, opencode, Codex, and Copilot. Add a source in `sources_from_env()` and `Tool` when adding another AI tool.
- `src/native_import.rs` owns native conversion and uses a richer `NativeSession` model; do not overload `SessionSource` for conversion fidelity.

## Commands

- Full pre-merge check: `cargo fmt -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`.
- After code changes, also build the release binary: `cargo build --release`.
- Focus native import tests with fixture output: `cargo test native_import -- --nocapture`.
- Probe latest-tool native import compatibility only after a release build: `scripts/probe-native-import-latest.sh`.
- Local smoke commands: `cargo run -- --list` and `cargo run -- convert --from claude --to opencode --id <session-id> --dry-run --plan-json`.
- Releases are triggered by changing the package version in `Cargo.toml` on `main`; update the matching root package version in `Cargo.lock`. A manual release workflow run only retries the current release and Homebrew sync.

## Session Stores

- Claude Code reads `$CLAUDE_CONFIG_DIR/projects`, else `~/.claude/projects`; the project folder encoding is lossy, so always read `cwd` from JSONL content.
- opencode reads SQLite from `$OPENCODE_DB`, else `$XDG_DATA_HOME/opencode/opencode.db`, else `~/.local/share/opencode/opencode.db`; browser reads must stay read-only because live opencode may hold WAL state.
- Codex listing reads `$CODEX_HOME` and `~/.codex` session JSONL trees; native import with `--source-store` or `--target-store` treats the path as one Codex root.
- Copilot reads `$COPILOT_HOME`, else `~/.copilot`; list metadata comes from `session-store.db`, while transcripts prefer `session-state/<id>/events.jsonl` before DB turns.

## Native Import

- Supported conversion versions are compiled from `docs/specs/native-session-import-versions.toml`; update that manifest and rebuild when changing version gates.
- Unknown source/target versions fail closed unless `--allow-unsupported-version` is passed.
- Write paths differ: Claude uses atomic JSONL writes, opencode uses a SQLite `BEGIN IMMEDIATE` transaction and backup by default, Codex writes rollout JSONL plus `session_index.jsonl`, Copilot writes `session-state/<id>/` plus `session-store.db` projections.
- Keep conversion tests contract-focused: plan/report shape, mapped/dropped/synthesized counts, readback, and fixture round trips rather than byte-for-byte database layout.

## UI And Wrapper Contracts

- Theme modes and palettes match navgator: `auto` follows macOS appearance, explicit `light`/`dark` come from `--theme` then `$SESSIONGATOR_THEME`, and non-macOS `auto` falls back to light.
- Search is substring-based, not `gator::fuzzy_match`; `SearchMode::All` unions transcript hits with title/path/model hits.
- The TUI emits tab-separated selection lines consumed by `scripts/sessiongator.zsh`: `resume`, `resume-here`, `path`, and `convert`. Update the wrapper and `session.rs` tests together if the contract changes.
- `Ctrl+T` should keep producing a dry-run `sessiongator convert ... --plan-json` command for the selected session.
