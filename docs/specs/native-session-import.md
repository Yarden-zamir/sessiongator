# Native Session Import Spec

## Goal

Add a native conversion/import feature that can take one session from a source AI coding tool, convert every representable field into the target tool's native storage format, save it safely, and expose the flow through both CLI and TUI.

Tools: Claude Code, opencode, Codex CLI (`codex-cli-native-import.md`), and GitHub Copilot CLI (`copilot-cli-native-import.md`).

The conversion must be version-gated. An unsupported or unknown target version must fail closed unless the user explicitly opts into an experimental conversion. An untested source version only warns, because reading never changes the source store.

## Current Research Snapshot

Research date: 2026-07-03.

Local versions inspected:

- Claude Code: `2.1.199`
- opencode: `1.17.13`
- opencode: `2.0.18` (2026-09-28)

Upstream references inspected:

- Claude Code CLI docs: `https://docs.anthropic.com/en/docs/claude-code/cli-reference`
- Claude Code `.claude` directory docs: `https://docs.anthropic.com/en/docs/claude-code/claude-directory`
- opencode SQL schema: `https://github.com/anomalyco/opencode/blob/dev/packages/core/src/session/sql.ts`
- opencode session schema: `https://github.com/anomalyco/opencode/blob/dev/packages/schema/src/session.ts`
- opencode session message schema: `https://github.com/anomalyco/opencode/blob/dev/packages/schema/src/session-message.ts`
- opencode ID format: `https://github.com/anomalyco/opencode/blob/dev/packages/core/src/id/id.ts`

Local Claude Code structure observed for `2.1.199`:

- Transcript files: `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`
- Background/agent status files: `~/.claude/sessions/<pid>.json`
- Tool sidecar content: `~/.claude/projects/<encoded-cwd>/<session-id>/tool-results/*.txt`
- Subagent sidecars: `~/.claude/projects/<encoded-cwd>/<session-id>/subagents/*.jsonl` and `*.meta.json`
- Common JSONL event `type` values: `system`, `user`, `assistant`, `ai-title`, `attachment`, `file-history-snapshot`, `last-prompt`, `mode`, `permission-mode`, `queue-operation`
- Common top-level JSONL fields: `uuid`, `parentUuid`, `sessionId`, `timestamp`, `type`, `cwd`, `gitBranch`, `version`, `message`, `messageId`, `requestId`, `isSidechain`, `leafUuid`, `toolUseID`, `toolUseResult`, `aiTitle`
- Common Claude message fields: `id`, `type`, `role`, `model`, `content`, `usage`, `stop_reason`, `stop_sequence`, `stop_details`, `context_management`, `container`, `diagnostics`
- Common Claude message content block types: `text`, `thinking`, `tool_use`, `tool_result`

Local opencode structure observed for `1.17.13`:

- Database path: `$XDG_DATA_HOME/opencode/opencode.db`, defaulting to `~/.local/share/opencode/opencode.db`
- Core tables relevant to imports: `project`, `project_directory`, `session`, `message`, `part`, `session_message`, `session_input`, `session_context_epoch`, `todo`, `event`, `event_sequence`
- opencode retains legacy `message`/`part` rows and newer `session_message` rows. sessiongator reads both and writes OpenCode 2 tables only (see `2.0.18` below); a target database without `session_v2` fails with "start OpenCode 2 once to migrate it".
- Common `message.data` keys: `role`, `parentID`, `agent`, `modelID`, `providerID`, `mode`, `path`, `cost`, `tokens`, `tools`, `summary`, `finish`, `error`
- Common `part.data` types: `text`, `reasoning`, `tool`, `patch`, `file`, `step-start`, `step-finish`, `compaction`
- Current upstream `session_message` types: `agent-switched`, `model-switched`, `user`, `synthetic`, `system`, `shell`, `assistant`, `compaction`

Local opencode structure observed for `2.0.18` (source: branch `v2` of anomalyco/opencode, `packages/core/src/database/v1-migration.bun.ts`, `packages/schema/src/session-message.ts`):

