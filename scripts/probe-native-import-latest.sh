#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="$repo_root/target/release/sessiongator"
fixture="$repo_root/fixtures/native-import/claude/2.1.199/basic-text/source"
source_id="11111111-2222-4333-8444-555555555555"
tmp="${TMPDIR:-/tmp}/sessiongator-native-import-latest-$$"

cleanup() {
  if [[ -n "${SESSIONGATOR_NATIVE_IMPORT_ARTIFACTS:-}" && -d "$tmp/artifacts" ]]; then
    mkdir -p "$SESSIONGATOR_NATIVE_IMPORT_ARTIFACTS"
    cp -R "$tmp/artifacts"/* "$SESSIONGATOR_NATIVE_IMPORT_ARTIFACTS/" 2>/dev/null || true
  fi
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

convert_pair() {
  local from="$1" source_id="$2" source_store="$3"
  local to="$4" target_id="$5" target_store="$6"
  "$bin" convert \
    --from "$from" \
    --to "$to" \
    --id "$source_id" \
    --source-store "$source_store" \
    --target-store "$target_store" \
    --target-id "$target_id" \
    "${allow_args[@]}" \
    --report-json > "$tmp/artifacts/$from-to-$to.json"
}

validate_copilot_session() {
  local home="$1" session_id="$2" label="$3"
  local log_dir="$tmp/artifacts/copilot-logs-$label"
  mkdir -p "$log_dir"
  set +e
  env -u COPILOT_GITHUB_TOKEN -u GH_TOKEN -u GITHUB_TOKEN \
    COPILOT_HOME="$home" copilot \
    --resume="$session_id" \
    --screen-reader \
    --no-color \
    --no-custom-instructions \
    --disable-builtin-mcps \
    --no-remote \
    --log-level debug \
    --log-dir "$log_dir" \
    > "$tmp/artifacts/copilot-native-resume-$label.txt" 2>&1
  set -e
  if grep -R -q -E "No session, task, or name matched|Failed to load workspace|Failed to initialize workspace|Session file is corrupted" \
    "$tmp/artifacts/copilot-native-resume-$label.txt" "$log_dir"; then
    grep -R -E "No session, task, or name matched|Failed to load workspace|Failed to initialize workspace|Session file is corrupted" \
      "$tmp/artifacts/copilot-native-resume-$label.txt" "$log_dir" >&2 || true
    echo "Copilot rejected the $label session" >&2
    exit 1
  fi
}

validate_codex_session() {
  local home="$1" session_id="$2" label="$3"
  local output="$tmp/artifacts/codex-native-read-$label.jsonl"
  (
    printf '%s\n' \
      '{"method":"initialize","id":0,"params":{"clientInfo":{"name":"sessiongator","title":"sessiongator matrix probe","version":"0.4.1"}}}' \
      '{"method":"initialized","params":{}}' \
      "{\"method\":\"thread/read\",\"id\":1,\"params\":{\"threadId\":\"$session_id\",\"includeTurns\":true}}"
    sleep 2
  ) | CODEX_HOME="$home" codex app-server > "$output"
  node -e '
    const fs = require("node:fs");
    const responses = fs.readFileSync(process.argv[1], "utf8")
      .trim().split("\n").map(JSON.parse);
    const thread = responses.find((value) => value.id === 1)?.result?.thread;
    if (!thread || !thread.turns?.some((turn) => turn.items?.length > 0)) {
      throw new Error("Codex did not project a non-empty turn");
    }
  ' "$output"
}

validate_claude_session() {
  local home="$1" session_id="$2" label="$3"
  CLAUDE_CONFIG_DIR="$home" claude \
    --resume "$session_id" \
    --print "Reply with exactly SESSIONGATOR_OK" \
    --tools "" \
    --permission-mode plan \
    --output-format json \
    > "$tmp/artifacts/claude-native-resume-$label.json"
  node -e '
    const fs = require("node:fs");
    const result = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    if (result.is_error || result.result?.trim() !== "SESSIONGATOR_OK") {
      throw new Error(`Claude native resume failed: ${result.result ?? "missing result"}`);
    }
  ' "$tmp/artifacts/claude-native-resume-$label.json"
}

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

if [[ -n "${ANTHROPIC_API_KEY:-}" ]]; then
  validate_claude_session \
    "$tmp/claude-from-opencode" 33333333-4444-4555-8666-777777777777 from-opencode
elif [[ "${SESSIONGATOR_REQUIRE_CLAUDE_NATIVE:-0}" == "1" ]]; then
  echo "ANTHROPIC_API_KEY is required for Claude native resume validation" >&2
  exit 1
fi

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
  cat "$tmp/artifacts/copilot-missing-session.txt" >&2
  echo "Copilot unexpectedly accepted a missing session" >&2
  exit 1
fi

validate_copilot_session \
  "$tmp/copilot" 66666666-7777-4888-8999-aaaaaaaaaaaa from-claude

"$bin" convert \
  --from copilot \
  --to claude \
  --id 66666666-7777-4888-8999-aaaaaaaaaaaa \
  --source-store "$tmp/copilot" \
  --target-store "$tmp/claude-from-copilot" \
  --target-id 77777777-8888-4999-8aaa-bbbbbbbbbbbb \
  "${allow_args[@]}" \
  --report-json > "$tmp/artifacts/copilot-to-claude.json"

if [[ -n "${ANTHROPIC_API_KEY:-}" ]]; then
  validate_claude_session \
    "$tmp/claude-from-codex" 55555555-6666-4777-8888-999999999999 from-codex
  validate_claude_session \
    "$tmp/claude-from-copilot" 77777777-8888-4999-8aaa-bbbbbbbbbbbb from-copilot
fi

# Complete the remaining six directions using the native stores generated
# above as source fixtures. Each SQLite target is initialized by opencode.
mkdir -p \
  "$tmp/opencode-from-codex" \
  "$tmp/opencode-from-copilot" \
  "$tmp/codex-from-opencode" \
  "$tmp/codex-from-copilot" \
  "$tmp/copilot-from-opencode" \
  "$tmp/copilot-from-codex"
OPENCODE_DB="$tmp/opencode-from-codex/opencode.db" opencode session list --format json \
  > "$tmp/artifacts/opencode-from-codex-empty.json"
OPENCODE_DB="$tmp/opencode-from-copilot/opencode.db" opencode session list --format json \
  > "$tmp/artifacts/opencode-from-copilot-empty.json"

convert_pair opencode ses_ci_native_import "$tmp/opencode/opencode.db" \
  codex 88888888-9999-4aaa-8bbb-cccccccccccc "$tmp/codex-from-opencode"
convert_pair opencode ses_ci_native_import "$tmp/opencode/opencode.db" \
  copilot 99999999-aaaa-4bbb-8ccc-dddddddddddd "$tmp/copilot-from-opencode"
convert_pair codex 44444444-5555-4666-8777-888888888888 "$tmp/codex" \
  opencode ses_ci_from_codex "$tmp/opencode-from-codex/opencode.db"
convert_pair codex 44444444-5555-4666-8777-888888888888 "$tmp/codex" \
  copilot aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee "$tmp/copilot-from-codex"
convert_pair copilot 66666666-7777-4888-8999-aaaaaaaaaaaa "$tmp/copilot" \
  opencode ses_ci_from_copilot "$tmp/opencode-from-copilot/opencode.db"
convert_pair copilot 66666666-7777-4888-8999-aaaaaaaaaaaa "$tmp/copilot" \
  codex bbbbbbbb-cccc-4ddd-8eee-ffffffffffff "$tmp/codex-from-copilot"

OPENCODE_DB="$tmp/opencode-from-codex/opencode.db" opencode export ses_ci_from_codex \
  > "$tmp/artifacts/opencode-from-codex-native-export.json"
OPENCODE_DB="$tmp/opencode-from-copilot/opencode.db" opencode export ses_ci_from_copilot \
  > "$tmp/artifacts/opencode-from-copilot-native-export.json"

validate_copilot_session \
  "$tmp/copilot-from-opencode" 99999999-aaaa-4bbb-8ccc-dddddddddddd from-opencode
validate_copilot_session \
  "$tmp/copilot-from-codex" aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee from-codex
validate_codex_session \
  "$tmp/codex" 44444444-5555-4666-8777-888888888888 from-claude
validate_codex_session \
  "$tmp/codex-from-opencode" 88888888-9999-4aaa-8bbb-cccccccccccc from-opencode
validate_codex_session \
  "$tmp/codex-from-copilot" bbbbbbbb-cccc-4ddd-8eee-ffffffffffff from-copilot
