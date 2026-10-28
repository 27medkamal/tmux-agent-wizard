#!/usr/bin/env bash
# A net of refusing stubs, first on PATH, for every agent, container runtime and ssh program a test could run.
#
# A test that types an agent's command into a pane, or runs a container runtime, has to find a fake of
# its own first: on the PATH of whoever runs the suite, `claude --resume` starts the Claude Code that is
# installed there, and `docker info` asks the real Docker. The same goes for ssh, which taw runs to reach
# other machines: a bare `ssh` would log in to a real one. Nothing in a test's own code can show that it
# never reaches them, so the scripts that run a suite (the gate, and the acceptance harness) take this net
# first: a folder of stubs, one for each name in scripts/agent-net-names.txt and scripts/ssh-net-names.txt,
# put first on PATH, so that such a command runs a stub, which notes the call in the net's log and refuses
# it (exit 127, as for a command that is not there). The script looks in the log when the suite is done:
# a line there is a test that ran an agent, a runtime or ssh from the runner's PATH. A test that puts
# fakes of its own first, or gives its panes a PATH of their own, never reaches the net.
#
# Sourced, not run:
#
#   source scripts/agent-net.sh
#   agent_net_make <dir>    # the stubs in <dir>/bin, first on PATH, their log <dir>/log
#   ... run the suite ...
#   agent_net_check || status=1   # says each line of the log, and fails, when there is one
#
# A stub writes to $TAW_AGENT_NET_LOG instead, when it is set: tests/agent_net.rs runs a stub of the net
# so, to show that the net is there, without a line in the log the gate looks in. agent_net_make takes it
# out of the environment, so the suite is not run with it.

# The net in use, which agent_net_check looks in.
AGENT_NET_DIR=""

# The names the net has a stub for: the agents and runtimes, then the ssh programs.
AGENT_NET_NAMES="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/agent-net-names.txt"
SSH_NET_NAMES="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/ssh-net-names.txt"

# agent_net_stub <dir> <name>: the stub for <name> in <dir>/bin.
agent_net_stub() {
  local dir="${1:?a directory for the net}" name="${2:?a name}"
  cat > "$dir/bin/$name" <<STUB || return 1
#!/bin/sh
# A stub of the test net (scripts/agent-net.sh): a test ran $name from the PATH of whoever runs
# the suite. It notes the call (its name, arguments, directory and pane) and refuses it.
printf '%s\t%s\t%s\t%s\n' "\${0##*/}" "\$*" "\$PWD" "\${TMUX_PANE:-}" >> "\${TAW_AGENT_NET_LOG:-$dir/log}"
echo "\${0##*/}: refused by the test net: a test runs no agent, container runtime or ssh from the PATH of whoever runs it" >&2
exit 127
STUB
  chmod +x "$dir/bin/$name"
}

# agent_net_make <dir>: a stub in <dir>/bin for each name, an empty log in <dir>/log, and <dir>/bin first
# on PATH; TAW_AGENT_NET names <dir>, for the guard test, and TAW_SSH_BIN its ssh stub. Fails when a
# names file cannot be read.
agent_net_make() {
  local dir="${1:?a directory for the net}" name names
  mkdir -p "$dir/bin" && : > "$dir/log" || return 1
  for names in "$AGENT_NET_NAMES" "$SSH_NET_NAMES"; do
    while IFS= read -r name; do
      case "$name" in '' | '#'*) continue ;; esac
      agent_net_stub "$dir" "$name" || return 1
    done < "$names" || return 1
  done
  AGENT_NET_DIR="$dir"
  unset TAW_AGENT_NET_LOG
  # TAW_SSH_BIN names the net's ssh, so that a server with no recorder of its own (the shell tests'
  # servers) runs it and not the ssh a login shell finds first on its PATH.
  export PATH="$dir/bin:$PATH" TAW_AGENT_NET="$dir" TAW_SSH_BIN="$dir/bin/ssh"
}

# The lines of the net's log, one a call: what ran an agent, a container runtime or ssh from the runner's PATH.
agent_net_hits() {
  [ -n "$AGENT_NET_DIR" ] && cat "$AGENT_NET_DIR/log" 2>/dev/null
}

# agent_net_check: 0 when nothing reached the net; else each call it refused, on standard error, and 1.
agent_net_check() {
  local hits
  hits="$(agent_net_hits)"
  [ -z "$hits" ] && return 0
  echo "these ran an agent, a container runtime or ssh from the PATH of whoever runs the suite (name, arguments, directory, pane):" >&2
  printf '%s\n' "$hits" >&2
  return 1
}
