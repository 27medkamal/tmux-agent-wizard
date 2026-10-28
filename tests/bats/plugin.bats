#!/usr/bin/env bats
#
# Integration tests for tmux-agent-wizard.tmux. Each test starts its own
# throwaway tmux server (tmux -L taw-bats-$$) and kills it in teardown; the
# user's real tmux server is never touched. These tests are about the
# *script's* decisions, so every test points the plugin at
# tests/bats/fake-taw.sh via TAW_BIN — it logs its argv to $TAW_FAKE_LOG and
# answers --version. (tests/e2e_plugin_script.rs runs the same script against
# the real binary.)
#
# The two tests that exercise `resolve_taw_bin`'s *failure* path run against
# an isolated copy of the plugin (isolated_plugin below) rather than the
# checkout: the real checkout grows bin/taw and target/release/taw as soon as
# anyone runs `mise run build`, and either would make the plugin resolve a
# binary in the very tests that require it to find none.

setup() {
  PLUGIN_DIR="$(cd "$(dirname "$BATS_TEST_FILENAME")/../.." && pwd)"
  PLUGIN="$PLUGIN_DIR/tmux-agent-wizard.tmux"
  FAKE_TAW="$BATS_TEST_DIRNAME/fake-taw.sh"
  # A tmux that is run with neither -L nor -S would reach the default socket, the server of whoever runs
  # the tests: this test has a tmux socket directory of its own (scripts/private-tmux-dir.sh), with
  # $TMUX unset, and teardown fails the test when anything is left in it, a default socket included.
  source "$PLUGIN_DIR/scripts/private-tmux-dir.sh"
  private_tmux_make /tmp/taw-bats-tmux
  # Unique per test, not just per process: bats may run tests in parallel,
  # and $$ alone is not guaranteed distinct across every test in that mode.
  SOCK="taw-bats-$$-${BATS_TEST_NUMBER}"
  TAW_FAKE_LOG="$BATS_TEST_TMPDIR/taw-fake.log"
  : >"$TAW_FAKE_LOG"
  # Where isolated_installable_plugin's fake install.sh logs its calls. Set
  # here, not in that helper: the helper runs in a $(...) subshell.
  INSTALL_LOG="$BATS_TEST_TMPDIR/install-calls.log"
  # -f /dev/null: a bare -L server still loads the host's tmux.conf (on this
  # machine that sets base-index 1 and auto-loads TPM plugins), which breaks
  # isolation from the user's real setup. -f only matters on the command
  # that actually starts the server, which is this one.
  #
  # The server's first pane runs the host's login shell, so the server starts with no history
  # file and a home of its own (here and not by set-environment: the first pane is made from the
  # environment of the command that starts the server). Nothing is typed into it, but a shell
  # reads its startup files from the home it has, and the runner's are not for a test to run.
  mkdir -p "$BATS_TEST_TMPDIR/shell-home"
  HISTFILE=/dev/null HOME="$BATS_TEST_TMPDIR/shell-home" ZDOTDIR="$BATS_TEST_TMPDIR/shell-home" tmux -L "$SOCK" -f /dev/null new-session -d -x 80 -y 24
  # tmux leaves the socket file of a server that has ended where it was: this server's is noted, by
  # the exact path the server says it listens on, for teardown to take away.
  SOCK_FILE="$(tmux -L "$SOCK" display-message -p '#{socket_path}')"
  # A plain `export` here would not help: run-shell/hooks are executed by
  # the tmux *server*, not as children of this bats process, so they never
  # see this shell's environment. set-environment -g puts it in the
  # server's own environment table, which run-shell and hook-triggered
  # commands (including the startup one-shots the plugin backgrounds with
  # `run-shell -b`) are spawned with.
  tmux -L "$SOCK" set-environment -g TAW_FAKE_LOG "$TAW_FAKE_LOG"
}

teardown() {
  # The server's jobs (what the plugin starts with run-shell -b) run in its process group and outlive
  # it: they are ended, by that group, before the test's tmux directory goes.
  local server
  server="$(tmux -L "$SOCK" display-message -p '#{pid}' 2>/dev/null)"
  tmux -L "$SOCK" kill-server >/dev/null 2>&1 || true
  # Only once the server no longer answers on it, and only a socket file with a test server's name.
  local waited=0
  if [ -S "${SOCK_FILE:-}" ]; then
    case "${SOCK_FILE##*/}" in
      taw-bats-*)
        while tmux -S "$SOCK_FILE" list-sessions >/dev/null 2>&1 && [ "$waited" -lt 100 ]; do
          sleep 0.05
          waited=$((waited + 1))
        done
        if ! tmux -S "$SOCK_FILE" list-sessions >/dev/null 2>&1; then
          rm -f "$SOCK_FILE"
        fi
        ;;
    esac
  fi
  # Then the test's own tmux directory, which is empty now unless something went wrong (a tmux that
  # named no socket, a server left running, a socket file left behind): what is there fails the test,
  # and the directory goes once nothing the test started is left (else it is kept, and said so). The
  # server's jobs as a group alone (-<pid>): the server has gone, and its pid may be another process's.
  private_tmux_down ${server:+"-$server"}
}

@test "the tmux directory of a test is a private one, and the tmux it was run in is gone" {
  # `|| false`: under bash 3.2 a [[ ]] that fails and is not the test's last command fails nothing.
  [[ "$TMUX_TMPDIR" == /tmp/taw-bats-tmux.* ]] || false
  [ -d "$TMUX_TMPDIR" ]
  [ -z "${TMUX:-}" ]
  [ -z "${TMUX_PANE:-}" ]
}

# Loads the plugin into the test server with TAW_BIN pointed at the fake
# taw. TAW_BIN is only read by the plugin script's own process (it resolves
# the binary once and bakes the path into every binding/hook it installs),
# so passing it inline here is enough; TAW_FAKE_LOG needs the server-wide
# set-environment above because fake-taw.sh is also invoked as a
# grandchild, spawned by the server for hooks and backgrounded run-shells.
load_plugin() {
  tmux -L "$SOCK" run-shell "TAW_BIN='$FAKE_TAW' bash '$PLUGIN'"
}

# Copies the plugin entry point and its helpers into $BATS_TEST_TMPDIR and
# echoes the copy's path — the same isolation install.bats uses, for the same
# reason. resolve_taw_bin's last three rungs are $CURRENT_DIR/bin/taw,
# $CURRENT_DIR/target/release/taw and `taw` on PATH; $CURRENT_DIR is wherever
# the *script* lives, so a copy in a fresh tmpdir has neither candidate,
# whatever the checkout currently holds.
isolated_plugin() {
  local work="$BATS_TEST_TMPDIR/plugin"
  mkdir -p "$work/scripts"
  cp "$PLUGIN_DIR/tmux-agent-wizard.tmux" "$work/tmux-agent-wizard.tmux"
  cp "$PLUGIN_DIR/scripts/helpers.sh" "$work/scripts/helpers.sh"
  echo "$work/tmux-agent-wizard.tmux"
}

# An isolated plugin copy (see isolated_plugin) that can install taw by itself:
# the real scripts/auto-install.sh, a Cargo.toml at version $1, and a fake
# scripts/install.sh standing in for the download. It logs "<argv> <TAW_NO_BUILD>"
# to $INSTALL_LOG (set in setup; created empty here), then either "downloads" (copies the fake taw to bin/taw and
# writes the release marker the real one writes), or with $2 = fail prints
# "no release" and exits 1, or with $2 = older finds only the release before $1, 9.9.8,
# which it leaves in place and records as the real one does. Echoes the copy's entry point.
isolated_installable_plugin() {
  local version="$1" behaviour="${2:-ok}" entry work
  entry=$(isolated_plugin)
  work=$(dirname "$entry")
  cp "$PLUGIN_DIR/scripts/auto-install.sh" "$work/scripts/auto-install.sh"
  printf '[package]\nname = "tmux-agent-wizard"\nversion = "%s"\n' "$version" >"$work/Cargo.toml"
  : >"$INSTALL_LOG"
  {
    printf '#!/usr/bin/env bash\n'
    printf 'printf "%%s %%s\\n" "$*" "${TAW_NO_BUILD:-}" >>%q\n' "$INSTALL_LOG"
    if [ "$behaviour" = fail ]; then
      printf 'echo "no release"\nexit 1\n'
    elif [ "$behaviour" = older ]; then
      printf 'cd "$(dirname "$0")/.." && echo "9.9.8 9.9.8 test-target" >bin/.taw-release\n'
    else
      printf 'cd "$(dirname "$0")/.." && mkdir -p bin && cp %q bin/taw && chmod +x bin/taw\n' "$FAKE_TAW"
      printf 'echo "%s %s test-target" >bin/.taw-release\n' "$version" "$version"
    fi
  } >"$work/scripts/install.sh"
  chmod +x "$work/scripts/install.sh"
  echo "$entry"
}

prefix_a_runs() {
  tmux -L "$SOCK" list-keys -T prefix A 2>/dev/null | grep -qF "$1"
}

messages_contain() {
  tmux -L "$SOCK" show-messages | grep -qF "$1"
}

