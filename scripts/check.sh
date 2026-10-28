#!/usr/bin/env bash
# CI-equivalent check script: cargo fmt/clippy/test, shellcheck, bats.
#
# A missing tool is a failure, not a notice, for every one of the three. This
# script is what "done" means for a change here, so exiting 0 having silently
# skipped the entire Rust test suite (because `cargo` was not on PATH) reports
# a pass that nothing verified. `mise install` provides all three; run through
# `mise exec` or `mise run check` if any are missing.
set -uo pipefail

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$CURRENT_DIR" || exit 1

# git runs the command of a `rebase -x` with GIT_DIR (and more) set, and a test that makes a repository
# of its own would write to the one being rebased. Every git of taw's and of the tests' takes these out
# itself (src/git.rs, and the guards that hold it to that run them with each pointing at a decoy); the
# gate takes them out for everything else it runs.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_NAMESPACE GIT_CONFIG GIT_IMPLICIT_WORK_TREE GIT_GRAFT_FILE GIT_NO_REPLACE_OBJECTS GIT_REPLACE_REF_BASE GIT_PREFIX GIT_SHALLOW_FILE

# A tmux that is run with neither -L nor -S reaches the server in $TMUX and, with none, the default
# one: the server of whoever runs the gate. No pattern over the sources finds every way such a command
# can be spelled (a test, a hook, a unit test, a `taw` run with no socket variable), so the gate runs
# with $TMUX unset and its own tmux socket directory, and looks in it when it is done: a default
# socket there is a tmux that named no socket, and any other file is a server that did not end with
# its socket. See scripts/private-tmux-dir.sh.
# shellcheck source=scripts/private-tmux-dir.sh
source "$CURRENT_DIR/scripts/private-tmux-dir.sh"
# And a test that runs an agent's command, or a container runtime, from the PATH of whoever runs the gate
# would start the agent installed there, or ask the real runtime: the gate puts a net of refusing stubs
# first on PATH for every step, in its own directory, and fails when one was run. See
# scripts/agent-net.sh.
# shellcheck source=scripts/agent-net.sh
source "$CURRENT_DIR/scripts/agent-net.sh"
# The same for the desktop of whoever runs the gate: a test that shows a notification or plays a sound with
# no recorder of its own would put it on their screen or their speakers (osascript, afplay and the rest).
# See scripts/desktop-net.sh.
# shellcheck source=scripts/desktop-net.sh
source "$CURRENT_DIR/scripts/desktop-net.sh"
# And a git that a test, a child of a test or a script runs with one of git's variables that point it at
# another repository set (GIT_DIR, which git sets for the command of a `rebase -x`) would write to that
# repository: the cargo test step and the bats step run with every one of them on a decoy repository made
# for the run, and the gate looks at the decoy when each has ended. See scripts/git-decoy.sh.
# shellcheck source=scripts/git-decoy.sh
source "$CURRENT_DIR/scripts/git-decoy.sh"

# The directory goes whatever happens to the script, but only once the step that is running has ended:
# tmux takes /tmp for a $TMUX_TMPDIR that is gone, so a step that ran on after it (a signal to this
# script alone leaves its child running) would have no net at all. Each step runs in a process group of
# its own (job control, for the fork that starts it: run_step), which the EXIT trap ends whole, and
# waits for, before the directory goes; it keeps the directory, and says so, if the step cannot be
# ended. A signal exits through that trap (and `wait` returns at once for a trapped one).
# private_tmux_down does nothing the second time. And what a step leaves running in its group when it
# ends is ended as soon as it has ended, and fails the gate (run_step). While a step is being started
# it is `starting`, and the trap takes `$!` for it (run_step says why), with `set +u`: `$!` names
# nothing before the first step, and bash, 3.2 and 5 alike, stops the trap on it under `set -u`. The
# traps are set before the directory is made: a signal that came once it was made, and before they
# were, would end the gate with nothing to take it down (private_tmux_down does nothing while there is
# no directory, and private_tmux_make names it in the command that makes it).
step=""
trap 'set +u; [ "$step" != starting ] || step="-$!"; private_tmux_down ${step:+"$step"} >/dev/null' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
private_tmux_make /tmp/taw-check || { echo "cannot make a directory for the gate's tmux sockets" >&2; exit 1; }
agent_net_make "$PRIVATE_TMUX_DIR/net" || { echo "cannot make the gate's net of refusing stubs" >&2; exit 1; }
desktop_net_make "$PRIVATE_TMUX_DIR/desktop-net" || { echo "cannot make the gate's net of refusing stubs for the desktop" >&2; exit 1; }
git_decoy_make "$PRIVATE_TMUX_DIR/git-decoy" || { echo "cannot make the gate's decoy repository for git's variables" >&2; exit 1; }

