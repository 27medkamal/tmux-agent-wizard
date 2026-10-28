#!/usr/bin/env bash
# A tmux socket directory of a script's own, so that no tmux it runs can reach the server of whoever
# runs the script.
#
# A tmux that is run with neither -L nor -S goes to the server in $TMUX and, with none, to the default
# socket (`$TMUX_TMPDIR/tmux-<uid>/default`, in /tmp where that is not set): the user's own server.
# Nothing can promise that no command in a test suite, a hook, a unit test or a `taw` run spells a
# tmux that way, so the scripts that run them (the gate, the acceptance harness, the bats run, the
# sweep) take this directory first: with $TMUX and $TMUX_PANE unset and $TMUX_TMPDIR on a fresh
# short directory of its own, such a tmux finds no server, or starts one of its own there, and
# reaches nothing else; and the script looks in the directory when it is done.
#
# Sourced, not run:
#
#   source scripts/private-tmux-dir.sh
#   private_tmux_make /tmp/taw-check      # a fresh directory /tmp/taw-check.XXXXXX, which is $TMUX_TMPDIR
#   ... run what needs tmux ...
#   private_tmux_left <group> <what> || status=1  # after a step run in a group of its own has ended
#   private_tmux_names                    # the files in it, one a line (the sockets of the servers there)
#   private_tmux_down [pid...] || status=1  # ends those, looks, ends what it finds, removes the directory
#
# The directory is short because a socket's path is at most about a hundred bytes (macOS has 104):
# the default $TMPDIR there is already fifty.
#
# It goes only once nothing the script started is left: tmux uses $TMUX_TMPDIR only while that
# directory exists, and takes /tmp otherwise, so a step still running after its directory went would
# have no net at all. A trap that removes it names what the script started (a step it runs, a server
# it started), and private_tmux_down ends that first, and keeps the directory, saying so, if it cannot.
# And what a step left running when it ended is ended then and there (private_tmux_left).

# The directory in use, which `private_tmux_down` removes.
PRIVATE_TMUX_DIR=""

# private_tmux_make <prefix>: makes <prefix>.XXXXXX and uses it. The prefix is a path, in /tmp.
#
# The directory's name goes straight into PRIVATE_TMUX_DIR, in the command that makes it: bash runs a
# trap once the command it lands in is over, so a trap that removes the directory (one a script sets
# before it calls this) finds it named, however soon after it was made a signal came.
private_tmux_make() {
  PRIVATE_TMUX_DIR="$(mktemp -d "${1:?a prefix for the directory}.XXXXXX")" || {
    PRIVATE_TMUX_DIR=""
    return 1
  }
  unset TMUX TMUX_PANE
  export TMUX_TMPDIR="$PRIVATE_TMUX_DIR"
}

# The directory tmux keeps its sockets in, under $TMUX_TMPDIR.
private_tmux_sockets() { printf '%s/tmux-%s' "$PRIVATE_TMUX_DIR" "$(id -u)"; }

# The files in it, one a line.
private_tmux_names() { ls -1 "$(private_tmux_sockets)" 2>/dev/null; }

# Process groups that private_tmux_left could not end: private_tmux_down tries them again, and keeps
# the directory while any of them is there.
PRIVATE_TMUX_UNENDED=()

# private_tmux_end <pid>...: ends what a script started: for each pid, the process group it leads (a
# step started with job control on, a tmux server and the jobs it runs), or else the process alone;
# for -<pid>, the group alone. That is the form for a leader that has ended and been waited for: its
# pid may be another process's by now, while the group's id stays the group's for as long as anything
# is in it. TERM, up to ten seconds to go, then KILL and up to five more. Never 0 or 1, nor this shell
# or the group it is in. What is still there then is said on standard error, and the status is 1.
private_tmux_end() {
  local pid target own n left=0 group_alone
  own="$(ps -o pgid= -p $$ 2>/dev/null | tr -d ' ')"
  for pid in "$@"; do
    group_alone=""
    case "$pid" in -*) group_alone=1; pid="${pid#-}" ;; esac
    case "$pid" in '' | *[!0-9]*) continue ;; esac
    [ "$pid" -gt 1 ] && [ "$pid" != "$$" ] && [ "$pid" != "$own" ] || continue
    if kill -0 -- "-$pid" 2>/dev/null; then
      target="-$pid"
    elif [ -z "$group_alone" ] && kill -0 "$pid" 2>/dev/null; then
      target="$pid"
    else
      continue
    fi
    kill -TERM -- "$target" 2>/dev/null
    n=0
    while kill -0 -- "$target" 2>/dev/null && [ "$n" -lt 100 ]; do sleep 0.1; n=$((n + 1)); done
    if kill -0 -- "$target" 2>/dev/null; then
      kill -KILL -- "$target" 2>/dev/null
      n=0
      while kill -0 -- "$target" 2>/dev/null && [ "$n" -lt 50 ]; do sleep 0.1; n=$((n + 1)); done
    fi
    if kill -0 -- "$target" 2>/dev/null; then
      echo "could not end $target, which this script started" >&2
      left=1
    fi
  done
  return "$left"
}

