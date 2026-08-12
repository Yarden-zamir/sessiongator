#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="$repo_root/target/release/sessiongator"
fixture="$repo_root/fixtures/native-import/claude/2.1.199/basic-text/source"
source_id="11111111-2222-4333-8444-555555555555"
tmp="${TMPDIR:-/tmp}/sessiongator-native-import-latest-$$"

cleanup() {
  rm -rf "$tmp"
}
trap cleanup EXIT

mkdir -p \
  "$tmp/artifacts" \
  "$tmp/opencode" \
  "$tmp/codex" \
  "$tmp/copilot" \
  "$tmp/claude-from-opencode" \
  "$tmp/claude-from-codex" \
  "$tmp/claude-from-copilot"

# A new opencode database must be initialized by opencode so its migration
# journal matches the installed harness before sessiongator adds a session.
OPENCODE_DB="$tmp/opencode/opencode.db" opencode session list --format json \
  > "$tmp/artifacts/opencode-empty-session-list.json"

allow_args=()
if [[ "${SESSIONGATOR_NATIVE_IMPORT_ALLOW_UNSUPPORTED:-1}" != "0" ]]; then
  allow_args+=(--allow-unsupported-version)
fi

"$bin" convert \
  --from claude \
  --to opencode \
  --id "$source_id" \
  --source-store "$fixture" \
  --target-store "$tmp/opencode/opencode.db" \
  --target-id ses_ci_native_import \
  "${allow_args[@]}" \
  --report-json > "$tmp/artifacts/claude-to-opencode.json"

OPENCODE_DB="$tmp/opencode/opencode.db" opencode export ses_ci_native_import \
  > "$tmp/artifacts/opencode-native-export.json"

"$bin" convert \
  --from opencode \
  --to claude \
  --id ses_ci_native_import \
  --source-store "$tmp/opencode/opencode.db" \
  --target-store "$tmp/claude-from-opencode" \
  --target-id 33333333-4444-4555-8666-777777777777 \
  "${allow_args[@]}" \
  --report-json > "$tmp/artifacts/opencode-to-claude.json"

"$bin" convert \
  --from claude \
  --to codex \
  --id "$source_id" \
  --source-store "$fixture" \
  --target-store "$tmp/codex" \
  --target-id 44444444-5555-4666-8777-888888888888 \
  "${allow_args[@]}" \
  --report-json > "$tmp/artifacts/claude-to-codex.json"

"$bin" convert \
  --from codex \
  --to claude \
  --id 44444444-5555-4666-8777-888888888888 \
  --source-store "$tmp/codex" \
  --target-store "$tmp/claude-from-codex" \
  --target-id 55555555-6666-4777-8888-999999999999 \
  "${allow_args[@]}" \
  --report-json > "$tmp/artifacts/codex-to-claude.json"

"$bin" convert \
  --from claude \
  --to copilot \
  --id "$source_id" \
  --source-store "$fixture" \
  --target-store "$tmp/copilot" \
  --target-id 66666666-7777-4888-8999-aaaaaaaaaaaa \
  "${allow_args[@]}" \
  --report-json > "$tmp/artifacts/claude-to-copilot.json"

if env -u COPILOT_GITHUB_TOKEN -u GH_TOKEN -u GITHUB_TOKEN \
  COPILOT_HOME="$tmp/copilot" copilot \
  --resume=00000000-0000-4000-8000-000000000000 \
  --screen-reader \
  --no-color \
  --no-custom-instructions \
  --disable-builtin-mcps \
  --no-remote \
  --log-level none \
  > "$tmp/artifacts/copilot-missing-session.txt" 2>&1; then
  echo "Copilot unexpectedly accepted a missing session" >&2
  exit 1
fi

env -u COPILOT_GITHUB_TOKEN -u GH_TOKEN -u GITHUB_TOKEN \
  COPILOT_HOME="$tmp/copilot" copilot \
  --resume=66666666-7777-4888-8999-aaaaaaaaaaaa \
  --screen-reader \
  --no-color \
  --no-custom-instructions \
  --disable-builtin-mcps \
  --no-remote \
  --log-level none \
  > "$tmp/artifacts/copilot-native-resume.txt" 2>&1

"$bin" convert \
  --from copilot \
  --to claude \
  --id 66666666-7777-4888-8999-aaaaaaaaaaaa \
  --source-store "$tmp/copilot" \
  --target-store "$tmp/claude-from-copilot" \
  --target-id 77777777-8888-4999-8aaa-bbbbbbbbbbbb \
  "${allow_args[@]}" \
  --report-json > "$tmp/artifacts/copilot-to-claude.json"

if [[ -n "${SESSIONGATOR_NATIVE_IMPORT_ARTIFACTS:-}" ]]; then
  mkdir -p "$SESSIONGATOR_NATIVE_IMPORT_ARTIFACTS"
  cp "$tmp/artifacts"/*.json "$SESSIONGATOR_NATIVE_IMPORT_ARTIFACTS/"
  cp "$tmp/artifacts"/*.txt "$SESSIONGATOR_NATIVE_IMPORT_ARTIFACTS/"
fi