not_installing() {
  [ -z "$(tmux -L "$SOCK" show-option -gqv @agent_wizard_installing)" ]
}

two_downloads() {
  [ "$(grep -cx -- "--download 1" "$INSTALL_LOG")" -eq 2 ]
}

# Echoes a PATH holding only the real tmux (plus /usr/bin:/bin for bash and
# the utilities the script uses), so `command -v taw` — resolve_taw_bin's
# final rung — cannot find a taw the developer happens to have installed.
clean_path() {
  local bin="$BATS_TEST_TMPDIR/cleanbin"
  mkdir -p "$bin"
  ln -sf "$(command -v tmux)" "$bin/tmux"
  echo "$bin:/usr/bin:/bin"
}

# Polls up to $1 seconds for "$@" to succeed — the startup one-shots run via
# `run-shell -b` (backgrounded), so they land asynchronously.
wait_for() {
  local timeout="$1" waited=0
  shift
  until "$@" >/dev/null 2>&1; do
    waited=$((waited + 1))
    if [ "$waited" -ge $((timeout * 10)) ]; then
      return 1
    fi
    sleep 0.1
  done
}

# Sets what the fake taw answers `notification open-target` with: the reply it prints
# ($1, the real binary prints a JSON envelope) and the status it exits with ($2, 1 for an
# error). Server-wide, like TAW_FAKE_LOG: the binding runs under the tmux server, not
# under this shell.
fake_open_target() {
  tmux -L "$SOCK" set-environment -g TAW_FAKE_OPEN_TARGET_REPLY "$1"
  tmux -L "$SOCK" set-environment -g TAW_FAKE_OPEN_TARGET_STATUS "$2"
}

# Presses the notification key: runs the command bound to prefix O, from the server, as
# tmux runs a binding (`list-keys` prints a binding in a form `source-file` reads back).
# Leaves what the command printed in $output, and what it asked tmux to show in $shown:
# the `display-message` commands the server's message log gained since (the log is
# newest-first, so the new entries are its first lines). Only those: the log also holds the
# `run-shell` line itself, which names both messages whatever the binding did.
press_notification_key() {
  local before after
  tmux -L "$SOCK" list-keys -T prefix O |
    sed -E 's/^bind-key +(-r +)?-T +prefix +O +//' >"$BATS_TEST_TMPDIR/notification-key.conf"
  before=$(tmux -L "$SOCK" show-messages | wc -l)
  run tmux -L "$SOCK" source-file "$BATS_TEST_TMPDIR/notification-key.conf"
  after=$(tmux -L "$SOCK" show-messages)
  shown=$(printf '%s\n' "$after" |
    head -n "$(($(printf '%s\n' "$after" | wc -l) - before))" |
    grep 'command: display-message' || true)
}

# Presses the key $1 on the prefix table: runs the command bound to it, from the server, as tmux
# runs a binding (`list-keys` prints a binding in a form `source-file` reads back). The server has
# one pane, so the pane the binding's `#{pane_id}` names is that one. What the command printed is
# left in $output.
press_prefix_key() {
  local key="$1"
  tmux -L "$SOCK" list-keys -T prefix "$key" |
    sed -E "s/^bind-key +(-r +)?-T +prefix +$key +//" >"$BATS_TEST_TMPDIR/pressed-key.conf"
  run tmux -L "$SOCK" source-file "$BATS_TEST_TMPDIR/pressed-key.conf"
}

# Whether a `display-message` command naming $1 has run on the server: what a binding asked tmux to
# show on the status line. The log also holds the `run-shell` line itself, which names every message
# the binding could show whatever it did, so only the `display-message` lines are read.
shown_on_the_status_line() {
  tmux -L "$SOCK" show-messages | grep 'command: display-message' | grep -q "$1"
}

@test "sets @agent-wizard to A by default" {
  load_plugin
  run tmux -L "$SOCK" show-option -gqv @agent-wizard
  [ "$status" -eq 0 ]
  [ "$output" = "A" ]
}

@test "keeps a pre-set @agent-wizard value" {
  tmux -L "$SOCK" set-option -g @agent-wizard "Z"
  load_plugin
  run tmux -L "$SOCK" show-option -gqv @agent-wizard
  [ "$status" -eq 0 ]
  [ "$output" = "Z" ]
}

@test "sets the remaining documented option defaults" {
  load_plugin
  run tmux -L "$SOCK" show-option -gqv @agent-wizard-height
  [ "$output" = "100" ]
  run tmux -L "$SOCK" show-option -gqv @agent-wizard-width
  [ "$output" = "100" ]
  run tmux -L "$SOCK" show-option -gqv @agent-wizard-notification-key
  [ "$output" = "O" ]
  run tmux -L "$SOCK" show-option -gqv @agent-wizard-watch
  [ "$output" = "on" ]
  run tmux -L "$SOCK" show-option -gqv @agent-wizard-bin
  [ "$output" = "" ]
  run tmux -L "$SOCK" show-option -gqv @agent-wizard-popup-title
  [ "$output" = " taw " ]
  run tmux -L "$SOCK" show-option -gqv @agent-wizard-popup-border
  [ "$output" = "" ]
  run tmux -L "$SOCK" show-option -gqv @agent-wizard-clear-key
  [ "$output" = "" ]
  # Listed, as the other options with nothing in them are, and bound to nothing.
  run bash -c "tmux -L '$SOCK' show-options -g | grep -c '^@agent-wizard-clear-key '"
  [ "$output" -eq 1 ]
}

@test "binds prefix A to run-shell taw ui open" {
  load_plugin
  run tmux -L "$SOCK" list-keys -T prefix A
  [ "$status" -eq 0 ]
  [[ "$output" == *"run-shell"* ]] || false
  [[ "$output" == *"$FAKE_TAW"* ]] || false
  [[ "$output" == *"ui open"* ]] || false
  # Geometry and the popup title/border are `taw ui open`'s own job: the
  # key binding does not bake in -w/-h/-T itself.
  [[ "$output" != *"display-popup"* ]] || false
}

@test "the popup binding swallows taw ui open's JSON envelope" {
  load_plugin
  run tmux -L "$SOCK" list-keys -T prefix A
  [ "$status" -eq 0 ]
  # `display-popup` blocks until the popup closes, so `ui open`'s success envelope
  # (`{"id":"cli:ui:open","result":{…}}`) is printed *after* the user has switched
  # away -- and tmux shows any run-shell output in a copy-mode view in the pane the
  # key was pressed in. Without the redirect every other run-shell binding in this
  # file has, that JSON greets the user the next time they land on that pane.
  [[ "$output" == *"ui open >/dev/null 2>&1"* ]] || false
  # A failure to open the popup at all is still told to the user, not swallowed.
  [[ "$output" == *"taw: could not open the popup"* ]] || false
}

@test "binds the notification key to notification open-target" {
  load_plugin
  run tmux -L "$SOCK" list-keys -T prefix O
  [ "$status" -eq 0 ]
  [[ "$output" == *"run-shell"* ]] || false
  [[ "$output" == *"$FAKE_TAW"* ]] || false
  [[ "$output" == *"notification open-target"* ]] || false
  # The message when there is no agent to go to, and the one for any other failure.
  [[ "$output" == *"taw: no agents"* ]] || false
  [[ "$output" == *"taw: next up failed (see taw notification open-target)"* ]] || false
}

@test "the notification key says so when no pane holds an agent" {
  load_plugin
  # What the real binary answers when nothing runs an agent: a `not_found` error.
  fake_open_target '{"id":"cli:notification:open-target","error":{"code":"not_found","message":"no agents: no pane holds an agent, so there is nowhere to go"}}' 1
  press_notification_key
  [ "$status" -eq 0 ]
  # The reply is read, not shown: run-shell would print it in a view in the pane the key
  # was pressed in.
  [ -z "$output" ]
  [[ "$shown" == *"taw: no agents"* ]] || false
  [[ "$shown" != *"next up failed"* ]] || false
}

@test "the notification key says next up failed, and where to look, for any other failure" {
  load_plugin
  # A tmux failure as the real binary reports it: an error that is not `not_found`.
  fake_open_target '{"id":"cli:notification:open-target","error":{"code":"tmux_error","message":"`tmux list-panes -a` failed (exit status: 1): error connecting to /tmp/tmux-501/default (No such file or directory)"}}' 1
  press_notification_key
  [ "$status" -eq 0 ]
  [ -z "$output" ]
  [[ "$shown" == *"taw: next up failed (see taw notification open-target)"* ]] || false
  [[ "$shown" != *"taw: no agents"* ]] || false
}

@test "the notification key says next up failed when the binary cannot run and says nothing" {
  load_plugin
  # No envelope at all, and the status a shell gives a command it could not run.
  fake_open_target '' 127
  press_notification_key
  [ "$status" -eq 0 ]
  [ -z "$output" ]
  [[ "$shown" == *"taw: next up failed (see taw notification open-target)"* ]] || false
  [[ "$shown" != *"taw: no agents"* ]] || false
}

