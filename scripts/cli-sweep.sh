#!/usr/bin/env bash
# Envelope conformance sweep: runs EVERY `taw` verb (success *and* failure paths)
# against a throwaway tmux server and checks each invocation's contract:
#
#   * stdout is one valid JSON envelope (except the two documented raw verbs)
#   * it carries an `id`, and exactly one of `result` / `error`
#   * a `result` has a `type`; an `error` has a code from the documented vocabulary
#   * the exit status matches the envelope: 0 for `result`, 1 for `error`
#   * nothing panics
#
# This is the mechanism behind the task report's "every verb machine-checked" claim, so
# the claim stays reproducible. `tests/cli_json.rs` asserts on *behaviour*; this asserts
# the envelope contract holds uniformly across the whole verb surface, which is what
# plugins and the agent skill depend on.
#
# Isolation: always `-L <unique socket>` with `-f /dev/null`, and everything it writes in one
# scratch directory that it removes when it ends: a home of its own, a config directory for
# each agent whose integration it installs or reads (`$CLAUDE_CONFIG_DIR`, `$CODEX_HOME`, the
# `$XDG_*` directories), zoxide's database, taw's config and state, and the pane's directory.
# It runs with none of the caller's environment but the variables that say where temporary
# files go, and how the terminal and the locale are, so it never reads or writes the files of
# whoever runs it. Its tmux sockets are in a directory of its own too (scripts/private-tmux-dir.sh),
# so that not even a tmux that names no socket can reach the server of whoever runs it: the sweep
# fails for anything it finds left there at the end.
#
# Usage: scripts/cli-sweep.sh [path/to/taw]
#   Defaults to target/debug/taw, building it if absent.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TAW="${1:-$REPO/target/debug/taw}"

if [[ ! -x "$TAW" ]]; then
  echo "building $TAW"
  (cd "$REPO" && cargo build --bin taw) || exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "SKIP: python3 is needed to validate the JSON envelopes" >&2
  exit 0
fi

# Where the proxy is that nothing is to listen on (see the exports below), unless a test puts a listener
# of its own there, to see what the sweep tries to reach: read before the environment is cleared.
PROXY="${TAW_SWEEP_PROXY:-http://127.0.0.1:1}"

# What the sweep runs does not see the caller's environment: the integrations look in a variable
# of their own for each agent (`$CLAUDE_CONFIG_DIR`, `$CODEX_HOME`, `$HERMES_HOME`, and one more
# for every agent that is added), and a list of those would miss the next. Only what says where
# temporary files go, and how the terminal and the locale are, is kept.
for name in $(compgen -e); do
  case "$name" in
    PATH | TERM | TMPDIR | LANG | LC_* | PWD | OLDPWD | SHLVL | _) ;;
    *) unset "$name" 2>/dev/null ;;
  esac
done

# shellcheck source=scripts/private-tmux-dir.sh
source "$REPO/scripts/private-tmux-dir.sh"

SOCK="taw-sweep-$$"
# The sweep's scratch directory, once it is made (below, after the traps).
SCRATCH=""

