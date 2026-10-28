#!/bin/sh
# Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
#
# Inside a sandbox (TAW_REPORT_FILE set) it writes the session's input to that file instead, and
# writes nothing when TAW_REPORT_AGENT names another agent.
#
# Session-only: this hook reports the pane's Antigravity conversation so taw can resume it.
# Lifecycle state comes from taw's screen detection.
#
# installed by: taw integration install {{AGENT}}
# managed by taw; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# Requires python3 (used for JSON parsing, matching upstream herdr behaviour).
#
# TAW_INTEGRATION_AGENT={{AGENT}}
# TAW_INTEGRATION_VERSION=2

set -eu

TAW_BIN={{TAW_BIN}}
# Antigravity CLI is `agy` everywhere taw names an agent (its detection manifest, its
# @agent_kind and its resume command), so that, not the install target's name, is what
# gets reported.
TAW_AGENT='agy'

# Antigravity CLI expects a JSON object on stdout and this hook never injects anything, so
# every exit path emits an empty object.
emit_and_exit() {
  printf '{}\n'
  exit 0
}

# Fast, no-op exits: never block the calling agent when we have nothing to do. Checked
# before touching stdin so a caller that never provides input is never kept waiting.
[ "${1:-}" = "session" ] || emit_and_exit

# Inside a sandbox (taw's own): the profile mounts the pane's report folder and
# names the file in TAW_REPORT_FILE. The session's input goes there as it came, at most 64 KiB,
# written beside the file and renamed into place, and nothing else is ever written: no taw, no
# python and no tmux are needed. taw reads it from outside, under this hook's rules.
if [ -n "${TAW_REPORT_FILE:-}" ]; then
  # Another agent's hook in here (an imported copy, or one run as a tool) writes nothing over the
  # sandbox's own agent's report.
  [ -z "${TAW_REPORT_AGENT:-}" ] || [ "$TAW_REPORT_AGENT" = "$TAW_AGENT" ] || emit_and_exit
  staged="$TAW_REPORT_FILE.$$"
  # The braces keep the shell's own "no such folder" complaint off the agent's screen too.
  if { head -c 65536 >"$staged"; } 2>/dev/null; then
    mv -f "$staged" "$TAW_REPORT_FILE" 2>/dev/null || rm -f "$staged"
  else
    rm -f "$staged"
  fi
  emit_and_exit
fi

[ -n "${TMUX_PANE:-}" ] || emit_and_exit
[ -x "$TAW_BIN" ] || emit_and_exit
command -v python3 >/dev/null 2>&1 || emit_and_exit

hook_input_file="$(mktemp "${TMPDIR:-/tmp}/taw-${TAW_AGENT}-hook.XXXXXX")" || emit_and_exit
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
# Cap at 64 KiB: hook payloads are small JSON objects; never buffer an unbounded stream.
head -c 65536 >"$hook_input_file" 2>/dev/null || true

TAW_BIN="$TAW_BIN" TAW_AGENT_NAME="$TAW_AGENT" TAW_PANE_ID="$TMUX_PANE" \
TAW_HOOK_INPUT_FILE="$hook_input_file" python3 - <<'PY' 2>/dev/null || true
import json
import os
import subprocess
import time

taw_bin = os.environ.get("TAW_BIN")
agent = os.environ.get("TAW_AGENT_NAME", "agy")
pane_id = os.environ.get("TAW_PANE_ID")
hook_input_file = os.environ.get("TAW_HOOK_INPUT_FILE")

if not taw_bin or not pane_id:
    raise SystemExit(0)

try:
    with open(hook_input_file, encoding="utf-8") as handle:
        payload = json.load(handle)
except Exception:
    raise SystemExit(0)

if not isinstance(payload, dict):
    raise SystemExit(0)


def text(name):
    value = payload.get(name)
    return value if isinstance(value, str) and value else None


session_id = text("conversationId")
if session_id is None:
    raise SystemExit(0)

args = [
    taw_bin, "pane", "report-agent-session", pane_id,
    "--source", "integration:antigravity-cli",
    "--agent", agent,
    "--seq", str(time.time_ns()),
    "--agent-session-id", session_id,
]

transcript_path = text("transcriptPath")
if transcript_path is not None:
    args += ["--agent-session-path", transcript_path]

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

emit_and_exit