@test "the notification key shows nothing when open-target goes somewhere" {
  load_plugin
  fake_open_target '{"id":"cli:notification:open-target","result":{"type":"ok","pane_id":"%3","target":"needs_you"}}' 0
  press_notification_key
  [ "$status" -eq 0 ]
  [ -z "$output" ]
  [[ "$shown" != *"taw: no agents"* ]] || false
  [[ "$shown" != *"next up failed"* ]] || false
}

@test "honours a custom @agent-wizard-notification-key" {
  tmux -L "$SOCK" set-option -g @agent-wizard-notification-key "N"
  load_plugin
  run tmux -L "$SOCK" list-keys -T prefix N
  [ "$status" -eq 0 ]
  [[ "$output" == *"notification open-target"* ]] || false
  run tmux -L "$SOCK" list-keys -T prefix O
  [[ "$output" != *"notification open-target"* ]] || false
}

@test "binds nothing to pane clear until @agent-wizard-clear-key is set" {
  load_plugin
  run bash -c "tmux -L '$SOCK' list-keys | grep -c 'pane clear'"
  [ "$output" -eq 0 ]
}

@test "binds @agent-wizard-clear-key to taw pane clear on the pane the key is pressed in" {
  tmux -L "$SOCK" set-option -g @agent-wizard-clear-key "K"
  load_plugin
  run tmux -L "$SOCK" list-keys -T prefix K
  [ "$status" -eq 0 ]
  # In the background: a shell that is slow to answer must not hold the client's keys back.
  [[ "$output" == *"run-shell -b"* ]] || false
  [[ "$output" == *"$FAKE_TAW"* ]] || false
  # `run-shell` has no $TMUX_PANE, so the binding names the pane itself, as tmux expands it.
  [[ "$output" == *"pane clear '#{pane_id}'"* ]] || false
  [[ "$output" == *">/dev/null 2>&1"* ]] || false
  # A failure to clear is told to the user, not swallowed.
  [[ "$output" == *"taw: could not clear the pane"* ]] || false
}

@test "binds every key in a space-separated @agent-wizard-clear-key list" {
  tmux -L "$SOCK" set-option -g @agent-wizard-clear-key "K C-k"
  load_plugin
  run tmux -L "$SOCK" list-keys -T prefix K
  [[ "$output" == *"pane clear"* ]] || false
  run tmux -L "$SOCK" list-keys -T prefix C-k
  [[ "$output" == *"pane clear"* ]] || false
}

@test "pressing the clear key clears the pane it was pressed in and shows nothing" {
  tmux -L "$SOCK" set-option -g @agent-wizard-clear-key "K"
  load_plugin
  pane=$(tmux -L "$SOCK" display-message -p '#{pane_id}')
  press_prefix_key K
  [ "$status" -eq 0 ]
  [ -z "$output" ]
  wait_for 3 grep -qx "pane clear $pane" "$TAW_FAKE_LOG"
  # Nothing is told to the user when it worked.
  sleep 0.3
  run shown_on_the_status_line "could not clear"
  [ "$status" -ne 0 ]
}

@test "a clear that fails says so on the status line" {
  tmux -L "$SOCK" set-option -g @agent-wizard-clear-key "K"
  tmux -L "$SOCK" set-environment -g TAW_FAKE_CLEAR_STATUS 1
  load_plugin
  press_prefix_key K
  [ "$status" -eq 0 ]
  [ -z "$output" ]
  wait_for 3 shown_on_the_status_line "taw: could not clear the pane"
}

# How many bindings on the prefix table for key $1 carry the text $2: 0 for a key that is bound to
# something else or to nothing.
bindings_with() {
  tmux -L "$SOCK" list-keys -T prefix "$1" 2>/dev/null | grep -c "$2" || true
}

@test "sourcing the plugin again with the same options leaves each of its three keys bound once" {
  tmux -L "$SOCK" set-option -g @agent-wizard-clear-key "K"
  load_plugin
  load_plugin
  [ "$(bindings_with A 'ui open')" -eq 1 ]
  [ "$(bindings_with O 'notification open-target')" -eq 1 ]
  [ "$(bindings_with K 'pane clear')" -eq 1 ]
}

@test "a clear key the option no longer names is unbound when the plugin is sourced again" {
  tmux -L "$SOCK" set-option -g @agent-wizard-clear-key "K"
  load_plugin
  [ "$(bindings_with K 'pane clear')" -eq 1 ]
  tmux -L "$SOCK" set-option -g @agent-wizard-clear-key "J"
  load_plugin
  [ "$(bindings_with J 'pane clear')" -eq 1 ]
  [ "$(bindings_with K 'pane clear')" -eq 0 ]
}

@test "emptying @agent-wizard-clear-key and sourcing the plugin again unbinds the key" {
  tmux -L "$SOCK" set-option -g @agent-wizard-clear-key "K"
  load_plugin
  tmux -L "$SOCK" set-option -g @agent-wizard-clear-key ""
  load_plugin
  [ "$(bindings_with K 'pane clear')" -eq 0 ]
  run bash -c "tmux -L '$SOCK' list-keys | grep -c 'pane clear'"
  [ "$output" -eq 0 ]
  # The popup and notification keys are where they were.
  [ "$(bindings_with A 'ui open')" -eq 1 ]
  [ "$(bindings_with O 'notification open-target')" -eq 1 ]
}

@test "a key dropped from a list of clear keys is unbound and the others stay" {
  tmux -L "$SOCK" set-option -g @agent-wizard-clear-key "K C-k M-k"
  load_plugin
  tmux -L "$SOCK" set-option -g @agent-wizard-clear-key "C-k"
  load_plugin
  [ "$(bindings_with C-k 'pane clear')" -eq 1 ]
  [ "$(bindings_with K 'pane clear')" -eq 0 ]
  [ "$(bindings_with M-k 'pane clear')" -eq 0 ]
}

@test "a key of the user's own that replaced a clear key is left alone when the option is emptied" {
  tmux -L "$SOCK" set-option -g @agent-wizard-clear-key "K"
  load_plugin
  # The user rebinds it afterwards, in their tmux.conf or by hand.
  tmux -L "$SOCK" bind K display-message "mine"
  tmux -L "$SOCK" set-option -g @agent-wizard-clear-key ""
  load_plugin
  run tmux -L "$SOCK" list-keys -T prefix K
  [[ "$output" == *"mine"* ]] || false
}

@test "a popup key the option no longer names is unbound when the plugin is sourced again" {
  tmux -L "$SOCK" set-option -g @agent-wizard "B"
  load_plugin
  [ "$(bindings_with B 'ui open')" -eq 1 ]
  [ "$(bindings_with A 'ui open')" -eq 0 ]
  tmux -L "$SOCK" set-option -g @agent-wizard "A B"
  load_plugin
  [ "$(bindings_with A 'ui open')" -eq 1 ]
  tmux -L "$SOCK" set-option -g @agent-wizard "B"
  load_plugin
  [ "$(bindings_with A 'ui open')" -eq 0 ]
  [ "$(bindings_with B 'ui open')" -eq 1 ]
}

@test "the notification key moved to another key is unbound at the old one when the plugin is sourced again" {
  load_plugin
  [ "$(bindings_with O 'notification open-target')" -eq 1 ]
  tmux -L "$SOCK" set-option -g @agent-wizard-notification-key "N"
  load_plugin
  [ "$(bindings_with N 'notification open-target')" -eq 1 ]
  [ "$(bindings_with O 'notification open-target')" -eq 0 ]
}

@test "a key of the user's own is left alone while the clear key is unset" {
  tmux -L "$SOCK" bind K display-message "mine"
  load_plugin
  run tmux -L "$SOCK" list-keys -T prefix K
  [[ "$output" == *"mine"* ]] || false
  [[ "$output" != *"pane clear"* ]] || false
}

@test "binds every key in a space-separated @agent-wizard list" {
  tmux -L "$SOCK" set-option -g @agent-wizard "A B"
  load_plugin
  run tmux -L "$SOCK" list-keys -T prefix A
  [[ "$output" == *"run-shell"* ]] || false
  [[ "$output" == *"ui open"* ]] || false
  run tmux -L "$SOCK" list-keys -T prefix B
  [[ "$output" == *"run-shell"* ]] || false
  [[ "$output" == *"ui open"* ]] || false
}

@test "installs each hook exactly once even when sourced twice" {
  load_plugin
  run tmux -L "$SOCK" show-hooks -g
  [ "$status" -eq 0 ]
  first_count=$(printf '%s\n' "$output" | grep -c 'taw')
  # 3 watch hooks + 4 seen hooks + 4 autosave hooks = 11 entries (after-new-window
  # and after-split-window each carry two: a watch attach entry and a session
  # autosave entry) — but bare `show-hooks -g` (no hook name) doesn't enumerate
  # pane-focus-in on this tmux even though it is genuinely set (queried directly,
  # it's there; see the "documented hooks" test below), so assert idempotency by
  # comparing against a second load rather than hardcoding tmux's own quirky count.
  [ "$first_count" -gt 0 ]

  load_plugin
  run tmux -L "$SOCK" show-hooks -g
  [ "$status" -eq 0 ]
  second_count=$(printf '%s\n' "$output" | grep -c 'taw')

  [ "$second_count" -eq "$first_count" ]
}