# Ends the server, takes its socket file away and removes the scratch directory. tmux leaves the
# socket file of a server that has ended where it was; this one is removed by the exact path the
# server said it listens on (SOCK_FILE, once it is up), once it no longer answers on it. The server's
# watchers write to their logs under the scratch directory until they see their pipes close, a moment
# after the server is gone, and a directory that gains a file while it is being removed is not: so it
# is removed again until it is gone. Then the sweep's tmux directory, which is empty now unless
# something went wrong (a tmux that named no socket, a server left running, a socket file left
# behind): what is there is said, and the sweep fails, whatever it found of the verbs.
SOCK_FILE=""
# shellcheck disable=SC2329  # run by the trap below
cleanup() {
  local tmux_left="" server=""
  # No server can be up before the sweep's tmux directory is made, and a tmux run without it would look
  # in /tmp (the caller's $TMUX_TMPDIR is not kept): so none is run then.
  if [[ -n "$PRIVATE_TMUX_DIR" ]]; then
    server="$(tmux -L "$SOCK" display-message -p '#{pid}' 2>/dev/null)"
    tmux -L "$SOCK" kill-server 2>/dev/null
    if [[ -n "$SOCK_FILE" && -S "$SOCK_FILE" ]]; then
      local waited=0
      while tmux -S "$SOCK_FILE" list-sessions >/dev/null 2>&1 && [[ "$waited" -lt 100 ]]; do
        sleep 0.05
        waited=$((waited + 1))
      done
      tmux -S "$SOCK_FILE" list-sessions >/dev/null 2>&1 || rm -f "$SOCK_FILE"
    fi
  fi
  # The server's jobs (the watchers its verbs started) run in its process group and outlive it: they are
  # ended before the directory goes, and it is kept, and said so, if they cannot be. As a group alone
  # (-<pid>): the server has gone by then, and its pid may be another process's.
  private_tmux_down ${server:+"-$server"} || tmux_left=1
  local tries=0
  while [[ -n "$SCRATCH" && -e "$SCRATCH" && "$tries" -lt 100 ]]; do
    rm -rf "$SCRATCH" 2>/dev/null
    [[ -e "$SCRATCH" ]] && sleep 0.1
    tries=$((tries + 1))
  done
  if [[ -n "$tmux_left" ]]; then
    echo "CLI SWEEP: FAILED, a tmux was left in the sweep's own socket directory"
    exit 1
  fi
}
trap cleanup EXIT
# A signal lets the verb that is running finish, and then exits through cleanup: one that ran at once
# would take the sweep's tmux directory away while that verb went on without it (tmux takes /tmp for a
# $TMUX_TMPDIR that is gone). And the traps are set before the two directories are made: a signal that
# came once one was made, and before they were, would end the sweep with nothing to take it away.
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# A tmux socket directory of the sweep's own, which is $TMUX_TMPDIR from here on: a tmux that names no
# socket, in a verb or a hook the sweep runs, finds no server there or starts one of its own, and
# never reaches the server of whoever runs the sweep. `cleanup` looks in it.
private_tmux_make /tmp/taw-sweep-tmux || { echo "cannot make a directory for the sweep's tmux sockets" >&2; exit 1; }

# With a template, so that $TMPDIR decides where it is: a bare `mktemp -d` on macOS asks the system
# for the user's temp directory first and reads $TMPDIR only as a fallback.
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/taw-sweep.XXXXXX")"
CFG="$SCRATCH/taw-config"
STATE="$SCRATCH/taw-state"
WORK="$SCRATCH/pane-cwd"
ERR_FILE="$SCRATCH/stderr.txt"
VERDICT_FILE="$SCRATCH/verdict.txt"
mkdir -p "$SCRATCH/home" "$SCRATCH/claude" "$SCRATCH/codex" "$SCRATCH/xdg/config" \
  "$SCRATCH/xdg/data" "$SCRATCH/xdg/state" "$SCRATCH/zoxide" "$CFG" "$STATE" "$WORK"

# The home, and a directory of its own for each of the variables the integrations and zoxide honour.
# The shells of the tmux server it starts get the same home, and no history file: the sweep types
# into the pane's shell (`pane run`, `send-text`), and a shell saves what it was typed when its
# server is killed, to $HISTFILE: ~/.bash_history where nothing says otherwise.
export HOME="$SCRATCH/home" ZDOTDIR="$SCRATCH/home" HISTFILE=/dev/null
export CLAUDE_CONFIG_DIR="$SCRATCH/claude" CODEX_HOME="$SCRATCH/codex"
export XDG_CONFIG_HOME="$SCRATCH/xdg/config" XDG_DATA_HOME="$SCRATCH/xdg/data"
export XDG_STATE_HOME="$SCRATCH/xdg/state" _ZO_DATA_DIR="$SCRATCH/zoxide"
# A proxy that nothing listens on, which taw's HTTP client reads from the environment: a fetch that is
# attempted anyway fails at once, at home (the marker below is what keeps it from being attempted).
export ALL_PROXY="$PROXY" all_proxy="$PROXY" NO_PROXY='' no_proxy=''
export TAW_TMUX_SOCKET_NAME="$SOCK" TAW_CONFIG_DIR="$CFG" TAW_STATE_DIR="$STATE"

