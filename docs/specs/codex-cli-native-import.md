# Codex CLI Native Import Spec

## Goal

Add native conversion support between `sessiongator`'s canonical session model and OpenAI Codex CLI sessions.

This spec covers both directions:

- `--from codex`: read Codex CLI native sessions and convert them into Claude Code, opencode, or another supported target.
- `--to codex`: write a converted session into Codex CLI's native local store so `codex resume` / Codex session discovery can load it.

Codex support must be version-gated. Unknown source or target versions fail closed unless the user passes `--allow-unsupported-version`.

## Current Research Snapshot

Research date: 2026-07-08.

Local version inspected:

- Codex CLI: `codex-cli 0.142.5`

Upstream references inspected:

- Codex CLI README: `https://github.com/openai/codex`
- Codex CLI docs: `https://developers.openai.com/codex/cli`
- Codex config docs: `https://developers.openai.com/codex/config-reference`
- Codex rollout persistence source: `https://github.com/openai/codex/tree/main/codex-rs/rollout`
- Codex thread store source: `https://github.com/openai/codex/tree/main/codex-rs/thread-store`
- Codex state DB source: `https://github.com/openai/codex/tree/main/codex-rs/state`
- Codex rollout trace docs: `https://github.com/openai/codex/blob/main/codex-rs/rollout-trace/README.md`

Local Codex structure observed for `0.142.5`:

- Config root: `~/.codex`, with auth/config files, `history.jsonl`, `sessions/`, `shell_snapshots/`, `skills/`, and temp/vendor directories.
- Session rollout root: `~/.codex/sessions/<year>/<month>/<day>/`.
- Session rollout file pattern from upstream writer: `rollout-<YYYY-MM-DDTHH-MM-SS>-<thread-id>.jsonl`.
- Archived session root from upstream constants: `~/.codex/archived_sessions/`.
- Session name index: `~/.codex/session_index.jsonl`, append-only JSONL with `id`, `thread_name`, `updated_at`.
- Optional SQLite metadata DB from upstream state crate: `~/.codex/state_5.sqlite`.
- Other SQLite DB names from upstream state crate: `logs_2.sqlite`, `goals_1.sqlite`, `memories_1.sqlite`.
- Local `state_5.sqlite` may be absent or empty; Codex can fall back to rollout JSONL discovery.

Upstream rollout model observations:

- Codex calls persisted session logs "rollouts".
- Rollouts are JSONL and are the canonical replay source for legacy/full-history reads.
- The first meaningful item is expected to include `SessionMeta` data containing session/thread id, timestamp, cwd, originator, CLI version, model provider, source, history mode, selected capability roots, and multi-agent metadata.
- `RolloutRecorder` writes canonical `RolloutItem` values and filters persistence through `persisted_rollout_items`.
- Codex metadata extraction can build `ThreadMetadata` with id, rollout path, created/updated/recency times, source, history mode, model provider, model, cwd, CLI version, title, preview, sandbox/approval policy, tokens used, first user message, archive state, and git metadata.
- Upstream `state_5.sqlite` `threads` table starts with `id`, `rollout_path`, timestamps, source, model provider, cwd, title, sandbox policy, approval mode, token counts, archive fields, and git fields; later migrations add fields such as `cli_version`, `first_user_message`, `model`, `reasoning_effort`, `agent_path`, `thread_source`, `preview`, recency millis, visible-sort indexes, and `history_mode`.

## Non-Goals

- Do not write `auth.json`, tokens, account state, model caches, shell snapshots, skills, or temp/vendor directories.
- Do not import Codex rollout traces unless the user explicitly points at a trace bundle; traces are diagnostic artifacts, not the default native session store.
- Do not synthesize successful shell/tool execution that did not happen in the source session.
- Do not write into a live Codex store without an explicit backup and atomic-write plan.
- Do not support Codex Cloud tasks or app/IDE-only remote state until their local persistence is separately sampled.

## User-Facing Behavior

Extend the `convert` command tool set:

```sh
sessiongator convert --from codex --to claude --id <thread-id> --dry-run --plan-json
sessiongator convert --from claude --to codex --id <session-id> --target-id <thread-id>
sessiongator convert --from opencode --to codex --id <session-id> --target-store ~/.codex-test
```

Codex-specific options:

- `--source-store <path>`: Codex config root. Defaults to `$CODEX_HOME` if Codex supports it for the installed version, else `~/.codex`.
- `--target-store <path>`: Codex config root for writes. Defaults to the same root.
- `--target-id <uuid>`: target Codex thread id. Default: generated UUID.
- `--title <title>`: target thread name. When set, append a `session_index.jsonl` entry.
- `--include-diagnostics`: include diagnostic trace references if present and explicitly requested.

Exit codes follow the native import spec.

## Source Adapter

### Discovery

Read Codex sessions from:

- `sessions/**/*.jsonl`
- `archived_sessions/**/*.jsonl`
- `session_index.jsonl` for latest human-readable names
- `state_5.sqlite` `threads` metadata when present and non-empty

Discovery rules:

- Prefer SQLite metadata for fast listing only when the rollout path still exists and resolves to the same thread id.
- Fall back to scanning rollout JSONL files when the DB is missing, empty, stale, or corrupted.
- Treat the rollout file as the source of truth for transcript/history.
- Never infer cwd from the dated directory path; read it from `SessionMeta` / extracted metadata.

### Parsing

Map Codex rollout items into `NativeSession`:

- `SessionMeta` -> session id, cwd, created time, CLI version, model provider, source, history mode, parent/fork relationships, selected capability roots.
- User-visible prompt items -> `NativeRole::User` text or attachment parts.
- Assistant/model output items -> `NativeRole::Assistant` text, reasoning, and tool-call parts where represented natively.
- Tool call/result items -> `NativePart::ToolCall` / `NativePart::ToolResult` with raw payload preserved in metadata.
- Compaction/checkpoint items -> `NativeRole::Compaction` or metadata, depending on target support.
- Multi-agent child thread metadata -> parent/child extras and optional sidecar session references.

