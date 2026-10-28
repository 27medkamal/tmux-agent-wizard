#!/usr/bin/env bash
# shellcheck disable=SC2015  # `cond && ok || bad` is intentional; ok/bad always return 0
# tmux-agent-wizard end-to-end harness (sourceable library).
#
#   source tests/e2e/harness.sh
#   trap e2e_down EXIT     # first: a run that fails or is stopped while it comes up takes it down too
#   e2e_up smoke           # bring up an isolated inner+outer tmux server pair
#   ... drive it with tm / taw / keys / screen ...
#   e2e_down               # tear the pair down (the trap's call then does nothing)
#
# ISOLATION: every tmux command targets a private socket named
# "taw-e2e-<slice>-<pid>"; the user's default tmux server is never addressed.
# And the sockets are in a tmux directory of the run's own ($TMUX_TMPDIR, from
# scripts/private-tmux-dir.sh), so that a tmux that names no socket could not
# reach that server either; e2e_down says so when one did.
# taw's config and state live under a per-run temp dir. Nothing here reads or
# writes ~/.config, ~/.claude, or the real ~/.local/state/tmux-agent-wizard.
# The shells the two servers run have no history file (a shell that is typed into
# saves what it was typed when its server is killed, to ~/.bash_history where
# nothing says otherwise) and a home of their own, $DIR/shell-home, for HOME and
# ZDOTDIR: no shell here reads the startup files of whoever runs the suite.
#
# Requires: bash, a tmux >= 3.2 on PATH, and a built `taw` (this script builds
# it via `mise run build` if bin/taw is missing).
set -u

# This script (and so every tmux server it starts) may itself be running inside
# some ambient, unrelated tmux session -- an agent's own sandbox, a developer's
# terminal, CI. `$TMUX`/`$TMUX_PANE` from that outer session would otherwise be
# inherited by the inner/outer servers below and leak into every `run-shell`
# they spawn, naming a pane on a *different* socket. taw already guards against
# exactly this for a headless invocation's own use of `$TMUX`/`$TMUX_PANE` (see
# src/cli/plugin.rs's caller-pane check and src/cli/common.rs's `caller_session_id`);
# this harness needs the same guard so "isolated from your real tmux" (this
# file's own header) holds for these two variables as much as it does for the
# socket name and config/state dirs below.
unset TMUX TMUX_PANE
# And the variables that point git at another repository than the one its directory names: git runs
# the command of a `rebase -x` with GIT_DIR set, and the sections that make a repository of their own
# (`git init`, `git commit`) would write to the one being rebased (taw's own git takes them out too:
# src/git.rs).
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_NAMESPACE GIT_CONFIG GIT_IMPLICIT_WORK_TREE GIT_GRAFT_FILE GIT_NO_REPLACE_OBJECTS GIT_REPLACE_REF_BASE GIT_PREFIX GIT_SHALLOW_FILE
# And the config of whoever runs the run: a global git config may sign every commit with their ssh key
# (`commit.gpgsign=true`), and a section that makes a repository commits in it. No file, so no signing
# and no name: a commit names its own (`-c user.name=... -c user.email=...`).
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1

# --- Locate the repo and the binary, build if needed --------------------------
e2e_repo() {
  # Directory two levels up from this file (tests/e2e/ -> repo root).
  local src="${BASH_SOURCE[0]}"
  cd "$(dirname "$src")/../.." && pwd
}
E2E_REPO="${E2E_REPO:-$(e2e_repo)}"
E2E_TMP="${E2E_TMP:-${TMPDIR:-/tmp}/taw-e2e}"
E2E_FIXTURES="$E2E_REPO/tests/fixtures/fake-agents"
# A tmux socket directory of the run's own (e2e_up makes it, e2e_down looks in it and removes it).
# shellcheck source=scripts/private-tmux-dir.sh
source "$E2E_REPO/scripts/private-tmux-dir.sh"
# And a net of refusing stubs first on PATH, in that directory, for every agent and container runtime a
# section could run from the PATH of whoever runs it (e2e_up makes it; acceptance.sh looks in its log).
# shellcheck source=scripts/agent-net.sh
source "$E2E_REPO/scripts/agent-net.sh"
# And the same for the desktop of whoever runs it: a section that showed a notification or played a sound
# with no recorder would put it on their screen or their speakers (e2e_up makes it; acceptance.sh looks in
# its log).
# shellcheck source=scripts/desktop-net.sh
source "$E2E_REPO/scripts/desktop-net.sh"

