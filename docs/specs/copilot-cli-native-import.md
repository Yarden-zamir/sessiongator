# GitHub Copilot CLI Native Import Spec

## Goal

Add native conversion support between `sessiongator`'s canonical session model and modern GitHub Copilot CLI sessions.

This spec covers both directions:

- `--from copilot`: read Copilot CLI native sessions and convert them into Claude Code, opencode, Codex CLI, or another supported target.
- `--to copilot`: write a converted session into Copilot CLI's native local store so `copilot --resume` / the Copilot session picker can load it.

Copilot support must be version-gated as in the native session import Version Policy. Unsupported target versions fail closed unless the user passes `--allow-unsupported-version`; untested source versions only warn.

## Current Research Snapshot

Research date: 2026-07-08.

Local version inspected:

- GitHub Copilot CLI: `GitHub Copilot CLI 1.0.68`

Upstream references inspected:

- GitHub Copilot CLI docs: `https://docs.github.com/en/copilot/concepts/agents/about-copilot-cli`
- GitHub Copilot CLI public repo README: `https://github.com/github/copilot-cli`
- GitHub Copilot CLI public changelog: `https://github.com/github/copilot-cli/blob/main/changelog.md`
- Deprecated `gh-copilot` extension README: `https://github.com/github/gh-copilot`
- Retired GitHub CLI Copilot extension docs: `https://docs.github.com/en/copilot/github-copilot-in-the-cli/using-github-copilot-in-the-cli`

Important distinction:

- `gh copilot suggest/explain` from `github/gh-copilot` is retired/deprecated and is not the target of this spec.
- The target is the modern standalone `copilot` binary from `github/copilot-cli` / `@github/copilot`.

Local Copilot structure observed for `1.0.68`:

- Config root: `~/.copilot`, overridable by `COPILOT_HOME` per changelog notes.
- Config/settings: `config.json`, `settings.json`, `command-history-state.json`.
- Logs: `logs/`.
- Skills/plugins/IDE state: `skills/`, `installed-plugins/`, `ide/`, `vscode.session.metadata.cache.json`.
- Global session search/index DB: `~/.copilot/session-store.db` with WAL/SHM sidecars.
- Session folders: `~/.copilot/session-state/<uuid>/`.
- Per-session observed files: `events.jsonl`, `session.db`, `workspace.yaml`, `vscode.metadata.json`, `checkpoints/`, `files/`, `research/`.
- Legacy sessions: `~/.copilot/history-session-state` per public changelog; modern sessions live in `~/.copilot/session-state`.

Local `session-store.db` schema observed:

- `schema_version(version)`.
- `sessions(id, cwd, repository, host_type, branch, summary, created_at, updated_at)`.
- `turns(id, session_id, turn_index, user_message, assistant_response, timestamp)`.
- `checkpoints(id, session_id, checkpoint_number, title, overview, history, work_done, technical_details, important_files, next_steps, created_at)`.
- `session_files(id, session_id, file_path, tool_name, turn_index, first_seen_at)`.
- `session_refs(id, session_id, ref_type, ref_value, turn_index, created_at)`.
- FTS5 `search_index(content, session_id, source_type, source_id)`.
- `dynamic_context_items(repository, branch, src, name, description, content, read_count, count)`.

Local per-session `session.db` schema observed from one session:

- `todos(id, title, description, status, created_at, updated_at)`.
- `todo_deps(todo_id, depends_on)`.
- `inbox_entries(id, recipient_session_id, sender_id, sender_name, sender_type, interaction_id, sequence, summary, content, unread, sent_at, read_at, notified_at)`.

Public changelog observations:

- `0.0.342` introduced the current session logging format, storing new sessions in `~/.copilot/session-state` and legacy sessions in `~/.copilot/history-session-state`.
- Changelog references local assistant usage in Chronicle and session SQL, session databases, session-store searches, `--session-id`, `--resume`, short session id prefixes, session names, and `COPILOT_HOME`.
- `--config-dir` is deprecated in favor of `COPILOT_HOME`.

## Non-Goals

- Do not target the retired `gh copilot` extension format except as a future legacy reader if users still have old data.
- Do not write GitHub auth tokens, Copilot account config, logs, plugins, skills, IDE metadata, or command history.
- Do not import/export cloud sandbox state, remote hosted sessions, or GitHub.com task state until separately sampled.
- Do not fabricate successful tool execution or PR/issue creation that did not happen in the source session.
- Do not write to live Copilot DBs outside one immediate SQLite transaction. `sessiongator undo --target` replaces file backups.

