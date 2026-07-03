# Native Session Import Spec

## Goal

Add a native conversion/import feature that can take one session from a source AI coding tool, convert every representable field into the target tool's native storage format, save it safely, and expose the flow through both CLI and TUI.

Initial tools:

- Claude Code
- opencode

The conversion must be version-gated. Unsupported or unknown source/target versions must fail closed unless the user explicitly opts into an experimental conversion.

## Current Research Snapshot

Research date: 2026-07-03.

Local versions inspected:

- Claude Code: `2.1.199`
- opencode: `1.17.13`

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
- opencode retains legacy `message`/`part` rows and newer `session_message` rows. Import must write whichever format the target version actually reads for resume, and should populate compatibility projections when required by tested versions.
- Common `message.data` keys: `role`, `parentID`, `agent`, `modelID`, `providerID`, `mode`, `path`, `cost`, `tokens`, `tools`, `summary`, `finish`, `error`
- Common `part.data` types: `text`, `reasoning`, `tool`, `patch`, `file`, `step-start`, `step-finish`, `compaction`
- Current upstream `session_message` types: `agent-switched`, `model-switched`, `user`, `synthetic`, `system`, `shell`, `assistant`, `compaction`

## Non-Goals

- Do not promise semantic losslessness. Tool runtimes have private state and replay semantics that are not identical.
- Do not write into a live opencode database without an explicit lock/backup strategy.
- Do not import hidden reasoning into target-visible transcript text. Preserve it only when the target has a native hidden/reasoning field.
- Do not fabricate successful tool execution if the source only contains a pending/interrupted tool call.
- Do not support unknown tool versions by default.

## User-Facing Behavior

### CLI

Add a `convert` command:

```sh
sessiongator convert --id <session-id> --from <claude|opencode> --to <claude|opencode> [options]
```

Options:

- `--target-store <path>`: target config root or database. Defaults to the target tool's normal store.
- `--source-store <path>`: source config root or database. Defaults to the source tool's normal store.
- `--target-id <id>`: explicit target session id. Default: generate a native id.
- `--cwd <path>`: override target session working directory. Default: source session cwd.
- `--title <title>`: override target title. Default: mapped source title.
- `--dry-run`: parse, validate, map, and print a conversion report without writing.
- `--plan-json`: print the conversion plan as JSON for CI/golden tests.
- `--report-json`: print the post-write verification report as JSON.
- `--backup`: create target-store backup before writing. Default true for live stores.
- `--no-backup`: only allowed with `--target-store` pointing to an isolated fixture/temp store.
- `--force`: overwrite an existing target session id. Default false.
- `--allow-unsupported-version`: run when source/target versions are unknown, marking the imported session as experimental.
- `--include-sidecars`: include Claude tool-result/subagent sidecars where a target-native mapping exists.
- `--redact-fixture`: export a sanitized fixture from the selected session instead of importing.

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
- `4`: lossy field would be dropped without an explicit policy.
- `5`: target store is live/locked and cannot be safely written.

### TUI

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

Add modules:

```text
src/import/
  mod.rs
  versions.rs
  model.rs
  report.rs
  fixtures.rs
  claude_reader.rs
  claude_writer.rs
  opencode_reader.rs
  opencode_writer.rs
  mapping.rs
```

Key types:

```rust
enum ImportTool { Claude, Opencode }

struct ToolVersion {
    tool: ImportTool,
    cli_version: Option<String>,
    store_version: Option<String>,
    schema_fingerprint: String,
}

struct NativeSession {
    tool: ImportTool,
    id: String,
    title: String,
    cwd: String,
    created_ms: i64,
    updated_ms: i64,
    model: Option<ModelRef>,
    messages: Vec<NativeMessage>,
    attachments: Vec<NativeAttachment>,
    todos: Vec<NativeTodo>,
    sidecars: Vec<NativeSidecar>,
    provenance: Provenance,
}

enum NativeMessage {
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    Tool(ToolMessage),
    Shell(ShellMessage),
    Compaction(CompactionMessage),
    Unknown(UnknownMessage),
}

struct ConversionReport {
    source: ToolVersion,
    target: ToolVersion,
    supported: bool,
    mapped: Vec<FieldMapping>,
    dropped: Vec<FieldDrop>,
    synthesized: Vec<SynthesizedField>,
    warnings: Vec<String>,
    errors: Vec<String>,
}
```

The existing `sources::SessionSource` stays read-only and summary-oriented. Native import should not overload it. It needs a richer `NativeReader` trait because import must preserve fields that the TUI preview intentionally ignores.

