# Private GitHub Session Sync Spec

Status: **draft**

## Goal

Synchronize restorable AI sessions between machines through a user-owned private GitHub repository. Repository content remains plaintext so sessions can be searched and browsed in GitHub.

Initial tools:

- Claude Code
- opencode
- Codex
- Copilot

## Scope

- Export every native record and sidecar required to reconstruct a session supported by sessiongator's version-gated writers.
- Export a generated Markdown transcript for browsing; native records remain the restore source of truth.
- Compare local and remote event ancestry and append a missing suffix when one history is a strict prefix of the other.
- Preserve divergent continuations as separate branches of the session instead of guessing an order.
- Keep tool settings, credentials, caches, and records unrelated to exported sessions out of the repository.

## Non-Goals

- End-to-end encryption or protection from GitHub and authorized repository users or applications.
- Synchronizing complete tool configuration directories or binary SQLite databases.
- Automatically combining two divergent conversation continuations.
- Propagating deletion in the first version.
- Real-time background synchronization.

## Trust And Safety

- The user explicitly accepts GitHub as a trusted plaintext data processor for conversation content, tool calls, tool results, file content, and metadata present in exported sessions.
- Sync must refuse to push when the GitHub repository is public or its visibility cannot be verified.
- Sync must never force-push.
- Pulls that modify a native store require a plan, a backup where the target supports one, version validation, and post-write readback.
- The repository and local clone may contain credentials captured in session content. Sync must warn about this during initialization and before its first push.

## Repository Format

The format is versioned independently from native tool versions.

```text
sessiongator-sync.json
sessions/<lineage-id>/
  metadata.json
  events/<event-id>.json
  blobs/<digest>
  heads/<device-id>.json
  transcript.md
conflicts/<conflict-id>.json
```

- `lineage-id` is generated on first export and preserved by every import, restore, and fork. It identifies one conversation history independently of tool-specific session identifiers.
- A native location is `(tool, store-id, native-session-id)`. `store-id` is a persistent random identifier recorded in local sync configuration because native session identifiers are not globally unique.
- Local sync configuration maps each native location to its lineage and current event head; this mapping must survive a restored session receiving a new native identifier.
- Event and blob files are immutable. Metadata, device heads, and generated transcripts may be rebuilt after reconciliation.
- Each event stores its stable lineage event identity, predecessor or parent identity, ordered canonical content, original native identity and record, content digest, and source tool/schema version.
- `blobs/` contains sidecars or attachments referenced by events. A digest must be verified before restore.
- `metadata.json` records title, working directory, model, timestamps, conversion provenance, event order, and current known heads.
- `transcript.md` is generated from canonical content and must never be used as restore input.
- SQLite-backed tools are exported as logical rows and relationships, not copied database files.

## Sync Rules

For each session lineage, compare the ordered event identities and content digests:

| Local | Remote | Result |
| --- | --- | --- |
| Equal | Equal | No change |
| Remote is a prefix of local | Local has a suffix | Append the suffix to the repository |
| Local is a prefix of remote | Remote has a suffix | Plan appending the suffix to the local native store |
| Neither is a prefix | Both changed after a common event | Record a conflict and preserve both heads |
| Same event identity, different digest | Existing content changed | Record a conflict |

- Automatic append is allowed only for a strict-prefix relationship.
- A conflict must not mutate the local native session or discard either remote head.
- Conflict resolution may select one head as primary or materialize one continuation as a new native session. Interleaving events is not supported.
- Missing remote sessions are imported as new native sessions and mapped to the existing lineage, even when the target tool generates a different session identifier.
- Absence does not mean deletion; deletion requires a future explicit tombstone contract.
- Repeating push or pull after success must be idempotent.

## Git Behavior

- Sync uses the repository's default branch so Markdown transcripts remain easy to browse and search.
- Before committing, fetch and reconcile the remote state logically; do not rely on Git's text merge for session events.
- If a push is rejected because the branch advanced, fetch, reconcile, regenerate mutable files, and retry without rewriting published history.
- Commits contain immutable event/blob additions and regenerated metadata, heads, transcripts, or conflict records.

## CLI

```sh
sessiongator sync init --repo <owner/private-repository>
sessiongator sync status
sessiongator sync push
sessiongator sync pull
sessiongator sync pull --apply
sessiongator sync resolve <conflict-id> --fork
```

- `init` verifies repository visibility, creates local sync identity/configuration, and records the repository format version.
- `status` reports local-only suffixes, remote-only suffixes, conflicts, and unsupported versions without writing.
- `push` exports local changes, reconciles remote changes, and pushes only after a clean plan.
- `pull` prints the native-store write plan and does not modify stores.
- `pull --apply` performs backed-up, version-gated appends/imports and verifies readback.
- `resolve --fork` preserves both continuations by materializing the non-primary head as a separate native session.

## Architecture Constraints

- `SessionSource` summaries and transcripts are insufficient for sync fidelity. Sync requires lossless, tool-specific export and append operations alongside the richer native import model.
- Native records are the backup source of truth; canonical events exist for comparison and rendering.
- Restore and append support must remain gated by `docs/specs/native-session-import-versions.toml`.
- A source adapter must prove isolated export, restore, suffix append, idempotency, and readback before that tool is marked sync-supported.

## Acceptance Criteria

- A session exported on one machine is searchable as Markdown in its private GitHub repository.
- A second machine can import the session and read back the same ordered canonical events.
- Continuing on one machine produces a suffix that another machine can append without duplicating prior events.
- Continuing independently on two machines produces a conflict that retains both continuations and does not modify either native session automatically.
- Public or unverifiable GitHub repository visibility blocks push.
- Repeated synchronization with no new events produces no native writes and no repository commit.