Unknown rollout item variants must be preserved as `NativePart::Raw` or session metadata, not silently dropped.

## Target Adapter

### Write Plan

Writing to Codex requires at least:

- A rollout JSONL under `sessions/<year>/<month>/<day>/rollout-<timestamp>-<thread-id>.jsonl`.
- A first metadata line equivalent to Codex `SessionMeta`.
- Canonical persisted rollout items for mapped messages/tool events.
- Optional `session_index.jsonl` entry when a title/name is available.
- Optional `state_5.sqlite` metadata upsert if the target version's state DB is initialized and schema-compatible.

The initial implementation should write a rollout JSONL and `session_index.jsonl` first, then let Codex backfill SQLite metadata when it next opens the store if the installed version supports that path. Direct SQLite writes should be a later phase gated by schema fingerprint tests.

### Atomicity

- Write rollout to `*.jsonl.tmp`, flush, then rename.
- Append `session_index.jsonl` only after rollout rename succeeds.
- If direct SQLite writes are enabled later, use a transaction and verify the stored `rollout_path` points to the new file.
- Back up `session_index.jsonl` and `state_5.sqlite*` before live-store writes.

### Verification

After writing:

- Re-read the rollout through sessiongator's Codex source adapter.
- Verify id, cwd, title/preview, message count, mapped part counts, and model/provider metadata.
- If Codex CLI exposes a non-interactive list/resume check for the installed version, run it only in isolated target-store CI.

## Mapping Policy

High-confidence mappings:

- Text messages map directly.
- Model id/provider maps to Codex model/model provider metadata where available.
- cwd maps to `SessionMeta.cwd` and SQLite `threads.cwd` if direct DB write is enabled.
- source session id maps into provenance metadata.
- tool calls/results map to Codex rollout items only after fixture verification for the target version.

Lossy mappings requiring warnings:

- Claude subagent sidecars -> Codex parent/child thread metadata only when a native Codex multi-agent mapping is confirmed.
- opencode patch/file/step parts -> Codex tool/runtime events where representable; otherwise raw metadata.
- Copilot checkpoints -> Codex compaction/checkpoint-like metadata only after local sampling.
- Hidden reasoning -> Codex reasoning items only if target version keeps them hidden from normal transcript display.

Blocked by default:

- Auth/account state.
- Runtime process state.
- Shell snapshots.
- Trace bundles and payloads unless explicitly requested.

## Version Policy

Add `codex` entries to `docs/specs/native-session-import-versions.toml` after fixture validation.

Required support metadata:

- CLI version from `codex --version`.
- Rollout schema fingerprint from observed JSONL item variants and `SessionMeta` keys.
- State DB fingerprint from SQLite tables, columns, indexes, and `_sqlx_migrations` versions when DB is initialized.
- Whether target writing uses JSONL-only or JSONL plus SQLite metadata.

Default support policy:

- Read support may allow patch versions with identical rollout item fingerprints.
- Write support requires exact known-good versions until CI proves compatibility.
- Direct SQLite writes require exact migration fingerprints.

## Fixtures

Add sanitized fixtures under:

```text
fixtures/native-import/codex/<version>/
  basic-text/
  tool-use/
  reasoning/
  attachments/
  compaction/
  multi-agent/
```

Each fixture should include:

- `source/.codex/sessions/.../*.jsonl`
- optional `source/.codex/session_index.jsonl`
- optional `source/.codex/state_5.sqlite.schema` or sanitized SQLite DB
- `expected-*/plan-summary.json`

Fixtures must redact secrets, absolute private paths outside the cwd, command output containing tokens, and account identifiers.

## CI Plan

- Install latest Codex CLI in the native import compatibility workflow.
- Generate an isolated Codex home and a minimal session if Codex supports non-interactive scripted session creation for the installed version.
- Run `sessiongator convert --from codex --dry-run --plan-json` against fixture and generated sessions.
- Run fixture-backed writes to an isolated Codex home.
- Re-run the Codex adapter readback on the written store.
- If latest Codex passes with an unknown version/schema, open a manifest-update PR instead of committing directly to `main`.

## Implementation Phases

1. Add `Tool::Codex` display/list support from rollout JSONL only.
2. Add Codex native reader into `NativeSession` with conservative raw preservation.
3. Add Codex dry-run conversion plans to existing Claude/opencode targets.
4. Add Codex JSONL-only writer into isolated target stores.
5. Add `session_index.jsonl` title/name writes.
6. Add optional `state_5.sqlite` metadata writes after migration-fingerprint CI.
7. Add live-store backup/locking and TUI conversion support.

## Acceptance Criteria

- `sessiongator --list` can include Codex sessions without reading secrets.
- `sessiongator convert --from codex --dry-run --plan-json` reports mapped/dropped/synthesized fields.
- JSONL-only Codex target writes round-trip through the Codex reader.
- Unknown Codex versions fail closed by default.
- Fixture tests cover text, tool use/result, reasoning, attachments, compaction, and multi-agent metadata.
- Live writes back up touched files and never mutate auth/config files.

## Open Questions

- Which Codex command is the safest non-interactive resume/list verifier for isolated target homes?
- Does the installed Codex version honor `CODEX_HOME`, or must tests override home/config by another documented mechanism?
- Which rollout item variants are required for Codex UI resume versus only useful for analytics/memories?
- Can Codex rebuild `state_5.sqlite` from rollouts reliably for imported sessions, or do current versions require direct SQLite upserts for session pickers?
