# GitHub Copilot CLI Native Import Spec

## Goal

Add native conversion support between `sessiongator`'s canonical session model and modern GitHub Copilot CLI sessions.

This spec covers both directions:

- `--from copilot`: read Copilot CLI native sessions and convert them into Claude Code, opencode, Codex CLI, or another supported target.
- `--to copilot`: write a converted session into Copilot CLI's native local store so `copilot --resume` / the Copilot session picker can load it.

Copilot support must be version-gated. Unknown source or target versions fail closed unless the user passes `--allow-unsupported-version`.

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
- Do not write to live Copilot DBs without backup and SQLite locking.

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
- `--include-checkpoints`: map source compactions/summaries to Copilot checkpoints where safe.
- `--legacy`: read legacy `history-session-state` sessions. Read-only until fixtures exist.

Exit codes follow the native import spec.

## Source Adapter

### Discovery

Read modern Copilot sessions from:

- `session-store.db` for fast listing and search metadata.
- `session-state/<uuid>/events.jsonl` for canonical event history.
- `session-state/<uuid>/workspace.yaml` and `vscode.metadata.json` for workspace metadata.
- `session-state/<uuid>/checkpoints/` for checkpoint details if not fully represented in the global DB.
- `session-state/<uuid>/files/` and `research/` only as referenced artifacts, not blindly indexed.

Discovery rules:

- Prefer `session-store.db.sessions` for list metadata when it references an existing `session-state/<id>` directory.
- Use `turns` and `search_index` only as derived/searchable projections; treat `events.jsonl` as the history source of truth after local sampling proves event semantics.
- Fall back to session directories if the global DB is missing or stale.
- Keep legacy `history-session-state` as read-only and unsupported by default until fixtures exist.

### Parsing

Map Copilot data into `NativeSession`:

- `sessions` row -> id, cwd, repository, branch, summary/title, created/updated times.
- `turns` row -> simple user/assistant text fallback when event parsing is unavailable.
- `events.jsonl` -> preferred turn/tool/checkpoint source after schema sampling.
- `checkpoints` -> `NativeRole::Compaction` or checkpoint metadata.
- `session_files` -> `NativePart::File` references or session metadata.
- `session_refs` -> issue/PR/URL/ref metadata.
- `todos` -> native todo metadata if sessiongator's import model grows todo support.
- Unknown per-session events must be preserved as raw metadata, not silently dropped.

## Target Adapter

### Write Plan

Writing to Copilot requires at least:

- A new `session-state/<uuid>/` directory.
- A native `events.jsonl` containing mapped conversation history.
- A per-session `session.db` if the target version expects it.
- A `workspace.yaml` with cwd/repository metadata when required.
- A global `session-store.db` transaction that inserts or updates `sessions`, `turns`, `checkpoints`, `session_files`, `session_refs`, and FTS `search_index` projections.

The first implementation should be conservative:

- Read support first.
- Dry-run target plans second.
- Target writes only into isolated `--target-store` fixtures until live-store resume is verified.
- Live writes only after exact version/schema fixtures prove `copilot --resume <id>` can load imported sessions.

### Atomicity

- Create `session-state/<uuid>.tmp/`, write all files, then rename to `session-state/<uuid>/`.
- Wrap global `session-store.db` changes in one immediate transaction.
- Back up `session-store.db`, `session-store.db-wal`, `session-store.db-shm`, and the target session directory before live writes.
- If any step fails, remove the temp directory and roll back the DB transaction.

### Verification

After writing:

- Re-read the new session through sessiongator's Copilot source adapter.
- Verify id, cwd, repository, branch, summary/title, turn count, checkpoint count, file/ref count, and searchable text.
- In isolated CI, run the safest available Copilot verifier, preferably a non-interactive resume/list command if exposed by the installed version.
- Do not run a live model turn as verification unless explicitly requested; it consumes credits and may mutate files.

## Mapping Policy

High-confidence mappings:

- Text user/assistant turns map to `turns` and event transcript entries.
- cwd/repository/branch map to `sessions` and `workspace.yaml`.
- source title/summary maps to `sessions.summary`.
- source file references map to `session_files` when file paths are local and safe.
- source issue/PR/URL references map to `session_refs`.

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

Add `copilot` entries to `docs/specs/native-session-import-versions.toml` after fixture validation.

Required support metadata:

- CLI version from `copilot --version`.
- Global DB schema fingerprint from tables, columns, indexes, FTS tables, and `schema_version`.
- Per-session DB schema fingerprint from `session.db` tables and columns.
- Event schema fingerprint from observed `events.jsonl` event types and top-level keys.
- Session directory layout fingerprint.

Default support policy:

- Read support can allow patch versions when DB/event fingerprints match.
- Target writes require exact version support until CI verifies `copilot --resume` can load imported sessions.
- Legacy `history-session-state` is read-only and unsupported by default.

## Fixtures

Add sanitized fixtures under:

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

- Install latest Copilot CLI in the native import compatibility workflow when licensing/auth allows.
- Prefer fixture tests for CI because real Copilot CLI may require authenticated entitlements and may consume credits.
- If auth is available in a private/manual workflow, generate an isolated `COPILOT_HOME` session with a harmless prompt and no write permissions.
- Run `sessiongator convert --from copilot --dry-run --plan-json` against fixtures and generated sessions.
- Run target writes only into isolated `COPILOT_HOME` stores.
- Verify readback through sessiongator first, then run Copilot's own resume/list verifier only if it is non-mutating.
- If latest Copilot passes with an unknown version/schema, open a manifest-update PR instead of committing directly to `main`.

## Implementation Phases

1. Add `Tool::Copilot` display/list support from `session-store.db` plus session directories.
2. Add Copilot native reader with `turns` fallback and raw `events.jsonl` preservation.
3. Add event parser fixtures for text, tool use, checkpoints, file refs, GitHub refs, and todos.
4. Add Copilot dry-run plans to existing Claude/opencode/Codex targets.
5. Add isolated target writer for `session-state/<uuid>/` and `session-store.db` projections.
6. Verify `copilot --resume <id>` in isolated stores before live writes.
7. Add live-store backup/locking and TUI conversion support.

## Acceptance Criteria

- `sessiongator --list` can include Copilot sessions without reading auth/config/log files.
- `sessiongator convert --from copilot --dry-run --plan-json` reports mapped/dropped/synthesized fields.
- Copilot source fixtures parse from both global DB metadata and per-session event files.
- Copilot target writes round-trip through the Copilot reader in an isolated store.
- Unknown Copilot versions fail closed by default.
- Live writes back up touched SQLite files and session directories.
- Deprecated `gh-copilot` data is not accidentally treated as modern Copilot CLI data.

## Open Questions

- Is `events.jsonl` the authoritative replay source for `1.0.68`, or does Copilot require additional per-session DB state to resume?
- What is the complete `events.jsonl` schema for tool calls, approvals, shell output, file writes, checkpoints, and remote/cloud sessions?
- Which command can verify imported sessions without consuming AI credits or mutating the working tree?
- Does Copilot provide a stable export/import or ACP-based API that is safer than native file/SQLite writes?
- How should remote/cloud sessions be represented: blocked, read-only metadata, or separate target mode?