@test "installs the watch/seen/autosave hooks with the documented verbs" {
  load_plugin

  for hook in after-new-window after-split-window after-new-session; do
    run tmux -L "$SOCK" show-hooks -g "$hook"
    [[ "$output" == *"run-shell -b"* ]] || false
    [[ "$output" == *"watch attach"* ]] || false
  done

  for hook in after-select-pane after-select-window client-session-changed pane-focus-in; do
    run tmux -L "$SOCK" show-hooks -g "$hook"
    [[ "$output" == *"run-shell -b"* ]] || false
    [[ "$output" == *" seen "* ]] || false
  done

  # Something opening saves at once (debounced). Something closing waits and then skips if
  # tmux has gone (`--on-close`), so a shutdown that closes panes one by one keeps the
  # layout it had. The flag has to be on exactly the two closing hooks: on an opening one
  # it would delay every save by five seconds, and missing from a closing one the last
  # panes of a quit would overwrite the full layout.
  for hook in after-new-window after-split-window; do
    run tmux -L "$SOCK" show-hooks -g "$hook"
    [[ "$output" == *"run-shell -b"* ]] || false
    [[ "$output" == *"session autosave"* ]] || false
    [[ "$output" != *"--on-close"* ]] || false
  done

  for hook in after-kill-pane session-closed; do
    run tmux -L "$SOCK" show-hooks -g "$hook"
    [[ "$output" == *"run-shell -b"* ]] || false
    [[ "$output" == *"session autosave --on-close"* ]] || false
  done
}

@test "installs one lifecycle-event hook per hook-sourced event, with q-escaped ids" {
  load_plugin

  # hook name -> the event it fires. One hook per event: an event installed on two
  # hooks would fire twice for a single change (see src/cli/events.rs).
  for pair in \
    "session-created workspace.created" \
    "window-linked tab.created" \
    "after-split-window pane.created" \
    "after-kill-pane pane.closed" \
    "window-unlinked pane.closed" \
    "window-unlinked tab.closed" \
    "session-window-changed tab.focused" \
    "after-rename-window tab.renamed" \
    "session-closed workspace.closed" \
    "client-session-changed workspace.focused" \
    "after-rename-session workspace.renamed" \
    "window-layout-changed layout.updated" \
    "pane-exited pane.exited" \
    "after-select-pane pane.focused"; do
    set -- $pair
    hook="$1"
    event="$2"
    run tmux -L "$SOCK" show-hooks -g "$hook"
    [ "$status" -eq 0 ]
    [[ "$output" == *"plugin event $event"* ]] || {
      echo "hook $hook is missing 'plugin event $event': $output" >&2
      return 1
    }
    count=$(printf '%s\n' "$output" | grep -c "plugin event $event")
    [ "$count" -eq 1 ]
  done

  # A tmux session id is `$N` and a window/session name is arbitrary text, both of which
  # a hook's `$SHELL -c` would otherwise expand or split. Every id and label goes through
  # `#{q:…}`, which tmux escapes for the shell.
  run tmux -L "$SOCK" show-hooks -g window-unlinked
  [[ "$output" == *'#{q:hook_window}'* ]] || false
  [[ "$output" == *'#{q:hook_session}'* ]] || false
  run tmux -L "$SOCK" show-hooks -g after-rename-session
  [[ "$output" == *'#{q:session_name}'* ]] || false
  run bash -c "tmux -L '$SOCK' show-hooks -g | grep 'plugin event' | grep -c '#{hook_session}'"
  [ "$output" -eq 0 ]
}

@test "event hooks survive a second source without stacking" {
  load_plugin
  load_plugin
  # Counted per *event*, not per hook: a hook can legitimately carry two events
  # (`window-unlinked` fires tab.closed and the pane.closed roster diff), but one event
  # installed twice on the same hook would fire twice for a single change.
  for pair in \
    "window-unlinked tab.closed" \
    "window-unlinked pane.closed" \
    "window-layout-changed layout.updated" \
    "after-select-pane pane.focused" \
    "window-linked tab.created" \
    "session-created workspace.created" \
    "after-kill-pane pane.closed"; do
    set -- $pair
    run tmux -L "$SOCK" show-hooks -g "$1"
    count=$(printf '%s\n' "$output" | grep -c "plugin event $2")
    [ "$count" -eq 1 ]
  done
}

@test "pane-exited marks the pane with a command of its own, ahead of the job that fires pane.exited, once" {
  load_plugin
  load_plugin
  run tmux -L "$SOCK" show-hooks -g pane-exited
  [ "$status" -eq 0 ]
  # Once, though the plugin was sourced twice: the strip that keeps every other hook from
  # stacking has to know this entry, which runs no taw binary.
  [ "$(printf '%s\n' "$output" | grep -c '@agent_wizard_exited')" -eq 1 ]
  [ "$(printf '%s\n' "$output" | grep -c 'plugin event pane.exited')" -eq 1 ]
  # A command of the hook's own and not a job it starts: a job is a process that has not begun
  # when a detection that fires beside it has already read the panes.
  mark=$(printf '%s\n' "$output" | grep '@agent_wizard_exited')
  [[ "$mark" != *"run-shell"* ]] || false
  # The entries of one hook run in the order they were added, so the mark has to be added first.
  mark_at=$(printf '%s\n' "$output" | grep -n '@agent_wizard_exited' | head -1 | cut -d: -f1)
  job_at=$(printf '%s\n' "$output" | grep -n 'plugin event pane.exited' | head -1 | cut -d: -f1)
  [ "$mark_at" -lt "$job_at" ]
}

@test "sourcing the plugin again never leaves a hook without its entries, not even between two steps" {
  load_plugin
  local dir="$BATS_TEST_TMPDIR/hook-steps" hook
  mkdir -p "$dir/bin"
  # Every hook the plugin has put entries on. Bare `show-hooks -g` leaves some out on 3.6b (the
  # focus hooks, pane-exited: see the idempotency test above), so the hooks the script names are
  # asked for by name too.
  { tmux -L "$SOCK" show-hooks -g | sed 's/\[.*//'
    grep -oE '^  "[a-z]+(-[a-z]+)+ ' "$PLUGIN" | tr -d ' "'
    echo pane-focus-in; echo client-focus-in; } |
    sort -u | while read -r hook; do
      if tmux -L "$SOCK" show-hooks -g "$hook" | grep -q -e "$FAKE_TAW" -e '@agent_wizard_exited'; then
        echo "$hook"
      fi
    done >"$dir/hooks"
  [ -s "$dir/hooks" ]
  # A tmux, first on PATH for the plugin script, that counts the plugin's entries on each of them
  # after every change to a hook: the script's own view of the order it works in.
  cat >"$dir/bin/tmux" <<EOF
#!/usr/bin/env bash
real='$(command -v tmux)'
"\$real" "\$@"
status=\$?
if [ "\$1" = set-hook ]; then
  {
    while read -r hook; do
      printf '%s %s\n' "\$hook" "\$("\$real" show-hooks -g "\$hook" 2>/dev/null | grep -c -e '$FAKE_TAW' -e '@agent_wizard_exited')"
    done <'$dir/hooks'
    echo --
  } >>'$dir/steps'
fi
exit \$status
EOF
  chmod +x "$dir/bin/tmux"

  tmux -L "$SOCK" run-shell "PATH='$dir/bin':\$PATH TAW_BIN='$FAKE_TAW' bash '$PLUGIN'"

  # It recorded its steps, every hook in each, and in none of them did a hook hold nothing of the
  # plugin's: an event that fires between two of them would run nothing.
  [ -s "$dir/steps" ]
  [ "$(grep -c '^pane-exited ' "$dir/steps")" -gt 1 ]
  ! grep -E ' 0$' "$dir/steps"
}

# The marks the pane-exited hook has left, and what the tests below wait for.
exit_marks() { tmux -L "$SOCK" show-options -gqv @agent_wizard_exited; }
exit_is_marked() { [ -n "$(exit_marks)" ]; }
only_the_first_pane_is_left() { [ "$(tmux -L "$SOCK" list-panes -a | wc -l)" -eq 1 ]; }

@test "a pane whose process ends is marked by its raw id, with or without a roster" {
  load_plugin
  # Without a roster too: the detection that is about to write the first one may already have
  # listed the pane, and has to find its mark.
  tmux -L "$SOCK" split-window -d -t 0 'sh -c "sleep 0.3"'
  wait_for 5 exit_is_marked
  [[ "$(exit_marks)" =~ ^%[0-9]+\ $ ]] || false
  first=$(exit_marks)

  # A second one is added after the first.
  tmux -L "$SOCK" split-window -d -t 0 'sh -c "sleep 0.3"'
  wait_for 5 two_exits_are_marked
  [[ "$(exit_marks)" == "$first"%[0-9]*" " ]] || false
}

two_exits_are_marked() { [ "$(exit_marks | wc -w)" -eq 2 ]; }

