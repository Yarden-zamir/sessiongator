#!/usr/bin/env bash
# Full pass: convert a session that holds a random code word into each
# tool, resume it with the real client and a real model, and ask for the
# word. A tool passes when sessiongator reads the session back and the last
# assistant message holds the word: the tool loaded the imported history,
# and sessiongator reads what this tool version wrote. Passing tools are
# recorded as `verified` in docs/specs/native-session-import-versions.toml.
#
# Each tool is its own check with its own result: passed, failed, or skipped.
# A tool is skipped when its client cannot take part (no login, usage limit),
# which says nothing about sessiongator. Only a failed tool fails the run.
#
# Needs: a release build of sessiongator, jq, node, and logged-in clients on
# PATH. Claude writes to its real store (a copied login could rotate the real
# refresh token); the session is moved to a temp trash afterwards. Codex uses
# FULL_PASS_CODEX_HOME, which must hold a Codex login. opencode and Copilot
# use temp stores.
#
# Environment:
#   FULL_PASS_TOOLS           tools to check (default: claude opencode codex copilot)
#   FULL_PASS_CLAUDE_MODEL    default: haiku
#   FULL_PASS_OPENCODE_AUTH_DB  opencode database that holds a provider login;
#                             default: ~/.local/share/sessiongator-fullpass/opencode-auth.db
#   FULL_PASS_OPENCODE_MODEL  provider/model for that login, e.g. openai/gpt-5.4-mini
#   FULL_PASS_COPILOT_MODEL   default: the Copilot default model
#   FULL_PASS_CODEX_HOME      default: ~/.local/share/sessiongator-fullpass/codex-home
#   FULL_PASS_RECORD          1 (default) records passing versions; 0 does not
#   FULL_PASS_ARTIFACTS       copy logs and the summary here
#   GH_TOKEN                  Copilot login; default: `gh auth token`
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="${SESSIONGATOR_BIN:-$repo_root/target/release/sessiongator}"
tools="${FULL_PASS_TOOLS:-claude opencode codex copilot}"
claude_model="${FULL_PASS_CLAUDE_MODEL:-haiku}"
opencode_auth_db="${FULL_PASS_OPENCODE_AUTH_DB:-$HOME/.local/share/sessiongator-fullpass/opencode-auth.db}"
codex_home="${FULL_PASS_CODEX_HOME:-$HOME/.local/share/sessiongator-fullpass/codex-home}"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/sessiongator-full-pass.XXXXXX")"
artifacts="$tmp/artifacts"
project="$tmp/project"
mkdir -p "$artifacts" "$project"
export SESSIONGATOR_STATE_DIR="$tmp/state"

