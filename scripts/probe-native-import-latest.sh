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

mkdir -p "$tmp/artifacts" "$tmp/opencode" "$tmp/claude-target"

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

"$bin" convert \
  --from opencode \
  --to claude \
  --id ses_ci_native_import \
  --source-store "$tmp/opencode/opencode.db" \
  --target-store "$tmp/claude-target" \
  --target-id 33333333-4444-4555-8666-777777777777 \
  "${allow_args[@]}" \
  --report-json > "$tmp/artifacts/opencode-to-claude.json"

if [[ -n "${SESSIONGATOR_NATIVE_IMPORT_ARTIFACTS:-}" ]]; then
  mkdir -p "$SESSIONGATOR_NATIVE_IMPORT_ARTIFACTS"
  cp "$tmp/artifacts"/*.json "$SESSIONGATOR_NATIVE_IMPORT_ARTIFACTS/"
fi
