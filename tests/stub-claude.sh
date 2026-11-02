#!/usr/bin/env bash
# Fake `claude -p` for dreamd tests. Prints a 3-event JSON array
# [system/init, assistant, result] like `claude -p --output-format json`.
#
# Modes (DREAMD_STUB_MODE):
#   unset|echo  result = text after "reply with exactly:" on the prompt's last line, else "ok"
#   worker      structured_output = Findings with one `project` insight citing
#               the token after "session id:" in the prompt (quote "stub")
#   aggregator  edits $PWD: appends a MEMORY.md line, writes dreamed.md,
#               if DREAMD_STUB_DELETE=1 lists fact-b.md in .dreamd-delete (the
#               deletion manifest; the aggregator has no shell) and drops its
#               MEMORY.md line, writes $DREAMD_STUB_ESCAPE if set, appends to
#               $DREAMD_STUB_TOUCH_LIVE if set
#   auto        worker when --json-schema is passed, else aggregator
#   fail        is_error: true, result "boom", exit 1
#   sleep       sleeps 30 s (timeout tests)
#   badjson     prints `not json`
# Extras:
#   DREAMD_STUB_FAIL_SESSION=<id>  worker mode fails (like `fail`) for that session id
#   DREAMD_STUB_PROMPT_OUT=<path>  write the received prompt there (every mode)
#   DREAMD_STUB_ARGV_OUT=<path>    append the received argv (one arg per line, then a blank line)
#   The init event reports permissionMode = the --permission-mode received
#   (omitted when none). DREAMD_STUB_PERMISSION_MODE=<value> overrides it, in
#   every mode or only in DREAMD_STUB_PERMISSION_MODE_FOR=worker|aggregator.
set -u

prompt=""
schema=""
perm=""
args=("$@")
while [ $# -gt 0 ]; do
  case "$1" in
    -p|--print) prompt="${2-}"; shift 2 || shift ;;
    --json-schema) schema="${2-}"; shift 2 || shift ;;
    --permission-mode) perm="${2-}"; shift 2 || shift ;;
    --no-session-persistence|--verbose) shift ;;
    --*) shift; [ $# -gt 0 ] && shift ;;
    *) shift ;;
  esac
done

if [ -n "${DREAMD_STUB_ARGV_OUT:-}" ]; then
  # Workers run concurrently: serialise appends so argv blocks never interleave.
  lock="$DREAMD_STUB_ARGV_OUT.lock"
  until mkdir "$lock" 2>/dev/null; do sleep 0.01; done
  { printf '%s\n' "${args[@]}"; printf '\n'; } >> "$DREAMD_STUB_ARGV_OUT"
  rmdir "$lock"
fi
if [ -n "${DREAMD_STUB_PROMPT_OUT:-}" ]; then
  printf '%s' "$prompt" > "$DREAMD_STUB_PROMPT_OUT"
fi

# JSON string literal (quotes, backslashes, control chars).
json_str() {
  local s=$1
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  s=${s//$'\n'/\\n}
  s=${s//$'\r'/\\r}
  s=${s//$'\t'/\\t}
  printf '"%s"' "$s"
}

# emit <result-text> <is_error> [structured_output-json]
emit() {
  local text=$1 is_error=$2 structured=${3-}
  local sid="stub-session-$$"
  local extra=""
  [ -n "$structured" ] && extra=",\"structured_output\":$structured"
  local pm=""
  [ -n "$perm" ] && pm=$(printf ',"permissionMode":%s' "$(json_str "$perm")")
  printf '[{"type":"system","subtype":"init","session_id":"%s","model":"stub"%s},' "$sid" "$pm"
  printf '{"type":"assistant","session_id":"%s","message":{"role":"assistant","content":[{"type":"text","text":%s}]}},' "$sid" "$(json_str "$text")"
  printf '{"type":"result","subtype":"success","is_error":%s,"result":%s,"total_cost_usd":0.001,"usage":{"input_tokens":10,"output_tokens":5},"duration_ms":12,"num_turns":1,"session_id":"%s"%s}]\n' \
    "$is_error" "$(json_str "$text")" "$sid" "$extra"
}

fail() { emit "boom" true; exit 1; }

mode="${DREAMD_STUB_MODE:-echo}"
if [ "$mode" = auto ]; then
  if [ -n "$schema" ]; then mode=worker; else mode=aggregator; fi
fi

if [ -n "${DREAMD_STUB_PERMISSION_MODE:-}" ]; then
  case "${DREAMD_STUB_PERMISSION_MODE_FOR:-}" in
    ""|"$mode") perm="$DREAMD_STUB_PERMISSION_MODE" ;;
  esac
fi

case "$mode" in
  echo)
    last=$(printf '%s\n' "$prompt" | tail -n 1)
    text="ok"
    case "$last" in
      *"reply with exactly:"*)
        text=${last#*reply with exactly:}
        text="${text#"${text%%[![:space:]]*}"}"
        text="${text%"${text##*[![:space:]]}"}"
        ;;
    esac
    emit "$text" false
    ;;
  worker)
    session=$(printf '%s\n' "$prompt" | sed -n 's/^.*session id:[[:space:]]*\([^[:space:]]*\).*$/\1/p' | head -n 1)
    if [ -n "${DREAMD_STUB_FAIL_SESSION:-}" ] && [ "$session" = "$DREAMD_STUB_FAIL_SESSION" ]; then
      fail
    fi
    findings=$(printf '{"insights":[{"kind":"project","statement":"stub insight","why":"stub","evidence":{"session_id":%s,"quote":"stub"},"confidence":0.9,"relates_to":[]}],"stale":[],"corrections":[]}' "$(json_str "$session")")
    emit "$findings" false "$findings"
    ;;
  aggregator)
    printf -- '- [Dreamed](dreamed.md) — stub insight\n' >> MEMORY.md
    printf -- '---\nname: dreamed\ndescription: Stub insight from a dream\nmetadata:\n  type: project\n---\n\nstub insight\n' > dreamed.md
    if [ "${DREAMD_STUB_DELETE:-}" = 1 ]; then
      printf 'fact-b.md\n' > .dreamd-delete
      grep -v 'fact-b.md' MEMORY.md > MEMORY.md.tmp; mv MEMORY.md.tmp MEMORY.md
    fi
    if [ -n "${DREAMD_STUB_TOUCH_LIVE:-}" ]; then
      printf 'touched by aggregator\n' >> "$DREAMD_STUB_TOUCH_LIVE"
    fi
    if [ -n "${DREAMD_STUB_ESCAPE:-}" ]; then
      printf 'escaped write\n' > "$DREAMD_STUB_ESCAPE"
    fi
    emit $'Merged 1 insight.\n\n## Summary\nMerged 1 insight.' false
    ;;
  fail) fail ;;
  sleep) exec sleep 30 ;;
  badjson) echo "not json" ;;
  *) echo "stub-claude: unknown DREAMD_STUB_MODE=$mode" >&2; exit 2 ;;
esac