# A watcher (`watch attach`) refreshes the cached manifests from herdr.dev in the background when
# the marker of the last check is missing or old, and the sweep must make no request over the
# network: a fresh marker says the cache was just checked. (The files that refresh writes, late,
# into the state directory were also what left the scratch directory behind now and then.)
mkdir -p "$STATE/agent-detection"
: > "$STATE/agent-detection/last-check"

# `/bin/sh` as the pane command: the host's login shell may print banners or open a
# prompt of its own, which would swallow the keystrokes the send/run verbs deliver.
tmux -L "$SOCK" -f /dev/null new-session -d -s one -x 80 -y 24 -c "$WORK" /bin/sh || exit 1
tmux -L "$SOCK" set-option -g default-shell /bin/sh
SOCK_FILE="$(tmux -L "$SOCK" display-message -p '#{socket_path}')"

PANE="$(tmux -L "$SOCK" display-message -p '#{pane_id}')"
WS="$(tmux -L "$SOCK" display-message -p '#{session_id}')"
WIN="$(tmux -L "$SOCK" display-message -p '#{window_id}')"

CHECKED=0
FAILED=0

# The documented error-code vocabulary (cli/envelope.rs `CODES`).
read -r -d '' VALIDATE <<'PY'
import json, sys
raw = sys.stdin.read()
try:
    v = json.loads(raw)
except Exception as e:
    print("BADJSON", e); sys.exit(1)
if "id" not in v:
    print("NOID"); sys.exit(1)
has_r, has_e = "result" in v, "error" in v
if has_r == has_e:
    print("BOTH-OR-NEITHER"); sys.exit(1)
if has_r:
    if "type" not in v["result"]:
        print("NO-RESULT-TYPE"); sys.exit(1)
    print("ok:" + v["result"]["type"]); sys.exit(0)
codes = {
    "not_found", "invalid_argument", "unsupported", "tmux_error", "timeout",
    "config_dir_missing", "trust_required", "plugin_build_failed", "unsupported_plugin_version",
    "checkout_unreadable", "not_a_git_repo", "session_exists",
    "workspace_group_close_required", "worktree_dirty",
    "agent_explain_file_read_failed",
    "hook_refused", "shell_not_ready", "agent_not_started",
    "agent_prompt_stalled", "agent_not_running", "agent_not_found",
    "agent_not_ready", "invalid_resume_argv", "resume_not_accepted",
    "machine_unreachable", "internal",
}
code = v["error"].get("code")
if code not in codes:
    print("UNDOCUMENTED-CODE", code); sys.exit(1)
print("err:" + code)
PY

# check <description> <taw args...>
check() {
  local desc="$1"; shift
  local out rc verdict
  CHECKED=$((CHECKED + 1))
  out="$("$TAW" "$@" 2>"$ERR_FILE")"
  rc=$?

  if grep -q 'panicked at' "$ERR_FILE"; then
    printf 'PANIC  %-30s taw %s\n' "$desc" "$*"
    head -3 "$ERR_FILE"
    FAILED=$((FAILED + 1))
    return
  fi

  printf '%s' "$out" | python3 -c "$VALIDATE" >"$VERDICT_FILE" 2>&1
  verdict="$(cat "$VERDICT_FILE")"
  case "$verdict" in
    ok:*)
      if [[ "$rc" != 0 ]]; then
        printf 'BAD    %-30s result envelope but exit %s\n' "$desc" "$rc"
        FAILED=$((FAILED + 1))
        return
      fi
      ;;
    err:*)
      if [[ "$rc" != 1 ]]; then
        printf 'BAD    %-30s error envelope but exit %s\n' "$desc" "$rc"
        FAILED=$((FAILED + 1))
        return
      fi
      ;;
    *)
      printf 'BAD    %-30s taw %s -> %s\n' "$desc" "$*" "$verdict"
      printf '       stdout: %s\n' "$out"
      FAILED=$((FAILED + 1))
      return
      ;;
  esac
  printf '  %-30s %s (exit %s)\n' "$desc" "$verdict" "$rc"
}

