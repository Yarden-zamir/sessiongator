_SESSIONGATOR_SCRIPT_PATH="${(%):-%N}"
_SESSIONGATOR_SCRIPT_DIR="${_SESSIONGATOR_SCRIPT_PATH:A:h}"

_sessiongator_bin() {
  if [[ -n "$SESSIONGATOR_BIN" && -x "$SESSIONGATOR_BIN" ]]; then
    echo "$SESSIONGATOR_BIN"
    return 0
  fi

  if command -v sessiongator >/dev/null 2>&1; then
    command -v sessiongator
    return 0
  fi

  if [[ -x "$_SESSIONGATOR_SCRIPT_DIR/../target/release/sessiongator" ]]; then
    echo "$_SESSIONGATOR_SCRIPT_DIR/../target/release/sessiongator"
    return 0
  fi

  if [[ -x "$_SESSIONGATOR_SCRIPT_DIR/../target/debug/sessiongator" ]]; then
    echo "$_SESSIONGATOR_SCRIPT_DIR/../target/debug/sessiongator"
    return 0
  fi

  return 1
}

# Widget: pick an AI session, then resume it in its original directory.
# Selection lines from the binary:
#   resume\t<tool>\t<id>\t<cwd>   (Enter)
#   path\t<source path>           (Ctrl+O)
ai-sessions() {
  local bin tmp exit_status selection kind tool id dir
  bin="$(_sessiongator_bin)" || { echo "sessiongator binary not found" >&2; return 127; }
  tmp="$(mktemp -t sessiongator.XXXXXX)" || return 1
  [[ -n "$ZLE" ]] && zle -I
  if command -v script >/dev/null 2>&1; then
    GATOR_OUTPUT="$tmp" script -q /dev/null "$bin" </dev/tty >/dev/tty 2>/dev/tty
  else
    GATOR_OUTPUT="$tmp" "$bin" </dev/tty >/dev/tty 2>/dev/tty
  fi
  exit_status=$?
  if [[ $exit_status -ne 0 ]]; then
    rm -f "$tmp"
    return $exit_status
  fi
  if [[ -s "$tmp" ]]; then
    selection="$(<"$tmp")"
  fi
  rm -f "$tmp"
  [[ -n "$selection" ]] || return 0

  kind="${selection%%$'\t'*}"
  case "$kind" in
    resume)
      local rest="${selection#*$'\t'}"
      tool="${rest%%$'\t'*}"; rest="${rest#*$'\t'}"
      id="${rest%%$'\t'*}"
      dir="${rest#*$'\t'}"
      if [[ -n "$dir" && ! -d "$dir" ]]; then
        echo "session directory gone: $dir — resuming from \$HOME" >&2
        dir="$HOME"
      fi
      [[ -n "$dir" && -d "$dir" ]] && { cd -- "$dir" || return $?; }
      case "$tool" in
        claude) BUFFER="claude --resume ${(q)id}" ;;
        opencode) BUFFER="opencode --session ${(q)id}" ;;
        *) echo "unknown session tool: $tool" >&2; return 1 ;;
      esac
      zle accept-line
      ;;
    path)
      # Insert the source path into the prompt for further scripting.
      BUFFER="${(q)${selection#*$'\t'}}"
      CURSOR=${#BUFFER}
      ;;
    *)
      echo "unexpected selection: $selection" >&2
      return 1
      ;;
  esac
}

zle -N ai-sessions
