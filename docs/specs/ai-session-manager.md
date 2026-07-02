# AI Session Manager (`sessiongator`)

Status: **implemented** (phases 1–5; provider traits deferred). See
"As-Built Notes" at the end for where the implementation deviates and why.

## Goals

- Add a third navgator implementation: a TUI to **browse, search, and resume**
  every local [Claude Code](https://docs.claude.com/en/docs/claude-code) and
  [opencode](https://opencode.ai) coding session from one window.
- Prove the provider/compositor model (see
  [provider-compositor-architecture.md](./provider-compositor-architecture.md))
  on a **non-file-backed, multi-source** domain: results come from a JSONL log
  tree *and* a SQLite database, not the filesystem.
- Reuse `gator` (`ensure_tty_stdin`, `write_selection`,
  `copy_to_clipboard`) and the `issuegator` crate shape (self-contained
  binary, `mpsc` streaming, `TerminalGuard`, ratatui).
- Selecting a session **resumes it in its original working directory** via the
  shell wrapper — the navgator "return a selection, let the wrapper act" model.
- **Standalone and self-contained.** No dependency on, or interop with, any
  other session tool: it owns its own caches (if any) under a navgator cache
  namespace and reads only the AI tools' native stores directly.

## Non-Goals (first implementation)

- No editing/merging/deleting sessions. Read-only, except our own cache.
- No opencode legacy file-store reading (`storage/{session,message,part}/…` is
  stale/pre-migration). SQLite DB only.
- No cloud sync, no web UI.
- No dynamic plugin loading. New crate, compiled in, like the other two.

## Data Sources (ground truth, verified on disk)

Parsing truth is always the **content of the file/row**, never a decoded
directory name.

### Claude Code

- **Location:** `~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`, one
  file per session; stem is the session id. Root honors `$CLAUDE_CONFIG_DIR`.
- 🔴 `<encoded-cwd>` replaces every `/` in the cwd with `-`, which is **lossy**
  (dir names containing `-` are ambiguous). **Never reverse-decode the folder;**
  read the real `cwd` from inside the JSONL.
- **Format:** newline-delimited JSON, one event per line. `type` ∈ `user`,
  `assistant`, `system`, `summary`, `ai-title`, `attachment`,
  `file-history-snapshot`, `last-prompt`, `mode`, `permission-mode`,
  `queue-operation`.
- `user`/`assistant` events carry `sessionId`, `uuid`, `parentUuid`, `cwd`,
  `gitBranch`, `timestamp` (ISO-8601 UTC), and `message`.
- **Title:** `ai-title` event's `aiTitle` field; else first `user` **string**
  content (skip `tool_result` user events), truncated. Titles are
  whitespace-normalized (newlines/tabs collapsed) so they are always safe for
  one-line rows and tab-separated `--list` output.
- **Model:** `assistant.message.model` (e.g. `claude-opus-4-8`).
- **Searchable text:** `message.content` string, or `content[].text` where
  `type == "text"`. `thinking`/`tool_use`/`tool_result` blocks are excluded from
  the search index.

### opencode

- **Storage:** SQLite at `~/.local/share/opencode/opencode.db` (WAL mode),
  overridable via `$OPENCODE_DB`; `$XDG_DATA_HOME` honored. **Open read-only**
  (`mode=ro`, busy timeout); live opencode may be writing concurrently.
- **Hierarchy:** `project → session → message → part`.
  - `session(id, project_id, parent_id, directory, title, agent, model,
    time_created, time_updated, tokens_input, tokens_output, …)`. `model` is a
    JSON blob `{"id": "...", "providerID": "..."}` — parse the `id`.
  - `message(id, session_id, data)` — `data` JSON `{role, time, modelID, path:{cwd,root}, …}`.
  - `part(id, message_id, session_id, time_created, data)` — `data` JSON with
    `type` ∈ `text, reasoning, tool, patch, step-start, step-finish, file,
    compaction`. **`text` parts hold the searchable prompts/replies.**
- Parse `part.data`/`message.data` JSON in Rust (do not rely on SQLite `json_extract`).
- `session.parent_id` marks agent/child sessions.

## Unified Session (domain model)

Each source normalizes to one record (the `ResultEntry` payload). The core
model carries **only fields every source can provide** — nothing specific to
one tool:

```
Session {
  tool:        Tool                 // source discriminant (Claude | Opencode | future)
  id:          String               // native id; result id = "<tool>:<native id>"
  title:       String
  cwd:         String               // absolute working directory (from content)
  created:     i64                  // epoch millis, UTC
  updated:     i64
  message_count: u32
  model:       Option<String>
  source_ref:  String               // where the session lives on disk
  extras:      Vec<(String, String)> // adapter-supplied, display-ready extras
}
```

Anything one source knows and another doesn't (branch, agent, parent session,
token counts, cost, …) goes into the generic `extras` list — adapters emit
whatever facts they have as display-ready key/value pairs, and the UI renders
whichever keys are present. The core model, search, sorting, and the compositor
never name a tool-specific field. (The architecture spec's typed `MetadataMap`
arrives with the provider-trait phase; the concrete phase keeps a plain list.)

Content (transcript text) is loaded lazily per session, never held in the list.

## Mapping Onto The navgator Architecture

The provider/compositor model maps 1:1. First implementation may keep providers
**concrete** (as `issuegator` does) and adopt the traits later (per the
architecture spec's Phase 3 → 4).

### `SessionResultsProvider` (ResultsProvider)

- Builds the result set from all registered adapters, merged and sorted. Emits
  `ResultEntry` per session: `id = "<tool>:<native id>"`, `display` = the
  aligned row, `metadata` = the universal fields plus whatever extras the
  adapter supplied.
- **Search** interprets the query for this domain:
  - Default (sessions mode): every whitespace token must be a case-insensitive
    **substring** of the name + full path + model blob. 🔴 Not
    `gator::fuzzy_match`: subsequence matching over long path+title
    blobs matches nearly everything (measured 338/395 for "dotfiles" vs 33/395
    with substring) — fuzzy is right for navigate's short folder names, wrong
    here.
  - "Search all" mode: a session matches if its **message content** matches
    (content search) **OR** its name/path/model matches — title/dir hits are
    never dropped just because the body differs. See Search below.
- **Sort modes** (compositor `Ctrl+S`, reusing the results-panel sort key):
  `updated → created → messages → path`.
- `row_spec`: glyph per tool, relative `updated`, `~`-shortened cwd, title,
  dim `<msgs>m <model>` tail.

### `SessionMetadataProvider` (MetadataProvider)

- Emits typed metadata for row rendering/sorting: `updated`/`created`
  (`DateTime`), `message_count` (`Number`), `model` (`Text`), plus any
  adapter-specific extras as generic `MetadataEntry` values — the provider and
  UI treat all keys uniformly and never special-case a tool.
- Adapters whose metadata needs a file scan load **async** on the session-load
  worker (no persistent cache — see As-Built Notes); DB-backed adapters are one
  cheap query.
- Future: a tag metadata provider (tag sessions in a sidecar file), reusing the
  navigate `Ctrl+T` tag-edit pattern.

### `TranscriptContentProvider` (ContentProvider, primary panel)

- Supports any session `ContentTarget`. Loads the transcript **async** and
  renders role-labeled, styled turns (user vs assistant). Emits `Loading`
  immediately, then the loaded transcript.
- **Highlight + scroll-to-match:** whenever the query is non-empty (in *both*
  search modes), occurrences in the transcript render in inverse video and the
  panel auto-scrolls to the first match (with 2 lines of context). Auto-scroll
  applies once per `(session, query)` so manual scrolling is never overridden;
  it re-applies when the query or selection changes, and fires when a
  still-loading transcript arrives. Scroll offsets count logical lines, so
  heavy wrapping can place the match lower in the viewport — it is always
  brought on-screen (same approximation as navigate/issues).
- Claude: stream `user`/`assistant` text from the JSONL in order. opencode:
  `SELECT … FROM part JOIN message … WHERE session_id=? ORDER BY time_created`,
  `type='text'` (optionally `reasoning`).
- Optional sub-targets: one `ContentTarget` per turn for jump-to-match, or a
  future "raw / text-only / +reasoning" tab set.

### `SessionInfoContentProvider` (ContentProvider, bottom tab — optional, not built)

- For the selected session's `cwd`: git branch/status (via
  `run_command_output` with `NO_COLOR`), model, tokens/cost, created/updated,
  source path. Lets you see whether the working directory still exists before
  resuming.

### Compositor

- Layout: results panel (left) + `TranscriptContentProvider` (right, ~60%), with
  an optional bottom tabbed panel (`SessionInfoContentProvider`).
- Owns sort (`Ctrl+S`), clear-input (`Ctrl+U`), search-mode toggle, focus/scroll.
- **Focus model (mirrors navigate):** `List` vs `Transcript`. `→` with the
  input cursor at its end moves focus into the transcript; `←` (or `↑` at the
  top of the transcript) moves back. The focused panel gets the accent border;
  the input cursor is hidden while the transcript is focused. Mouse click on a
  panel focuses it.
- Keybindings (route to providers / built-ins):
  - `Enter` → **resume** the selected session (selection contract below).
  - `Ctrl+F` → toggle "search all" (name+path+content); `Ctrl+F` again back.
  - `Ctrl+S` → cycle sort. `Ctrl+Y` → copy session id (`copy_to_clipboard`).
  - `Ctrl+O` → return the source path instead of resuming (scripting).
  - `↑`/`↓` → move selection (list focus) / scroll transcript (transcript
    focus). `PgUp`·`PgDn` scroll the transcript from either focus;
    `Home`/`End` jump top/bottom in transcript focus (`Ctrl+Home`/`End`
    globally). `Esc` quits.
- Typing costs one filter pass per frame, not one per keystroke: the event
  loop drains all pending events before refiltering (content matching scans
  every indexed transcript, so per-key filtering would lag).

## Selection & Resume Contract

navgator returns a selection via `gator::write_selection` (to
`$GATOR_OUTPUT` or stdout); the shell wrapper acts on it. Resume is more than
a path, so define a small typed line the wrapper interprets:

- `SelectionValue` for a session:
  `ProviderSpecific { provider_id: "sessions", value: "resume\t<tool>\t<id>\t<cwd>" }`
  (or `path\t<source_ref>` for `Ctrl+O`).
- Wrapper `sessiongator.zsh` (bound to a zsh key, e.g. `Ctrl+S`) parses
  the value and runs:
  - claude: `cd <cwd> && exec claude --resume <id>`
  - opencode: `exec opencode <cwd> --session <id>`
  - if `<cwd>` is gone: warn and resume from `$HOME`.

Verified resume shapes: `claude --resume <session-id>` (run in the session cwd);
`opencode <directory> --session <session-id>`.

Rationale: the binary stays pure (returns a value); the wrapper performs the
`cd`/`exec` so the resumed CLI lands in the user's terminal — same division of
labor as `navgator` returning a path the wrapper `cd`s into.

## Search

- **Content index.** In-process and self-contained: after the session list
  loads, one background worker extracts each session's searchable text
  (lowercased `text` content) and streams `(key, text)` entries over `mpsc`
  into an in-memory map. The list header shows `indexing…` until done, and
  active content searches live-refilter as entries arrive. The index is built
  once per run; matching is a case-insensitive substring scan over the map,
  done on the main thread once per frame (fast in practice; the event loop
  drains keystrokes so it is one scan per frame, not per key).
- Optional persistence (not built): spill extracted text to
  `$XDG_CACHE_HOME/navgator/sessions/` keyed by `<tool>/<id>` + `updated`, so
  cold starts skip re-extraction. Navgator-owned path; no external tools
  (no `rg` dependency) and no shared caches with anything else.
- The results provider unions content hits with name/path/model matches so
  "search all" is a superset (never hides a title/dir match).
- Sessions-mode matching is substring-per-token over the full (untruncated)
  `cwd + ~cwd + title + model` blob (see the search rationale above).

## Config (not built — zero-config only)

- Zero-config default. `$CLAUDE_CONFIG_DIR`, `$OPENCODE_DB`, `$XDG_DATA_HOME`
  are honored. A config file is future work if needed:
- Optional `figment`/`schemars` config
  (`~/.config/navgator/sessions.toml` or a `[sessions]` table):
  `claude_root`, `opencode_db` overrides, `default_sort`, `default_tool_filter`,
  `include_reasoning`. Honor `$CLAUDE_CONFIG_DIR`, `$OPENCODE_DB`,
  `$XDG_DATA_HOME`, `$XDG_CACHE_HOME`.

## Dependencies

- Workspace already provides: `crossterm`, `ratatui`, `tui-input`, `serde`,
  `serde_json`, `figment`, `schemars`, `libc`, `gator`.
- **New:** `rusqlite` (with the `bundled` feature) for the opencode DB — the
  first navgator crate to need SQLite. Keep it isolated to the opencode adapter
  module. Add to `[workspace.dependencies]` and reference it only here.
  🔴 Pinned to **0.37**: rusqlite 0.38+ pulls libsqlite3-sys ≥0.38 whose build
  script uses `cfg_select!`, requiring Rust ≥1.94 (toolchain here is 1.93).
  Bump when the toolchain moves.
- Timestamps: keep epoch `i64` and format manually (as `issuegator` does
  with `short_timestamp`) to avoid a `chrono` dependency.

## Infra Changes Required

Everything below is one-time wiring; day-to-day releases stay fully automatic.

1. ✅ **Workspace (`Cargo.toml`).** `"crates/sessiongator"` added to
   `[workspace.members]`; `rusqlite = { version = "0.37", features = ["bundled"] }`
   in `[workspace.dependencies]`. `bundled` compiles SQLite into the binary, so
   no new system dependency for CI, Homebrew, or source builds — only added
   compile time.
2. ✅ **CI (`.github/workflows/ci.yml`).** No change needed. `cargo
   fmt/test/build` run `--workspace` and pick the new crate up automatically;
   the release flow (tag → release → `brew bump-formula-pr`) is crate-agnostic.
3. ⬜ **Homebrew formula (tap `Formula/navgator.rb`) — one-time manual edit.**
   The install block cargo-installs each crate explicitly, and the automated
   `bump-formula-pr` only rewrites `url`/`sha256`. When the crate ships, add:
   `system "cargo", "install", *std_cargo_args(path: "crates/sessiongator")`
   and `pkgshare.install "scripts/sessiongator.zsh"`, and mention the new
   widget in `caveats`.
4. ✅ **Wrapper script.** `scripts/sessiongator.zsh` (the existing
   `navgator.zsh` is navigate-specific): binary lookup order
   `$SESSIONGATOR_BIN` → `sessiongator` on `PATH` →
   `target/release` → `target/debug`; zle widget **`ai-sessions`** runs the
   picker, parses the selection line, and performs the resume `cd` + command.
5. ⬜ **Dotfiles.** Point the session keybinding (e.g. `^s`) at the
   `ai-sessions` widget; the dotfiles `gh_source` line already builds
   `--release --workspace`, so the new binary appears without changes there.
6. ✅ **AGENTS.md.** "Project Shape" updated to three implementation crates.

## Module Layout (as built)

Mirrors `issuegator` (self-contained) with adapter separation:

```text
crates/sessiongator/
  Cargo.toml
  src/
    main.rs        // CLI dispatch (--list/--help) → select_session() → write_selection()
    model.rs       // Session, Tool, SortMode, clean_title, epoch↔civil time helpers
    sources/
      mod.rs       // SessionSource trait: tool(), available(), list(), transcript(id)
      claude.rs    // JSONL scan (meta + transcript), corrupt-line tolerant
      opencode.rs  // rusqlite read-only adapter (mode=ro, busy_timeout)
    search.rs      // SearchMode, substring filter, content-map matching
    ui.rs          // layout, list rows, transcript render + highlight/first-match
    session.rs     // event loop, focus model, 3 mpsc workers, selection lines
```

(The draft's separate `content.rs` folded into `ui.rs` — transcript rendering
is one function; a module for it was indirection without duplication.)

Adding a third AI tool later = one new file under `sources/` plus a line in
`sources_from_env()`.

## Async & Responsiveness Rules

Follow the architecture spec and the `issuegator` pattern:

- Event loop never runs external commands or DB/file scans directly.
- Session list loads fast (metadata first); Claude scans stream in on an `mpsc`
  channel; opencode is one query.
- Transcript loads async per selection (`spawn_transcript_load`, like
  `spawn_issue_detail_load`); emit `Loading` immediately.
- Content **extraction** runs on a worker streaming `(key, text)` entries;
  matching is a per-frame in-memory scan on the main loop (events drained, one
  scan per frame). Late-arriving entries just refilter the active query.
- Transcript results are keyed by session, so late arrivals apply safely.

## Implementation Phases

1. ✅ **Adapters + list.** `Session` model, Claude + opencode adapters, merged
   sorted list, `sessiongator --list` sanity dump. Unit-tested against
   fixtures (a tiny JSONL tree + a generated fixture DB).
2. ✅ **TUI browse.** ratatui list, substring filter, sort cycle,
   `TerminalGuard`, `write_selection` of the source path (`Ctrl+O`).
3. ✅ **Transcript panel.** Async transcript loading, styled role turns,
   scroll, focus model.
4. ✅ **Search all.** In-process content index, union semantics, query
   highlight + scroll-to-match (both modes).
5. ✅ **Resume.** Selection-line contract + `sessiongator.zsh` wrapper
   (`ai-sessions` widget). Zsh keybinding wiring is the dotfiles step in Infra.
6. ⬜ **(Optional) Provider traits.** Once boundaries are stable, express the
   adapters as `ResultsProvider`/`ContentProvider`/`MetadataProvider` and share
   the compositor with `navgator` per the architecture spec.

## Verification

- ✅ `cargo fmt -- --check`, `cargo check --workspace`,
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
  `cargo test --workspace` (20 crate tests), `cargo build --release --workspace`.
- ✅ Behavior tests (in-crate):
  - Claude: `aiTitle` vs first-user-string title; `cwd` read from content, not
    the folder name; text extraction excludes `thinking`/`tool_result`;
    corrupt JSONL lines skipped without dropping the session.
  - opencode: `model` JSON parsing; text-only transcript order; missing DB
    unavailable; fixture DB round-trip.
  - Search: sessions-mode path/title matching; all-mode union with content;
    sort orderings; ISO↔epoch round-trip incl. leap day.
  - Selection: exact `resume`/`path` lines for both tools; UI: truncation,
    highlight spans (incl. multi-byte safety), first-match line index.
- ✅ Live verification (tmux, real stores — 395 sessions): list + transcript
  render, path filter, sort cycle, content search live-updating during
  indexing, highlight + scroll-to-match while moving between results, focus
  borders/arrows, `Enter`/`GATOR_OUTPUT` selection lines, `--list` in
  ~0.16s (release). Missing-cwd fallback lives in the wrapper (untested
  end-to-end — needs a session whose directory was deleted).

## Design Guardrails

- Read-only against both stores; open the opencode DB `mode=ro`.
- Core model must not assume files: a session id is a string, resume is a
  `SelectionValue`, not a path.
- The compositor must not know Claude/opencode internals — adapters own their
  domain; the UI renders semantic `ContentBlock`s and metadata.
- Prefer explicit structs over stringly-typed maps; keep `rusqlite` confined to
  the opencode adapter.
- Background workers idempotent and safe to apply late; never block the loop.

## As-Built Notes (deviations from the draft)

- **Substring, not fuzzy, matching** for session rows — subsequence fuzzy
  over-matched badly on long path+title blobs (details in the Search section).
  `gator::fuzzy_match` stays untouched for navigate/issues.
- **Highlight + scroll-to-match run in both search modes**, not only "search
  all" — moving between results always shows where the text matched.
- **`extras: Vec<(String, String)>`** instead of a typed `MetadataMap` — the
  concrete phase keeps a plain display list; typed metadata comes with the
  provider-trait phase.
- **No `content.rs`** — transcript rendering lives in `ui.rs`.
- **No per-file Claude metadata cache** — a full scan of ~45 JSONL files
  (largest 24 MB) streams in well under a second on a background thread;
  a persistent cache is complexity the measurements didn't justify. Revisit if
  stores grow 10×.
- **Content index is per-run, in-memory** (no `$XDG_CACHE_HOME` spill yet);
  matching runs on the main thread once per frame rather than a search worker —
  measured fast enough with events drained per frame.
- **rusqlite pinned to 0.37** (toolchain: see Dependencies).
- **Event draining**: all pending input events are processed before one
  refilter/redraw, which is what keeps content-mode typing instant.
