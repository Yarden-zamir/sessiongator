# Codex CLI Native Import Spec

## Goal

Add native conversion support between `sessiongator`'s canonical session model and OpenAI Codex CLI sessions.

This spec covers both directions:

- `--from codex`: read Codex CLI native sessions and convert them into Claude Code, opencode, or another supported target.
- `--to codex`: write a converted session into Codex CLI's native local store so `codex resume` / Codex session discovery can load it.

Codex support must be version-gated as in the native session import Version Policy. Unsupported target versions fail closed unless the user passes `--allow-unsupported-version`; untested source versions only warn.

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
- Do not write into a live Codex store without an atomic-write plan. `sessiongator undo --target` replaces file backups.
- Do not support Codex Cloud tasks or app/IDE-only remote state until their local persistence is separately sampled.

## User-Facing Behavior

Extend the `convert` command tool set:

```sh
sessiongator convert --from codex --to claude --id <thread-id> --dry-run --plan-json
sessiongator convert --from claude --to codex --id <session-id> --target-id <thread-id>
sessiongator convert --from opencode --to codex --id <session-id> --target-store ~/.codex-test
```

Codex-specific options:

- `--source-store <path>`: one Codex config root. Without it, sessiongator reads `$CODEX_HOME` and `~/.codex`.
- `--target-store <path>`: Codex config root for writes. Default: `$CODEX_HOME`, else `~/.codex`.
- `--target-id <uuid>`: target Codex thread id. Default: generated UUID.
- `--title <title>`: target thread name. Every write appends a `session_index.jsonl` entry with the title.
- Not built: `--include-diagnostics`.

Exit codes follow the native import spec.

## Source Adapter

### Discovery

Read Codex sessions from:

- `sessions/**/*.jsonl`
- `archived_sessions/**/*.jsonl`
- Not read yet: `session_index.jsonl` names and `state_5.sqlite` `threads` metadata.

Discovery rules:

- List by scanning rollout JSONL files; the id comes from the file name.
- Treat the rollout file as the source of truth for transcript/history.
- Current Codex writes each visible message twice, as `response_item` and as `event_msg`. Read the `event_msg` copy when the rollout has one, else the `response_item` copy, so no message appears twice.
- Never infer cwd from the dated directory path; read it from `SessionMeta` / extracted metadata.

### Parsing

Map Codex rollout items into `NativeSession`:

- `SessionMeta` -> cwd, title, created time, CLI version, model provider.
- User-visible prompt items -> `NativeRole::User` text.
- Assistant/model output items -> `NativeRole::Assistant` text; `reasoning` items -> reasoning parts.
- `function_call` / `function_call_output` / `web_search_call` -> `NativePart::ToolCall` / `NativePart::ToolResult`.
- Unknown `response_item` payload types -> `NativePart::Raw`, reported as dropped in the plan. Other top-level line types are skipped.
- Not built: compaction/checkpoint items, multi-agent child threads, parent/fork relationships, attachments.

## Target Adapter

### Write Plan

Writing to Codex requires at least:

- A rollout JSONL under `sessions/<year>/<month>/<day>/rollout-<timestamp>-<thread-id>.jsonl`.
- A first metadata line equivalent to Codex `SessionMeta`.
- Canonical top-level `session_meta`, `response_item`, and `event_msg` rollout envelopes for mapped messages; user/agent event messages provide Codex turn projection while response items retain model history.
- A `session_index.jsonl` entry with the title.
- No `state_5.sqlite` write: Codex backfills its metadata from the rollout. The probe's app-server `thread/read` projects imported sessions without it.

### Atomicity

- Write the rollout to a temp file, then rename.
- Append `session_index.jsonl` only after the rollout rename succeeds.
- No file backups: `sessiongator undo --target` moves the rollout to the trash and removes its `session_index.jsonl` lines.

### Verification

After writing:

- `convert` re-reads the rollout through the Codex store and compares title, cwd, and the count of user and assistant messages that hold text. A mismatch moves the written session to the trash and fails.
- The CI probe runs Codex app-server `thread/read` in an isolated `CODEX_HOME` and requires a non-empty turn.
- The full pass resumes the session with `codex exec resume <id>` and a real model.

## Mapping Policy

High-confidence mappings:

- Text messages map directly.
- Model provider maps to `SessionMeta.model_provider`.
- cwd maps to `SessionMeta.cwd`.
- source session id maps into provenance metadata.
- Not built: tool calls/results, reasoning, and non-text parts are not written to Codex; only user and assistant text is.

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

Codex follows the Version Policy of `native-session-import.md`; `codex` entries live in `docs/specs/native-session-import-versions.toml`.

Recorded support metadata:

- CLI version from `codex --version`, and `cli_version` from `SessionMeta` for sources.
- A store fingerprint with the rollout count and whether `session_index.jsonl` exists.
- Not recorded yet: rollout item variant and state DB fingerprints. sessiongator writes JSONL only.

## Fixtures

Not built: tests create Codex rollouts inline in `src/stores/codex.rs`. Planned sanitized fixtures:

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

- The daily probe installs the latest Codex CLI, converts into and out of isolated Codex homes, and checks each written session with app-server `thread/read`.
- The full pass on the VPS resumes an imported session with a real model; it needs a Codex login in `FULL_PASS_CODEX_HOME`.
- Both open a manifest-update pull request instead of committing to `main`.

## Implementation Phases

1. ✅ Add `Tool::Codex` display/list support from rollout JSONL only.
2. ✅ Add Codex native reader into `NativeSession` with conservative raw preservation.
3. ✅ Add Codex dry-run conversion plans to existing Claude/opencode targets.
4. ✅ Add Codex JSONL-only writer.
5. ✅ Add `session_index.jsonl` title/name writes.
6. ⬜ Add optional `state_5.sqlite` metadata writes after migration-fingerprint CI.
7. ✅ Live writes use atomic renames and `undo`; TUI conversion is the `Ctrl+T` dry-run command.

## Acceptance Criteria

- `sessiongator list` can include Codex sessions without reading secrets.
- `sessiongator convert --from codex --dry-run --plan-json` reports mapped/dropped/synthesized fields.
- JSONL-only Codex target writes round-trip through the Codex reader, and every write is read back.
- Unsupported Codex target versions fail closed by default.
- Inline tests cover text, tool use/result, and the two message copies; attachments, compaction, and multi-agent metadata are not covered.
- Live writes never mutate auth/config files; `undo --target` removes a written session.

## Open Questions

- Answered: app-server `thread/read` is the model-free verifier; `codex exec resume <id>` is the model check.
- Answered: Codex honors `CODEX_HOME`; its login file lives there.
- Which rollout item variants are required for Codex UI resume versus only useful for analytics/memories?
- Can Codex rebuild `state_5.sqlite` from rollouts reliably for imported sessions, or do current versions require direct SQLite upserts for session pickers?