# check_raw <description> <expected substring> <taw args...>
# For the two verbs that deliberately print a raw payload instead of an envelope.
check_raw() {
  local desc="$1" needle="$2"; shift 2
  local out rc
  CHECKED=$((CHECKED + 1))
  out="$("$TAW" "$@" 2>"$ERR_FILE")"
  rc=$?
  if [[ "$rc" != 0 ]]; then
    printf 'BAD    %-30s raw verb exited %s\n' "$desc" "$rc"
    FAILED=$((FAILED + 1))
    return
  fi
  case "$out" in
    '{'*)
      printf 'BAD    %-30s raw verb emitted an envelope\n' "$desc"
      FAILED=$((FAILED + 1))
      return
      ;;
  esac
  if [[ "$out" != *"$needle"* ]]; then
    printf 'BAD    %-30s raw output missing %s\n' "$desc" "$needle"
    FAILED=$((FAILED + 1))
    return
  fi
  printf '  %-30s raw (exit %s)\n' "$desc" "$rc"
}

echo "########## agent ##########"
check "agent list"              agent list
check "agent get missing"       agent get ghost
check "agent read missing"      agent read ghost
check "agent send-keys missing" agent send-keys ghost esc
check "agent prompt missing"    agent prompt ghost hi
check "agent focus missing"     agent focus ghost
check "agent attach missing"    agent attach ghost
check "agent rename missing"    agent rename ghost bob
check "agent wait missing"      agent wait ghost --state idle --timeout 300
check "agent start bad kind"    agent start rev --kind nope --pane "$PANE"
check "agent explain no agent"  agent explain "$PANE"

"$TAW" pane report-agent "$PANE" --source sweep --agent claude --state idle >/dev/null
check "agent get"               agent get "$PANE"
check "agent read"              agent read "$PANE" --lines 20
check "agent read ansi"         agent read "$PANE" --format ansi
check "agent read visible"      agent read "$PANE" --source visible
check "agent read unwrapped"    agent read "$PANE" --source recent-unwrapped
check "agent read detection"    agent read "$PANE" --source detection
check "agent explain"           agent explain "$PANE"
check "agent explain --agent"   agent explain "$PANE" --agent codex
check "agent rename"            agent rename "$PANE" reviewer
check "agent rename invalid"    agent rename "$PANE" "BAD NAME"
check "agent wait"              agent wait reviewer --state idle --timeout 500
check "agent wait --until"      agent wait reviewer --until idle --timeout 500
check "agent wait timeout"      agent wait reviewer --state blocked --timeout 300
check "agent send-keys"         agent send-keys "$PANE" esc ctrl+c
check "agent send-keys bad"     agent send-keys "$PANE" not+a+key
check "agent prompt"            agent prompt "$PANE" true
check "agent focus"             agent focus "$PANE"
check "agent attach"            agent attach "$PANE" --takeover
check "agent rename --clear"    agent rename "$PANE" --clear
check "agent list (one)"        agent list