- Same database path. Sessions live in `session_v2`. `session_message.session_id` has a foreign key to `session_v2`, not `session`.
- The first 2.x start copies every 1.x session into `session_v2`/`session_message` once, then marks `kv['migration.v1-v2']` completed. The 1.x tables stay. A fresh 2.x database has no 1.x tables.
- If 1.x tables appear later and the marker is absent, 2.x runs the copy again and first deletes every row in `event`. The writer must never create 1.x tables in a 2.x database.
- `session_message.data` holds the message without `id` and `type` (those are columns). 2.x decodes each row with the `Session.Message.Info` schema and ignores unknown keys. One row that does not decode makes 2.x fail to load the whole session.
- `event_sequence(aggregate_id = session id, seq)` must equal the highest `session_message.seq` of the session. 2.x numbers the next message after it.
- `session_v2.agent` must be NULL or a known agent. 2.x refuses to prompt a session with an unknown agent (`AgentNotFoundError`).

## Non-Goals

- Do not promise semantic losslessness. Tool runtimes have private state and replay semantics that are not identical.
- Do not write into a live opencode database outside one `BEGIN IMMEDIATE` transaction. `undo --target` replaces a full backup.
- Do not import hidden reasoning into target-visible transcript text. Preserve it only when the target has a native hidden/reasoning field.
- Do not fabricate successful tool execution if the source only contains a pending/interrupted tool call.
- Do not support unknown tool versions by default.

## User-Facing Behavior

### CLI

Add a `convert` command:

```sh
sessiongator convert [<session>] [--id <session-id>] [--from <tool>] [--to <tool>] [options]
```

`<session>` is an id, a unique id prefix, `tool:id`, or text that matches exactly one session title, path, or model. It sets `--from` and `--id`. More than one match fails with a candidate list.

Options:

- `--from <tool>`: source tool. Default: the tool that owns `<session>`. `--from` and `--id` together skip the session lookup.
- `--to <tool>`: target tool. Default: opencode for Claude sessions, else claude.

- `--target-store <path>`: target config root or database. Defaults to the target tool's normal store.
- `--source-store <path>`: source config root or database. Defaults to the source tool's normal store. Requires `--from` and `--id`.
- `--target-id <id>`: explicit target session id. Default: generate a native id.
- `--cwd <path>`: override target session working directory. Default: source session cwd.
- `--title <title>`: override target title. Default: mapped source title.
- `--dry-run`: parse, validate, map, and print a conversion summary without writing. With `--plan-json`, print the plan JSON instead.
- `--plan-json`: print the conversion plan as JSON for CI/golden tests.
- `--report-json`: print the post-write verification report as JSON.
- `--mark-source`: after the write, append an assistant message to the source session that names the new session and its resume command. `sessiongator undo` removes the message.
- `--delete-source`: after the write, move the source session to the sessiongator trash. `sessiongator undo` restores it. Conflicts with `--mark-source`.
- `--backup`: copy the whole opencode database with `VACUUM INTO` before writing. Off by default: `sessiongator undo --target` removes a written session without a full copy.
- `--force`: overwrite an existing target session id. Default false.
- `--allow-unsupported-version`: run when the target version is unsupported, marking the imported session as experimental.
- Not built: `--include-sidecars` (include Claude tool-result/subagent sidecars) and `--redact-fixture` (export a sanitized fixture).

Examples:

```sh
sessiongator convert --from claude --id 31918b78-1817-4eea-885d-b1e7ce15e6fb --to opencode --dry-run
sessiongator convert --from claude --id 31918b78-1817-4eea-885d-b1e7ce15e6fb --to opencode
sessiongator convert --from opencode --id ses_01K... --to claude --target-id 11111111-2222-4333-8444-555555555555
```

Exit codes:

- `0`: import succeeded or dry-run had no errors.
- `1`: runtime/read/write failure.
- `2`: invalid arguments.
- `3`: unsupported source/target version.
- `4`: reserved: lossy field would be dropped without an explicit policy. Not used yet.
- `5`: reserved: target store is live/locked and cannot be safely written. Not used yet.