@test "the marks never grow past the last 400 characters" {
  load_plugin
  # A server nobody listens to takes the marks and never takes them off.
  long=$(printf '%%7 %.0s' $(seq 1 200))
  [ "${#long}" -gt 400 ]
  tmux -L "$SOCK" set-option -g @agent_wizard_exited "$long"
  tmux -L "$SOCK" split-window -d -t 0 'sh -c "sleep 0.3"'
  wait_for 5 only_the_first_pane_is_left
  marks=$(tmux -L "$SOCK" show-options -gqv @agent_wizard_exited)
  [ "${#marks}" -le 400 ]
  # What is kept is the end of the marks, with the new one last (the first pane made is %1).
  [[ "$marks" == *"%7 %1 " ]] || false
}

@test "after-split-window carries watch, autosave, pane.created and the roster refresh, once each" {
  load_plugin
  load_plugin
  run tmux -L "$SOCK" show-hooks -g after-split-window
  [ "$(printf '%s\n' "$output" | grep -c 'watch attach')" -eq 1 ]
  [ "$(printf '%s\n' "$output" | grep -c 'session autosave')" -eq 1 ]
  [ "$(printf '%s\n' "$output" | grep -c 'plugin event pane.created')" -eq 1 ]
  [ "$(printf '%s\n' "$output" | grep -c 'plugin event pane.closed')" -eq 1 ]
}

@test "the pane.closed roster refresh rides on every hook where a pane can appear" {
  load_plugin
  # The diff is only as good as the set it compares against: a pane created since the
  # last pass would be missing from the roster and its close would go unnoticed.
  for hook in after-split-window after-new-window after-new-session; do
    run tmux -L "$SOCK" show-hooks -g "$hook"
    [[ "$output" == *"plugin event pane.closed"* ]] || false
  done
  # And it takes no identifier: tmux names no killed pane.
  run tmux -L "$SOCK" show-hooks -g after-kill-pane
  [[ "$output" == *"plugin event pane.closed"* ]] || false
  [[ "$output" != *"pane.closed --pane"* ]] || false
}

@test "after-select-pane carries both seen and pane.focused, once each" {
  load_plugin
  load_plugin
  run tmux -L "$SOCK" show-hooks -g after-select-pane
  [ "$(printf '%s\n' "$output" | grep -c ' seen ')" -eq 1 ]
  [ "$(printf '%s\n' "$output" | grep -c 'plugin event pane.focused')" -eq 1 ]
}