echo "########## pane ##########"
check "pane list"               pane list
check "pane list --workspace"   pane list --workspace "$WS"
check "pane current no env"     pane current
check "pane get"                pane get "$PANE"
check "pane get missing"        pane get w9:p99
check "pane layout"             pane layout "$PANE"
check "pane layout by tab"      pane layout "$WIN"
check "pane process-info"       pane process-info --pane "$PANE"
check "pane edges"              pane edges --pane "$PANE"
check "pane neighbor none"      pane neighbor --pane "$PANE" --direction left
check "pane focus none"         pane focus --pane "$PANE" --direction left
check "pane resize"             pane resize --pane "$PANE" --direction up --amount 2
check "pane resize negative"    pane resize --pane "$PANE" --direction up --amount=-1
check "pane zoom --on"          pane zoom "$PANE" --on
check "pane zoom --off"         pane zoom "$PANE" --off
check "pane zoom --toggle"      pane zoom "$PANE" --toggle
check "pane zoom on+off"        pane zoom "$PANE" --on --off
check "pane read"               pane read "$PANE"
check "pane read --raw"         pane read "$PANE" --raw
check "pane rename"             pane rename "$PANE" build logs
check "pane rename --clear"     pane rename "$PANE" --clear
check "pane rename no label"    pane rename "$PANE"
check "pane input"              pane input "$PANE" --right-click pane
check "pane split"              pane split "$PANE" --direction down --no-focus
NEW="$("$TAW" pane list | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["panes"][-1]["pane_id"])')"
check "pane split --ratio"      pane split "$PANE" --direction right --ratio 0.3 --no-focus
check "pane split bad ratio"    pane split "$PANE" --ratio 9 --no-focus
check "pane split bad env"      pane split "$PANE" --env NOEQUALS --no-focus
check "pane neighbor found"     pane neighbor --pane "$PANE" --direction down
check "pane focus found"        pane focus --pane "$PANE" --direction down
check "pane swap panes"         pane swap --source-pane "$PANE" --target-pane "$NEW"
check "pane swap --direction"   pane swap --pane "$PANE" --direction down
check "pane swap no target"     pane swap --pane "$PANE"
check "pane send-text"          pane send-text "$PANE" hello
check "pane send-keys"          pane send-keys "$PANE" enter
check "pane run"                pane run "$PANE" true
check "pane clear"              pane clear "$PANE"
check "pane clear missing"      pane clear w9:p99
check "pane wait-output hit"    pane wait-output "$PANE" --match '$' --timeout 2000
check "pane wait-output miss"   pane wait-output "$PANE" --match zzzznope --timeout 400
check "pane wait-output regex"  pane wait-output "$PANE" --regex . --timeout 2000
check "pane wait-output both"   pane wait-output "$PANE" --match a --regex b --timeout 100
check "pane wait-output none"   pane wait-output "$PANE" --timeout 100
check "pane wait-output bad re" pane wait-output "$PANE" --regex '[' --timeout 100
check "pane move --new-tab"     pane move "$NEW" --new-tab --no-focus
check "pane move no flags"      pane move "$PANE"
check "pane report-agent"       pane report-agent "$PANE" --source sweep --agent claude --state working
check "pane report-a-session"   pane report-agent-session "$PANE" --source sweep --agent claude --agent-session-id s1 --session-start-source startup
check "pane report-metadata"    pane report-metadata "$PANE" --source sweep --display-agent Rev --title T --token k=v
check "pane bad state-label"    pane report-metadata "$PANE" --source sweep --state-label nope=X
check "pane bad token name"     pane report-metadata "$PANE" --source sweep --token "bad name=v"
check "pane metadata clear"     pane report-metadata "$PANE" --source sweep --clear-title --clear-display-agent --clear-state-labels --clear-token k
check "pane release-agent"      pane release-agent "$PANE" --source sweep --agent claude
check "pane release again"      pane release-agent "$PANE" --source sweep --agent claude
check "pane close"              pane close "$NEW"

echo "########## workspace / tab ##########"
check "workspace list"          workspace list
check "workspace get"           workspace get "$WS"
check "workspace get missing"   workspace get w99
check "workspace create"        workspace create --label sweep2 --cwd "$WORK" --no-focus
check "workspace create --env"  workspace create --label sweep3 --env A=B --no-focus
check "workspace create bad env" workspace create --label sweep4 --env NOEQ --no-focus
check "workspace rename"        workspace rename sweep2 sweep2b
check "workspace metadata"      workspace report-metadata "$WS" --source sweep --token a=b
check "workspace clear token"   workspace report-metadata "$WS" --source sweep --clear-token a
check "workspace focus"         workspace focus "$WS"
check "workspace id no env"     workspace id
check "workspace template"      workspace template
check "workspace close"         workspace close sweep2b
check "tab list"                tab list
check "tab list --workspace"    tab list --workspace "$WS"
check "tab create"              tab create --workspace "$WS" --label logs --no-focus
check "tab get"                 tab get "$WIN"
check "tab get missing"         tab get w9:t99
check "tab rename"              tab rename "$WIN" main
check "tab focus"               tab focus "$WIN"