### TUI

As built: `Ctrl+T` places a dry-run `sessiongator convert ... --plan-json` command in the shell prompt. The dialog below is not built.

Add conversion support to the current session picker:

- `c`: open convert dialog for selected session.
- Dialog asks target tool, target store, and whether to dry-run or write.
- Dialog shows:
  - detected source version
  - detected target version
  - support status from `docs/specs/native-session-import-versions.toml`
  - count of mapped, dropped, synthesized, and blocked fields
  - backup path if writing to a live store
- `Enter`: run dry-run from dialog.
- `Shift+Enter` or `w`: write after a successful dry-run.
- After import, the list refreshes and selects the new target session.
- Imported rows should display an `imported-from=<tool>:<id>` extra metadata field.

## Architecture

As built:

```text
src/
  native.rs          // NativeSession, NativeMessage, NativePart, transcript turns
  stores/            // one Store per tool: list, read, write, footprint, append_note
    claude.rs  opencode.rs  codex.rs  copilot.rs  sqlite.rs
  convert/
    mod.rs           // ConvertArgs, plan, mapping, write, mark/delete, reports
    versions.rs      // manifest, Support, CLI version detection
  trash.rs           // move a Footprint to the trash and back
  history.rs         // operation journal for `undo` and `history`
```

The browser and conversion share one `Store` trait per tool. `read` returns the full `NativeSession`; the TUI transcript is the text parts of its user and assistant messages (`native::turns`), so one reader serves both.

```rust
trait Store {
    fn list(&self) -> AppResult<Vec<Session>>;
    fn read(&self, id: &str) -> AppResult<NativeSession>;
    fn fingerprint(&self) -> AppResult<Fingerprint>;
    fn write(&self, request: &WriteRequest) -> AppResult<Written>;
    /// Files, SQLite rows, and JSONL lines that belong to one session.
    fn footprint(&self, id: &str) -> AppResult<Footprint>;
    /// Append one assistant message; returns the footprint of that message.
    fn append_note(&self, id: &str, note: &Note) -> AppResult<Footprint>;
}
```

### Delete, mark, and undo

- A `Footprint` lists paths, SQLite row sets (`table`, `column`, `value`, children before parents), and exact lines in shared JSONL files.
- `trash` writes `record.json` with copies of every row and line first, then deletes rows with foreign keys off, removes lines, and moves paths into `<state dir>/trash/<time>-<name>/`. A failure halfway leaves enough to restore.
- `restore` moves paths back, appends missing lines, and inserts each row set only when none of its rows exist. A second restore changes nothing.
- `<state dir>/history.jsonl` records each conversion and delete. `<state dir>` is `$SESSIONGATOR_STATE_DIR`, else `$XDG_STATE_HOME/sessiongator`, else `~/.local/state/sessiongator`.
- `undo [op]` reverts the newest open operation: it removes the migration note, restores a deleted source, and with `--target` also moves the converted session to the trash.
- Limit: deleting an opencode session does not delete its child (subagent) sessions.

## Version Policy

Version support lives in `docs/specs/native-session-import-versions.toml`. sessiongator compiles it in; rebuild after you change it. `sessiongator compat` prints the installed versions and the recorded ones.

Recorded statuses:

- `verified`: a full pass (`scripts/full-pass-native-import.sh`) on a machine with real clients and model keys. The target tool resumed an imported session and answered with a code word that only the imported history holds, and sessiongator read the reply back. Only the full pass writes `verified`.
- `probe-passed`: the daily CI probe (`scripts/probe-native-import-latest.sh`) converted every tool pair in isolated stores, and the target tool exported, read, or parsed each written session without model calls.
- `read-observed`: sessions written by a version range were read. Source only.

Derived statuses (sessiongator computes them; the manifest never holds them):

