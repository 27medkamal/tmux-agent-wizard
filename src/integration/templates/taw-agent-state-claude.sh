#!/bin/sh
# Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
#
# It skips Claude Code sessions embedded in Cursor, and refuses any event that is not
# SessionStart: Grok imports Claude's settings.json and may not honour the matcher of the
# installed hook (see claude.rs). Inside a sandbox (TAW_REPORT_FILE set) it writes the session's
# input to that file instead, and writes nothing when TAW_REPORT_AGENT names another agent.
#
# installed by: taw integration install {{AGENT}}
# managed by taw; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# Requires python3 (used for JSON parsing, matching upstream herdr behaviour).
#
# TAW_INTEGRATION_AGENT={{AGENT}}
# TAW_INTEGRATION_VERSION=5

set -eu

TAW_BIN={{TAW_BIN}}
TAW_AGENT='{{AGENT}}'
action="${1:-}"

# Inside a sandbox (taw's own): the profile mounts the pane's report folder and
# names the file in TAW_REPORT_FILE. The session's input goes there as it came, at most 64 KiB,
# written beside the file and renamed into place, and nothing else is ever written: no taw, no
# python and no tmux are needed. taw reads it from outside, under this hook's rules.
if [ -n "${TAW_REPORT_FILE:-}" ]; then
  # Another agent's hook in here (an imported copy, such as Grok running Claude's hooks, or
  # `claude -p` run as a Codex tool) writes nothing over the sandbox's own agent's report.
  [ -z "${TAW_REPORT_AGENT:-}" ] || [ "$TAW_REPORT_AGENT" = "$TAW_AGENT" ] || exit 0
  [ "$action" = session ] || exit 0
  # Cursor embeds Claude Code and says so: that run is not a plain claude session.
  [ -z "${CURSOR_VERSION+set}" ] || exit 0
  # Grok runs Claude's hooks too and says so; its own hook reports its session.
  [ -z "${GROK_SESSION_ID+set}" ] || exit 0
  staged="$TAW_REPORT_FILE.$$"
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
agent = os.environ.get("TAW_AGENT_NAME", "claude")
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

# A subagent's hook events carry a non-empty agent_id: never report those as the pane's
# own session (herdr guard, kept verbatim).
if hook_input.get("agent_id"):
    raise SystemExit(0)

# Cursor embeds Claude Code and sets CURSOR_VERSION (env) or cursor_version (hook
# payload) for that embedded run; skip it here so it does not get reported as a plain
# claude session (herdr guard, kept verbatim).
if "CURSOR_VERSION" in os.environ or "cursor_version" in hook_input:
    raise SystemExit(0)

hook_event_name = str(hook_input.get("hook_event_name") or "")
# Only a genuine SessionStart payload is ever reported. This also covers SubagentStop (a
# completion event for a *subagent* turn, which can fire after the main turn has already
# stopped and must never revive/relabel an idle pane) and anything Grok's imported copy
# of this script might be invoked under that is not SessionStart at all (herdr guard).
if hook_event_name != "SessionStart":
    raise SystemExit(0)

session_id = hook_input.get("session_id")
agent_session_id = session_id if isinstance(session_id, str) and session_id else None
if not agent_session_id:
    raise SystemExit(0)

transcript_path = hook_input.get("transcript_path")
agent_session_path = transcript_path if isinstance(transcript_path, str) and transcript_path else None

session_start_source = hook_input.get("source") if hook_event_name == "SessionStart" else None
if not isinstance(session_start_source, str) or not session_start_source:
    session_start_source = None

args = [
    taw_bin, "pane", "report-agent-session", pane_id,
    "--source", "integration:" + agent,
    "--agent", agent,
    "--seq", str(time.time_ns()),
    "--agent-session-id", agent_session_id,
]
if agent_session_path:
    args += ["--agent-session-path", agent_session_path]
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