@test "seen-on-focus is installed on the running server (pane-focus-in always, client-focus-in on 3.6+)" {
  load_plugin

  # pane-focus-in carries `seen` on every supported tmux. On 3.6b it is dropped
  # from bare `show-hooks -g` (see the idempotency test) but is genuinely set and
  # firing; queried by name, it is there — so seen-on-focus is not lost there.
  run tmux -L "$SOCK" show-hooks -g pane-focus-in
  [[ "$output" == *"run-shell -b"* ]] || false
  [[ "$output" == *" seen "* ]] || false

  # tmux 3.6 added client-focus-in, the canonical "the terminal regained focus"
  # signal; the plugin installs it there so the refocus path does not lean on the
  # display-quirky pane-focus-in alone, and it does enumerate in bare show-hooks.
  ver=$(tmux -L "$SOCK" display-message -p '#{version}')
  major=${ver%%.*}
  major=${major%%[!0-9]*}
  minor=${ver#*.}
  minor=${minor%%[!0-9]*}
  if [ "$major" -gt 3 ] || { [ "$major" -eq 3 ] && [ "$minor" -ge 6 ]; }; then
    run tmux -L "$SOCK" show-hooks -g client-focus-in
    [[ "$output" == *"run-shell -b"* ]] || false
    [[ "$output" == *" seen "* ]] || false
    run tmux -L "$SOCK" show-hooks -g
    [[ "$output" == *"client-focus-in"* ]] || false
  fi
}

@test "after-new-window and after-split-window get both watch attach and session autosave, exactly once each even sourced twice" {
  load_plugin
  load_plugin

  for hook in after-new-window after-split-window; do
    run tmux -L "$SOCK" show-hooks -g "$hook"
    [ "$status" -eq 0 ]
    [[ "$output" == *"watch attach"* ]] || false
    [[ "$output" == *"session autosave"* ]] || false
    watch_count=$(printf '%s\n' "$output" | grep -c 'watch attach')
    autosave_count=$(printf '%s\n' "$output" | grep -c 'session autosave')
    [ "$watch_count" -eq 1 ]
    [ "$autosave_count" -eq 1 ]
  done
}

@test "never touches an existing unrelated hook" {
  tmux -L "$SOCK" set-hook -ga after-new-window 'run-shell -b "echo unrelated-marker"'
  load_plugin
  run tmux -L "$SOCK" show-hooks -g after-new-window
  [[ "$output" == *"unrelated-marker"* ]] || false
  [[ "$output" == *"watch attach"* ]] || false
}

@test "respects @agent-wizard-watch off: no watch hooks, no watch attach-all" {
  tmux -L "$SOCK" set-option -g @agent-wizard-watch "off"
  load_plugin
  wait_for 2 grep -q "status refresh" "$TAW_FAKE_LOG"

  for hook in after-new-window after-split-window after-new-session; do
    run tmux -L "$SOCK" show-hooks -g "$hook"
    [[ "$output" != *"watch attach"* ]] || false
  done

  run grep -c "watch attach-all" "$TAW_FAKE_LOG"
  [ "$status" -ne 0 ] || [ "$output" -eq 0 ]

  # seen/autosave hooks are unaffected by the watch setting
  run tmux -L "$SOCK" show-hooks -g pane-focus-in
  [[ "$output" == *" seen "* ]] || false
}

@test "runs the startup one-shots and records the reported version" {
  load_plugin
  wait_for 2 grep -q "manifest update --if-stale" "$TAW_FAKE_LOG"

  run grep -c "watch attach-all" "$TAW_FAKE_LOG"
  [ "$output" -eq 1 ]
  run grep -c "^status refresh$" "$TAW_FAKE_LOG"
  [ "$output" -eq 1 ]
  run grep -c "^plugin startup$" "$TAW_FAKE_LOG"
  [ "$output" -eq 1 ]
  run grep -c "manifest update --if-stale" "$TAW_FAKE_LOG"
  [ "$output" -eq 1 ]
  run grep -c "^server started$" "$TAW_FAKE_LOG"
  [ "$output" -eq 1 ]

  run tmux -L "$SOCK" show-option -gqv @agent_wizard_version
  [ "$output" = "taw 0.0.0-fake" ]
}

@test "the server started one-shot runs on every load, and its output is discarded" {
  load_plugin
  wait_for 2 grep -q "^server started$" "$TAW_FAKE_LOG"
  # A second load, as a source-file of tmux.conf makes, runs it again: the verb itself says the
  # same server is nothing to do.
  load_plugin
  wait_for 2 bash -c "[ \"\$(grep -c '^server started\$' '$TAW_FAKE_LOG')\" -eq 2 ]"
  # The fake prints a line on both streams, as the real verb's envelope would. Left to tmux, a
  # background run-shell puts what it prints in a view in a pane, which is waited for here before
  # looking: the one-shot has run (it is in the log), and the other one-shots have as well.
  wait_for 2 grep -q "manifest update --if-stale" "$TAW_FAKE_LOG"
  run tmux -L "$SOCK" list-panes -a -F '#{pane_in_mode}'
  [[ "$output" != *1* ]] || false
}

@test "refuses tmux < 3.2 with a display-message and installs nothing" {
  shim_dir="$BATS_TEST_TMPDIR/oldtmux"
  mkdir -p "$shim_dir"
  real_tmux=$(command -v tmux)
  # An old tmux reported through BOTH channels the gate can read: the binary's
  # own `-V`, and the running server's `#{version}` (which the gate prefers).
  # Everything else is the real tmux against the real server.
  {
    printf '#!/usr/bin/env bash\n'
    printf 'if [ "$1" = "-V" ]; then\n'
    printf '  echo "tmux 3.1a"\n'
    printf '  exit 0\n'
    printf 'fi\n'
    printf 'if [ "$1" = "display-message" ]; then\n'
    printf '  for a in "$@"; do\n'
    printf '    if [ "$a" = "#{version}" ]; then echo "3.1a"; exit 0; fi\n'
    printf '  done\n'
    printf 'fi\n'
    printf 'exec %q "$@"\n' "$real_tmux"
  } >"$shim_dir/tmux"
  chmod +x "$shim_dir/tmux"

  # The isolated copy plus a clean PATH: the assertion below is that *nothing*
  # was installed, which must hold because of the version gate alone and not
  # because TAW_BIN happened to be the only resolvable binary.
  isolated=$(isolated_plugin)
  tmux -L "$SOCK" run-shell "PATH='$shim_dir:$(clean_path)' TAW_BIN='$FAKE_TAW' bash '$isolated'"

  run tmux -L "$SOCK" show-messages
  [ "$status" -eq 0 ]
  [[ "$output" == *"tmux-agent-wizard"* ]] || false
  [[ "$output" == *"requires tmux"* ]] || false

  run tmux -L "$SOCK" list-keys -T prefix A
  [[ "$output" != *"$FAKE_TAW"* ]] || false
  run tmux -L "$SOCK" show-hooks -g after-new-window
  [[ "$output" != *"watch attach"* ]] || false
}

@test "the version floor reads the running server, not a newer tmux on PATH" {
  # A stub whose `-V` claims a version below the floor, but which is *not* the
  # server: the server is the real tmux (>= 3.2). Because the gate reads the
  # server's #{version} (execed through to the real tmux here), it passes and the
  # plugin installs — the stub's fake -V, which a version check that read it would trust, is never
  # consulted. A check that read the stub's -V would refuse to install.
  shim_dir="$BATS_TEST_TMPDIR/newtmux"
  mkdir -p "$shim_dir"
  real_tmux=$(command -v tmux)
  {
    printf '#!/usr/bin/env bash\n'
    printf 'if [ "$1" = "-V" ]; then\n'
    printf '  echo "tmux 2.0"\n'
    printf '  exit 0\n'
    printf 'fi\n'
    printf 'exec %q "$@"\n' "$real_tmux"
  } >"$shim_dir/tmux"
  chmod +x "$shim_dir/tmux"

  isolated=$(isolated_plugin)
  tmux -L "$SOCK" run-shell "PATH='$shim_dir:$(clean_path)' TAW_BIN='$FAKE_TAW' bash '$isolated'"

  # Installed: the gate saw the server's real version, not the stub's -V=2.0.
  run tmux -L "$SOCK" list-keys -T prefix A
  [[ "$output" == *"$FAKE_TAW"* ]] || false
  run tmux -L "$SOCK" show-hooks -g after-new-window
  [[ "$output" == *"watch attach"* ]] || false
  # And no version-refusal message was shown.
  run tmux -L "$SOCK" show-messages
  [[ "$output" != *"requires tmux"* ]] || false
}

@test "reports a clear message and exits cleanly when no taw binary resolves" {
  # No TAW_BIN, no @agent-wizard-bin, an isolated plugin dir (so neither
  # bin/taw nor target/release/taw sits next to the script) and a PATH with no
  # taw on it: every rung of resolve_taw_bin must come up empty.
  isolated=$(isolated_plugin)
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  run tmux -L "$SOCK" show-messages
  [ "$status" -eq 0 ]
  [[ "$output" == *"taw binary not found"* ]] || false
  [[ "$output" == *"scripts/install.sh"* ]] || false
  # Nothing was installed, so the plugin's keys and hooks are absent.
  run tmux -L "$SOCK" list-keys -T prefix A
  [[ "$output" != *"display-popup"* ]] || false
  run tmux -L "$SOCK" show-hooks -g after-new-window
  [[ "$output" != *"watch attach"* ]] || false
}

@test "with no taw, the plugin downloads it in the background, then loads itself" {
  isolated=$(isolated_installable_plugin 9.9.9)
  work=$(dirname "$isolated")
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  run tmux -L "$SOCK" show-messages
  [[ "$output" == *"downloading taw for this machine"* ]] || false
  # The background install puts bin/taw in place and loads the plugin again,
  # so the popup key ends up bound to the downloaded binary.
  wait_for 10 prefix_a_runs "$work/bin/taw"
  wait_for 10 messages_contain "taw is ready, press prefix + A"
  run tmux -L "$SOCK" show-hooks -g after-new-window
  [[ "$output" == *"watch attach"* ]] || false
  # It asked for a download that may fall back to a source build.
  grep -qx -- "--download " "$INSTALL_LOG"
  run tmux -L "$SOCK" show-option -gqv @agent_wizard_installing
  [ -z "$output" ]
}

@test "a failed download says so, names the log, and binds nothing" {
  isolated=$(isolated_installable_plugin 9.9.9 fail)
  work=$(dirname "$isolated")
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  wait_for 10 messages_contain "could not install taw, see $work/bin/install.log"
  grep -q "no release" "$work/bin/install.log"
  run tmux -L "$SOCK" list-keys -T prefix A
  [[ "$output" != *"ui open"* ]] || false
  run tmux -L "$SOCK" show-option -gqv @agent_wizard_installing
  [ -z "$output" ]
}

@test "@agent-wizard-auto-install off shows the plain message and downloads nothing" {
  isolated=$(isolated_installable_plugin 9.9.9)
  tmux -L "$SOCK" set-option -g @agent-wizard-auto-install off
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  run tmux -L "$SOCK" show-messages
  [[ "$output" == *"taw binary not found"* ]] || false
  [[ "$output" == *"scripts/install.sh"* ]] || false
  sleep 1
  [ -f "$INSTALL_LOG" ]
  [ ! -s "$INSTALL_LOG" ]
}

@test "a second load while a download is running does not start another" {
  isolated=$(isolated_installable_plugin 9.9.9)
  tmux -L "$SOCK" set-option -g @agent_wizard_installing 1
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  run tmux -L "$SOCK" show-messages
  [[ "$output" == *"still downloading taw"* ]] || false
  sleep 1
  [ -f "$INSTALL_LOG" ]
  [ ! -s "$INSTALL_LOG" ]
}

@test "a downloaded taw is refreshed in the background when the plugin moves to a new version" {
  isolated=$(isolated_installable_plugin 9.9.9)
  work=$(dirname "$isolated")
  mkdir -p "$work/bin"
  cp "$FAKE_TAW" "$work/bin/taw"
  echo "9.9.8 9.9.8 test-target" >"$work/bin/.taw-release"
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  # Loaded straight away with the binary it already has...
  run tmux -L "$SOCK" list-keys -T prefix A
  [[ "$output" == *"$work/bin/taw"* ]] || false
  # ...and fetched the new one in the background, with no source-build fallback.
  wait_for 10 grep -qx -- "--download 1" "$INSTALL_LOG"
  wait_for 10 messages_contain "taw updated to 9.9.9"
}

@test "after a background update replaces taw, the watchers already running start again from the new one" {
  isolated=$(isolated_installable_plugin 9.9.9)
  work=$(dirname "$isolated")
  mkdir -p "$work/bin"
  cp "$FAKE_TAW" "$work/bin/taw"
  echo "9.9.8 9.9.8 test-target" >"$work/bin/.taw-release"
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  wait_for 10 messages_contain "taw updated to 9.9.9"
  wait_for 10 grep -qx -- "watch attach-all --restart" "$TAW_FAKE_LOG"
}

@test "with @agent-wizard-watch off, a background update starts no watcher" {
  isolated=$(isolated_installable_plugin 9.9.9)
  work=$(dirname "$isolated")
  mkdir -p "$work/bin"
  cp "$FAKE_TAW" "$work/bin/taw"
  echo "9.9.8 9.9.8 test-target" >"$work/bin/.taw-release"
  tmux -L "$SOCK" set-option -g @agent-wizard-watch off
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  wait_for 10 messages_contain "taw updated to 9.9.9"
  [ "$(grep -c -- "watch attach-all" "$TAW_FAKE_LOG")" -eq 0 ]
}

@test "a downloaded taw older than the checkout is tried again at every load, quietly while its release is not out" {
  isolated=$(isolated_installable_plugin 9.9.9 older)
  work=$(dirname "$isolated")
  mkdir -p "$work/bin"
  cp "$FAKE_TAW" "$work/bin/taw"
  echo "9.9.8 9.9.8 test-target" >"$work/bin/.taw-release"
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  wait_for 10 grep -qx -- "--download 1" "$INSTALL_LOG"
  wait_for 10 not_installing
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  wait_for 10 two_downloads
  wait_for 10 not_installing
  run tmux -L "$SOCK" show-messages
  [[ "$output" != *"taw updated"* ]] || false
}

@test "a taw you built yourself, or one already fetched for this version, is left alone" {
  isolated=$(isolated_installable_plugin 9.9.9)
  work=$(dirname "$isolated")
  mkdir -p "$work/bin"
  cp "$FAKE_TAW" "$work/bin/taw"
  # No marker: a source build or a copy you put there.
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  # A marker for this checkout's own version: nothing newer to fetch.
  echo "9.9.9 9.9.9 test-target" >"$work/bin/.taw-release"
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  sleep 1
  [ -f "$INSTALL_LOG" ]
  [ ! -s "$INSTALL_LOG" ]
  run tmux -L "$SOCK" list-keys -T prefix A
  [[ "$output" == *"$work/bin/taw"* ]] || false
}

@test "bin/taw linked to a build in target/release, with no marker, is used and never replaced" {
  isolated=$(isolated_installable_plugin 9.9.9)
  work=$(dirname "$isolated")
  # The checkout as scripts/install.sh with no flag (or `mise run build`) leaves it.
  mkdir -p "$work/target/release" "$work/bin"
  cp "$FAKE_TAW" "$work/target/release/taw"
  ln -s ../target/release/taw "$work/bin/taw"
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  run tmux -L "$SOCK" list-keys -T prefix A
  [[ "$output" == *"$work/bin/taw"* ]] || false
  # The plugin moves to a new version, as an update through TPM does, and loads again.
  printf '[package]\nname = "tmux-agent-wizard"\nversion = "9.9.10"\n' >"$work/Cargo.toml"
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  sleep 1
  [ -f "$INSTALL_LOG" ]
  [ ! -s "$INSTALL_LOG" ]
  [ "$(readlink "$work/bin/taw")" = "../target/release/taw" ]
  cmp -s "$FAKE_TAW" "$work/target/release/taw"
  [ ! -e "$work/bin/.taw-release" ]
  run tmux -L "$SOCK" show-messages
  [[ "$output" != *"downloading taw"* ]] || false
}

@test "a bin/taw link whose build is gone is left for you to build again, not downloaded over" {
  isolated=$(isolated_installable_plugin 9.9.9)
  work=$(dirname "$isolated")
  # The link a source build made, after a `cargo clean` took the build away.
  mkdir -p "$work/bin"
  ln -s ../target/release/taw "$work/bin/taw"
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  run tmux -L "$SOCK" show-messages
  [[ "$output" == *"bin/taw links to a build that is not there"* ]] || false
  [[ "$output" == *"$work/scripts/install.sh"* ]] || false
  [[ "$output" != *"downloading taw"* ]] || false
  sleep 1
  [ -f "$INSTALL_LOG" ]
  [ ! -s "$INSTALL_LOG" ]
  [ "$(readlink "$work/bin/taw")" = "../target/release/taw" ]
  run tmux -L "$SOCK" list-keys -T prefix A
  [[ "$output" != *"ui open"* ]] || false
}

@test "a bin/taw link is not refreshed, even beside a marker an earlier download left" {
  isolated=$(isolated_installable_plugin 9.9.9)
  work=$(dirname "$isolated")
  mkdir -p "$work/target/release" "$work/bin"
  cp "$FAKE_TAW" "$work/target/release/taw"
  ln -s ../target/release/taw "$work/bin/taw"
  # A release tried out in the checkout (docs/RELEASING.md) wrote the marker; the build
  # after it (`mise run build`) put the link back and left the marker there.
  echo "9.9.8 9.9.8 test-target" >"$work/bin/.taw-release"
  tmux -L "$SOCK" run-shell "PATH='$(clean_path)' bash '$isolated'"
  run tmux -L "$SOCK" list-keys -T prefix A
  [[ "$output" == *"$work/bin/taw"* ]] || false
  sleep 1
  [ -f "$INSTALL_LOG" ]
  [ ! -s "$INSTALL_LOG" ]
  [ "$(readlink "$work/bin/taw")" = "../target/release/taw" ]
}

@test "@agent-wizard-bin takes precedence over PATH resolution" {
  tmux -L "$SOCK" set-option -g @agent-wizard-bin "$FAKE_TAW"
  tmux -L "$SOCK" run-shell "TAW_FAKE_LOG='$TAW_FAKE_LOG' bash '$PLUGIN'"
  run tmux -L "$SOCK" list-keys -T prefix A
  [ "$status" -eq 0 ]
  [[ "$output" == *"$FAKE_TAW"* ]] || false
}

@test "every hook and one-shot discards its output, so run-shell never prints an envelope into a pane" {
  # Regression: `run-shell -b` shows whatever the command printed once it
  # exits, so a bare `taw seen` hook painted its JSON envelope into the pane
  # on every window switch.
  load_plugin

  run bash -c "tmux -L '$SOCK' show-hooks -g | grep -c 'run-shell -b'"
  [ "$output" -ge 10 ]
  run bash -c "tmux -L '$SOCK' show-hooks -g | grep 'run-shell -b' | grep -vc '>/dev/null 2>&1'"
  [ "$output" -eq 0 ]

  # The notification key reads the verb's reply instead of discarding it, so it captures
  # it (`out=$(… 2>&1)`); the tests that press the key check that nothing is printed.
  run tmux -L "$SOCK" list-keys -T prefix O
  [[ "$output" == *"notification open-target 2>&1)"* ]] || false

  # The startup one-shots run before any test can observe them, so pin the
  # script text: each `tmux run-shell -b "$bin_q …"` must end in a redirect.
  run bash -c "grep -c 'tmux run-shell -b \"\$bin_q [a-z -]* >/dev/null 2>&1 || true\"' '$PLUGIN'"
  [ "$output" -eq 5 ]
  run bash -c "grep -c 'tmux run-shell -b \"\$bin_q [a-z -]*\"' '$PLUGIN'"
  [ "$output" -eq 0 ]
}

@test "every hook job and every startup job exits 0 whatever taw does, so tmux never prints a returned message into a pane" {
  # A job that exits non-zero makes tmux print "'<command>' returned 1" into the job's pane, and with
  # that pane gone with its session, into another one it goes looking for (cmd_find_from_nothing,
  # which crashes a tmux 3.6a or 3.6b server while a session is being closed). The output is already
  # discarded; the exit status ends in `|| true` so that it is 0 too.
  load_plugin

  # Every hook with an entry of the plugin's, asked for by name: bare `show-hooks -g` leaves some out
  # on 3.6b (the focus hooks, pane-exited, window-layout-changed), so the hooks the script names are
  # asked for too.
  local hook line jobs=0 ends_in_true=' >/dev/null 2>&1 \|\| true["'"'"']$'
  while read -r hook; do
    while IFS= read -r line; do
      case "$line" in
      *"$FAKE_TAW"*) ;;
      *) continue ;;
      esac
      jobs=$((jobs + 1))
      [[ "$line" == *"run-shell -b "* ]] || { echo "not a background job: $line"; false; }
      # tmux lists the command in whichever quotes it finds fit, so the last character is any quote.
      [[ "$line" =~ $ends_in_true ]] || { echo "can exit non-zero: $line"; false; }
    done < <(tmux -L "$SOCK" show-hooks -g "$hook")
  done < <({ tmux -L "$SOCK" show-hooks -g | sed 's/\[.*//'
    grep -oE '^  "[a-z]+(-[a-z]+)+ ' "$PLUGIN" | tr -d ' "'
    echo pane-focus-in; echo client-focus-in; } | sort -u)
  # watch attach (3 hooks), seen (4, and client-focus-in on tmux 3.6 and later), session autosave (4)
  # and plugin event (17 entries): a count that a run which read only some of them could not reach.
  local ver major minor expected=28
  ver=$(tmux -L "$SOCK" display-message -p '#{version}')
  major=${ver%%.*}
  major=${major%%[!0-9]*}
  minor=${ver#*.}
  minor=${minor%%[!0-9]*}
  if [ "$major" -gt 3 ] || { [ "$major" -eq 3 ] && [ "$minor" -ge 6 ]; }; then
    expected=29
  fi
  [ "$jobs" -eq "$expected" ] || { echo "found $jobs hook jobs, expected $expected"; false; }

  # The startup one-shots and the two download jobs run before any test can observe them, so pin the
  # script text: every `tmux run-shell -b` the plugin runs at load ends in `|| true"`.
  run bash -c "grep -c 'tmux run-shell -b ' '$PLUGIN'"
  [ "$output" -eq 7 ]
  run bash -c "grep 'tmux run-shell -b ' '$PLUGIN' | grep -vc ' >/dev/null 2>&1 || true\"\$'"
  [ "$output" -eq 0 ]
}