# private_tmux_left <group> <what>: what is still in a process group whose leader has ended and been
# waited for (a step run with job control): what <what> left running when it ended, which nothing
# waited for, and which would run on after the directory went, with no net. Each process of it is said
# on standard error, by its pid and its command line, the group is ended (the group alone: see
# private_tmux_end), and the status is 1; with nothing there, 0. Called as soon as the leader has been
# waited for, while the group's id cannot be anybody else's. A group that cannot be ended is kept in
# PRIVATE_TMUX_UNENDED, and private_tmux_down keeps the directory while it is there.
private_tmux_left() {
  local group="${1:-}" what="${2:-a step}"
  case "$group" in '' | *[!0-9]*) return 0 ;; esac
  [ "$group" -gt 1 ] && [ "$group" != "$$" ] && kill -0 -- "-$group" 2>/dev/null || return 0
  [ "$group" != "$(ps -o pgid= -p $$ 2>/dev/null | tr -d ' ')" ] || return 0
  echo "$what left these running when it ended, which nothing waited for; they are ended now:" >&2
  ps -ww -A -o pgid= -o pid= -o command= 2>/dev/null |
    awk -v group="$group" '$1 == group { sub(/^ *[0-9]+ +/, "  "); print }' >&2
  private_tmux_end "-$group" || PRIVATE_TMUX_UNENDED+=("-$group")
  return 1
}

# private_tmux_down [pid...]: ends what the script names it started (private_tmux_end), and any group
# private_tmux_left could not end, then looks in the directory, which a script that did everything
# right finds empty: every server started in it has ended and had its socket file taken away (tmux
# leaves the file of a server that has ended where it was). What is there is said on standard error,
# one line a file, and the status is 1:
#
#   * `default` is a tmux that named no socket and started a server (the user's own server was
#     not touched: this is where it went);
#   * a socket that answers is a server that was left running, and is ended, by the exact path of its
#     socket and nothing else, and its jobs with it, so that nothing of it runs on without the directory;
#   * a socket that does not is a file the server that made it did not take away.
#
# The directory is removed then, whatever was in it, if nothing the script started is left; else it is
# kept, and said so. Run a second time, this does nothing.
private_tmux_down() {
  local found=0 left=0 file name pid n
  [ -n "$PRIVATE_TMUX_DIR" ] || return 0
  private_tmux_end "$@" ${PRIVATE_TMUX_UNENDED[@]+"${PRIVATE_TMUX_UNENDED[@]}"} || left=1
  if [ -d "$(private_tmux_sockets)" ]; then
    for file in "$(private_tmux_sockets)"/*; do
      [ -e "$file" ] || continue
      name="${file##*/}"
      found=1
      if [ "$name" = default ]; then
        echo "a tmux that names no socket (neither -L nor -S) started a server on the default socket: $file" >&2
      elif tmux -S "$file" list-sessions >/dev/null 2>&1; then
        echo "a tmux server was left running: $file" >&2
      else
        echo "a tmux socket file was left behind: $file" >&2
      fi
      pid="$(tmux -S "$file" display-message -p '#{pid}' 2>/dev/null)"
      tmux -S "$file" kill-server >/dev/null 2>&1
      n=0
      while tmux -S "$file" list-sessions >/dev/null 2>&1 && [ "$n" -lt 50 ]; do sleep 0.1; n=$((n + 1)); done
      tmux -S "$file" list-sessions >/dev/null 2>&1 && left=1
      # Its group alone: the server has gone, and its pid may be another process's by now.
      private_tmux_end ${pid:+"-$pid"} || left=1
    done
  fi
  if [ "$left" = 0 ]; then
    rm -rf "$PRIVATE_TMUX_DIR"
  else
    echo "kept the tmux directory $PRIVATE_TMUX_DIR: something this script started is still running, and without the directory a tmux of it would reach /tmp" >&2
  fi
  PRIVATE_TMUX_DIR=""
  [ "$found" = 0 ] && [ "$left" = 0 ]
}