```rust
trait NativeReader {
    fn detect_version(&self) -> Result<ToolVersion, ImportError>;
    fn read_native_session(&self, id: &str) -> Result<NativeSession, ImportError>;
}

trait NativeWriter {
    fn detect_version(&self) -> Result<ToolVersion, ImportError>;
    fn validate_target(&self, plan: &ConversionPlan) -> Result<(), ImportError>;
    fn backup(&self) -> Result<Option<PathBuf>, ImportError>;
    fn write_native_session(&self, session: &NativeSession) -> Result<WriteReceipt, ImportError>;
    fn verify_written_session(&self, receipt: &WriteReceipt) -> Result<ConversionReport, ImportError>;
}
```

## Version Policy

Version support lives in `docs/specs/native-session-import-versions.toml` until implementation. During implementation, copy or parse that file from runtime-visible assets.

Version checks must include:

- CLI version command output, when available.
- Store version fields, when available.
- Schema fingerprint derived from table names, column names/types/nullability, and migration names for SQLite stores.
- Fixture compatibility hash for sanitized saved examples.

Default behavior:

- Exact known-good versions are allowed.
- Patch versions may be allowed only when schema fingerprint and fixture round-trip checks match a known-good version.
- Unknown versions fail closed with remediation text.
- `--allow-unsupported-version` turns unsupported-version errors into warnings but still blocks destructive writes unless `--target-store` is isolated or `--backup` succeeds.

## Initial Supported-Version Matrix

This matrix is not a claim that import is implemented today. It is the initial compatibility target for implementation and CI fixtures.

| Tool | Version | Store/schema | Status | Notes |
| --- | --- | --- | --- | --- |
| Claude Code | `2.1.199` | JSONL under `~/.claude/projects`, active session JSON under `~/.claude/sessions` | target-supported | Local schema sampled; CLI docs include `claude -r <session>` resume. Native import must be fixture-verified because transcript format is not documented as stable. |
| opencode | `1.17.13` | SQLite with `session`, `message`, `part`, `session_message`, `session_input`, `session_context_epoch` | target-supported | Local DB schema and upstream Drizzle schema sampled. Writer must prefer current `session_message` model and maintain compatibility projections if required. |
| opencode | `1.17.9` to `1.17.13` | Same local migration family observed | read-observed | Existing local sessions exist; write support requires fixture verification per version. |
| opencode | `1.14.22` to `1.14.48` | Local rows observed in current DB after migrations | read-observed | Historical session versions can be read from migrated DB; do not write new sessions as these versions. |
| opencode | `1.1.17` to `1.2.27` | Local rows observed in current DB after migrations | read-observed | Historical only. |

## Mapping Rules

### Claude Code to opencode

| Claude source | opencode target | Policy |
| --- | --- | --- |
| Session id UUID | `session.id` as generated `ses_*`; original id in metadata | Do not force Claude UUID into opencode id unless user passes `--target-id` and it matches opencode prefix rules. |
| `cwd` | `session.directory`, `project.worktree`, `project_directory.directory` | Create/reuse project by cwd. |
| `gitBranch` | session metadata | Preserve as metadata; opencode project has no direct branch column. |
| `ai-title` / first user text | `session.title`, `session.slug` | Slug is derived from title. |
| Event `timestamp` | message/session `time_created`, `time_updated` | Preserve per event; synthesize monotonic times for untimestamped parts. |
| User text content | `session_message` type `user` with `text`, `files`, `agents` | File attachments map to `files` where possible. |
| Assistant text content | `session_message` type `assistant`, content item `text` | Preserve ordering within the assistant event. |
| Assistant `thinking` content | assistant content item `reasoning` | Preserve only as reasoning, not visible text. Include provider metadata/signature when present. |
| `tool_use` block | assistant content item `tool` with pending/running/completed/error state | Match by `id` / `tool_use_id`. |
| `tool_result` block | completion/error state on matching tool content | If no matching tool call exists, create synthetic tool result warning in metadata and report. |
| `toolUseResult` top-level and `tool-results/*.txt` | tool output/content/attachments | Preserve text, output paths, and attachment references where opencode supports them. |
| `attachment` events | user `files` or metadata | Preserve file name/mime/url/path where available. |
| `file-history-snapshot` | session snapshot metadata | Do not create fake patches. Preserve raw snapshot metadata. |
| `permission-mode`, `mode` | session permission/metadata | Map known permission modes only; unknown values become metadata. |
| `system` events | `system` session messages or metadata | Preserve visible system text; preserve structured fields in metadata. |
| Subagent JSONL sidecars | imported sidecar metadata or synthetic messages | Do not flatten into main transcript unless target supports subtask/subagent semantics. |
| Usage/cost/model | assistant tokens/cost/model and session totals | Sum when target expects session totals. |

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
| `session_message.assistant.content.tool` | Claude `tool_use` / paired `tool_result` blocks | Convert completed/error/pending states; pending imports should be marked interrupted to avoid dangling tool calls. |
| `shell` messages | Claude tool use/result for Bash when possible | Otherwise preserve as synthetic user-visible text with metadata warning. |
| `compaction` | Claude summary/compaction-equivalent metadata | Native exact mapping unknown; preserve as metadata and optional visible synthetic summary. |
| `todo` rows | Claude task/todo sidecar if schema known | Otherwise preserve in metadata sidecar and report as not natively mapped. |