cleanup() {
  if [[ -n "${FULL_PASS_ARTIFACTS:-}" ]]; then
    mkdir -p "$FULL_PASS_ARTIFACTS"
    cp -R "$artifacts"/. "$FULL_PASS_ARTIFACTS/" 2>/dev/null || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

# A check returns this status when its client cannot take part.
unavailable_status=75
unavailable() {
  echo "$*" >&2
  return "$unavailable_status"
}

limit() {
  if command -v timeout > /dev/null; then timeout 600 "$@"; else "$@"; fi
}

word="SG$(node -e 'console.log(require("node:crypto").randomBytes(4).toString("hex").toUpperCase())')"
question="What code word did I ask you to remember? Reply with only the word."

# The source: a Claude session in a temp store whose only secret is $word.
source_root="$tmp/source"
source_id="$(node -e 'console.log(require("node:crypto").randomUUID())')"
mkdir -p "$source_root/projects/source"
now="$(date -u +%Y-%m-%dT%H:%M:%S.000Z)"
jq -nc --arg id "$source_id" --arg cwd "$project" --arg now "$now" --arg word "$word" '
  {type: "user", uuid: "u1", parentUuid: null, sessionId: $id, cwd: $cwd, timestamp: $now,
   version: "2.1.299", message: {role: "user", content: ("Remember the code word " + $word + ". Reply with only OK.")}},
  {type: "assistant", uuid: "a1", parentUuid: "u1", sessionId: $id, cwd: $cwd, timestamp: $now,
   version: "2.1.299", message: {role: "assistant", model: "claude-haiku", content: [{type: "text", text: "OK"}]}}
' > "$source_root/projects/source/$source_id.jsonl"

# convert <from> <id> <source store> <to> [target store]; prints the new id.
convert() {
  local from="$1" id="$2" source_store="$3" to="$4" target_store="${5:-}"
  local report="$artifacts/convert-$from-to-$to.json"
  "$bin" convert --from "$from" --id "$id" --to "$to" --source-store "$source_store" \
    ${target_store:+--target-store "$target_store"} --cwd "$project" \
    --allow-unsupported-version --report-json > "$report"
  jq -er .targetId "$report"
}

# The last assistant message, as sessiongator reads it, holds the word.
answered() {
  local tool="$1" id="$2"
  "$bin" show --tool "$tool" "$tool:$id" > "$artifacts/readback-$tool.json"
  jq -e --arg word "$word" \
    '[.turns[] | select(.role == "assistant") | .text] | last // "" | ascii_upcase | contains($word)' \
    "$artifacts/readback-$tool.json" > /dev/null
}

check_claude() {
  # Claude cannot convert to itself, so the source goes through Copilot.
  local bridge="$tmp/bridge-copilot" bridge_id id resumed
  bridge_id="$(convert claude "$source_id" "$source_root" copilot "$bridge")"
  id="$(convert copilot "$bridge_id" "$bridge" claude)"
  (cd "$project" && limit claude -p --permission-mode default --model "$claude_model" \
    --resume "$id" --output-format json "$question") > "$artifacts/answer-claude.json"
  # Settings with `verbose` print an array of events instead of one result.
  local result
  result="$(jq -c 'if type == "array" then map(select(.type == "result"))[0] else . end' \
    "$artifacts/answer-claude.json")"
  resumed="$(jq -r .session_id <<< "$result")"
  local status=0
  if [[ "$(jq -r .is_error <<< "$result")" == true ]]; then
    # Claude reported its own error (usage limit, login) instead of an answer.
    unavailable "claude: $(jq -r .result <<< "$result")" || status=$?
  else
    answered claude "$resumed" || status=1
  fi
  "$bin" delete --tool claude "$id" --yes > /dev/null
  if [[ "$resumed" != "$id" ]]; then
    "$bin" delete --tool claude "$resumed" --yes > /dev/null
  fi
  return "$status"
}

check_opencode() {
  local home="$tmp/opencode" id
  mkdir -p "$home"/{data,state,config,cache}
  # `--standalone` keeps opencode off the shared background service, which
  # uses the real database.
  oc() {
    limit env OPENCODE_DB="$home/data/opencode/opencode.db" XDG_DATA_HOME="$home/data" \
      XDG_STATE_HOME="$home/state" XDG_CONFIG_HOME="$home/config" XDG_CACHE_HOME="$home/cache" \
      opencode "$@" --standalone
  }
  # OpenCode's free models reject resumed imported sessions (FreeTierError,
  # HTTP 403), so the full pass needs a real provider login. OpenCode 2 keeps
  # logins in its database; the login moves into the temp database unprinted.
  if [[ ! -f "$opencode_auth_db" || -z "${FULL_PASS_OPENCODE_MODEL:-}" ]]; then
    unavailable "opencode: needs a provider login in $opencode_auth_db and FULL_PASS_OPENCODE_MODEL"
    return
  fi
  oc session list --format json > /dev/null
  OPENCODE_DB="$opencode_auth_db" limit opencode auth export --standalone |
    oc auth import > /dev/null
  id="$(convert claude "$source_id" "$source_root" opencode "$home/data/opencode/opencode.db")"
  (cd "$project" && oc run --session "$id" -m "$FULL_PASS_OPENCODE_MODEL" --format json "$question") \
    > "$artifacts/answer-opencode.jsonl"
  OPENCODE_DB="$home/data/opencode/opencode.db" answered opencode "$id"
}

check_codex() {
  local id status=0
  if ! CODEX_HOME="$codex_home" codex login status > "$artifacts/codex-login.txt" 2>&1; then
    unavailable "codex: not logged in at $codex_home"
    return
  fi
  id="$(convert claude "$source_id" "$source_root" codex "$codex_home")"
  (cd "$project" && CODEX_HOME="$codex_home" limit codex exec resume --skip-git-repo-check --json \
    "$id" "$question" < /dev/null) > "$artifacts/answer-codex.jsonl"
  CODEX_HOME="$codex_home" answered codex "$id" || status=1
  CODEX_HOME="$codex_home" "$bin" delete --tool codex "$id" --yes > /dev/null
  return "$status"
}

check_copilot() {
  local home="$tmp/copilot" id token
  token="${GH_TOKEN:-$(gh auth token 2> /dev/null || true)}"
  if [[ -z "$token" ]]; then
    unavailable "copilot: no GH_TOKEN and no gh login"
    return
  fi
  id="$(convert claude "$source_id" "$source_root" copilot "$home")"
  (cd "$project" && COPILOT_HOME="$home" GH_TOKEN="$token" limit copilot --resume="$id" \
    -p "$question" ${FULL_PASS_COPILOT_MODEL:+--model "$FULL_PASS_COPILOT_MODEL"} \
    --allow-all-tools --no-auto-update -s < /dev/null) > "$artifacts/answer-copilot.txt"
  COPILOT_HOME="$home" answered copilot "$id"
}

passed=()
failed=()
summary="[]"
for tool in $tools; do
  version="$("$tool" --version 2> /dev/null | head -1 || true)"
  code=0
  "check_$tool" 2> "$artifacts/error-$tool.txt" || code=$?
  case "$code" in
    0)
      passed+=("$tool")
      result=passed
      ;;
    "$unavailable_status")
      result=skipped
      echo "$tool skipped:" >&2
      tail -5 "$artifacts/error-$tool.txt" >&2
      ;;
    *)
      failed+=("$tool")
      result=failed
      echo "$tool failed:" >&2
      tail -20 "$artifacts/error-$tool.txt" >&2
      ;;
  esac
  echo "$tool ($version): $result"
  summary="$(jq -c --arg tool "$tool" --arg version "$version" --arg result "$result" \
    '. + [{tool: $tool, version: $version, result: $result}]' <<< "$summary")"
done
echo "$summary" > "$artifacts/full-pass.json"

if [[ "${FULL_PASS_RECORD:-1}" == 1 && ${#passed[@]} -gt 0 ]]; then
  node "$repo_root/scripts/update-native-import-versions.mjs" --status verified \
    --source "full-pass-$(hostname -s)" \
    --notes "Full pass: the tool resumed an imported session with a real model and answered from its history; sessiongator read the reply back." \
    "${passed[@]}"
fi
[[ ${#failed[@]} -eq 0 ]]