- `bracketed`: between two `verified` versions of the same major version. Example: with `2.1.229` and `2.1.299` verified, `2.1.250` is bracketed. Bracketed counts as verified: both ends passed the full pass with the same store format.
- `untested`: same major version as a `verified` or `probe-passed` version, and not older than the oldest one. Conversion runs with a warning that names the nearest known older version.
- `unsupported`: no version detected, a major version without a recorded version, or older than the oldest known version of its major version.

Rules:

- Order of checks: `verified`, `bracketed`, `probe-passed`, `read-observed` (source only), `untested`, `unsupported`.
- `verified`, `bracketed`, and `read-observed` convert without a warning. `probe-passed` and `untested` convert with a warning.
- An unsupported target version fails closed with exit code `3`. `--allow-unsupported-version` turns that error into a warning.
- The source version is the version that wrote the session (Claude event `version`, opencode `session.version`, Codex `cli_version`, Copilot `copilotVersion`). When the session has none, the installed CLI version is used. Source support never blocks; a source session with no readable messages fails.
- The plan JSON reports `version`, `cliVersion`, `storeVersion`, `schemaFingerprint`, and `support` for `source` and `target`.
- An entry only moves up (`probe-passed` to `verified`), never down.

## Initial Supported-Version Matrix

Superseded by the manifest and `sessiongator compat`. OpenCode 1.x is `read-observed` only; sessiongator writes OpenCode 2.

## Mapping Rules

### Claude Code to opencode

| Claude source | opencode target | Policy |
| --- | --- | --- |
| Session id UUID | `session.id` as generated `ses_*`; original id in metadata | Do not force Claude UUID into opencode id unless user passes `--target-id` and it matches opencode prefix rules. |
| `cwd` | `session.directory`, `project.worktree`, `project_directory.directory` | Create/reuse project by cwd. |
| `gitBranch` | session metadata | Not built: read into metadata, not written to opencode. |
| `ai-title` / first user text | `session.title`, `session.slug` | Slug is derived from title. |
| Event `timestamp` | message/session `time_created`, `time_updated` | Preserve per event; synthesize monotonic times for untimestamped parts. |
| User text content | `session_message` type `user` with `text`, `files`, `agents` | File attachments map to `files` where possible. |
| Assistant text content | `session_message` type `assistant`, content item `text` | Preserve ordering within the assistant event. |
| Assistant `thinking` content | assistant content item `reasoning` | Preserve only as reasoning, not visible text. The signature survives only in `sessiongatorParts`. |
| `tool_use` block | assistant content item `tool` with completed/error state | Match by `id` / `tool_use_id`; a call without a result reads as interrupted (error). |
| `tool_result` block | completion/error state on matching tool content | A result without a matching call is kept only in `sessiongatorParts`, not in opencode content. Not built: the warning for it. |
| `toolUseResult` top-level and `tool-results/*.txt` | tool output/content/attachments | Not built: not read. |
| `attachment` events | user `files` or metadata | Not built: `attachment` events are not read; `file` content blocks map to `files`. |
| `file-history-snapshot` | session snapshot metadata | Not built: not read. Do not create fake patches. |
| `permission-mode`, `mode` | session permission/metadata | Not built: not read. |
| `system` events | `system` session messages | Visible system text maps; structured fields are not kept. |
| Subagent JSONL sidecars | imported sidecar metadata or synthetic messages | Not built: not read. Do not flatten into the main transcript. |
| Usage/cost/model | assistant tokens/cost/model and session totals | Model maps; tokens and cost are written as zero. |

### opencode to Claude Code