e2e_bin() {
  if [ -n "${TAW_BIN:-}" ] && [ -x "$TAW_BIN" ]; then echo "$TAW_BIN"; return; fi
  if [ -x "$E2E_REPO/bin/taw" ]; then echo "$E2E_REPO/bin/taw"; return; fi
  if [ -x "$E2E_REPO/target/release/taw" ]; then echo "$E2E_REPO/target/release/taw"; return; fi
  echo "building taw (bin/taw missing)..." >&2
  ( cd "$E2E_REPO" && { mise run build >/dev/null 2>&1 || cargo build --release >/dev/null 2>&1; } )
  if [ -x "$E2E_REPO/bin/taw" ]; then echo "$E2E_REPO/bin/taw"; else echo "$E2E_REPO/target/release/taw"; fi
}
TAW_BIN="$(e2e_bin)"

# --- Bring up / tear down an isolated inner+outer server pair ------------------
# Inner server: the plugin is loaded, panes run a non-interactive /bin/sh with
# the fixture dir first on PATH (so a bare `claude` in a pane is the fixture,
# not any real agent the login shell would resolve). Outer server: a single
# pane holds an attached client of the inner server, giving the inner server a
# real tty (needed for display-popup / display-message / notifications).
e2e_up() {
  SLICE="${1:-run}"
  # A tmux that is run with neither -L nor -S would reach the default socket, the server of whoever runs
  # this: so the run has a tmux socket directory of its own (scripts/private-tmux-dir.sh), with $TMUX
  # unset, which e2e_down looks in and removes. The two servers' sockets are in it too: a caller that
  # reaches them from outside (tests/common/harness.rs) does so by the exact paths, E2E_SOCK_FILE and
  # E2E_OUTER_FILE. It is made in /tmp, or in E2E_TMUX_BASE, a directory of a test's own that ends
  # whatever a run that died before its e2e_down left there (tests/harness_cleanup.rs).
  private_tmux_make "${E2E_TMUX_BASE:-/tmp}/taw-e2e-tmux" || return 1
  # Before the servers' PATH is written: their panes find the fixtures first, then the nets.
  agent_net_make "$PRIVATE_TMUX_DIR/net" || return 1
  desktop_net_make "$PRIVATE_TMUX_DIR/desktop-net" || return 1
  # A signal to the script lets the command it is running finish, and then exits through its EXIT trap
  # (e2e_down): one that ran at once would take the run's directory away while that command went on
  # without it, and tmux takes /tmp for a $TMUX_TMPDIR that is gone. A trap the script set itself stays.
  # In a script only: in an interactive shell (the README's way to explore) the next Ctrl-C at the
  # prompt would exit the shell, with the harness still up.
  case "$-" in
    *i*) ;;
    *)
      [ -n "$(trap -p TERM)" ] || trap 'exit 143' TERM
      [ -n "$(trap -p INT)" ] || trap 'exit 130' INT
      [ -n "$(trap -p HUP)" ] || trap 'exit 129' HUP
      ;;
  esac
  SOCK="taw-e2e-$SLICE-$$"
  OUTER="taw-e2e-$SLICE-$$-outer"
  DIR="$E2E_TMP/$SLICE-$$"
  mkdir -p "$DIR/config" "$DIR/state" "$DIR/home" "$DIR/zoxide" "$DIR/shell-home"
  # An e2e run NEVER fires a real macOS/Linux popup or plays a sound. It records the notifications its
  # sections check, with a stub of its own in $TAW_NOTIFY_CMD in place of the desktop net's refusing
  # recorder, which logs to a file the tests can read; and it plays no sound: $TAW_SOUND_CMD stays the
  # net's refusing recorder, so a sound that a section played fails the run's last check. Both go into
  # the servers' launch environment (below), which is what a pipe-pane watcher child actually inherits.
  E2E_NOTIFY_LOG="$DIR/notify.log"; : > "$E2E_NOTIFY_LOG"
  printf '#!/bin/sh\necho "$@" >> "%s"\n' "$E2E_NOTIFY_LOG" > "$DIR/notify-stub.sh"
  chmod +x "$DIR/notify-stub.sh"
  export TAW_NOTIFY_CMD="$DIR/notify-stub.sh"
  export TAW_CONFIG_DIR="$DIR/config" TAW_STATE_DIR="$DIR/state" TAW_TMUX_SOCKET_NAME="$SOCK"
  # zoxide gets a database of its own. Opening a directory runs `zoxide add`, and a section
  # with no fake zoxide first on PATH (the popup's picker, the first `session open`) would
  # otherwise put a temp path into the database of whoever runs the suite.
  export _ZO_DATA_DIR="$DIR/zoxide"
  # The cached manifests are refreshed from herdr.dev, by the plugin's startup one-shot and by every
  # watcher, when the marker of the last check is missing or old. A run makes no request over the
  # network (and no rule of a manifest fetched a minute ago changes under a check): the marker is
  # fresh from the start.
  mkdir -p "$DIR/state/agent-detection" && : > "$DIR/state/agent-detection/last-check"
  # And under that, a net: a proxy that nothing listens on, which taw's HTTP client reads from the
  # environment, so that a fetch that is attempted anyway fails at once, at home. (E2E_PROXY puts a
  # listener of a test's own there, to see what is attempted: tests/offline.rs does.)
  export ALL_PROXY="${E2E_PROXY:-http://127.0.0.1:1}" all_proxy="${E2E_PROXY:-http://127.0.0.1:1}"
  export NO_PROXY="" no_proxy=""
  cat > "$DIR/tmux.conf" <<CONF