## User-Facing Behavior

Extend the `convert` command tool set:

```sh
sessiongator convert --from copilot --to claude --id <session-id> --dry-run --plan-json
sessiongator convert --from claude --to copilot --id <session-id> --target-id <uuid>
sessiongator convert --from opencode --to copilot --id <session-id> --target-store ~/.copilot-test
```

Copilot-specific options:

- `--source-store <path>`: Copilot home. Defaults to `$COPILOT_HOME`, else `~/.copilot`.
- `--target-store <path>`: Copilot home for writes. Defaults to the same root.
- `--target-id <uuid>`: Copilot session id. Default: generated UUID.
- `--title <title>`: target session summary/name where the target schema supports it.
- Not built: `--include-checkpoints` (map source compactions/summaries to Copilot checkpoints) and `--legacy` (read legacy `history-session-state` sessions).

Exit codes follow the native import spec.

## Source Adapter

### Discovery

Read modern Copilot sessions from:

- `session-store.db` `sessions` and `turns` for list metadata and a text fallback.
- `session-state/<uuid>/events.jsonl` for canonical event history.
- `session-state/<uuid>/workspace.yaml` for list metadata of sessions without a database row.
- Not read yet: `vscode.metadata.json`, `checkpoints/`, `files/`, and `research/`.

Discovery rules:

- List every `session-state/<id>/` that has an `events.jsonl`, merged with `session-store.db.sessions` rows. Copilot does not record every session in the database (2026-10-14: 16 sessions with event logs, 22 database rows, only some overlapping). A database row title wins; `workspace.yaml` `name` is used only when `user_named` is true, because Copilot fills it with pasted prompts otherwise.
- Use `turns` and `search_index` only as derived/searchable projections; treat `events.jsonl` as the history source of truth after local sampling proves event semantics.
- Keep legacy `history-session-state` as read-only and unsupported by default until fixtures exist.

### Parsing

Map Copilot data into `NativeSession`:

- `sessions` row -> id, cwd, repository, branch, summary/title, created/updated times.
- `turns` row -> user/assistant text fallback when `events.jsonl` has no messages.
- `events.jsonl` -> `session.start` (cwd, `copilotVersion`), `user.message`, `assistant.message`, `tool.execution_start` (tool call), `tool.execution_complete` (tool result), `permission.requested` (system text), and `session.shutdown` with token counts (compaction text).
- Not read yet: `checkpoints`, `session_files`, `session_refs`, and `todos`. Other event types are skipped, not preserved.

## Target Adapter

### Write Plan

The writer creates:

- A new `session-state/<uuid>/` directory.
- A native `events.jsonl`: `session.start`, then one `user.message` or `assistant.message` per user or assistant message, chained by `parentId`. `sessiongatorParts` keeps the native parts for a lossless read back.
- A per-session `session.db` with the observed empty `todos`, `todo_deps`, and `inbox_entries` tables.
- A `workspace.yaml` with cwd and title.
- A global `session-store.db` transaction that inserts `sessions` and `turns` rows. Each turn pairs a user message with the next assistant message. Not written yet: `checkpoints`, `session_files`, `session_refs`, and the `search_index` projection.

Live writes are allowed: the full pass resumed an imported session with `copilot --resume=<id> -p` and a real model on `1.0.97`.

### Atomicity

- Create `session-state/<uuid>.tmp/`, write all files, then rename to `session-state/<uuid>/`.
- Wrap global `session-store.db` changes in one immediate transaction. The rename happens inside it, before the commit.
- Do not copy `session-store.db` before live writes: the writer only adds rows, and `sessiongator undo --target` removes them with the session directory.
- If any step fails, remove the temp directory and roll back the DB transaction.

### Verification

After writing:

- `convert` re-reads the new session through the Copilot store and compares title, cwd, and the count of user and assistant messages that hold text. A mismatch moves the written session to the trash and fails.
- The CI probe runs `copilot --resume=<id>` without credentials in an isolated `COPILOT_HOME` and requires successful parsing.
- Only the full pass runs a live model turn; it consumes one premium request per run.

## Mapping Policy

High-confidence mappings:

- Text user/assistant turns map to `turns` and event transcript entries.
- cwd maps to `sessions` and `workspace.yaml`; a source `gitBranch` maps to `sessions.branch`.
- source title/summary maps to `sessions.summary`.
- Not built: file references to `session_files` and issue/PR/URL references to `session_refs`.

Lossy mappings requiring warnings:

- Claude/opencode/Codex tool calls -> Copilot events only after event schema fixtures identify native tool-call records.
- Hidden reasoning -> checkpoint or raw metadata only; do not expose hidden reasoning as assistant text by default.
- Codex multi-agent or Copilot inbox entries -> raw metadata until native semantics are sampled.
- opencode todo/compaction parts -> Copilot todos/checkpoints only when target schema supports equivalent fields.

Blocked by default:

- GitHub auth state and PAT-derived data.
- Remote/cloud sandbox state.
- Plugin state and installed MCP server state.
- Logs and command history.

## Version Policy

Copilot follows the Version Policy of `native-session-import.md`; `copilot` entries live in `docs/specs/native-session-import-versions.toml`.

Recorded support metadata:

- CLI version from `copilot --version`, and `copilotVersion` from `session.start` for sources.
- `session-store.db` `schema_version` and a schema fingerprint of its tables and columns.
- Not recorded yet: per-session `session.db`, event, and directory layout fingerprints.

Legacy `history-session-state` is not read.

## Fixtures

Not built: tests create Copilot stores inline in `src/stores/copilot.rs`. Planned sanitized fixtures:

```text
fixtures/native-import/copilot/<version>/
  basic-text/
  tool-use/
  checkpoints/
  file-refs/
  github-refs/
  todos/
```

Each fixture should include:

- `source/.copilot/session-store.db` or a schema plus minimal sanitized rows.
- `source/.copilot/session-state/<uuid>/events.jsonl`.
- `source/.copilot/session-state/<uuid>/session.db` where present.
- `source/.copilot/session-state/<uuid>/workspace.yaml`.
- `expected-*/plan-summary.json`.

Fixtures must redact repository private URLs, GitHub usernames where not needed, issue/PR content, tokens, command output containing secrets, local absolute paths outside the fixture cwd, and any generated files under `files/` or `research/` unless explicitly needed.

## CI Plan

- The daily probe installs the latest Copilot CLI, converts into and out of isolated `COPILOT_HOME` stores, and runs `copilot --resume=<id>` without credentials and without a terminal; it requires successful parsing and never submits a prompt.
- The full pass on the VPS validates model continuation with `GH_TOKEN` from `gh auth token`.
- Both open a manifest-update pull request instead of committing to `main`.

## Implementation Phases

1. ✅ Add `Tool::Copilot` display/list support from `session-store.db` plus session directories.
2. ✅ Add Copilot native reader with `turns` fallback. Raw preservation of unknown events is not built.
3. ⬜ Add event parser fixtures for text, tool use, checkpoints, file refs, GitHub refs, and todos. Inline test stores cover text and tool events.
4. ✅ Add Copilot dry-run plans to existing Claude/opencode/Codex targets.
5. ✅ Add the target writer for `session-state/<uuid>/` and `session-store.db` `sessions`/`turns` rows.
6. ✅ Verify `copilot --resume <id>` in isolated stores before live writes.
7. ✅ Live writes use an immediate transaction and `undo`; TUI conversion is the `Ctrl+T` dry-run command.

## Acceptance Criteria

- `sessiongator list` can include Copilot sessions without reading auth/config/log files.
- `sessiongator convert --from copilot --dry-run --plan-json` reports mapped/dropped/synthesized fields.
- Copilot source stores parse from both global DB metadata and per-session event files.
- Copilot target writes round-trip through the Copilot reader in an isolated store, and every write is read back.
- Unsupported Copilot target versions fail closed by default.
- `sessiongator undo --target` removes a written session's directory and rows.
- Deprecated `gh-copilot` data is not accidentally treated as modern Copilot CLI data.

## Open Questions

- Answered for `1.0.97`: `events.jsonl` plus the `session-store.db` rows sessiongator writes are enough to resume with a model turn.
- What is the complete `events.jsonl` schema for tool calls, approvals, shell output, file writes, checkpoints, and remote/cloud sessions?
- Answered: `copilot --resume=<id>` without credentials and without a terminal parses the session and exits without a model request.
- Does Copilot provide a stable export/import or ACP-based API that is safer than native file/SQLite writes?
- How should remote/cloud sessions be represented: blocked, read-only metadata, or separate target mode?