| opencode source | Claude target | Policy |
| --- | --- | --- |
| `session.id` | new Claude UUID file name; original id in metadata | Claude resume expects JSONL session id shape; use UUID unless proven otherwise. |
| `session.directory` | event `cwd`, encoded project folder | Always store real cwd in events; folder name is only storage layout. |
| `session.title` | `ai-title` event | Also use first user event fallback if title is empty. |
| `session.version` | metadata/provenance | Do not claim imported session was created by Claude version; use current target Claude version in event `version`, keep source version in provenance. |
| `session_message.user` | Claude `user` event with text/file content blocks | Preserve files as content blocks/attachment events when possible. |
| `session_message.assistant.content.text` | Claude `assistant.message.content[].text` | Preserve order. |
| `session_message.assistant.content.reasoning` | Claude `thinking` content block | Preserve provider metadata/signature when Claude accepts it; otherwise metadata sidecar and warning. |
| `session_message.assistant.content.tool` | Claude `tool_use` block | The call maps; the tool state output is not read back as a `tool_result` unless sessiongator wrote the session (`sessiongatorParts`). |
| `shell` messages | Claude user text `$ <command>` plus output | No Bash tool mapping; no warning. |
| `compaction` | Claude user text with the summary | Native exact mapping unknown. |
| `todo` rows | Claude task/todo sidecar if schema known | Not built: not read. |

## Safety Rules

- Always read source stores read-only.
- For opencode target writes:
  - Refuse if the database is locked by a live writer unless SQLite WAL-safe transaction and backup pass.
  - Use `BEGIN IMMEDIATE` transaction.
  - Insert all rows, then verify by reading through the normal opencode reader path.
  - Roll back on any failed verification.
  - Refuse a database without `session_v2`; never create 1.x tables.
  - With `--backup`, copy the database with `VACUUM INTO` first.
- For Claude target writes:
  - Never overwrite an existing JSONL unless `--force` is set.
  - Write to a temp file and atomic-rename into place.
  - Create sidecar directories only after transcript validation passes.
- For `--mark-source`, `--delete-source`, and `delete`:
  - Change the source only after the target write succeeds.
  - Record the operation in the history even when the source change fails, so `undo --target` can remove the new session.
- Imported sessions must include provenance metadata:
  - source tool/version/store fingerprint/session id
  - sessiongator version/commit when available
  - conversion timestamp
  - mapping profile version

## Fixture Strategy

Fixtures must be committed to git and sanitized. The manifest, the probe, and the full pass decide supported versions; fixtures pin reader and writer behavior.

As built: one fixture, `claude/2.1.199/basic-text` (text, reasoning, tool call, tool result), with `expected-opencode/plan-summary.json` checked by `fixture_plan_matches_the_expected_summary`. Other tools build test stores inline. Proposed layout:

```text
fixtures/native-import/
  claude/2.1.199/basic-text/
    source/projects/-tmp-demo/<uuid>.jsonl
    expected-opencode/plan.json
    expected-opencode/readback.json
  claude/2.1.199/tool-use/
  claude/2.1.199/attachments/
  claude/2.1.199/subagent-sidecar/
  opencode/1.17.13/basic-text/
    source/opencode.db
    expected-claude/plan.json
    expected-claude/readback.json
  opencode/1.17.13/tool-use/
  opencode/1.17.13/reasoning/
  opencode/1.17.13/compaction/
```

Fixture rules:

- No real prompts, real paths, tokens, emails, URLs with secrets, or company data.
- Use deterministic ids and timestamps.
- Include one fixture per mapping category.
- Include at least one fixture with unknown/extra fields to verify forward-compatible preservation/reporting.
- Include schema fingerprint files generated from each fixture target store.
- Expected outputs assert structure, mapping reports, and resumability/readback, not byte-for-byte DB layout unless that layout is the contract.

## CI Strategy

`.github/workflows/native-import-compat.yml`:

- `fixtures` job, on every push and pull request: `cargo test -- convert:: stores:: trash:: history::`.
- `latest-tools` job, daily and on demand:
  - Install the latest `@anthropic-ai/claude-code`, `@opencode/cli` (OpenCode 2), `@openai/codex`, and `@github/copilot`.
  - Run `scripts/probe-native-import-latest.sh`: convert the Claude fixture to every tool, then every written session to every other tool. Every opencode command runs with `--standalone` and private `XDG_*` directories, because without `--standalone` OpenCode 2 talks to the shared background service and its real database.
  - Check each written session with the target tool, without model calls: `opencode session export` decodes it, Codex app-server `thread/read` projects a non-empty turn, `copilot --resume=<id>` parses it without credentials, and sessiongator reads Claude targets back.
  - Record the installed versions as `probe-passed` with `scripts/update-native-import-versions.mjs` and open a pull request. Never commit to `main` directly.