set -g default-terminal tmux-256color
set -g default-command 'export PATH=$E2E_FIXTURES:\$PATH; exec /bin/sh'
set -g mouse on
set -g status-right ' #{@agent_wizard_status}'
set -g status-right-length 60
set -g detach-on-destroy off
set-environment -g TAW_CONFIG_DIR $DIR/config
set-environment -g TAW_STATE_DIR $DIR/state
set-environment -g TAW_TMUX_SOCKET_NAME $SOCK
set-environment -g _ZO_DATA_DIR $DIR/zoxide
set-environment -g PATH $E2E_FIXTURES:$PATH
set -g @agent-wizard-bin $TAW_BIN
run-shell "$E2E_REPO/tmux-agent-wizard.tmux"
CONF
  # The shells' variables go on the commands that start the servers, which is where a server (and its
  # first pane) takes its environment from: a set-environment afterwards cannot reach the first pane.
  # Not exported here: the rest of a script that sources this keeps the home it was run with. With them
  # go the places a person's own files are found by besides the home (the XDG directories and every
  # agent's config, the names of tests/common/shell_env.rs's PINNED, which the guard holds this to),
  # inside that home; zoxide's is the run's own, exported above.
  local h="$DIR/shell-home"
  local pinned=(
    XDG_CONFIG_HOME="$h/.config" XDG_DATA_HOME="$h/.local/share" XDG_STATE_HOME="$h/.local/state"
    ANTIGRAVITY_CLI_CONFIG_DIR="$h/.gemini/config" CLAUDE_CONFIG_DIR="$h/.claude" CODEX_HOME="$h/.codex"
    COPILOT_HOME="$h/.copilot" CURSOR_CONFIG_DIR="$h/.cursor" GROK_HOME="$h/.grok"
    HERMES_HOME="$h/.hermes" KIMI_CODE_HOME="$h/.kimi-code" PI_CODING_AGENT_DIR=
    PI_CONFIG_DIR="$h/.omp" QODER_CONFIG_DIR="$h/.qoder" QWEN_HOME="$h/.qwen"
  )
  HISTFILE=/dev/null HOME="$h" ZDOTDIR="$h" env "${pinned[@]}" tmux -L "$SOCK" -f "$DIR/tmux.conf" new-session -d -s main -x 180 -y 45 -c "$DIR/home"
  HISTFILE=/dev/null HOME="$h" ZDOTDIR="$h" env "${pinned[@]}" tmux -L "$OUTER" -f /dev/null new-session -d -s host -x 180 -y 45 "tmux -L $SOCK attach -t main"
  # tmux leaves the socket file of a server that has ended where it was: each one is noted, by the
  # exact path its server says it listens on, for e2e_down to take away.
  E2E_SOCK_FILE=$(tmux -L "$SOCK" display-message -p '#{socket_path}' 2>/dev/null)
  E2E_OUTER_FILE=$(tmux -L "$OUTER" display-message -p '#{socket_path}' 2>/dev/null)
  # Wait (bounded) for the inner client to attach rather than sleeping blind.
  local i=0
  while [ "$i" -lt 50 ]; do
    tmux -L "$SOCK" list-clients -F '#{client_tty}' 2>/dev/null | grep -q . && break
    i=$((i+1)); command sleep 0.1
  done
  tmux -L "$SOCK" list-clients -F '#{client_tty}' 2>/dev/null | grep -q . \
    && echo "up: inner=$SOCK outer=$OUTER dir=$DIR" \
    || { echo "FAILED: no inner client attached"; return 1; }
}
# Removes the socket file of a test server that has ended, by the exact path the server said it listened
# on: only a socket file with a test server's name (taw-...), and only once the server no longer answers
# on it (a server that is still there after five seconds keeps its file: it is how it is reached).
e2e_remove_socket() {
  local path="${1:-}" i=0
  [ -n "$path" ] && [ -S "$path" ] || return 0
  case "${path##*/}" in taw-*) ;; *) return 0 ;; esac
  while tmux -S "$path" list-sessions >/dev/null 2>&1; do
    i=$((i+1)); [ "$i" -lt 100 ] || return 0
    command sleep 0.05
  done
  rm -f "$path"
}
e2e_down() {
  local server pid groups=""
  e2e_end_tracked
  for server in "${OUTER:-}" "${SOCK:-}"; do
    [ -n "$server" ] || continue
    pid="$(tmux -L "$server" display-message -p '#{pid}' 2>/dev/null)"
    groups="$groups${pid:+ -$pid}"
    tmux -L "$server" kill-server 2>/dev/null
  done
  e2e_remove_socket "${E2E_OUTER_FILE:-}"
  e2e_remove_socket "${E2E_SOCK_FILE:-}"
  # The sockets of the test servers a section started (e2e_track_socket), whose servers e2e_end_tracked ended.
  for server in ${E2E_SOCKET_FILES[@]+"${E2E_SOCKET_FILES[@]}"}; do e2e_remove_socket "$server"; done
  E2E_SOCKET_FILES=()
  # The servers' jobs (the plugin's one-shots, the watchers, the hooks) run in their servers' process
  # groups and outlive them: they are ended before the run's tmux directory goes, and it is kept if they
  # cannot be. Each as a group alone (-<pid>): the servers have gone by then, and a pid of theirs may be
  # another process's. What is left in the directory says what went wrong (a tmux that named no socket,
  # a server that did not end with its socket): said, and ended, before it goes.
  # shellcheck disable=SC2086  # the groups, a word each
  private_tmux_down $groups || echo "FAILED: a tmux was left in the run's own socket directory" >&2
  echo "down: ${SOCK:-?}/${OUTER:-?}"
  # Forgotten once they are down, so that a second call (a script's own, then its EXIT trap's) does
  # nothing: by then the run's tmux directory is gone, and tmux would take /tmp for it.
  SOCK="" OUTER="" E2E_SOCK_FILE="" E2E_OUTER_FILE=""
}