status=0

run_step() {
  local desc="$1" ran
  shift
  echo "== $desc =="
  # Job control is on for the fork that starts the step, which gives it a process group of its own,
  # and for nothing else: off, bash adds no line of its own ("[1]+ Terminated ...") when a signal ends
  # a step (the gate says what failed), and nothing else the gate runs gets a group. No step reads the
  # terminal: in a group of its own, one that did would be stopped, and waited for.
  set -m
  # A signal that lands while bash forks the step is handled at the start of the next command, before
  # `step=$!`, with `$!` the new step already: so until then the step is `starting`, and the EXIT trap
  # takes the group of `$!` (before the fork, the last step's, whose group is gone, or nothing).
  step=starting
  "$@" < /dev/null &
  step=$!
  set +m
  wait "$step"
  ran=$?
  # Its leader has ended and been waited for: from here on the step is its process group alone.
  step="-$step"
  # What it left running there (a child of a test that nothing waited for, which cargo leaves where it
  # is when it exits) would run on after the directory went, with no net. It is said, ended now, while
  # the group's id cannot be anybody else's (a pid kept to the end could be), and it fails the gate, so
  # that the leak is seen, and fixed.
  private_tmux_left "${step#-}" "$desc" || ran=1
  step=""
  if [ "$ran" -ne 0 ]; then
    echo "FAILED: $desc" >&2
    status=1
  fi
}

if command -v cargo >/dev/null 2>&1; then
  run_step "cargo fmt --check" cargo fmt --check
  # Every target, so a finding in a test or an example fails the gate as well: without
  # --all-targets clippy lints only the library and the binary.
  run_step "cargo clippy --all-targets -- -D warnings" cargo clippy --all-targets -- -D warnings
  # Run with git's variables on the decoy. Nothing cargo, rustc or a build script runs reads them: there is
  # no build script and no git dependency, and nothing in a test binary does but a git that kept one.
  # --no-fail-fast: a test binary that fails does not stop the ones after it, so one run shows every failure.
  run_step "cargo test" git_decoy_run cargo test --no-fail-fast
  git_decoy_check "cargo test" || status=1
else
  echo "MISSING: cargo not found on PATH, so fmt/clippy/test cannot run" >&2
  status=1
fi

if command -v shellcheck >/dev/null 2>&1; then
  run_step "shellcheck" shellcheck tmux-agent-wizard.tmux scripts/*.sh scripts/demo/*.sh tests/bats/*.sh tests/e2e/*.sh src/sandbox/default-profile.sh
  run_step "shellcheck the sandbox's start script" scripts/lint-start-script.sh
else
  echo "MISSING: shellcheck not found on PATH, so the shell scripts cannot be linted" >&2
  status=1
fi

bats_cmd=()
if command -v bats >/dev/null 2>&1; then
  bats_cmd=(bats)
elif command -v mise >/dev/null 2>&1; then
  bats_cmd=(mise x bats@1.11.0 -- bats)
fi

if [ "${#bats_cmd[@]}" -gt 0 ]; then
  run_step "bats tests/bats" git_decoy_run "${bats_cmd[@]}" tests/bats
  git_decoy_check "bats tests/bats" || status=1
else
  echo "MISSING: bats not found (tried PATH and mise), so tests/bats cannot run" >&2
  status=1
fi

# The nets' logs, before their directory goes with the gate's.
echo "== no agent, container runtime or ssh run from the gate's PATH =="
if ! agent_net_check; then
  echo "FAILED: no agent, container runtime or ssh run from the gate's PATH" >&2
  status=1
fi
echo "== no notification or sound reached the desktop of whoever runs the gate =="
if ! desktop_net_check; then
  echo "FAILED: no notification or sound reached the desktop of whoever runs the gate" >&2
  status=1
fi

# In this shell, which is the one that knows the directory, and not as a step of its own. No step is
# running by now, and what each one left was ended when it ended, so there is nothing for it to end
# first (but a group that could not be ended, which keeps the directory: private_tmux_left).
echo "== no tmux server or socket file left in the gate's own socket directory =="
# shellcheck disable=SC2119  # no pid: nothing of the gate's runs here any more
if ! private_tmux_down; then
  echo "FAILED: no tmux server or socket file left in the gate's own socket directory" >&2
  status=1
fi

exit "$status"