## Full Pass

`scripts/full-pass-native-import.sh` runs on a machine with logged-in clients (a scheduled T3 task on the VPS):

- Write a Claude source session whose only secret is a random code word.
- Convert it to each tool, resume the converted session with the real client and a real model, and ask for the word.
- A tool passes when sessiongator reads the session back and the last assistant message holds the word. This checks that the tool loaded the imported history and that sessiongator reads what this tool version wrote.
- Check each tool alone. Each tool has its own result: `passed`, `failed`, or `skipped`.
- A tool is `skipped` when its client cannot take part: no login, no model setting, or an error reported by the client itself, such as a usage limit. A skipped tool says nothing about sessiongator. It is not recorded.
- A tool is `failed` when the client ran and the reply did not hold the code word, or when a conversion or read-back failed. Only a failed tool makes the script exit with a non-zero status.
- Record passing versions as `verified` and open a pull request. A failed or skipped tool never blocks the record of the other tools.
- Stores: Claude uses its real store and the session moves to a temp trash afterwards, because a copied login could rotate the real refresh token. Codex uses `FULL_PASS_CODEX_HOME`, which must hold a Codex login. opencode and Copilot use temp stores; the opencode login is imported from `FULL_PASS_OPENCODE_AUTH_DB`.
- Limit: OpenCode's free models reject resumed imported sessions (`FreeTierError`, HTTP 403), even though OpenCode loads the session; a native session with an appended sessiongator message is accepted. So the opencode check needs a provider login and `FULL_PASS_OPENCODE_MODEL`. Revisit when OpenCode documents what the free tier checks.

## Implementation Phases

1. ✅ Add version detection and schema fingerprinting.
2. ✅ Add sanitized fixture format and fixture test harness.
3. ✅ Add native readers that produce `NativeSession`; see Mapping Rules for fields not read.
4. ✅ Add mapping/report generation with dry-run.
5. ✅ Add opencode writer.
6. ✅ Add Claude writer.
7. ✅ Add live-store writes: immediate transactions, atomic renames, readback, and `undo` instead of backups.
8. ✅ Add CLI `convert` command.
9. ⬜ Add TUI conversion dialog; the `Ctrl+T` keybinding places a dry-run command.
10. ✅ Add daily latest-tool CI and the full pass.

## Acceptance Criteria

- `sessiongator convert --dry-run` reports exactly what will map, drop, and synthesize.
- Supported versions are loaded from a checked-in manifest.
- Unsupported versions fail closed by default.
- Fixture tests cover text, reasoning, and tool use/result; attachments, compaction, todo, and sidecar cases are not covered yet.
- Every write is read back: title, cwd, and the count of user and assistant text messages must match, else the written session moves to the trash.
- Live writes verify readback before success; `undo --target` removes a written session.
- TUI conversion is available without breaking current resume/path/copy behavior.
- Daily CI records latest-tool round trips as `probe-passed` in a reviewable pull request; only the full pass records `verified`.

## Open Questions

- Claude Code does not document JSONL as a stable import API. The full pass resumes an imported session with `claude -p --resume <id>` and a real model for each verified version.
- opencode may rely on event projection tables beyond direct session/message rows. Confirm whether writing canonical `session_message` plus event rows is required for current and future resume flows. For `2.0.18`: `session_message` plus `event_sequence` is enough for `session export`; no `event` rows are written.
- Decided: imported hidden reasoning is preserved by default as the target's reasoning part (Claude `thinking`, opencode `reasoning`). Copilot keeps it only in the hidden `sessiongatorParts` field; Codex gets text only. No `--include-reasoning` flag.
- Decide whether tool results that include local file paths should copy sidecar files into the target store or reference original paths.