# --- Processes a run starts and does not wait for -----------------------------------------------
# A process that a section leaves running (the `cat` of a terminal's input, below) is noted with
# e2e_track, by its pid and by the exact command line it has, and e2e_down ends it with the run, pass
# or fail (the run's EXIT trap calls it). Only a process that still has that command line is ended: a
# pid that some other process has been given since is not this run's, and is left alone.
E2E_TRACKED=()
e2e_command_of() { ps -ww -o command= -p "$1" 2>/dev/null; }   # the whole command line of a pid, or nothing
e2e_track() { E2E_TRACKED+=("$1 $2"); }   # e2e_track <pid> <its command line, whole>
# The socket file of a test server a section started (its own `tmux -L`), by the path the server says it listens
# on: e2e_down removes it, once the server (noted with e2e_track by its pid) has ended, as it does the run's own.
E2E_SOCKET_FILES=()
e2e_track_socket() { E2E_SOCKET_FILES+=("$1"); }   # e2e_track_socket <socket path>
e2e_end_tracked() {
  local entry pid cmd
  for entry in ${E2E_TRACKED[@]+"${E2E_TRACKED[@]}"}; do
    pid="${entry%% *}"; cmd="${entry#* }"
    [ "$(e2e_command_of "$pid")" = "$cmd" ] && kill "$pid" 2>/dev/null
  done
  E2E_TRACKED=()
}
# How many processes have exactly this command line: the line is read off `ps` whole and must be the
# same, so no other run's process counts and nothing is matched by a pattern. Read only: for a check
# that a run has left none of a process it started.
# shellcheck disable=SC2009  # pgrep -f takes a pattern; this compares the whole line as a plain string
e2e_running() { ps -ww -Ao command= | grep -cxF -- "$1"; }

