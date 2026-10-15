# AGENTS.md

## Project Shape

- This repo is the standalone Rust CLI/TUI binary `sessiongator`; generic terminal/tooling helpers belong in the `gator` dependency, not here.
- `src/main.rs` opens the TUI when no command is given; `src/cli.rs` owns the clap commands (`list`, `search`, `show`, `resume`, `path`, `convert`, `delete`, `undo`, `history`, `compat`, `completions`); `src/session.rs` owns the TUI event loop and shell selection lines.
- CLI commands take a session reference resolved by `search::resolve_session`: exact `tool:id` or id, else the single match among id prefixes and title/path/model text. Ambiguous references fail with a candidate list.
- `src/stores/<tool>.rs` holds everything about one tool's store: list, read into `native::NativeSession`, write, `footprint`, and `append_note`. The TUI transcript is `native::turns` of `read`. Add a store in `stores::store_for` and `Tool` when adding another AI tool.
- `src/convert/` owns conversion (`mod.rs`) and the version policy (`versions.rs`); `src/trash.rs` moves a footprint to the trash and back; `src/history.rs` records operations for `undo`.

## Commands

- Full pre-merge check: `cargo fmt -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`.
- After code changes, also build the release binary: `cargo build --release`.
- Focus conversion tests: `cargo test -- convert:: stores:: trash:: history::`.
- Probe latest-tool native import compatibility only after a release build: `scripts/probe-native-import-latest.sh` (no model calls; records `probe-passed`).
- The full pass `scripts/full-pass-native-import.sh` resumes imported sessions with real clients and model keys and records `verified`; it runs as a scheduled T3 task on the VPS.
- Tests and scripts that write must set `SESSIONGATOR_STATE_DIR` to a temp directory, so the undo history and trash stay out of the real state directory.
- The agent skill `skills/sessiongator/SKILL.md` documents the CLI for coding agents. Update it when a command, flag, or exit code changes. The dotfiles repo links it into `.claude/skills`.
- Local smoke commands: `cargo run -- list -n 5`, `cargo run -- search <text> -n 5`, and `cargo run -- convert <session> --dry-run`.
- Releases are triggered by changing the package version in `Cargo.toml` on `main`; update the matching root package version in `Cargo.lock`. A manual release workflow run only retries the current release and Homebrew sync.

## Session Stores

- Claude Code reads `$CLAUDE_CONFIG_DIR/projects`, else `~/.claude/projects`; the project folder encoding is lossy, so always read `cwd` from JSONL content.
- opencode reads SQLite from `$OPENCODE_DB`, else `$XDG_DATA_HOME/opencode/opencode.db`, else `~/.local/share/opencode/opencode.db`; browser reads must stay read-only because live opencode may hold WAL state. One database can hold 1.x tables (`session`/`message`/`part`) and OpenCode 2 tables (`session_v2`/`session_message`); `src/stores/opencode.rs` `newest_generation` picks the newer copy of a session. Writes go to OpenCode 2 tables only. Run every OpenCode 2 command in scripts with `--standalone`, or it talks to the shared background service and the real database.
- Codex listing reads `$CODEX_HOME` and `~/.codex` session JSONL trees; native import with `--source-store` or `--target-store` treats the path as one Codex root.
- Copilot reads `$COPILOT_HOME`, else `~/.copilot`; the list holds every `session-state/<id>/` with an `events.jsonl` merged with `session-store.db` rows (Copilot does not record every session in the database), and transcripts prefer `events.jsonl` before DB turns.

## Native Import

- Supported conversion versions are compiled from `docs/specs/native-session-import-versions.toml`; update that manifest and rebuild when changing version gates.
- Statuses: `verified` (full pass), `bracketed` (between two verified versions of one major version; counts as verified), `probe-passed` (CI), `read-observed` (source only), `untested` (same major version as a known version; converts with a warning), `unsupported`. See the Version Policy in `docs/specs/native-session-import.md`. Unsupported target versions fail closed with exit code 3 unless `--allow-unsupported-version` is passed. Source versions come from the session itself and only warn.
- Write paths differ: Claude uses atomic JSONL writes, opencode uses a SQLite `BEGIN IMMEDIATE` transaction (a `VACUUM INTO` backup only with `--backup`), Codex writes rollout JSONL plus `session_index.jsonl`, Copilot writes `session-state/<id>/` plus `session-store.db` projections.
- Keep conversion tests contract-focused: plan/report shape, mapped/dropped/synthesized counts, readback, and fixture round trips rather than byte-for-byte database layout.

## UI And Wrapper Contracts

- Theme modes and palettes match navgator: `auto` follows macOS appearance, explicit `light`/`dark` come from `--theme` then `$SESSIONGATOR_THEME`, and non-macOS `auto` falls back to light.
- Search is substring-based, not `gator::fuzzy_match`; `SearchMode::All` unions transcript hits with title/path/model hits.
- The TUI emits tab-separated selection lines consumed by `scripts/sessiongator.zsh`: `resume`, `resume-here`, `path`, and `convert`. Update the wrapper and `session.rs` tests together if the contract changes.
- `Ctrl+T` should keep producing a dry-run `sessiongator convert ... --plan-json` command for the selected session.
