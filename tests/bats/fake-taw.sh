#!/usr/bin/env bash
# Fake `taw` binary used by the bats suite in place of the real Rust binary
# (built by a parallel task, not available here). Records every invocation's
# argv — space-joined, one line per call — to $TAW_FAKE_LOG when it is set,
# and answers --version the way the real binary will.
#
# Its filename deliberately contains "taw": the plugin's hook-idempotency
# check matches a hook command against the literal resolved binary path, and
# tests point TAW_BIN straight at this script, so the path itself satisfies
# that match without any extra fakery.

if [ -n "${TAW_FAKE_LOG:-}" ]; then
  printf '%s\n' "$*" >>"$TAW_FAKE_LOG"
fi

if [ "${1:-}" = "--version" ]; then
  echo "taw 0.0.0-fake"
fi

# `notification open-target` answers the way a test says the real one does: the reply is
# printed on stdout (the real binary prints a JSON envelope) and the exit status is the
# one given (1 for an error, which is how the real one reports every failure). A test
# sets both server-wide with `set-environment -g`, like TAW_FAKE_LOG, because the
# notification key's binding runs under the tmux server. With neither set, it succeeds
# and prints nothing, like every other verb here.
if [ "${1:-}" = "notification" ] && [ "${2:-}" = "open-target" ]; then
  if [ -n "${TAW_FAKE_OPEN_TARGET_REPLY:-}" ]; then
    printf '%s\n' "$TAW_FAKE_OPEN_TARGET_REPLY"
  fi
  exit "${TAW_FAKE_OPEN_TARGET_STATUS:-0}"
fi

# `pane clear` answers the way a test says the real one does: it exits with the status
# $TAW_FAKE_CLEAR_STATUS names (0 when it is not set, 1 for a failure). A test sets it server-wide
# with `set-environment -g`, like TAW_FAKE_LOG, because the clear key's binding runs under the tmux
# server.
if [ "${1:-}" = "pane" ] && [ "${2:-}" = "clear" ]; then
  exit "${TAW_FAKE_CLEAR_STATUS:-0}"
fi

# `server started` is the one-shot the plugin runs at load, and is told to discard its output: this
# is what a verb that prints a line (the real one prints its envelope) would leave in a pane's view if
# the plugin did not.
if [ "${1:-}" = "server" ] && [ "${2:-}" = "started" ]; then
  echo "server started: output the plugin must discard"
  echo "server started: and its errors" >&2
  exit 0
fi

exit 0