# --- A terminal to attach a client from, outside tmux ---------------------------------------------
# e2e_terminal <fifo> <command...>: runs <command> in the background on a pty from `script`, as a
# terminal would, and sets E2E_TERM_PID to the pid of that `script` (killing it detaches the client;
# never `wait` for it: bash waits for the whole pipeline, and the `cat` below ends only when the fifo
# is let go).
#
# Its input is held open by a `cat` of <fifo>, which the caller keeps open for writing (`exec 7<>fifo`,
# and `exec 7>&-` to let go) for as long as the terminal is to stay: an input that closes sends the
# command's pty an end-of-file, which a client attached there hands to its pane's shell as `exit`, and
# BSD `script` refuses a fifo for its own input but takes the pipe from `cat`.
#
# That `cat` is noted for e2e_down (E2E_TERM_CAT_PID is its pid), which ends it with the run. And
# neither it nor `script` has descriptor 7: a process that had it would hold the fifo open as well,
# so that the `cat` could never see the end of the file when the caller lets go, which is how every
# run left three of them behind for good (ppid 1, hours and days old).
# shellcheck disable=SC2034  # E2E_TERM_PID and E2E_TERM_CAT_PID are the caller's to read
e2e_terminal() {
  local fifo="$1" pidfile n=0
  shift
  pidfile="$(mktemp "$fifo.pid.XXXXXX")" || return 1
  # The pid of the `cat` is written by the process that becomes it (exec keeps the pid).
  if script --version 2>&1 | grep -qi util-linux; then
    sh -c 'echo $$ > "$1"; exec cat "$2"' sh "$pidfile" "$fifo" 7>&- | script -qec "$(printf '%q ' "$@")" /dev/null 7>&- >/dev/null 2>&1 &
  else
    sh -c 'echo $$ > "$1"; exec cat "$2"' sh "$pidfile" "$fifo" 7>&- | script -q /dev/null "$@" 7>&- >/dev/null 2>&1 &
  fi
  E2E_TERM_PID=$!   # the last of the pipeline: script
  # Waits for that pid, and then for the line `ps` shows for it to say it has become the `cat`: what
  # is noted is what e2e_end_tracked will find, whenever it looks.
  E2E_TERM_CAT_PID=""
  while [ "$n" -lt 80 ]; do
    [ -n "$E2E_TERM_CAT_PID" ] || E2E_TERM_CAT_PID="$(tr -d ' \n' < "$pidfile" 2>/dev/null)"
    [ -n "$E2E_TERM_CAT_PID" ] && [ "$(e2e_command_of "$E2E_TERM_CAT_PID")" = "cat $fifo" ] && break
    command sleep 0.05; n=$((n+1))
  done
  rm -f "$pidfile"
  [ -n "$E2E_TERM_CAT_PID" ] && [ "$(e2e_command_of "$E2E_TERM_CAT_PID")" = "cat $fifo" ] || return 1
  e2e_track "$E2E_TERM_CAT_PID" "cat $fifo"
}

# --- Convenience wrappers -----------------------------------------------------
tm()     { tmux -L "$SOCK" "$@"; }                       # inner server
outer()  { tmux -L "$OUTER" "$@"; }                      # outer server (drives the client)
taw()    { "$TAW_BIN" "$@"; }                            # CLI (env already points at the inner socket)
# E2E_HOST names the outer window whose client is driven: `host`, the harness's own, unless a
# section attaches another client of its own (acceptance.sh, a group close from inside its sessions).
keys()   { outer send-keys -t "${E2E_HOST:-host}" "$@"; }   # keystrokes -> attached client -> inner server
screen() { outer capture-pane -p -t "${E2E_HOST:-host}"; }  # what the user would see (plain)
screenc(){ outer capture-pane -p -e -t "${E2E_HOST:-host}"; } # ... with colour escapes
prefix() { keys C-b "$@"; }                              # inner prefix is C-b