## Safety Rules

- Always read source stores read-only.
- For opencode target writes:
  - Refuse if the database is locked by a live writer unless SQLite WAL-safe transaction and backup pass.
  - Use `BEGIN IMMEDIATE` transaction.
  - Insert all rows, then verify by reading through the normal opencode reader path.
  - Roll back on any failed verification.
  - Create a timestamped `.bak` beside the DB for live stores.
- For Claude target writes:
  - Never overwrite an existing JSONL unless `--force` is set.
  - Write to a temp file and atomic-rename into place.
  - Create sidecar directories only after transcript validation passes.
- Imported sessions must include provenance metadata:
  - source tool/version/store fingerprint/session id
  - sessiongator version/commit when available
  - conversion timestamp
  - mapping profile version

## Fixture Strategy

Fixtures must be committed to git and sanitized. They are the source of truth for supported versions.

Proposed layout:

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

Add `.github/workflows/native-import-compat.yml`:

```yaml
name: native import compatibility

on:
  pull_request:
  push:
    branches: [main]
  schedule:
    - cron: "17 5 * * *"
  workflow_dispatch:

jobs:
  fixtures:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test native_import -- --nocapture

  latest-tools:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: Install latest opencode
        run: |
          # Use the official install path chosen during implementation.
          # Capture `opencode --version` into artifacts/latest-tools.json.
          true
      - name: Install latest Claude Code
        run: |
          # Claude install may require auth/license constraints. If unavailable,
          # mark as skipped and keep fixture tests authoritative.
          true
      - run: cargo test native_import_latest -- --nocapture
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: native-import-latest-report
          path: artifacts/native-import-latest/**
```

Daily CI responsibilities:

- Install latest available tool versions where licensing/auth allows.
- Generate fresh minimal sessions in isolated HOME/XDG dirs.
- Run `sessiongator convert --dry-run --plan-json` in both directions.
- Run fixture imports into isolated target stores.
- Verify target tool can list/resume or at least parse/read back imported sessions without crashing.
- Compare detected schemas against the known-version manifest.
- If latest passes but is unknown, fail with a report that includes:
  - detected versions
  - schema fingerprint
  - fixture result summary
  - exact manifest entry to add after review
- If known versions fail, fail hard.

## Implementation Phases

1. Add version detection and schema fingerprinting.
2. Add sanitized fixture format and fixture test harness.
3. Add native readers that produce `NativeSession` without dropping known fields.
4. Add mapping/report generation with dry-run only.
5. Add opencode writer into isolated DB fixtures.
6. Add Claude writer into isolated config fixtures.
7. Add live-store backup/locking and write support.
8. Add CLI `convert` command.
9. Add TUI conversion dialog and keybinding.
10. Add daily latest-tool CI.

## Acceptance Criteria

- `sessiongator convert --dry-run` reports exactly what will map, drop, and synthesize.
- Supported versions are loaded from a checked-in manifest.
- Unsupported versions fail closed by default.
- Fixture tests cover text, reasoning, tool use/result, attachments, compaction, todo, and sidecar cases.
- Native target readback matches the conversion report for every fixture.
- Live writes create backups and verify readback before success.
- TUI conversion is available without breaking current resume/path/copy behavior.
- Daily CI validates latest known tool versions and produces actionable reports for unknown passing versions.

## Open Questions

- Claude Code does not document JSONL as a stable import API. Before enabling Claude target writes to live stores by default, verify imported fixtures can be resumed with `claude -r <id>` on each supported version.
- opencode may rely on event projection tables beyond direct session/message rows. Confirm whether writing canonical `session_message` plus event rows is required for current and future resume flows.
- Decide whether imported hidden reasoning should be preserved by default, stripped by default, or controlled by `--include-reasoning`.
- Decide whether tool results that include local file paths should copy sidecar files into the target store or reference original paths.