echo "########## worktree / notification / session ##########"
check "worktree list"           worktree list
check "worktree create no repo" worktree create --workspace "$WS" --branch x
check "worktree create no name" worktree create --workspace "$WS" --branch ""
check "worktree open missing"   worktree open --workspace nope
check "worktree open no arg"    worktree open
check "worktree remove missing" worktree remove --workspace nope
check "notification show"       notification show hello --body world
check "notification --sound"    notification show hi --sound request --position top-right
check "notification open-target" notification open-target
check "session list"            session list
check "session save"            session save --name snap
check "session restore --dry"   session restore --name snap --dry-run
check "session restore missing" session restore --name nosuch
check "session autosave"        session autosave
check "session delete"          session delete snap
check "session delete missing"  session delete snap

echo "########## integration / plugin / manifest ##########"
check "integration status"      integration status
check "integration status one"  integration status claude
check "integration status bad"  integration status nope
check "integration install bad" integration install nope
check "integration uninst bad"  integration uninstall nope
# devin is a real integration: its directory is under the scratch $XDG_CONFIG_HOME, which has none
# yet, so the first install is refused, and the rest run for real on files of the sweep's own.
check "integration install none" integration install devin
mkdir -p "$XDG_CONFIG_HOME/devin"
check "integration install"     integration install devin
check "integration status devin" integration status devin
check "integration uninstall"   integration uninstall devin
check "plugin list"             plugin list
check "plugin list --plugin"    plugin list --plugin nope
check "plugin config-dir"       plugin config-dir demo
check "plugin log"              plugin log demo
check "plugin log list"         plugin log list
check "plugin action list"      plugin action list
check "plugin action invoke"    plugin action invoke nope.act
check "plugin pane open miss"   plugin pane open nope.pane
check "plugin pane focus miss"  plugin pane focus nope.pane
check "plugin pane close miss"  plugin pane close nope.pane
check "plugin enable missing"   plugin enable ghost
check "plugin disable missing"  plugin disable ghost
check "plugin unlink missing"   plugin unlink ghost
check "plugin uninstall missing" plugin uninstall ghost
check "plugin link missing"     plugin link /nonexistent/path
check "plugin install two refs" plugin install owner/repo@a --ref b
check "plugin startup"          plugin startup
check "manifest list"           manifest list
check "manifest show"           manifest show claude
check "manifest show missing"   manifest show nope

echo "########## watch / seen / status / config / skill ##########"
check "watch status"            watch status
check "watch attach"            watch attach "$PANE"
check "watch detach"            watch detach "$PANE"
check "watch attach-all"        watch attach-all
check "watch attach-all restart" watch attach-all --restart
check "watch detach again"      watch detach "$PANE"
check "watch attach missing"    watch attach w9:p99
check "watch detach missing"    watch detach w9:p99
check "seen"                    seen "$PANE"
check "seen missing"            seen w9:p99
check "status"                  status
check "status refresh"          status refresh
check "config check"            config check
check "config path"             config path
check "machine list"             machine list
check "machine check --json"     machine check --json
# The two documented raw verbs: payload straight to stdout, no envelope.
check_raw "config default"      "[ui.toast]" config default
check_raw "skill"               "taw"        skill

echo
echo "checked $CHECKED invocations"
if [[ "$FAILED" == 0 ]]; then
  echo "CLI SWEEP: OK"
  exit 0
fi
echo "CLI SWEEP: $FAILED FAILED"
exit 1