@test "stops an outer agent's session markers reaching new panes, and touches nothing else" {
  # Start from a known server environment: whatever the developer's own shell exports (an
  # agent's own markers, when the suite is run from inside one) must not decide this test.
  local name
  for name in CODEX_THREAD_ID OMPCODE CLAUDECODE CLAUDE_CODE_CHILD_SESSION \
    CLAUDE_CODE_SESSION_ID CLAUDE_CODE_MESSAGING_TOKEN; do
    tmux -L "$SOCK" set-environment -gu "$name"
  done
  tmux -L "$SOCK" set-environment -g CLAUDECODE 1
  tmux -L "$SOCK" set-environment -g CODEX_THREAD_ID outer-thread
  tmux -L "$SOCK" set-environment -g ANTHROPIC_API_KEY keep-me

  load_plugin

  # Present markers are flagged "remove before starting a process": tmux lists those as -NAME.
  run tmux -L "$SOCK" show-environment -g CLAUDECODE
  [ "$output" = "-CLAUDECODE" ]
  run tmux -L "$SOCK" show-environment -g CODEX_THREAD_ID
  [ "$output" = "-CODEX_THREAD_ID" ]
  # Anything else is left exactly as it was.
  run tmux -L "$SOCK" show-environment -g ANTHROPIC_API_KEY
  [ "$output" = "ANTHROPIC_API_KEY=keep-me" ]
  # A marker the server never had is not flagged: nothing is added for it.
  run tmux -L "$SOCK" show-environment -g OMPCODE
  [ "$status" -ne 0 ]

  # Sourcing again (TPM does, on every prefix + I) leaves it as it is.
  load_plugin
  run tmux -L "$SOCK" show-environment -g CLAUDECODE
  [ "$output" = "-CLAUDECODE" ]
  run tmux -L "$SOCK" show-environment -g OMPCODE
  [ "$status" -ne 0 ]
}

# What the server's global environment holds for $1, as `show-environment -g` lists it:
# NAME=value, -NAME when it is flagged removed, and "unset" when it is not in it at all.
global_environment_of() {
  tmux -L "$SOCK" show-environment -g "$1" 2>/dev/null || echo unset
}

