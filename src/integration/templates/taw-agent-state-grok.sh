#!/bin/sh
# Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
#
# Inside a sandbox (TAW_REPORT_FILE set) it writes the session's input to that file instead, and
# writes nothing when TAW_REPORT_AGENT names another agent.
#
# installed by: taw integration install {{AGENT}}
# managed by taw; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# Requires python3 (used for JSON parsing, matching upstream herdr behaviour).
#
# TAW_INTEGRATION_AGENT={{AGENT}}
# TAW_INTEGRATION_VERSION=3

set -eu

TAW_BIN={{TAW_BIN}}
TAW_AGENT='{{AGENT}}'
action="${1:-}"

# Inside a sandbox (taw's own): the profile mounts the pane's report folder and
# names the file in TAW_REPORT_FILE. The session's input goes there as it came, at most 64 KiB,
# written beside the file and renamed into place, and nothing else is ever written: no taw, no
# python and no tmux are needed. taw reads it from outside, under this hook's rules.
if [ -n "${TAW_REPORT_FILE:-}" ]; then
  # Another agent's hook in here (an imported copy, or one run as a tool) writes nothing over the
  # sandbox's own agent's report.
  [ -z "${TAW_REPORT_AGENT:-}" ] || [ "$TAW_REPORT_AGENT" = "$TAW_AGENT" ] || exit 0
  [ "$action" = session ] || exit 0
  staged="$TAW_REPORT_FILE.$$"
  # Grok gives every hook its session id, which its hook prefers to the input's: a report of
  # taw's own shape carries it, when it is a plain token. The input is read and dropped first, so
  # that Grok never writes into a closed pipe. It names no source: sh reads no JSON.
  case "${GROK_SESSION_ID:-}" in
    ""|*[!A-Za-z0-9._:-]*) ;;
    *)
      head -c 65536 >/dev/null 2>&1 || true
      if { printf '{"hook_event_name":"SessionStart","session_id":"%s","taw_agent":"%s"}' "$GROK_SESSION_ID" "$TAW_AGENT" >"$staged"; } 2>/dev/null; then
        mv -f "$staged" "$TAW_REPORT_FILE" 2>/dev/null || rm -f "$staged"
      else
        rm -f "$staged"
      fi
      exit 0 ;;
  esac
  # The braces keep the shell's own "no such folder" complaint off the agent's screen too.
  if { head -c 65536 >"$staged"; } 2>/dev/null; then
    mv -f "$staged" "$TAW_REPORT_FILE" 2>/dev/null || rm -f "$staged"
  else
    rm -f "$staged"
  fi
  exit 0
fi

# Fast, no-op exits: never block the calling agent when we have nothing to do. Checked
# before touching stdin so a caller that never provides input is never kept waiting.
[ -n "${TMUX_PANE:-}" ] || exit 0
[ -x "$TAW_BIN" ] || exit 0
case "$action" in
  session) ;;
  *) exit 0 ;;
esac
command -v python3 >/dev/null 2>&1 || exit 0

hook_input_file="$(mktemp "${TMPDIR:-/tmp}/taw-${TAW_AGENT}-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
# Cap at 64 KiB: hook payloads are small JSON objects; never buffer an unbounded stream.
head -c 65536 >"$hook_input_file" 2>/dev/null || true

TAW_BIN="$TAW_BIN" TAW_AGENT_NAME="$TAW_AGENT" TAW_PANE_ID="$TMUX_PANE" \
TAW_HOOK_INPUT_FILE="$hook_input_file" python3 - <<'PY'
import json
import os
import subprocess
import time

taw_bin = os.environ.get("TAW_BIN")
agent = os.environ.get("TAW_AGENT_NAME", "grok")
pane_id = os.environ.get("TAW_PANE_ID")
hook_input_file = os.environ.get("TAW_HOOK_INPUT_FILE")

if not taw_bin or not pane_id:
    raise SystemExit(0)

hook_input = {}
if hook_input_file:
    try:
        with open(hook_input_file, encoding="utf-8") as handle:
            content = handle.read()
        if content.strip():
            parsed = json.loads(content)
            if isinstance(parsed, dict):
                hook_input = parsed
    except Exception:
        hook_input = {}


def first_text(*keys):
    for key in keys:
        value = hook_input.get(key)
        if isinstance(value, str) and value:
            return value
    return None


# Backstop: only report on session-start payloads. Grok emits "session_start" and accepts
# the Claude/Cursor spellings, so tolerate all three; a missing field is allowed for
# forward compatibility (herdr guard, kept verbatim).
hook_event_name = first_text("hook_event_name", "hookEventName")
if hook_event_name not in (None, "session_start", "SessionStart", "sessionStart"):
    raise SystemExit(0)
session_start_source = first_text("source")

# Grok injects GROK_SESSION_ID into every hook process; prefer it and fall back to the
# event payload's session id fields.
session_id = os.environ.get("GROK_SESSION_ID") or first_text("session_id", "sessionId")
if not isinstance(session_id, str) or not session_id:
    raise SystemExit(0)

args = [
    taw_bin, "pane", "report-agent-session", pane_id,
    "--source", "integration:" + agent,
    "--agent", agent,
    "--seq", str(time.time_ns()),
    "--agent-session-id", session_id,
]
if session_start_source:
    args += ["--session-start-source", session_start_source]

try:
    subprocess.run(
        args,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=5,
    )
except Exception:
    pass
PY
