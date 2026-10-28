#!/usr/bin/env bash
# A net of refusing stubs, first on PATH, for every program that would reach the desktop of whoever runs
# the tests: a notification on their screen, or a sound from their speakers.
#
# taw's `system` toast runs osascript (macOS) or notify-send (Linux), and its sound afplay (macOS) or paplay
# or aplay (Linux), unless $TAW_NOTIFY_CMD and $TAW_SOUND_CMD name a program to run in their place, which is
# how a test records what it is sent. A test whose watcher, or whose taw, has neither puts a real
# notification on the screen of whoever runs the suite (one run put "claude done" in a session
# named "one" on a desktop). Nothing in a test's own code can show that it never does, so the
# scripts that run a suite (the gate, and the acceptance harness) take this net first: a folder of
# stubs, one for each name in scripts/desktop-net-names.txt, put first on PATH, so that such a
# program runs a stub, which notes the call in the net's log and refuses it (exit 127, as for a
# command that is not there). And the two variables, exported, name two refusing recorders of the
# net's, which note what taw gave them and refuse it (exit 1, as a notifier that failed), for every
# process that inherits them and has no recorder of its own: taw then runs neither the desktop's
# program nor a stub of it. The script looks in the log when the suite is done: a line there is a
# test that would have reached the desktop. A test that records its notifications sets
# $TAW_NOTIFY_CMD to its own recorder (a test server is started with refusing recorders of its own
# where the test sets none: tests/common/shell_env.rs), and never reaches the net.
#
# Sourced, not run:
#
#   source scripts/desktop-net.sh
#   desktop_net_make <dir>    # the stubs in <dir>/bin, first on PATH, the recorders in <dir>/seam, the log <dir>/log
#   ... run the suite ...
#   desktop_net_check || status=1   # says each line of the log, and fails, when there is one
#
# A stub or a recorder writes to $TAW_DESKTOP_NET_LOG instead, when it is set: tests/desktop_net.rs runs
# them so, to show that the net is there, without a line in the log the gate looks in. desktop_net_make
# takes it out of the environment, so the suite is not run with it.

# The net in use, which desktop_net_check looks in.
DESKTOP_NET_DIR=""

# The names the net has a stub for.
DESKTOP_NET_NAMES="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/desktop-net-names.txt"

# desktop_net_make <dir>: a stub in <dir>/bin for each name, a refusing recorder in <dir>/seam for each of
# the two variables, an empty log in <dir>/log, and <dir>/bin first on PATH; TAW_NOTIFY_CMD and
# TAW_SOUND_CMD name the recorders, and TAW_DESKTOP_NET names <dir>, for the guard test and for the
# recorders of a test server (tests/common/shell_env.rs), which note what they refuse in the same log.
desktop_net_make() {
  local dir="${1:?a directory for the net}" name variable
  mkdir -p "$dir/bin" "$dir/seam" && : > "$dir/log" || return 1
  while IFS= read -r name; do
    case "$name" in '' | '#'*) continue ;; esac
    cat > "$dir/bin/$name" <<STUB || return 1
#!/bin/sh
# A stub of the desktop net (scripts/desktop-net.sh): a test ran $name from the PATH of whoever runs
# the tests, which reaches their screen or their speakers. It notes the call (its name, arguments,
# directory and pane) and refuses it.
printf '%s\t%s\t%s\t%s\n' "\${0##*/}" "\$*" "\$PWD" "\${TMUX_PANE:-}" >> "\${TAW_DESKTOP_NET_LOG:-$dir/log}"
echo "\${0##*/}: refused by the test net: a test shows no notification and plays no sound to whoever runs it" >&2
exit 127
STUB
    chmod +x "$dir/bin/$name" || return 1
  done < "$DESKTOP_NET_NAMES"
  for variable in TAW_NOTIFY_CMD TAW_SOUND_CMD; do
    cat > "$dir/seam/$variable" <<RECORDER || return 1
#!/bin/sh
# The refusing recorder of the desktop net (scripts/desktop-net.sh) that \$$variable names: taw was to
# show a notification or play a sound, for a test that records none. It notes the call (the variable,
# what taw gave it, the directory and the pane) and refuses it.
printf '%s\t%s\t%s\t%s\n' $variable "\$*" "\$PWD" "\${TMUX_PANE:-}" >> "\${TAW_DESKTOP_NET_LOG:-$dir/log}"
echo "$variable: refused by the test net: a test that records no notification or sound has none" >&2
exit 1
RECORDER
    chmod +x "$dir/seam/$variable" || return 1
  done
  DESKTOP_NET_DIR="$dir"
  unset TAW_DESKTOP_NET_LOG
  export PATH="$dir/bin:$PATH" TAW_DESKTOP_NET="$dir" \
    TAW_NOTIFY_CMD="$dir/seam/TAW_NOTIFY_CMD" TAW_SOUND_CMD="$dir/seam/TAW_SOUND_CMD"
}

# The lines of the net's log, one a call: what would have reached the desktop of whoever runs the tests.
desktop_net_hits() {
  [ -n "$DESKTOP_NET_DIR" ] && cat "$DESKTOP_NET_DIR/log" 2>/dev/null
}

# desktop_net_check: 0 when nothing reached the net; else each call it refused, on standard error, and 1.
desktop_net_check() {
  local hits
  hits="$(desktop_net_hits)"
  [ -z "$hits" ] && return 0
  echo "these were to show a notification or play a sound to whoever runs the tests (program or variable, arguments, directory, pane):" >&2
  printf '%s\n' "$hits" >&2
  return 1
}