# Run the fixture agent in a fresh inner window and return its pane id on stdout.
# Detection is driven by the kind hint (taw agent start), which is how a spawn
# is identified regardless of the fixture's interpreter shebang. Second arg is
# the target session (default "main"); either way it is always explicit -- see
# e2e_start_replay's comment -- an unqualified `new-window -d` follows tmux's own
# notion of the "current" session, which an earlier `switch-client` elsewhere
# (this suite's own code, e.g. `taw workspace create`) can have moved off "main"
# (or off whatever session the caller actually meant).
#
# `agent start` refuses a name another live agent has, and this suite starts many agents of a
# kind, so each is started under a name of its own (the kind and the pane's number). The name is
# taken off again: the popup draws an agent's name where it has one and its kind where it has not,
# and the rows of every section here are the kind's.
e2e_start_agent() {
  local kind="${1:-claude}" session="${2:-main}"
  local win; win=$(tm new-window -t "$session" -P -F '#{pane_id}' -d)
  taw agent start --kind "$kind" --pane "$win" "${kind}-${win#%}" >/dev/null 2>&1
  tm set-option -p -u -t "$win" @agent_name 2>/dev/null
  echo "$win"
}
# Poll a pane option until it equals a value, up to <timeout_ms>. 0 = ok.
e2e_wait_opt() {
  local pane="$1" opt="$2" want="$3" timeout="${4:-3000}"; local waited=0
  while [ "$waited" -lt "$timeout" ]; do
    [ "$(tm display-message -p -t "$pane" "#{$opt}" 2>/dev/null)" = "$want" ] && return 0
    command sleep 0.2; waited=$((waited+200))
  done
  return 1
}
# The other half of `e2e_wait_opt`: poll a pane's @agent_state every 50 ms for <ms>, and
# fail (1) on the first value that is not <state> -- for a state that must hold, where a
# later poll would only see it already back. That first other value is left in
# E2E_STATE_LEFT for the failure message. 0 = it held for the whole window.
# shellcheck disable=SC2034  # E2E_STATE_LEFT is the caller's to read
e2e_state_stays() {
  local pane="$1" want="$2" ms="${3:-3000}"; local waited=0 now
  E2E_STATE_LEFT=""
  while [ "$waited" -lt "$ms" ]; do
    now=$(tm display-message -p -t "$pane" '#{@agent_state}' 2>/dev/null)
    [ "$now" = "$want" ] || { E2E_STATE_LEFT="$now"; return 1; }
    command sleep 0.05; waited=$((waited+50))
  done
  return 0
}
# Poll the outer client's rendered screen (see `screen`) for an extended-regex match,
# up to <timeout_ms>. Same bounded-poll idiom as `e2e_wait_opt`, for asserting on
# what a real user would see (a popup rendered, a display-message landed) instead of
# a tmux option -- a fixed sleep before that kind of assertion is a latent flake
# under a slow/contended host. 0 = matched before the timeout.
e2e_wait_screen() {
  local pattern="$1" timeout="${2:-3000}"; local waited=0
  while [ "$waited" -lt "$timeout" ]; do
    screen | grep -qE "$pattern" && return 0
    command sleep 0.05; waited=$((waited+50))
  done
  return 1
}
# The same poll over one inner pane's own screen (`capture-pane` of that pane, not the
# client's view): for asserting what a pane shows before the popup covers it.
e2e_wait_pane() {
  local pane="$1" pattern="$2" timeout="${3:-3000}"; local waited=0
  while [ "$waited" -lt "$timeout" ]; do
    tm capture-pane -p -t "$pane" 2>/dev/null | grep -qE "$pattern" && return 0
    command sleep 0.1; waited=$((waited+100))
  done
  return 1
}

# Runs the `replay` fixture in a fresh window, with identity coming *only* from
# @agent_kind_hint (never from the pane's own command name/interpreter -- replay is
# POSIX sh, so `#{pane_current_command}` reads "sh", which names no bundled manifest).
# Prints the new pane id on stdout. See tests/fixtures/fake-agents/replay.
#
# `-t main` is deliberate, not cosmetic: `new-window -d` with no target lands in
# whatever tmux currently considers the "current" session for an unqualified
# command, which drifts to wherever an earlier `switch-client` (e.g. `taw
# workspace create`'s own internal one) last left it -- observed landing this
# fixture in an unrelated session's window instead of "main".
e2e_start_replay() {
  local id="$1"
  local win; win=$(tm new-window -t main -P -F '#{pane_id}' -d)
  tm set-option -p -t "$win" @agent_kind_hint "$id"
  tm send-keys -t "$win" "replay $id" Enter
  echo "$win"
}

# Sets the inner server's status-position (top/bottom), for `ui open` geometry checks.
e2e_status_position() { tm set-option -g status-position "$1"; }

# Runs `taw ui open` the way the plugin's key binding does (`run-shell "<taw> ui
# open"`), but backgrounded (`-b`): `display-popup` (and so `ui open`'s own call into
# it) blocks until the popup closes, so a synchronous run-shell here would hang the
# calling script for the popup's whole lifetime. `-b` only affects whether *this*
# invocation waits; the popup's own geometry and lifecycle are identical either way.
# The redirect is the binding's too: run-shell paints anything the command prints into
# the origin pane as a copy-mode view once it exits, and `ui open` prints its envelope
# only after the popup has closed. Any arguments are `ui open`'s own (`--new-session`).
e2e_ui_open() { tm run-shell -b "$(printf '%q' "$TAW_BIN") ui open $* >/dev/null 2>&1"; }

# Whether the popup is on screen right now. A full-size popup has no
# frame of its own (`display-popup -B`: the status line's bar names it), so there is no
# `┌─ taw` title; the tree panel's own top border, `╭ sessions`, is the popup's
# first row instead. Nothing but the popup draws that border.
popup_open() { screen | grep -q '╭ sessions'; }
# Poll [`popup_open`] until the popup is up, up to <timeout_ms> (default 3000). 0 = open.
e2e_wait_popup_open() {
  local timeout="${1:-3000}"; local waited=0
  while [ "$waited" -lt "$timeout" ]; do
    popup_open && return 0
    command sleep 0.1; waited=$((waited+100))
  done
  return 1
}
# Poll until the popup has left the screen, up to <timeout_ms> (default 5000). 0 = closed.
e2e_wait_popup_closed() {
  local timeout="${1:-5000}"; local waited=0
  while [ "$waited" -lt "$timeout" ]; do
    popup_open || return 0
    command sleep 0.2; waited=$((waited+200))
  done
  return 1
}
# Poll until <session>'s status line is its own again after the popup closed, up to
# <timeout_ms> (default 3000). `ui open` puts the line back, and drops the session's
# `@agent_wizard_status_saved` record last, right after `display-popup` returns -- a
# moment after the popup itself has left the screen. 0 = the record is gone.
e2e_wait_bar_restored() {
  local session="${1:-main}" timeout="${2:-3000}"; local waited=0
  while [ "$waited" -lt "$timeout" ]; do
    [ -z "$(tm show-options -t "$session" -v @agent_wizard_status_saved 2>/dev/null)" ] && return 0
    command sleep 0.1; waited=$((waited+100))
  done
  return 1
}

# The screen line number (1-based, into `screen`/`screenc`) carrying the popup's
# selection highlight (the default theme's `selection_bg`, Catppuccin Mocha's
# `#45475a` = `48;2;69;71;90`), or empty if no row is highlighted right now. Its own
# helper because more than one check needs the *line number* itself, to poll for the
# cursor landing on a specific row rather than reading what that row says
# (`e2e_popup_cursor_row`, right below, wants the latter).
e2e_popup_cursor_line() { screenc | grep -Fn $'\x1b[48;2;69;71;90m' | head -1 | cut -d: -f1; }
# The plain-text content of whichever tree-panel row currently carries the popup's
# selection highlight. Cross-references a coloured capture (`screenc`, via
# `e2e_popup_cursor_line`, to find the highlighted line number) with a plain one
# (`screen`, for readable text), since `capture-pane -p` strips styling entirely and
# a bare text diff cannot otherwise tell "the cursor moved to a different,
# identical-looking row" from "nothing moved".
e2e_popup_cursor_row() {
  local n; n=$(e2e_popup_cursor_line)
  [ -n "$n" ] && screen | sed -n "${n}p"
}
# Poll [`e2e_popup_cursor_row`] for an extended-regex match, up to <timeout_ms>.
# Plain `e2e_wait_screen` matches text anywhere on screen, which is too weak a
# signal after a Tab/movement key when the target text (e.g. an agent's name)
# was already visible somewhere else *before* the cursor actually got there --
# this instead waits for the *highlighted* row specifically to say so.
e2e_wait_cursor() {
  local pattern="$1" timeout="${2:-3000}"; local waited=0
  while [ "$waited" -lt "$timeout" ]; do
    e2e_popup_cursor_row | grep -qE "$pattern" && return 0
    command sleep 0.1; waited=$((waited+100))
  done
  return 1
}

# The style one glyph is drawn in: the SGR escapes a coloured capture (`screenc`) carries
# on <line> right in front of the first <glyph>, i.e. everything after the last space before
# it. tmux writes an escape only where the style changes, so this is the glyph's own style.
# A grep of the whole line for a colour cannot tell: every tree row starts with the tree
# panel's own border, drawn in the accent colour. Empty when nothing changes the style there.
e2e_sgr_before() { local pre="${1%%"$2"*}"; printf '%s' "${pre##* }"; }

# The style <text> is drawn in on a coloured capture (`screenc`): its foreground and whether
# it is bold, as `<fg> <bold>` -- `205;214;244 1` for a 24-bit colour in bold, `default 0`
# when nothing has set either. `e2e_sgr_before` reads only the escapes right in front of a
# glyph, which is the glyph's whole style only when every part of it changes there: tmux
# writes an escape where the style changes and nothing where it stays, so a column drawn in
# the colour of the space before it carries nothing in front of it but its bold. This
# follows every escape from the top of the capture -- tmux carries a style over from one line
# into the next as well -- up to the first <text>. 1, printing nothing, when <text> is not
# on the capture.
e2e_style_at() {
  case "$1" in *"$2"*) ;; *) return 1 ;; esac
  printf '%s' "${1%%"$2"*}" | grep -oE $'\033''\[[0-9;:]*m' | awk '
    BEGIN { fg = "default"; bold = 0 }
    {
      n = split(substr($0, 3, length($0) - 3), p, ";")
      if (n == 0) { fg = "default"; bold = 0 }
      for (i = 1; i <= n; i++) {
        c = p[i]
        if (c == "" || c == "0") { fg = "default"; bold = 0 }
        else if (c == "1") bold = 1
        else if (c == "22") bold = 0
        else if (c == "39") fg = "default"
        else if (c == "38" && p[i + 1] == "2") { fg = p[i + 2] ";" p[i + 3] ";" p[i + 4]; i += 4 }
        else if (c == "38" && p[i + 1] == "5") { fg = "5;" p[i + 2]; i += 2 }
        else if (c == "48" && p[i + 1] == "2") i += 4
        else if (c == "48" && p[i + 1] == "5") i += 2
        else if (c ~ /^(3[0-7]|9[0-7])$/) fg = c
      }
    }
    END { print fg " " bold }'
}

