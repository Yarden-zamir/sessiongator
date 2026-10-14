#!/usr/bin/env bash
# Probe: convert the Claude fixture through every tool pair in isolated
# stores, and check each written session with the target tool itself,
# without model calls. A pass records the installed versions as
# `probe-passed` (scripts/update-native-import-versions.mjs).
#
# Needs on PATH: claude, opencode (OpenCode 2), codex, copilot, jq, and a
# release build of sessiongator.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="${SESSIONGATOR_BIN:-$repo_root/target/release/sessiongator}"
fixture="$repo_root/fixtures/native-import/claude/2.1.199/basic-text/source"
fixture_id="11111111-2222-4333-8444-555555555555"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/sessiongator-probe.XXXXXX")"
artifacts="$tmp/artifacts"
mkdir -p "$artifacts"

cleanup() {
  if [[ -n "${SESSIONGATOR_NATIVE_IMPORT_ARTIFACTS:-}" ]]; then
    mkdir -p "$SESSIONGATOR_NATIVE_IMPORT_ARTIFACTS"
    cp -R "$artifacts"/. "$SESSIONGATOR_NATIVE_IMPORT_ARTIFACTS/" 2>/dev/null || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

# The undo history of this run stays in the temp directory.
export SESSIONGATOR_STATE_DIR="$tmp/state"

# OpenCode 2 talks to a shared background service unless `--standalone` is
# passed, and keeps logs and repos under XDG_DATA_HOME, so every call gets a
# private server and private XDG directories.
opencode_isolated() {
  local home="$1"
  shift
  env OPENCODE_DB="$home/data/opencode/opencode.db" \
    XDG_DATA_HOME="$home/data" \
    XDG_STATE_HOME="$home/state" \
    XDG_CONFIG_HOME="$home/config" \
    XDG_CACHE_HOME="$home/cache" \
    opencode "$@" --standalone
}

# Print a new empty store path for `tool`. An opencode store must be created
# by opencode itself so its migration journal matches the installed version.
new_store() {
  local tool="$1" name="$2"
  local home="$tmp/stores/$name"
  mkdir -p "$home"
  if [[ "$tool" == opencode ]]; then
    mkdir -p "$home"/{data,state,config,cache}
    opencode_isolated "$home" session list --format json > /dev/null
    echo "$home/data/opencode/opencode.db"
  else
    echo "$home"
  fi
}

# convert <from> <id> <source store> <to> <target store>; prints the new id.
convert() {
  local from="$1" id="$2" source_store="$3" to="$4" target_store="$5"
  local report="$artifacts/$from-$id-to-$to.json"
  "$bin" convert --from "$from" --id "$id" --to "$to" \
    --source-store "$source_store" --target-store "$target_store" \
    --allow-unsupported-version --report-json > "$report"
  jq -er .targetId "$report"
}

# The target tool must accept the written session without a model call.
validate() {
  local tool="$1" store="$2" id="$3"
  local log="$artifacts/validate-$tool-$id.txt"
  case "$tool" in
    # Claude Code has no model-free resume check; the full pass covers it,
    # and sessiongator reads the session back here.
    claude)
      CLAUDE_CONFIG_DIR="$store" "$bin" show --tool claude "$id" > "$log"
      ;;
    opencode)
      local home="${store%/data/opencode/opencode.db}"
      opencode_isolated "$home" session export "$id" > "$log"
      jq -e '[.messages[] | (.info.type // .type)] | (index("user") != null)' "$log" > /dev/null
      ;;
    codex)
      (
        printf '%s\n' \
          '{"method":"initialize","id":0,"params":{"clientInfo":{"name":"sessiongator","title":"sessiongator probe","version":"1"}}}' \
          '{"method":"initialized","params":{}}' \
          "{\"method\":\"thread/read\",\"id\":1,\"params\":{\"threadId\":\"$id\",\"includeTurns\":true}}"
        sleep 2
      ) | CODEX_HOME="$store" codex app-server > "$log"
      jq -se 'map(select(.id == 1))[0].result.thread.turns | any(.items | length > 0)' "$log" > /dev/null
      ;;
    copilot)
      set +e
      env -u COPILOT_GITHUB_TOKEN -u GH_TOKEN -u GITHUB_TOKEN \
        COPILOT_HOME="$store" copilot --resume="$id" --screen-reader --no-color \
        --no-custom-instructions --disable-builtin-mcps --no-remote --log-level none \
        < /dev/null > "$log" 2>&1
      set -e
      if grep -q -E "No session, task, or name matched|Failed to load workspace|Failed to initialize workspace|Session file is corrupted" "$log"; then
        cat "$log" >&2
        return 1
      fi
      ;;
  esac
}

# Bash 3.2 (macOS) has no associative arrays: `store_<tool>` and `id_<tool>`
# hold the sessions converted from the fixture.
for tool in opencode codex copilot; do
  printf -v "store_$tool" '%s' "$(new_store "$tool" "from-claude-$tool")"
  store_var="store_$tool"
  printf -v "id_$tool" '%s' "$(convert claude "$fixture_id" "$fixture" "$tool" "${!store_var}")"
  id_var="id_$tool"
  validate "$tool" "${!store_var}" "${!id_var}"
done

# Every remaining direction starts from a session written above.
for from in opencode codex copilot; do
  store_var="store_$from"
  id_var="id_$from"
  for to in claude opencode codex copilot; do
    [[ "$to" == "$from" ]] && continue
    target_store="$(new_store "$to" "$from-to-$to")"
    target_id="$(convert "$from" "${!id_var}" "${!store_var}" "$to" "$target_store")"
    validate "$to" "$target_store" "$target_id"
  done
done

echo "native import probe passed for every tool pair"