@test "stops the terminal that started tmux reaching new panes, and touches nothing else" {
  # Start from a known server environment: whatever the developer's own terminal exports
  # (iTerm2's and Kitty's handles, when the suite is run from one) must not decide this test.
  local name
  for name in ITERM_SESSION_ID LC_TERMINAL LC_TERMINAL_VERSION WEZTERM_PANE KITTY_WINDOW_ID \
    WT_SESSION STY ZELLIJ ZELLIJ_SESSION_NAME ZELLIJ_PANE_ID; do
    tmux -L "$SOCK" set-environment -gu "$name"
  done
  tmux -L "$SOCK" set-environment -g ITERM_SESSION_ID w0t0p0:outer
  tmux -L "$SOCK" set-environment -g LC_TERMINAL iTerm2
  tmux -L "$SOCK" set-environment -g KITTY_WINDOW_ID 7
  tmux -L "$SOCK" set-environment -g STY 123.pts-0.outer
  tmux -L "$SOCK" set-environment -g ZELLIJ_PANE_ID 4
  # tmux sets TMUX and TMUX_PANE for every pane itself, so a server started inside another
  # tmux holding them is none of the plugin's business.
  tmux -L "$SOCK" set-environment -g TMUX outer-socket,1,0
  tmux -L "$SOCK" set-environment -g TMUX_PANE %9
  # TERM_PROGRAM names the starting terminal too, but tmux gives every pane its own, and it is
  # not one of the ten.
  tmux -L "$SOCK" set-environment -g TERM_PROGRAM WezTerm
  tmux -L "$SOCK" set-environment -g ITERM_PROFILE Default
  tmux -L "$SOCK" set-environment -g ANTHROPIC_API_KEY keep-me

  load_plugin

  # Present handles are flagged "remove before starting a process": tmux lists those as -NAME.
  for name in ITERM_SESSION_ID LC_TERMINAL KITTY_WINDOW_ID STY ZELLIJ_PANE_ID; do
    [ "$(global_environment_of "$name")" = "-$name" ] ||
      { echo "$name should be flagged removed, tmux lists it as: $(global_environment_of "$name")" >&2; false; }
  done
  # A handle the server never had is not flagged: nothing is added for it.
  for name in LC_TERMINAL_VERSION WEZTERM_PANE WT_SESSION ZELLIJ ZELLIJ_SESSION_NAME; do
    [ "$(global_environment_of "$name")" = unset ] ||
      { echo "$name was never set and should not be flagged: $(global_environment_of "$name")" >&2; false; }
  done
  # Anything else is left exactly as it was, including tmux's own two, TERM_PROGRAM and another
  # variable of the same terminal that is not one of the ten.
  [ "$(global_environment_of TMUX)" = "TMUX=outer-socket,1,0" ]
  [ "$(global_environment_of TMUX_PANE)" = "TMUX_PANE=%9" ]
  [ "$(global_environment_of TERM_PROGRAM)" = "TERM_PROGRAM=WezTerm" ]
  [ "$(global_environment_of ITERM_PROFILE)" = "ITERM_PROFILE=Default" ]
  [ "$(global_environment_of ANTHROPIC_API_KEY)" = "ANTHROPIC_API_KEY=keep-me" ]

  # Sourcing again (TPM does, on every prefix + I) leaves it as it is.
  load_plugin
  [ "$(global_environment_of ITERM_SESSION_ID)" = "-ITERM_SESSION_ID" ]
  [ "$(global_environment_of WEZTERM_PANE)" = unset ]
  [ "$(global_environment_of TMUX)" = "TMUX=outer-socket,1,0" ]
  [ "$(global_environment_of TERM_PROGRAM)" = "TERM_PROGRAM=WezTerm" ]
}

# The PATH link of install step 4 (`ln -s <state>/bin/taw ~/.local/bin/taw`) points at taw's own link, so a
# load that has no bin/taw of its own and takes a `taw` off PATH must never take that one: it would point
# the state link at a link that points back at it, and `taw` would be gone from every pane. These tests
# give the load a scratch HOME and state folder, an earlier install (a real taw, the state link to it, the
# step-4 link) and an isolated plugin copy with no bin/taw. $LOOP_HOME holds all of it.
loop_scene() {
  LOOP_HOME="$BATS_TEST_TMPDIR/loop-home"
  LOOP_STATE="$LOOP_HOME/state"
  LOOP_REAL="$LOOP_HOME/old/bin/taw"
  mkdir -p "$LOOP_STATE/bin" "$LOOP_HOME/.local/bin" "$LOOP_HOME/old/bin"
  cp "$FAKE_TAW" "$LOOP_REAL"
  chmod +x "$LOOP_REAL"
  ln -s "$LOOP_STATE/bin/taw" "$LOOP_HOME/.local/bin/taw"
  LOOP_ENTRY=$(isolated_plugin)
  tmux -L "$SOCK" set-option -g @agent-wizard-auto-install off
}

# Loads the isolated plugin with the scene's HOME, state folder and a PATH of $1 (a colon list) then
# clean_path's folders, and no TAW_BIN. Not the real tmux's folder: a Homebrew one can hold a taw link of
# the machine's own, which the load would take.
loop_load() {
  tmux -L "$SOCK" run-shell "HOME='$LOOP_HOME' TAW_STATE_DIR='$LOOP_STATE' TAW_FAKE_LOG='$TAW_FAKE_LOG' PATH='$1:$(clean_path)' bash '$LOOP_ENTRY'"
}

@test "a load with no bin/taw and only the step-4 link on PATH leaves a good state link alone" {
  loop_scene
  ln -s "$LOOP_REAL" "$LOOP_STATE/bin/taw"
  loop_load "$LOOP_HOME/.local/bin"
  [ "$(readlink "$LOOP_STATE/bin/taw")" = "$LOOP_REAL" ]
  [ -x "$LOOP_STATE/bin/taw" ]
  [ -x "$LOOP_HOME/.local/bin/taw" ]
}

@test "a load with no bin/taw skips the step-4 link and takes the next taw on PATH" {
  loop_scene
  ln -s "$LOOP_REAL" "$LOOP_STATE/bin/taw"
  loop_load "$LOOP_HOME/.local/bin:$LOOP_HOME/old/bin"
  [ "$(readlink "$LOOP_STATE/bin/taw")" = "$LOOP_REAL" ]
  prefix_a_runs "$LOOP_REAL"
}

@test "a state link that is already a loop is replaced by the first good taw on PATH at the next load" {
  loop_scene
  ln -s "$LOOP_HOME/.local/bin/taw" "$LOOP_STATE/bin/taw"
  [ ! -e "$LOOP_STATE/bin/taw" ]
  loop_load "$LOOP_HOME/.local/bin:$LOOP_HOME/old/bin"
  [ "$(readlink "$LOOP_STATE/bin/taw")" = "$LOOP_REAL" ]
  [ -x "$LOOP_HOME/.local/bin/taw" ]
}

@test "a state link that is already a loop is replaced by the plugin's own bin/taw at the next load" {
  loop_scene
  mkdir -p "$(dirname "$LOOP_ENTRY")/bin"
  cp "$FAKE_TAW" "$(dirname "$LOOP_ENTRY")/bin/taw"
  chmod +x "$(dirname "$LOOP_ENTRY")/bin/taw"
  ln -s "$LOOP_HOME/.local/bin/taw" "$LOOP_STATE/bin/taw"
  loop_load "$LOOP_HOME/.local/bin"
  [ "$(readlink "$LOOP_STATE/bin/taw")" = "$(dirname "$LOOP_ENTRY")/bin/taw" ]
  [ -x "$LOOP_HOME/.local/bin/taw" ]
}

@test "a state link whose target is gone is replaced by the first good taw on PATH at the next load" {
  loop_scene
  ln -s "$LOOP_HOME/gone/taw" "$LOOP_STATE/bin/taw"
  loop_load "$LOOP_HOME/.local/bin:$LOOP_HOME/old/bin"
  [ "$(readlink "$LOOP_STATE/bin/taw")" = "$LOOP_REAL" ]
}

@test "a taw on PATH that is a link in a loop, or to nothing, is never taken" {
  loop_scene
  mkdir -p "$LOOP_HOME/bad1" "$LOOP_HOME/bad2"
  ln -s "$LOOP_HOME/bad1/taw" "$LOOP_HOME/bad1/taw"
  ln -s "$LOOP_HOME/gone/taw" "$LOOP_HOME/bad2/taw"
  ln -s "$LOOP_REAL" "$LOOP_STATE/bin/taw"
  loop_load "$LOOP_HOME/bad1:$LOOP_HOME/bad2:$LOOP_HOME/.local/bin"
  [ "$(readlink "$LOOP_STATE/bin/taw")" = "$LOOP_REAL" ]
  ! prefix_a_runs "$LOOP_HOME/bad"
}

@test "a taw on PATH that is a link to a working taw is taken" {
  loop_scene
  mkdir -p "$LOOP_HOME/hop1"
  ln -s "$LOOP_REAL" "$LOOP_HOME/hop1/taw"
  loop_load "$LOOP_HOME/hop1"
  [ "$(readlink "$LOOP_STATE/bin/taw")" = "$LOOP_HOME/hop1/taw" ]
  [ -x "$LOOP_STATE/bin/taw" ]
}
