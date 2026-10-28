#!/bin/sh
# Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
#
# The `devin list` fallback's test injection variable is $TAW_DEVIN_LIST_JSON. Inside a sandbox
# (TAW_REPORT_FILE set) it writes the session's input to that file instead, and writes nothing
# when TAW_REPORT_AGENT names another agent.
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
agent = os.environ.get("TAW_AGENT_NAME", "devin")
pane_id = os.environ.get("TAW_PANE_ID")

if not taw_bin or not pane_id:
    raise SystemExit(0)


def load_hook_input(path):
    if not path:
        return {}
    try:
        with open(path, encoding="utf-8") as handle:
            content = handle.read()
        if not content.strip():
            return {}
        parsed = json.loads(content)
        return parsed if isinstance(parsed, dict) else {}
    except Exception:
        return {}


def load_session_list(project_dir):
    # A test (or a caller with the list already in hand) can supply the payload directly
    # rather than have this shell out to devin.
    injected = os.environ.get("TAW_DEVIN_LIST_JSON")
    if injected is not None:
        try:
            parsed = json.loads(injected)
            return parsed if isinstance(parsed, list) else []
        except Exception:
            return []

    try:
        result = subprocess.run(
            ["devin", "list", "--format", "json"],
            cwd=project_dir or None,
            capture_output=True,
            text=True,
            timeout=2,
            check=False,
        )
    except Exception:
        return []

    if result.returncode != 0:
        return []

    try:
        parsed = json.loads(result.stdout)
        return parsed if isinstance(parsed, list) else []
    except Exception:
        return []


def normalize_path(path):
    if not isinstance(path, str) or not path:
        return None
    try:
        return os.path.realpath(path)
    except Exception:
        return path


def first_text(payload, *keys):
    for key in keys:
        value = payload.get(key)
        if isinstance(value, str) and value:
            return value
    return None


def hook_event_name(payload):
    value = payload.get("hook_event_name")
    return value if isinstance(value, str) else ""


def allow_session_list_fallback(payload):
    # herdr guard, kept verbatim: these two events fire before devin has a session worth
    # looking up, so asking `devin list` would attribute somebody else's session to
    # this pane.
    event = hook_event_name(payload)
    if event == "UserPromptSubmit":
        return False
    if event == "SessionStart" and payload.get("source") == "startup":
        return False
    return True


def resolve_session_id(project_dir, payload):
    direct = first_text(payload, "session_id", "sessionId")
    if direct:
        return direct
    if not allow_session_list_fallback(payload):
        return None

    entries = load_session_list(project_dir)
    wanted = normalize_path(project_dir)
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        session_id = entry.get("id")
        if not isinstance(session_id, str) or not session_id:
            continue
        if normalize_path(entry.get("working_directory")) == wanted:
            return session_id
    return None


hook_input = load_hook_input(os.environ.get("TAW_HOOK_INPUT_FILE"))
project_dir = os.environ.get("DEVIN_PROJECT_DIR") or os.getcwd()

session_id = resolve_session_id(project_dir, hook_input)
if not session_id:
    raise SystemExit(0)

args = [
    taw_bin, "pane", "report-agent-session", pane_id,
    "--source", "integration:" + agent,
    "--agent", agent,
    "--seq", str(time.time_ns()),
    "--agent-session-id", session_id,
]

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