# The style the number that starts a tree line is drawn in, as `<fg> <bold>` (e2e_style_at),
# on the first line of the plain screen that matches the extended regex <pattern>: the first
# glyph after the tree panel's border. The number is a digit, and so are the colours of the
# escapes in front of it, so its style is read off the escapes between the border and the
# number rather than found by its text. 1, printing nothing, when no line matches.
e2e_number_style_on() {
  local n line esc
  n=$(screen | grep -nE "$1" | head -1 | cut -d: -f1)
  [ -n "$n" ] || return 1
  line=$(screenc | sed -n "${n}p")
  esc=$(printf '%s' "${line#*│}" | grep -oE $'^(\033\\[[0-9;:]*m)+' | head -1)
  e2e_style_at "${line%%│*}│${esc}#" '#'
}
# The style of <session>'s header's number (e2e_number_style_on).
e2e_number_style() { e2e_number_style_on "^│[0-9] [* -] $1( |\$)"; }

# Poll until the popup's own rows -- screen lines 1-39 of the suite's 120x40 client with the
# status line at the bottom; that line carries the bar's clock -- have not changed for
# <quiet_ms> (default 1000: two of the popup's half-second refreshes), up to <timeout_ms>
# (default 5000). 0 = still. For a check that then watches for nothing to change after a
# key: what the popup reads off-thread lands a moment after the rows are drawn -- a session's
# git branch, the preview of the row the cursor has just reached -- and would otherwise be
# the change that watch sees.
e2e_wait_still() {
  local quiet="${1:-1000}" timeout="${2:-5000}" waited=0 still=0 last now
  last=$(screen | sed -n '1,39p')
  while [ "$waited" -lt "$timeout" ]; do
    command sleep 0.1; waited=$((waited + 100))
    now=$(screen | sed -n '1,39p')
    if [ "$now" = "$last" ]; then
      still=$((still + 100))
      [ "$still" -ge "$quiet" ] && return 0
    else
      still=0; last="$now"
    fi
  done
  return 1
}
