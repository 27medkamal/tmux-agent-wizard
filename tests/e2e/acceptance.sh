#!/usr/bin/env bash
# shellcheck disable=SC2015  # `cond && ok || bad` is intentional; ok/bad always return 0
# Mechanical end-to-end acceptance run for tmux-agent-wizard, on a fully
# isolated tmux server. Exercises the feature set a user actually touches and
# self-checks every step, exiting non-zero if any check fails. Safe to run on a
# machine with a live tmux session: it never addresses the default socket.
#
#   tests/e2e/acceptance.sh            # run all checks, print a PASS/FAIL table
#
# This complements `mise run check` (unit + bats). It proves the wired-up
# binary + plugin behave end to end, which the unit suite cannot.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=tests/e2e/harness.sh
source "$HERE/harness.sh"

pass=0; fail=0
ok()   { pass=$((pass+1)); printf '  PASS  %s\n' "$1"; }
bad()  { fail=$((fail+1)); printf '  FAIL  %s\n' "$1"; [ -n "${2:-}" ] && printf '        %s\n' "$2"; }
# check <label> <expected> <actual>
check(){ [ "$2" = "$3" ] && ok "$1" || bad "$1" "expected [$2] got [$3]"; }
# jget <json> <key>  -> value of a top-level "key":"value" or "key":value
jget() { printf '%s' "$1" | sed -n "s/.*\"$2\":\"\{0,1\}\([^\",}]*\)\"\{0,1\}.*/\1/p" | head -1; }

echo "== tmux-agent-wizard acceptance =="
echo "repo=$E2E_REPO"
echo "taw=$TAW_BIN ($($TAW_BIN --version))"
echo "tmux=$(tmux -V)"
# Set before e2e_up: a run that fails while it comes up (its client not attached in time), or that is
# told to stop then, takes down what it has started all the same (e2e_down with nothing up does nothing).
trap 'e2e_down; tests_dir="$E2E_TMP"' EXIT
e2e_up accept || { echo "harness did not come up"; exit 1; }

echo "-- config / manifests / skill"
check "config path is json envelope" "cli:config:path" "$(jget "$(taw config path)" id)"
check "22 bundled manifests" "22" "$(taw manifest list | grep -o '"active_version"' | wc -l | tr -d ' ')"
taw manifest show claude | grep -q '"id":"claude"' && ok "manifest show claude" || bad "manifest show claude"
[ -n "$(taw skill | grep -i 'SKILL\|agent' | head -1)" ] && ok "skill prints markdown" || bad "skill prints markdown"
[ "$(taw config default | head -1 | cut -c1-1)" = "#" ] && ok "config default is raw toml" || bad "config default is raw toml"
# claude carries 6 taw-supplement rules on top of herdr's own and codex carries 1; every
# other bundled agent carries none (src/cli/manifest.rs's supplement_rules count).
claude_entry=$(taw manifest list | grep -oE '"id":"claude"[^}]*}')
check "manifest list: claude supplement_rules=6" "6" "$(jget "$claude_entry" supplement_rules)"
codex_entry=$(taw manifest list | grep -oE '"id":"codex"[^}]*}')
check "manifest list: codex supplement_rules=1" "1" "$(jget "$codex_entry" supplement_rules)"
taw manifest show claude | grep -q '"source":"taw"' && ok "manifest show claude: a rule sourced from taw" || bad "manifest show claude: a rule sourced from taw"

echo "-- notification open-target (next up)"
# Run before anything else creates an agent pane, so "no agents at all" is
# actually true rather than assumed. taw's own `main:0` pane is a bare shell
# (watched, never classified) so this server is genuinely agent-free here.
check "open-target with no agents -> not_found" "not_found" "$(jget "$(taw notification open-target 2>&1)" code)"
ot_pane=$(e2e_start_agent claude)
e2e_wait_opt "$ot_pane" @agent_kind claude 4000 >/dev/null
e2e_wait_opt "$ot_pane" @agent_state idle 7000 >/dev/null
tm select-window -t main:0 2>/dev/null                      # background it
ot_fpid=$(tm display-message -p -t "$ot_pane" '#{pane_pid}')
ot_apid=$(pgrep -P "$ot_fpid" -f claude 2>/dev/null | head -1); ot_apid="${ot_apid:-$ot_fpid}"
kill -USR2 "$ot_apid" 2>/dev/null
e2e_wait_opt "$ot_pane" @agent_state blocked 3000 >/dev/null
ot_out=$(taw notification open-target 2>&1)
check "open-target reports the group of the pane it went to: needs_you" "needs_you" "$(jget "$ot_out" target)"
check "open-target jumps to the blocked pane" "$(jget "$ot_out" pane_id)" "$(tm display-message -p -t main '#{pane_id}')"
tm kill-window -t "$ot_pane" 2>/dev/null   # keep later sections' agent census clean

echo "-- workspace / tab / pane CRUD"
mkdir -p "$E2E_TMP/accept-$$/home"
ws=$(taw workspace create --cwd "$E2E_TMP/accept-$$/home" 2>/dev/null)
[ -n "$(jget "$ws" id)" ] && ok "workspace create" || bad "workspace create" "$ws"
before=$(tm list-windows -t main | wc -l | tr -d ' ')
taw tab create --workspace w0 >/dev/null 2>&1
after=$(tm list-windows -t main | wc -l | tr -d ' ')
[ "$after" -gt "$before" ] && ok "tab create adds a window" || bad "tab create adds a window" "$before->$after"
p0=$(tm list-panes -t main -F '#{pane_id}' | head -1)
taw pane split --pane "$p0" --direction right >/dev/null 2>&1
[ "$(tm list-panes -t main | wc -l | tr -d ' ')" -ge 2 ] && ok "pane split" || bad "pane split"
check "pane layout resolves ids (no null)" "0" "$(taw pane layout w0:t0 2>/dev/null | grep -c '"pane_id":null')"
check "bad workspace id -> not_found" "not_found" "$(jget "$(taw workspace get w999 2>&1)" code)"

echo "-- watcher lifecycle (fixture via kind hint)"
pane=$(e2e_start_agent claude)
if e2e_wait_opt "$pane" @agent_kind claude 4000; then ok "agent detected (kind=claude)"; else bad "agent detected"; fi
e2e_wait_opt "$pane" @agent_state idle 7000 && ok "state idle" || bad "state idle" "$(tm display-message -p -t "$pane" '#{@agent_state} rule=#{@agent_rule}')"
fpid=$(tm display-message -p -t "$pane" '#{pane_pid}')
# fixture records its own pid; signal the process group leader running it
apid=$(pgrep -P "$fpid" -f claude 2>/dev/null | head -1); apid="${apid:-$fpid}"
kill -USR1 "$apid" 2>/dev/null; e2e_wait_opt "$pane" @agent_state working 3000 && ok "USR1 -> working" || bad "USR1 -> working"
# A turn's redraws caught half-way (a "done" while the agent still works): ALRM draws six
# 250 ms blank frames, each followed by 420 ms of the working screen, under the turn's own
# `✻` title, so a blank matches no rule. The pane is in the background and unseen, so a
# blank taken for the turn's end would be written `done` -- and toasted, with no delay, to
# the stub log. The burst is over 4.0 s after the fixture takes the signal (between its
# 0.2 s sleeps), so the 4.6 s window also keeps the USR2 below out of it.
printf '[ui.toast]\ndelivery = "system"\ndelay_seconds = 0\ndone_delay_seconds = 0\n' > "$TAW_CONFIG_DIR/config.toml"
: > "$E2E_NOTIFY_LOG"
kill -ALRM "$apid" 2>/dev/null
e2e_state_stays "$pane" working 4600 && ok "blank frames mid-turn keep working" \
  || bad "blank frames mid-turn keep working" "went to [$E2E_STATE_LEFT]; now $(tm display-message -p -t "$pane" '#{@agent_state} rule=#{@agent_rule}')"
check "blank frames mid-turn: no done toast" "0" "$(grep -c 'done' "$E2E_NOTIFY_LOG")"
rm -f "$TAW_CONFIG_DIR/config.toml"
kill -USR2 "$apid" 2>/dev/null; e2e_wait_opt "$pane" @agent_state blocked 3000 && ok "USR2 -> blocked" || bad "USR2 -> blocked"
taw watch status | grep -q '"watched"' && ok "watch status lists panes" || bad "watch status lists panes"

echo "-- watchers on an old taw: marked by watch status, started again by one command"
# The plugin's watcher on this pane wrote down which taw it runs, before its pid, and `watch
# status` finds it current. Taking the record away is what a watcher of a taw from before the
# record looks like: old, with the reason in words, and the command that starts it again.
ow_pid=$(tm display-message -p -t "$pane" '#{@agent_watch_pid}')
check "the watcher's record starts with its own pid" "$ow_pid" "$(tm display-message -p -t "$pane" '#{@agent_watch_build}' | cut -d' ' -f1)"
ow_status=$(taw watch status)
check "watch status: no watcher is old" "0" "$(jget "$ow_status" old)"
printf '%s' "$ow_status" | grep -q '"binary":"current"' && ok "watch status: a watcher on the installed taw is current" || bad "watch status: a watcher on the installed taw is current" "$ow_status"
tm set-option -p -u -t "$pane" @agent_watch_build
ow_status=$(taw watch status)
check "watch status: a watcher with no record is old" "1" "$(jget "$ow_status" old)"
printf '%s' "$ow_status" | grep -q '"binary_note":"started by an older taw' && ok "watch status says why, in words" || bad "watch status says why, in words" "$ow_status"
printf '%s' "$ow_status" | grep -q 'taw watch attach-all --restart' && ok "watch status names the command that starts it again" || bad "watch status names the command that starts it again" "$ow_status"
ow_out=$(taw watch attach-all --restart)
check "attach-all --restart starts the old watcher again" "1" "$(jget "$ow_out" restarted)"
check "attach-all --restart: no watcher failed to start again" "0" "$(jget "$ow_out" failed)"
ow_new=""; ow_i=0
while [ "$ow_i" -lt 50 ]; do
  ow_new=$(tm display-message -p -t "$pane" '#{@agent_watch_pid}')
  [ -n "$ow_new" ] && [ "$ow_new" != "$ow_pid" ] && [ -n "$(tm display-message -p -t "$pane" '#{@agent_watch_build}')" ] && break
  command sleep 0.2; ow_i=$((ow_i+1))
done
[ -n "$ow_new" ] && [ "$ow_new" != "$ow_pid" ] && ok "a new watcher is on the pane" || bad "a new watcher is on the pane" "pid was $ow_pid, now [$ow_new]"
check "the new watcher's record starts with its own pid" "$ow_new" "$(tm display-message -p -t "$pane" '#{@agent_watch_build}' | cut -d' ' -f1)"
check "watch status: nothing is old again" "0" "$(jget "$(taw watch status)" old)"

echo "-- notifications (stubbed by the harness; no real popups/sounds)"
printf '[ui.toast]\ndelivery = "system"\ndelay_seconds = 0\n' > "$TAW_CONFIG_DIR/config.toml"
# Re-attach so the watcher child picks up the config; the stub notify cmd is
# already in the server env from e2e_up.
taw watch detach "$pane" >/dev/null 2>&1; taw watch attach "$pane" >/dev/null 2>&1
: > "$E2E_NOTIFY_LOG"
tm select-window -t main:0 2>/dev/null            # background the agent pane
kill -USR1 "$apid" 2>/dev/null; e2e_wait_opt "$pane" @agent_state working 2500
kill -USR2 "$apid" 2>/dev/null                     # background blocked -> should notify
fired=0; i=0; while [ "$i" -lt 25 ]; do [ -s "$E2E_NOTIFY_LOG" ] && { fired=1; break; }; command sleep 0.2; i=$((i+1)); done
[ "$fired" = 1 ] && ok "system notification fired (stub log)" || bad "system notification fired" "no line in $E2E_NOTIFY_LOG"
rm -f "$TAW_CONFIG_DIR/config.toml"

echo "-- session save / restore"
taw session save >/dev/null 2>&1 && ok "session save" || bad "session save"
taw session list | grep -q '"' && ok "session list" || bad "session list"
taw session restore --dry-run >/dev/null 2>&1 && ok "session restore --dry-run" || bad "session restore --dry-run"

echo "-- worktree on a scratch repo"
repo="$E2E_TMP/wt-repo-$$"; mkdir -p "$repo"; ( cd "$repo" && git init -q && git -c user.email=e2e@example.invalid -c user.name=e2e commit -q --allow-empty -m init )
printf '[worktrees]\ndirectory = "%s/wt-out-$$"\n' "$E2E_TMP" > "$TAW_CONFIG_DIR/config.toml"
wsrepo=$(taw workspace create --cwd "$repo" 2>/dev/null); wid=$(jget "$wsrepo" workspace_id)
wt=$(taw worktree create --workspace "$wid" --branch feat/x 2>&1)
# A worktree's record, and not only an envelope, which an error has as well: the result's type, the
# branch, and a checkout of that branch at the path the record gives.
wt_path=$(jget "$wt" path)
[ "$(jget "$wt" type)" = "worktree_created" ] && [ "$(jget "$wt" branch)" = "feat/x" ] \
  && [ -n "$wt_path" ] && [ "$(git -C "$wt_path" rev-parse --abbrev-ref HEAD 2>/dev/null)" = "feat/x" ] \
  && ok "worktree create (slash branch)" || bad "worktree create (slash branch)" "$wt"
taw worktree list | grep -q 'feat' && ok "worktree list finds it" || bad "worktree list finds it"
check "worktree bad branch rejected" "invalid_argument" "$(jget "$(taw worktree create --workspace "$wid" --branch=-evil 2>&1)" code)"
rm -f "$TAW_CONFIG_DIR/config.toml"

echo "-- plugin (local link + action)"
plug="$E2E_TMP/echo-plugin-$$"; cp -R "$E2E_REPO/tests/fixtures/plugins/echo-plugin" "$plug" 2>/dev/null
if [ -d "$plug" ]; then
  taw plugin link "$plug" >/dev/null 2>&1 && ok "plugin link" || bad "plugin link"
  taw plugin list | grep -q echo && ok "plugin list shows it" || bad "plugin list shows it"
else bad "plugin fixture present" "no echo-plugin fixture"; fi

echo "-- remaining socket-method verbs"
taw session snapshot | grep -q '"id":"cli:session:snapshot"' && ok "session snapshot envelope" || bad "session snapshot envelope"
taw terminal title set "taw e2e title" >/dev/null 2>&1 && ok "terminal title set" || bad "terminal title set"
check "terminal title applied" "taw e2e title" "$(tm show-option -gv set-titles-string 2>/dev/null)"
taw terminal title clear >/dev/null 2>&1 && ok "terminal title clear" || bad "terminal title clear"
taw layout export w0:t0 | grep -q '"id":"cli:layout:export"' && ok "layout export envelope" || bad "layout export envelope"
taw config reload | grep -q '"id":"cli:config:reload"' && ok "config reload envelope" || bad "config reload envelope"
taw manifest reload | grep -q '"id":"cli:manifest:reload"' && ok "manifest reload envelope" || bad "manifest reload envelope"
printf '[keys]\nquit = "q"\n[ui]\nconfirm_close = true\n' > "$TAW_CONFIG_DIR/config.toml"
taw config reset-keys >/dev/null 2>&1; grep -q '^\[keys\]' "$TAW_CONFIG_DIR/config.toml" && bad "config reset-keys drops [keys]" || ok "config reset-keys drops [keys]"
grep -q 'confirm_close = true' "$TAW_CONFIG_DIR/config.toml" && ok "config reset-keys keeps other tables" || bad "config reset-keys keeps other tables"
rm -f "$TAW_CONFIG_DIR/config.toml"
check "popup close without popup -> no panic" "0" "$(taw popup close 2>&1 | grep -c 'panicked')"

echo "-- agent view projection"
taw agent view set --source "plugin:e2e" --label "e2e" --filter '{"op":"any","filters":[{"field":"status","op":"eq","value":"working"}]}' >/dev/null 2>&1 && ok "agent view set" || bad "agent view set" "$(taw agent view set --source plugin:e2e --label e2e --filter '{"op":"any","filters":[{"field":"status","op":"eq","value":"working"}]}' 2>&1 | head -c 200)"
tm show-option -gqv @agent_wizard_view | grep -q '"label":"e2e"' && ok "@agent_wizard_view written" || bad "@agent_wizard_view written" "$(tm show-option -gqv @agent_wizard_view | head -c 120)"
check "agent view set rejects empty any" "invalid_argument" "$(jget "$(taw agent view set --source plugin:e2e --filter '{"op":"any","filters":[]}' 2>&1)" code)"
taw agent view clear >/dev/null 2>&1; [ -z "$(tm show-option -gqv @agent_wizard_view)" ] && ok "agent view clear unsets" || bad "agent view clear unsets"

echo "-- integrations and pane history"
# `integration status` reads the config directory of every agent, each found by a variable of its own
# ($CLAUDE_CONFIG_DIR, $CODEX_HOME, ...) that a home of the run's alone would not keep from the runner's:
# so it runs with none of the runner's environment, in the home of the run's shells; and with the run's
# own tmux directory, which keeps any tmux it ran off the runner's server.
integrations=$(env -i PATH="$PATH" HOME="$DIR/shell-home" TMUX_TMPDIR="$TMUX_TMPDIR" "$TAW_BIN" integration status 2>&1)
check "18 integrations listed" "18" "$(printf '%s' "$integrations" | grep -o '"agent":' | wc -l | tr -d ' ')"
check "integration status looks for all 18 agents' config in the run's own home" "18" "$(printf '%s' "$integrations" | grep -o '"path":"[^"]*"' | grep -cF "\"path\":\"$DIR/shell-home/")"
printf '[experimental]\npane_history = true\n' > "$TAW_CONFIG_DIR/config.toml"
taw session save >/dev/null 2>&1; [ -n "$(find "$TAW_STATE_DIR/sessions" -name '*history*' 2>/dev/null | head -1)" ] && ok "pane_history writes session-history" || bad "pane_history writes session-history" "$(find "$TAW_STATE_DIR/sessions" -type f 2>/dev/null | tr '\n' ' ')"
rm -f "$TAW_CONFIG_DIR/config.toml"

echo "-- lifecycle events from native tmux actions (hook-sourced)"
if taw plugin list | grep -q echo; then
  before=$(find "$TAW_STATE_DIR" -path '*echo-plugin*' -name '*tab.created*' 2>/dev/null | wc -l | tr -d ' ')
  tm new-window -d -t main 2>/dev/null                      # plain tmux, no taw verb
  i=0; while [ "$i" -lt 25 ]; do
    after=$(find "$TAW_STATE_DIR" -path '*echo-plugin*' -name '*tab.created*' 2>/dev/null | wc -l | tr -d ' ')
    [ "$after" -gt "$before" ] && break; command sleep 0.2; i=$((i+1))
  done
  [ "${after:-0}" -gt "$before" ] && ok "native new-window fires tab.created to a plugin" || bad "native new-window fires tab.created" "no echo-plugin tab.created record under $TAW_STATE_DIR"
  victim=$(tm split-window -d -P -F '#{pane_id}' -t main 2>/dev/null); command sleep 1
  tm kill-pane -t "$victim" 2>/dev/null                      # hand kill -> pane.closed via the roster
  i=0; closed=0; while [ "$i" -lt 25 ]; do
    [ -n "$(find "$TAW_STATE_DIR" -path '*echo-plugin*' -name '*pane.closed*' 2>/dev/null | head -1)" ] && { closed=1; break; }; command sleep 0.2; i=$((i+1))
  done
  [ "$closed" = 1 ] && ok "hand kill-pane fires pane.closed to a plugin" || bad "hand kill-pane fires pane.closed" "no echo-plugin pane.closed record"
else
  bad "lifecycle event checks" "echo-plugin not linked"
fi

echo "-- ui open: the status line shows taw's bar while the popup is open, top and bottom"
# Earlier sections (workspace/worktree create) switch-client to sessions of their
# own; pin the attached client back to "main" so the status line has a known name
# to look for once it is back.
tm switch-client -t main 2>/dev/null
# A 120x40 client. The popup is full-size (the default 100% caps), so it runs frameless
# and covers every row but the status line's. In `sed -n 'Np'` screen lines, counted from
# 1 (the labels count tmux's rows from 0): with the line at the bottom the popup is lines
# 1-39 (tree border 1, hint 39) and the bar line 40; with it at the top the bar is line 1
# and the popup lines 2-40 (tree border 2, hint 40). The e2e tmux.conf sets no
# status-format of its own, so main's line is tmux's default, `[main] 0:...`, inherited
# from the global option.
outer resize-window -t host -x 120 -y 40 >/dev/null 2>&1
# Short display-time: `e2e_wait_screen` below typically catches a fired message
# within tens of ms, so 250ms is generous for that; it also needs to be comfortably
# under the gap to the next sub-case's own "does the status line read main's own text
# again" check, which is otherwise at the mercy of a stale message that has not yet
# expired.
tm set -g display-time 250

e2e_status_position top
e2e_ui_open; e2e_wait_popup_open
row0=$(screen | sed -n '1p'); row1=$(screen | sed -n '2p')
echo "$row0" | grep -q '^ taw ' && ok "top: row 0 is taw's bar" || bad "top: row 0 is taw's bar" "$row0"
echo "$row0" | grep -qE '[0-9]+ sessions?  ·  [0-9]+ agents?' && ok "top: the bar counts the sessions and the agents" || bad "top: the bar counts the sessions and the agents" "$row0"
echo "$row0" | grep -qE 'need|nothing|[⏸✓●▶⏵]|│' && bad "top: the bar holds no need words, no glyph and no rail (the border says what needs you)" "$row0" || ok "top: the bar holds no need words, no glyph and no rail (the border says what needs you)"
echo "$row0" | grep -qE '[0-9]+ agents? +[0-9]{2}:[0-9]{2} *$' && ok "top: the clock closes the bar on the right" || bad "top: the clock closes the bar on the right" "$row0"
echo "$row0" | grep -q 'main' && bad "top: the bar no longer names the session you came from" "$row0" || ok "top: the bar no longer names the session you came from"
echo "$row1" | grep -q '^╭ sessions' && ok "top: row 1 is the tree panel's border, no frame" || bad "top: row 1 is the tree panel's border, no frame" "$row1"
check "top: the hint line is the last row" "1" "$(screen | sed -n '40p' | grep -c '^ 1-9 session')"
check "top: no popup frame anywhere on screen" "0" "$(screen | grep -c '┌')"
tm display-message 'hello-taw'; e2e_wait_screen 'hello-taw' 800
check "top: display-message still draws on row 0 while the popup is open" "hello-taw" "$(screen | sed -n '1p')"
check "the session's saved record exists while the popup is open" "1" "$(tm show-options -t main -v @agent_wizard_status_saved 2>/dev/null | grep -c .)"
# Row 0 draws the bar from a session option rather than holding its text, so the popup's
# refreshes write only that option: one that lands after the restore draws nothing.
check "while the popup is open, main's row 0 draws the bar from its own option" '#{T:@agent_wizard_bar}' "$(tm show-options -t main -v 'status-format[0]' 2>/dev/null)"
tm show-options -t main -v @agent_wizard_bar 2>/dev/null | grep -q '^#\[fg=[^]]*,bold\] taw #\[default\]' && ok "that option holds taw's bar while the popup is open" || bad "that option holds taw's bar while the popup is open" "$(tm show-options -t main -v @agent_wizard_bar 2>&1)"
keys q; e2e_wait_popup_closed
e2e_wait_bar_restored main && ok "the saved record is gone after the popup closes" || bad "the saved record is gone after the popup closes" "$(tm show-options -t main -v @agent_wizard_status_saved 2>&1)"
check "the bar's option is gone with it" "0" "$(tm show-options -t main -v @agent_wizard_bar 2>/dev/null | grep -c .)"
check "main inherits its status line again: no status-format of its own" "0" "$(tm show-options -t main 2>/dev/null | grep -c 'status-format')"
e2e_wait_screen '^\[main\]' 2000
screen | sed -n '1p' | grep -q '^\[main\]' && ok "top: the status line has its own text back after q" || bad "top: the status line has its own text back after q" "$(screen | sed -n '1p')"

e2e_status_position bottom
e2e_ui_open; e2e_wait_popup_open
row40=$(screen | sed -n '40p'); row1=$(screen | sed -n '1p')
echo "$row40" | grep -q '^ taw ' && ok "bottom: the last row is taw's bar" || bad "bottom: the last row is taw's bar" "$row40"
echo "$row1" | grep -q '^╭ sessions' && ok "bottom: row 0 is the tree panel's border, no frame" || bad "bottom: row 0 is the tree panel's border, no frame" "$row1"
check "bottom: the hint line sits right above the bar" "1" "$(screen | sed -n '39p' | grep -c '^ 1-9 session')"
check "bottom: no popup frame anywhere on screen" "0" "$(screen | grep -c '┌')"
tm display-message 'hello-taw-2'; e2e_wait_screen 'hello-taw-2' 800
check "bottom: display-message still draws on the last row while the popup is open" "hello-taw-2" "$(screen | sed -n '40p')"
keys q; e2e_wait_popup_closed; e2e_wait_bar_restored main
e2e_wait_screen '^\[main\]' 2000
screen | sed -n '40p' | grep -q '^\[main\]' && ok "bottom: the status line has its own text back after q" || bad "bottom: the status line has its own text back after q" "$(screen | sed -n '40p')"

echo "-- ui open: a stale saved record from a dead launcher is restored on the next open"
# What a launcher killed while its popup was open leaves behind: a bar still set on main's
# own row 0, and the record saying main had inherited its line. Without the recovery the
# next open would save that bar as main's own row, and put it back again on close.
tm set -t main 'status-format[0]' 'STALE-BAR'
tm set -t main @agent_wizard_status_saved 'unset'
e2e_wait_screen '^STALE-BAR' 2000 && ok "the stale bar is on the status line before the next open" || bad "the stale bar is on the status line before the next open" "$(screen | sed -n '40p')"
e2e_ui_open; e2e_wait_popup_open
check "the next open puts taw's bar up in its place" "1" "$(screen | sed -n '40p' | grep -c '^ taw ')"
keys q; e2e_wait_popup_closed
e2e_wait_bar_restored main && ok "the record is gone after that popup closes" || bad "the record is gone after that popup closes" "$(tm show-options -t main -v @agent_wizard_status_saved 2>&1)"
check "main inherits its status line again: the stale row is gone" "0" "$(tm show-options -t main 2>/dev/null | grep -c 'status-format')"
e2e_wait_screen '^\[main\]' 2000 && ok "the status line reads main's own line again, not the stale bar" || bad "the status line reads main's own line again, not the stale bar" "$(screen | sed -n '40p')"

echo "-- ui open: a status line of the session's own comes back exactly as it was"
# The other half of the restore: main with a row 0 of its own. The bar replaces that row
# while the popup is open, and closing puts that very row back -- not tmux's default line,
# and not a blank one.
tm set -t main 'status-format[0]' 'MINE-ROW0'
e2e_wait_screen '^MINE-ROW0' 2000 && ok "main's own row is on the status line before the open" || bad "main's own row is on the status line before the open" "$(screen | sed -n '40p')"
e2e_ui_open; e2e_wait_popup_open
check "the bar replaces main's own row while the popup is open" "1" "$(screen | sed -n '40p' | grep -c '^ taw ')"
keys q; e2e_wait_popup_closed
e2e_wait_bar_restored main && ok "the record is gone after the popup closes" || bad "the record is gone after the popup closes" "$(tm show-options -t main -v @agent_wizard_status_saved 2>&1)"
check "main's own row 0 is set back to what it was" "MINE-ROW0" "$(tm show-options -t main -v 'status-format[0]' 2>/dev/null)"
e2e_wait_screen '^MINE-ROW0' 2000 && ok "the status line shows main's own row again" || bad "the status line shows main's own row again" "$(screen | sed -n '40p')"
# Whatever the checks above found, leave main inheriting tmux's default line again, with no
# record, for every later section. A popup still open is closed first, and a restore still
# in flight is waited out: either would otherwise write its row back after this cleanup.
popup_open && { taw popup close >/dev/null 2>&1; e2e_wait_popup_closed; }
e2e_wait_bar_restored main
tm set -u -t main status-format 2>/dev/null
tm set -u -t main @agent_wizard_status_saved 2>/dev/null

outer resize-window -t host -x 180 -y 45 >/dev/null 2>&1   # leave the shared server as found

echo "-- ui open: the bound key leaves no JSON behind in the origin pane"
# Through the plugin's real binding (prefix A), not e2e_ui_open: `display-popup`
# blocks until the popup closes, so `ui open`'s envelope prints only after the user
# has left -- and tmux paints run-shell output into the origin pane as a copy-mode
# view. Regression: the user landed back on `{"id":"cli:ui:open",...}`.
tm switch-client -t main 2>/dev/null
origin=$(tm display-message -p -t main '#{pane_id}')
# Start from a plain pane, so a view left by anything earlier cannot be mistaken for
# one this binding left.
tm send-keys -t "$origin" -X cancel 2>/dev/null
check "origin pane starts out of copy-mode" 0 "$(tm display-message -p -t "$origin" '#{pane_in_mode}')"
prefix A
if e2e_wait_popup_open 4000; then
  keys q
  # Bounded poll: the pane must come back plain (no view-mode), not merely "not yet".
  i=0; mode=1; while [ "$i" -lt 15 ]; do
    mode=$(tm display-message -p -t "$origin" '#{pane_in_mode}'); [ "$mode" = 0 ] && break
    command sleep 0.2; i=$((i+1))
  done
  [ "$mode" = 0 ] && ok "closing the popup opened by the bound key leaves the origin pane out of copy-mode" \
                  || bad "bound key leaves origin pane plain" "pane_in_mode=$mode mode=$(tm display-message -p -t "$origin" '#{pane_mode}')"
else
  bad "bound key opens the popup" "no tree panel border seen after prefix A"
fi

echo "-- session open: creates with hooks + zoxide, switches on re-open, collisions, not_found"
so_tmp="$E2E_TMP/session-open-$$"; mkdir -p "$so_tmp"
so_tmp="$(cd "$so_tmp" && pwd -P)"   # resolve symlinks (e.g. /tmp -> /private/tmp) up
                                     # front so it matches taw's own canonicalized path
mkdir -p "$so_tmp/oss/api" "$so_tmp/work/api"
so_hook_log="$so_tmp/hook.log"
cat > "$so_tmp/post-hook.sh" <<HOOK
#!/bin/sh
printf '%s %s\n' "\$1" "\$2" >> "$so_hook_log"
HOOK
chmod +x "$so_tmp/post-hook.sh"
so_fakebin="$so_tmp/fakebin"; mkdir -p "$so_fakebin"
so_zoxide_log="$so_tmp/zoxide.log"
cat > "$so_fakebin/zoxide" <<FAKEZOXIDE
#!/bin/sh
case "\$1" in
  add) printf 'add %s\n' "\$2" >> "$so_zoxide_log" ;;
  query) printf '/fake/zoxide/dir-a\n/fake/zoxide/dir-b\n' ;;
esac
FAKEZOXIDE
chmod +x "$so_fakebin/zoxide"
printf '[sessions]\nname = "directory"\npost_create_hook = "%s/post-hook.sh"\n' "$so_tmp" > "$TAW_CONFIG_DIR/config.toml"

so_out1=$(PATH="$so_fakebin:$PATH" taw session open "$so_tmp/work/api" 2>&1)
check "session open (work/api): created" "true" "$(jget "$so_out1" created)"
check "session open (work/api): session name" "api" "$(jget "$so_out1" session)"
check "post_create_hook recorded \"<session> <dir>\"" "api $so_tmp/work/api" "$(cat "$so_hook_log" 2>/dev/null)"
check "zoxide add recorded the new directory" "add $so_tmp/work/api" "$(cat "$so_zoxide_log" 2>/dev/null)"

so_out2=$(PATH="$so_fakebin:$PATH" taw session open "$so_tmp/work/api" 2>&1)
check "session open (work/api) again: not re-created" "false" "$(jget "$so_out2" created)"
check "no duplicate 'api' session" "1" "$(tm list-sessions -F '#{session_name}' | grep -c '^api$')"

so_out3=$(PATH="$so_fakebin:$PATH" taw session open "$so_tmp/oss/api" 2>&1)
check "session open (oss/api): collision suffix applied" "oss/api" "$(jget "$so_out3" session)"
check "'oss/api' session now exists" "1" "$(tm list-sessions -F '#{session_name}' | grep -c '^oss/api$')"

check "session open on a missing directory -> not_found" "not_found" "$(jget "$(PATH="$so_fakebin:$PATH" taw session open "$so_tmp/no-such-dir" 2>&1)" code)"
rm -f "$TAW_CONFIG_DIR/config.toml"

echo "-- session open: create hooks are command lines (flags, a name and a directory back, asked on every open, never read by a shell, stopped when they run too long)"
ch_tmp="$E2E_TMP/create-hooks-$$"; mkdir -p "$ch_tmp"
ch_tmp="$(cd "$ch_tmp" && pwd -P)"
# shellcheck disable=SC2016  # single quotes on purpose: a name a shell would act on, kept literal
ch_evil='a $(touch pwned) `touch pwned2` b ; c'
mkdir -p "$ch_tmp/work/ideas" "$ch_tmp/elsewhere" "$ch_tmp/work/notes/.git" "$ch_tmp/work/$ch_evil"
ch_asked="$ch_tmp/asked.log"
# A pre hook with a flag of its own that answers with two lines: a prefixed name, then a directory.
cat > "$ch_tmp/pre.sh" <<HOOK
#!/bin/sh
[ "\$1" = --prefix ] || exit 9
printf '%s\t%s\n' "\$3" "\$4" >> "$ch_asked"
echo "\$2\$3"
echo "$ch_tmp/elsewhere"
HOOK
chmod +x "$ch_tmp/pre.sh"
printf '[sessions]\nzoxide = false\npre_create_hook = "%s/pre.sh --prefix acc-"\n' "$ch_tmp" > "$TAW_CONFIG_DIR/config.toml"
ch_out1=$(taw session open "$ch_tmp/work/ideas" 2>&1)
check "pre hook with a flag names the session" "acc-ideas" "$(jget "$ch_out1" session)"
check "... which is created" "true" "$(jget "$ch_out1" created)"
check "the hook's second line is where the session runs" "$ch_tmp/elsewhere" "$(tm display-message -p -t '=acc-ideas:' '#{session_path}')"
ch_out2=$(taw session open "$ch_tmp/work/ideas" 2>&1)
check "opening it again finds the session the hook names" "acc-ideas" "$(jget "$ch_out2" session)"
check "... without creating another" "false" "$(jget "$ch_out2" created)"
check "the hook was asked on the second open as well" "2" "$(wc -l < "$ch_asked" | tr -d ' ')"
check "... with the name taw proposes and the directory picked" "$(printf 'ideas\t%s' "$ch_tmp/work/ideas")" "$(sed -n 2p "$ch_asked")"

# A directory whose name a shell would act on reaches the hook as those characters and nothing in it runs.
cat > "$ch_tmp/rec.sh" <<HOOK
#!/bin/sh
printf '%s\n%s\n' "\$1" "\$2" > "$ch_tmp/rec.log"
HOOK
chmod +x "$ch_tmp/rec.sh"
printf '[sessions]\nzoxide = false\npre_create_hook = "cd %s && %s/rec.sh"\n' "$ch_tmp" "$ch_tmp" > "$TAW_CONFIG_DIR/config.toml"
taw session open "$ch_tmp/work/$ch_evil" >/dev/null 2>&1
check "a directory name with a substitution, backticks and a semicolon reaches the hook as it is" "$ch_evil
$ch_tmp/work/$ch_evil" "$(cat "$ch_tmp/rec.log" 2>/dev/null)"
check "... and nothing in it ran" "0" "$(find "$ch_tmp" -maxdepth 1 -name 'pwned*' | wc -l | tr -d ' ')"

# The wizard's own project-layout example, unchanged, as a post hook: windows for a new session, none for a reuse.
printf '[sessions]\nzoxide = false\npost_create_hook = "TMUX='"'"'%s,0,0'"'"' '"'"'%s/tests/fixtures/session-wizard-hooks/project-layout-hook.sh'"'"'"\n' "$(tm display-message -p '#{socket_path}')" "$E2E_REPO" > "$TAW_CONFIG_DIR/config.toml"
taw session open "$ch_tmp/work/notes" >/dev/null 2>&1
check "the wizard's layout example builds its windows for a new session" "editor shell git" "$(tm list-windows -t '=notes' -F '#{window_name}' | tr '\n' ' ' | sed 's/ $//')"
taw session open "$ch_tmp/work/notes" >/dev/null 2>&1
check "... and none more when the session is reused" "3" "$(tm list-windows -t '=notes' | wc -l | tr -d ' ')"

# A pre hook that exits non-zero stops the open, even of a directory that already has a session.
# The hook answers in a moment; its time limit is a ceiling here, long enough that a machine that
# stalls (this check failed once under load, with the text of the refusal not in the output, and
# the output was not kept) cannot turn the refusal into a time-out. A check that fails shows the
# whole output.
printf '#!/bin/sh\necho "not today" >&2\nexit 1\n' > "$ch_tmp/refuse.sh"; chmod +x "$ch_tmp/refuse.sh"
printf '[sessions]\nzoxide = false\npre_create_hook = "%s/refuse.sh"\nhook_timeout_seconds = 60\n' "$ch_tmp" > "$TAW_CONFIG_DIR/config.toml"
ch_out3=$(taw session open "$ch_tmp/work/notes" 2>&1)
case "$ch_out3" in
  *"pre-create hook "*" failed (exit status: 1): not today"*) ok "a refusing pre hook stops the open of a running directory, saying which hook and why" ;;
  *) bad "a refusing pre hook stops the open of a running directory, saying which hook and why" "$ch_out3" ;;
esac
check "... with the error code hook_refused" "hook_refused" "$(jget "$ch_out3" code)"
mkdir -p "$ch_tmp/work/refused"
taw session open "$ch_tmp/work/refused" >/dev/null 2>&1
check "... and one that would create a session creates nothing" "0" "$(tm list-sessions -F '#{session_name}' | grep -c '^refused$')"

# A pre hook that runs past [sessions] hook_timeout_seconds is stopped, and refuses the open.
printf '#!/bin/sh\nsleep 30\n' > "$ch_tmp/slow.sh"; chmod +x "$ch_tmp/slow.sh"
printf '[sessions]\nzoxide = false\npre_create_hook = "%s/slow.sh"\nhook_timeout_seconds = 1\n' "$ch_tmp" > "$TAW_CONFIG_DIR/config.toml"
mkdir -p "$ch_tmp/work/slow"
ch_start=$(date +%s)
ch_out4=$(taw session open "$ch_tmp/work/slow" 2>&1)
ch_took=$(( $(date +%s) - ch_start ))
check "a pre hook that runs past hook_timeout_seconds refuses the open, saying it took too long" "pre-create hook took longer than 1 s" "$(jget "$ch_out4" message)"
check "... with the error code hook_refused" "hook_refused" "$(jget "$ch_out4" code)"
[ "$ch_took" -lt 10 ] && ok "... and comes back after the limit, not after the hook's 30 seconds" || bad "... comes back after the limit" "took ${ch_took}s"
check "... and creates nothing" "0" "$(tm list-sessions -F '#{session_name}' | grep -c '^slow$')"
rm -f "$TAW_CONFIG_DIR/config.toml"
tm kill-session -t '=acc-ideas' 2>/dev/null; tm kill-session -t '=notes' 2>/dev/null; tm kill-session -t "=$ch_evil" 2>/dev/null
tm switch-client -t main 2>/dev/null
rm -rf "$ch_tmp"

echo "-- done-class notification delay (working -> idle while backgrounded = done)"
# promote_done (src/watch/state.rs) only turns a working->idle edge into the `done`
# state when the pane is backgrounded and not yet seen -- so this drives the fixture
# in the background throughout, exactly like the "notifications" section above.
dd_delay=2
printf '[ui.toast]\ndelivery = "system"\ndelay_seconds = 0\ndone_delay_seconds = %s\n' "$dd_delay" > "$TAW_CONFIG_DIR/config.toml"
dd_pane=$(e2e_start_agent claude)
e2e_wait_opt "$dd_pane" @agent_kind claude 4000 >/dev/null
e2e_wait_opt "$dd_pane" @agent_state idle 7000 >/dev/null
dd_fpid=$(tm display-message -p -t "$dd_pane" '#{pane_pid}')
dd_apid=$(pgrep -P "$dd_fpid" -f claude 2>/dev/null | head -1); dd_apid="${dd_apid:-$dd_fpid}"
tm select-window -t main:0 2>/dev/null   # background it before the first working->idle edge
: > "$E2E_NOTIFY_LOG"

kill -USR1 "$dd_apid" 2>/dev/null   # idle -> working
e2e_wait_opt "$dd_pane" @agent_state working 3000 >/dev/null
kill -USR1 "$dd_apid" 2>/dev/null   # working -> idle, backgrounded+unseen -> promotes to "done"
e2e_wait_opt "$dd_pane" @agent_state "done" 3000 >/dev/null
kill -USR1 "$dd_apid" 2>/dev/null   # quick revert, well inside the delay window
e2e_wait_opt "$dd_pane" @agent_state working 3000 >/dev/null
command sleep $((dd_delay + 3))     # comfortably past the original delay's recheck
check "no done toast after a quick revert" "0" "$(wc -l < "$E2E_NOTIFY_LOG" | tr -d ' ')"

kill -USR1 "$dd_apid" 2>/dev/null   # working -> done again, no revert this time
e2e_wait_opt "$dd_pane" @agent_state "done" 3000 >/dev/null
command sleep $((dd_delay + 3))
check "exactly one done toast after the full delay" "1" "$(grep -c 'done' "$E2E_NOTIFY_LOG")"
rm -f "$TAW_CONFIG_DIR/config.toml"
tm kill-window -t "$dd_pane" 2>/dev/null   # keep later sections' agent census clean

echo "-- done is decided when the turn ends: a glance earlier in it does not count"
# herdr's completion rule. You look at the agent mid-turn (its window selected in
# the attached session: tmux's focus hooks run `taw seen`), go back to your own window,
# and the turn ends there. Out of your sight the finish is `done`, with its toast.
printf '[ui.toast]\ndelivery = "system"\ndelay_seconds = 0\ndone_delay_seconds = 0\n' > "$TAW_CONFIG_DIR/config.toml"
gl_pane=$(e2e_start_agent claude)
e2e_wait_opt "$gl_pane" @agent_kind claude 4000 >/dev/null
e2e_wait_opt "$gl_pane" @agent_state idle 7000 >/dev/null
gl_fpid=$(tm display-message -p -t "$gl_pane" '#{pane_pid}')
gl_apid=$(pgrep -P "$gl_fpid" -f claude 2>/dev/null | head -1); gl_apid="${gl_apid:-$gl_fpid}"
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
kill -USR1 "$gl_apid" 2>/dev/null                 # a new turn, which starts unseen
e2e_wait_opt "$gl_pane" @agent_state working 3000 >/dev/null
tm select-window -t "$gl_pane" 2>/dev/null        # the glance
e2e_wait_opt "$gl_pane" @agent_seen 1 3000 && ok "a glance at a working agent marks it seen" \
  || bad "a glance at a working agent marks it seen" "@agent_seen=[$(tm display-message -p -t "$gl_pane" '#{@agent_seen}')]"
tm select-window -t main:0 2>/dev/null            # back to their own window
: > "$E2E_NOTIFY_LOG"
kill -USR1 "$gl_apid" 2>/dev/null                 # the turn ends while they are away
e2e_wait_opt "$gl_pane" @agent_state "done" 3000 && ok "a turn that ends while you are away is done, glance or not" \
  || bad "a turn that ends while you are away is done, glance or not" "$(tm display-message -p -t "$gl_pane" '#{@agent_state} seen=#{@agent_seen}')"
gl_toast=0; i=0; while [ "$i" -lt 25 ]; do grep -q 'done' "$E2E_NOTIFY_LOG" && { gl_toast=1; break; }; command sleep 0.2; i=$((i+1)); done
check "... and it earns its done toast" "1" "$gl_toast"
rm -f "$TAW_CONFIG_DIR/config.toml"
tm kill-window -t "$gl_pane" 2>/dev/null   # keep later sections' agent census clean

echo "-- hooks: [hooks] runs a command when an agent finishes and when taw notifies, and pre_close can refuse"
# Three things a person configures: a command on agent.done, a command on every notification
# (phone push is this), and a pre_close that says no. Each hook writes a file this run reads,
# and every file, window and session made here is removed at the end.
hk_out="$E2E_TMP/hooks-$$"; mkdir -p "$hk_out"
hk_out="$(cd "$hk_out" && pwd -P)"
hk_delay=4
cat > "$hk_out/on-done.sh" <<HOOK
#!/bin/sh
printf '%s|%s|%s|%s|%s|%s\n' "\$TAW_EVENT" "\$TAW_STATE" "\$TAW_PREVIOUS_STATE" "\$TAW_AGENT" "\$TAW_SESSION" "\$TAW_PANE" > "$hk_out/agent-done.txt"
printf '%s|%s|%s\n' "\$TAW_E2E_HOOK_TOPIC" "\$TAW_E2E_HOOK_LAYER" "\$TMUX" > "$hk_out/agent-done-env.txt"
HOOK
cat > "$hk_out/on-notification.sh" <<HOOK
#!/bin/sh
echo "\$TAW_KIND|\$TAW_MESSAGE" >> "$hk_out/notification.txt"
HOOK
chmod +x "$hk_out/on-done.sh" "$hk_out/on-notification.sh"
# A done toast that waits $hk_delay seconds, delivered to the harness's stub.
printf '[ui.toast]\ndelivery = "system"\ndelay_seconds = 0\ndone_delay_seconds = %s\n\n[hooks]\n"agent.done" = "%s/on-done.sh"\nnotification = "%s/on-notification.sh"\n' \
  "$hk_delay" "$hk_out" "$hk_out" > "$TAW_CONFIG_DIR/config.toml"
hk_pane=$(e2e_start_agent claude)
e2e_wait_opt "$hk_pane" @agent_kind claude 4000 >/dev/null
e2e_wait_opt "$hk_pane" @agent_state idle 7000 >/dev/null
hk_fpid=$(tm display-message -p -t "$hk_pane" '#{pane_pid}')
hk_apid=$(pgrep -P "$hk_fpid" -f claude 2>/dev/null | head -1); hk_apid="${hk_apid:-$hk_fpid}"
# The agent's watcher is running, and it is a pipe-pane child: its own environment is the one the
# server was launched with. What a person sets afterwards with set-environment -g (a push topic),
# and what the session has of its own (the ssh agent of the terminal it was attached from), is
# what the hook is to see all the same.
tm set-environment -g TAW_E2E_HOOK_TOPIC "hk-topic-$$"
tm set-environment -g TAW_E2E_HOOK_LAYER "from-the-global-environment"
tm set-environment -t main TAW_E2E_HOOK_LAYER "from-the-session"
tm select-window -t main:0 2>/dev/null   # background it, so the finish is done and unseen
: > "$E2E_NOTIFY_LOG"
kill -USR1 "$hk_apid" 2>/dev/null   # idle -> working
e2e_wait_opt "$hk_pane" @agent_state working 3000 >/dev/null
kill -USR1 "$hk_apid" 2>/dev/null   # working -> idle, backgrounded and unseen: done
e2e_wait_opt "$hk_pane" @agent_state "done" 3000 >/dev/null
# The notification hook goes with the toast, so it waits out the same delay: nothing yet.
[ ! -e "$hk_out/notification.txt" ] && ok "the notification hook has not run when the agent finishes, its toast waits $hk_delay s" \
  || bad "the notification hook has not run when the agent finishes" "$(cat "$hk_out/notification.txt")"
# The agent.done hook is not the toast's: it runs at once.
hk_wait_file() { local file="$1" n=0; while [ ! -s "$file" ] && [ "$n" -lt "$2" ]; do command sleep 0.1; n=$((n+1)); done; }
hk_wait_file "$hk_out/agent-done.txt" 100
check "agent.done runs its command, told the event, states, agent, session and pane" \
  "agent.done|done|working|claude|main|$hk_pane" "$(cat "$hk_out/agent-done.txt" 2>/dev/null)"
hk_wait_file "$hk_out/agent-done-env.txt" 100
hk_env=$(cat "$hk_out/agent-done-env.txt" 2>/dev/null)
check "the watcher's hook sees a variable set with set-environment -g after the watcher started" "hk-topic-$$" "$(printf '%s' "$hk_env" | cut -d'|' -f1)"
check "... and the session's own variable over the global one of its name" "from-the-session" "$(printf '%s' "$hk_env" | cut -d'|' -f2)"
hk_session_no=$(tm display-message -p -t main '#{session_id}'); hk_session_no="${hk_session_no#\$}"
check "... and TMUX names this server and the session the event is about" \
  "$(tm display-message -p '#{socket_path}'),$(tm display-message -p '#{pid}'),$hk_session_no" "$(printf '%s' "$hk_env" | cut -d'|' -f3)"
tm set-environment -gu TAW_E2E_HOOK_TOPIC; tm set-environment -gu TAW_E2E_HOOK_LAYER; tm set-environment -t main -u TAW_E2E_HOOK_LAYER
hk_wait_file "$hk_out/notification.txt" 200
case "$(cat "$hk_out/notification.txt" 2>/dev/null)" in
  "done|claude done "*) ok "the notification hook runs after the delay, a done, with the toast's text" ;;
  *) bad "the notification hook runs after the delay, a done, with the toast's text" "[$(cat "$hk_out/notification.txt" 2>/dev/null)]" ;;
esac
check "... and the toast came with it" "1" "$(grep -c 'done' "$E2E_NOTIFY_LOG")"
command sleep 1
check "... once for the one toast" "1" "$(wc -l < "$hk_out/notification.txt" | tr -d ' ')"
tm kill-window -t "$hk_pane" 2>/dev/null   # keep later sections' agent census clean

# pre_close: a refusal keeps a session open, and the same close goes through once it agrees.
tm new-session -d -s hk-keep -c "$hk_out"
cat > "$TAW_CONFIG_DIR/config.toml" <<'CONF'
[hooks]
pre_close = "echo 'agent is still working' >&2; exit 1"
CONF
hk_close=$(taw workspace close hk-keep 2>&1)
check "a pre_close refusal is the error code hook_refused" "hook_refused" "$(jget "$hk_close" code)"
case "$hk_close" in
  *"pre-close hook"*"agent is still working"*) ok "... naming the hook and saying why" ;;
  *) bad "... naming the hook and saying why" "$hk_close" ;;
esac
check "... and the session is still open" "1" "$(tm list-sessions -F '#{session_name}' | grep -c '^hk-keep$')"
printf '[hooks]\npre_close = "exit 0"\n' > "$TAW_CONFIG_DIR/config.toml"
taw workspace close hk-keep >/dev/null 2>&1
check "the same close goes through once the hook agrees" "0" "$(tm list-sessions -F '#{session_name}' | grep -c '^hk-keep$')"

# taw hooks list / test / log.
printf '[hooks]\n"agent.done" = "exit 0"\n"tab.focused" = "echo boom >&2; exit 5"\n' > "$TAW_CONFIG_DIR/config.toml"
taw hooks list | grep -q '"name":"agent.done"[^}]*"commands":\["exit 0"\]' && ok "hooks list shows what is hooked to agent.done" \
  || bad "hooks list shows what is hooked to agent.done" "$(taw hooks list | head -c 300)"
hk_test=$(taw hooks test agent.done)
check "hooks test runs the hook of an event" "0" "$(jget "$hk_test" status)"
check "... with the sample's state" "done" "$(jget "$hk_test" TAW_STATE)"
# The sample's ids name nothing on any server, so a hook that acts on them cannot touch a pane of yours.
check "... whose pane is one no server has" "%999999" "$(jget "$hk_test" TAW_PANE)"
# tmux reads a session name in a target as the start of a name when no session has exactly it, so the
# sample's session is named as exactly that session: `=` for the name, `:` because a command that takes a
# window reads a bare name as a window's first and then as the start of a session's.
check "... and whose session is named as exactly that session" "=taw-hooks-test:" "$(jget "$hk_test" TAW_SESSION)"
check "... and the server really has no such pane" "0" "$(tm list-panes -a -F '#{pane_id}' | grep -c '^%999999$')"
check "... and TAW_TEST says it is a test" "1" "$(jget "$hk_test" TAW_TEST)"
# Every event is told all three ids, the ones a real event has none for too: an empty tmux target is
# the current pane, which a test must never act on.
hk_test_ws=$(taw hooks test workspace.renamed)
check "hooks test tells an event with no pane of its own a pane no server has" "%999999" "$(jget "$hk_test_ws" TAW_PANE)"
check "... and a window no server has" "@999999" "$(jget "$hk_test_ws" TAW_WINDOW)"
# --pane tells the event about a real pane instead: its own id, and the output says so.
hk_real=$(tm display-message -p -t main:0 '#{pane_id}')
hk_test_real=$(taw hooks test agent.done --pane "$hk_real")
check "hooks test --pane uses that real pane's own id" "$hk_real" "$(jget "$hk_test_real" TAW_PANE)"
check "... and says it used a real pane" "$hk_real" "$(jget "$hk_test_real" real_pane)"
# An event about something going away is refused on a real pane unless --force: stderr, status 2, nothing run.
taw hooks test pre_close --pane "$hk_real" >/dev/null 2>"$hk_out/refused.txt"; hk_rc=$?
check "hooks test --pane refuses an event about something going away without --force" "2" "$hk_rc"
grep -q "add --force if that is what you want" "$hk_out/refused.txt" && ok "... saying how to go on" \
  || bad "... saying how to go on" "$(cat "$hk_out/refused.txt")"
taw plugin event tab.focused --tab "$(tm display-message -p -t main '#{window_id}')" --workspace "$(tm display-message -p -t main '#{session_id}')" >/dev/null 2>&1
hk_logged=0; hk_i=0; while [ "$hk_i" -lt 100 ]; do taw hooks log | grep -q 'tab.focused failed (exit status 5)' && { hk_logged=1; break; }; command sleep 0.1; hk_i=$((hk_i+1)); done
check "a hook that failed in the background is in taw hooks log" "1" "$hk_logged"
# A session of yours whose name only begins like the sample's is out of reach of a hook that acts on it,
# as a session or as a window (which is the last window it has).
tm new-session -d -s taw-hooks-testing -c "$hk_out"
# The hook names the harness's server itself (its shell has the name from taw's environment): a tmux
# with no -L goes to the server in $TMUX, else to the default one, which is never to be this suite's.
cat > "$TAW_CONFIG_DIR/config.toml" <<'CONF'
[hooks]
"workspace.renamed" = 'tmux -L "$TAW_TMUX_SOCKET_NAME" rename-session -t "$TAW_SESSION" renamed-by-a-hook; tmux -L "$TAW_TMUX_SOCKET_NAME" kill-window -t "$TAW_SESSION"'
CONF
hk_look=$(taw hooks test workspace.renamed 2>&1)
check "a hook that renames the sample's session or closes its window leaves a session of yours that begins the same way alone" "1" "$(tm list-sessions -F '#{session_name}' | grep -c '^taw-hooks-testing$')"
# ... which proves nothing unless the hook did reach tmux: what it says is what tmux answered.
case "$hk_look" in
  *"can't find session"*) ok "... and the hook did reach tmux, which found no session of that name" ;;
  *) bad "... and the hook did reach tmux, which found no session of that name" "$(printf '%s' "$hk_look" | head -c 300)" ;;
esac
tm kill-session -t '=taw-hooks-testing' 2>/dev/null
rm -f "$TAW_CONFIG_DIR/config.toml"
rm -rf "$hk_out"

echo "-- replay: every bundled manifest resolves @agent_kind from its hint alone"
# tests/fixtures/fake-agents/replay is a POSIX sh script; #{pane_current_command}
# reads "sh" for it, which names no bundled manifest, so this only passes if identity
# truly comes from @agent_kind_hint (src/detect/identity.rs), not from the process name.
# Strip the envelope's own top-level "id" (e.g. "cli:manifest:list") before
# extracting, so only each manifest object's "id" is picked up.
mapfile -t rp_ids_arr < <(taw manifest list | sed 's/.*"manifests"://' | grep -oE '"id":"[^"]+"' | sed -E 's/"id":"([^"]+)"/\1/')
if [ "${#rp_ids_arr[@]}" -eq 0 ]; then
  # Guards against a silent pass: an empty id list would otherwise skip the loop
  # below entirely and still report "resolved for all 0 bundled manifests".
  bad "replay: manifest id list is non-empty" "taw manifest list produced no ids to replay"
else
  declare -a rp_panes=()
  for rp_id in "${rp_ids_arr[@]}"; do
    rp_panes+=("$(e2e_start_replay "$rp_id")")
  done
  rp_fail=0
  for rp_i in "${!rp_panes[@]}"; do
    if ! e2e_wait_opt "${rp_panes[$rp_i]}" @agent_kind "${rp_ids_arr[$rp_i]}" 5000; then
      rp_fail=$((rp_fail + 1))
      bad "replay: @agent_kind resolves to '${rp_ids_arr[$rp_i]}'" \
        "got '$(tm display-message -p -t "${rp_panes[$rp_i]}" '#{@agent_kind}')'"
    fi
  done
  [ "$rp_fail" -eq 0 ] && ok "replay: @agent_kind resolved for all ${#rp_panes[@]} bundled manifests"
  for rp_win in "${rp_panes[@]}"; do tm kill-window -t "$rp_win" 2>/dev/null; done
fi

echo "-- popup: attention walk, filter, confirmations, help, toast, prefix chord"
# What the popup may not show: an em-dash, an en-dash, an arrow and the four
# curly quotes, and their look-alikes: a horizontal bar, a minus sign, a
# double arrow and a heavy round-tipped arrow. An extended regex, written as the characters'
# UTF-8 bytes, so that this file does not hold the characters it looks for.
pp_banned() { printf '\342\200\224|\342\200\223|\342\206\222|\342\200\230|\342\200\231|\342\200\234|\342\200\235|\342\200\225|\342\210\222|\342\207\222|\342\236\234'; }
# One agent blocked+unseen in "main" (the tree's one hot row), one plain session
# with no agent, so the tree has both a header with an agent and one without.
# The "watcher lifecycle" / "notifications" sections above leave their own
# claude fixture ($pane) alive and blocked, AND (see the report's "concerns" --
# reproducible, cause not isolated) some run of this suite leaves a second,
# unrelated pane carrying @agent_kind=claude too, most often inside the
# worktree section's scratch session. Sweep every existing agent-kind pane
# server-wide first, so "⏸1 needs you" and every "the only claude row"
# assertion below is about *this* section's one agent, not however many
# leftover ones happen to still be around.
while read -r pp_stray; do
  [ -n "$pp_stray" ] && tm kill-window -t "$pp_stray" 2>/dev/null
done < <(tm list-panes -a -F '#{pane_id} #{@agent_kind}' | awk 'NF>1{print $1}')
outer resize-window -t host -x 120 -y 40 >/dev/null 2>&1
tm switch-client -t main 2>/dev/null
pp_pane=$(e2e_start_agent claude)
e2e_wait_opt "$pp_pane" @agent_kind claude 4000 >/dev/null
e2e_wait_opt "$pp_pane" @agent_state idle 7000 >/dev/null
pp_fpid=$(tm display-message -p -t "$pp_pane" '#{pane_pid}')
pp_apid=$(pgrep -P "$pp_fpid" -f claude 2>/dev/null | head -1); pp_apid="${pp_apid:-$pp_fpid}"
tm select-window -t main:0 2>/dev/null
kill -USR2 "$pp_apid" 2>/dev/null
e2e_wait_opt "$pp_pane" @agent_state blocked 3000 >/dev/null
tm new-session -d -s pp-other -c "$DIR/home" 2>/dev/null
tm switch-client -t main 2>/dev/null

# By this point in the run the live tmux state (several sessions and windows
# from every section above) is much bigger than in a fresh popup: the panels'
# borders draw at once, but the tree's own content -- built from a snapshot of
# everything -- can still be populating a moment later. Poll for the hint line
# specifically (only present once the first real render is in), not just a
# border, before trusting any row's content.
#
# Rows below are the 120x40 client's, status line at the bottom: the full-size
# popup has no frame, so the tree panel's top border (its
# title and the "needs you" chip) is row 1, its bottom border (the view chip)
# row 38, the hint line row 39, and the status line -- taw's bar while the
# popup is open -- row 40.
e2e_ui_open
e2e_wait_screen 'Tab next up' 3000

echo "-- popup: 1) initial render (title, chip, cursor, hint)"
pp_title_row=$(screen | sed -n '1p')
echo "$pp_title_row" | grep -q ' sessions ' && ok "tree panel title ' sessions '" || bad "tree panel title ' sessions '" "$pp_title_row"
echo "$pp_title_row" | grep -q '⏸1 needs you' && ok "border chip '⏸1 needs you'" || bad "border chip '⏸1 needs you'" "$pp_title_row"
# What needs you is counted by kind, each group in its state's own colour: the blocked
# agent is the attention colour, #fab387 (250;179;135), bold; the words are the text colour, #cdd6f4
# (205;214;244), not bold. The border's row is the first of the coloured capture.
pp1_chip=$(screenc | sed -n '1p')
check "the chip's blocked group is the attention colour, bold" "250;179;135 1" "$(e2e_style_at "$pp1_chip" '⏸1')"
check "the chip's words are the text colour, not bold" "205;214;244 0" "$(e2e_style_at "$pp1_chip" 'needs you')"
# The popup opens from main:0's shell, which depth "agents" never draws, so the
# cursor opens on main's own header row -- not on the blocked agent folded
# beneath it (the popup always opens on your own row).
pp_cursor=$(e2e_popup_cursor_row)
echo "$pp_cursor" | grep -qE '^[│ ]*[0-9]? [* -] main\b' \
  && ok "cursor opens on main's header, the pane it was opened from is folded" \
  || bad "cursor opens on main's header, the pane it was opened from is folded" "$pp_cursor"
pp_hint=$(screen | sed -n '39p')
pp_hint_ok=0
for pp_tok in '1-9' 'Tab' 'next up' 'Enter go' '?'; do
  echo "$pp_hint" | grep -qF "$pp_tok" || pp_hint_ok=1
done
[ "$pp_hint_ok" -eq 0 ] && ok "hint line names 1-9, Tab, next up, Enter go, ?" || bad "hint line names 1-9, Tab, next up, Enter go, ?" "$pp_hint"

echo "-- popup: 2) Tab / Shift-Tab keep the highlight with one agent that needs you"
# There is nothing new to poll *for* here -- the assertion is that the cursor
# row never changes -- so a single sleep-then-check could pass on nothing but
# the key not having been processed yet. watch_cursor_unchanged instead samples
# e2e_popup_cursor_row continuously across a bounded window and fails the
# instant it ever differs, only passing once the whole window has elapsed
# with no change observed.
watch_cursor_unchanged() {
  local label="$1" before="$2" window_ms="${3:-800}"; local waited=0 now
  while [ "$waited" -lt "$window_ms" ]; do
    command sleep 0.1; waited=$((waited + 100))
    now=$(e2e_popup_cursor_row)
    [ "$now" != "$before" ] && { bad "$label" "moved to: $now"; return; }
  done
  ok "$label"
}
# The cursor opens on main's header (check 1); move it onto the one hot agent
# row first so both probes below watch the agent row itself, not the header.
keys Tab; e2e_wait_cursor 'claude' 1500
pp_before=$(e2e_popup_cursor_row)
keys Tab
watch_cursor_unchanged "Tab stays on the only hot agent" "$pp_before"
keys BTab
watch_cursor_unchanged "Shift-Tab stays on the only hot agent" "$pp_before"

echo "-- popup: 3) / filter narrows the tree, Esc restores it"
keys /
e2e_wait_screen 'search…' 1500
# A number is yellow exactly while its key works, and typing it in the
# filter takes the cursor to its session, so with / open main's number stays yellow and bold
# (the default theme's #f9e2af); a digit is still text, so the hint line names no number key.
check "with / open main's number stays yellow and bold: typing it reaches main" "249;226;175 1" "$(e2e_number_style main)"
check "and the hint line names no number key" "0" "$(screen | sed -n '39p' | grep -c '1-9 session')"
# Typed with fzf's ^, the prefix anchor: it anchors the session's own name, as a row is also matched
# without its label in front, so only the session named so is left; a bare pp-other
# could match other rows letter by letter. A header's folder is not matched.
keys -l "^pp-other"
e2e_wait_screen '^ /\^pp-other▏' 2000 || bad "check 3: the query reads ^pp-other" "$(screen | sed -n '39p')"
# While the filter has focus a letter is text for the query, so the hint line names only the
# keys that work there: no j/k, / or ?.
check "the filter's hint line names only the keys that work in it" " /^pp-other▏  ·  4a session 4 row a  ·  Esc clear  ·  Tab next up  ·  ↑/↓ move  ·  Enter go" "$(screen | sed -n '39p' | sed 's/ *$//')"
# The word claude is counted in the tree's rows only, each cut at the preview panel's left
# border (`││`): the preview shows the pane's own screen and directory, and a temp path with
# claude in it, a scratchpad say, would count there and fail these checks.
pp_tree_claude() { screen | grep '^│' | sed 's/││.*//' | grep -c claude; }
check "filter hides the non-matching session's agent" "0" "$(pp_tree_claude)"
# fzf's own keys move through what the filter left, beside ctrl-j and ctrl-k:
# ctrl-n down one row, ctrl-p back up. "'main" is matched by main's header and the rows under
# it, which start with its name (a header's folder is not matched, so the query
# "main home" matches nothing). The lines are the
# screen's: the tree's rows start on line 2, right under its border, and the highlighted one
# is found with e2e_popup_cursor_line.
pp3_wait_at() {
  local want="$1" waited=0
  while [ "$waited" -lt 2000 ]; do
    [ "$(e2e_popup_cursor_line)" = "$want" ] && return 0
    command sleep 0.1; waited=$((waited + 100))
  done
  return 1
}
keys C-u
keys -l "'main"
e2e_wait_screen "^ /'main▏" 2000 || bad "check 3: the query reads 'main" "$(screen | sed -n '39p')"
pp3_n0=$(e2e_popup_cursor_line)
if [ -z "$pp3_n0" ]; then
  bad "the filter's best match is highlighted" "no row is highlighted: $(screen | sed -n '2,8p')"
else
  # Where ctrl-n goes: the row drawn below the cursor's, or the first one when it is the last.
  if screen | sed -n "$((pp3_n0 + 1))p" | grep -qE '^│ *[^ │]'; then pp3_down=$((pp3_n0 + 1)); else pp3_down=2; fi
  keys C-n
  pp3_wait_at "$pp3_down" \
    && ok "ctrl-n moves the cursor down one row in the filter" \
    || bad "ctrl-n moves the cursor down one row in the filter" "from line $pp3_n0 to line [$(e2e_popup_cursor_line)], not $pp3_down"
  keys C-p
  pp3_wait_at "$pp3_n0" \
    && ok "ctrl-p moves it back up" \
    || bad "ctrl-p moves it back up" "on line [$(e2e_popup_cursor_line)], not $pp3_n0"
  check "neither key is typed into the query" "1" "$(screen | sed -n '39p' | grep -c "^ /'main▏")"
fi
keys Escape
# The modal hint line leads with the number keys ("1-9 session  ·  Tab next
# up  ·  ..."), and nothing stands in front of it: the
# popup has no frame, so the line starts at the screen's first column.
e2e_wait_screen '^ 1-9 session' 2000
check "Esc restores the agent row" "1" "$(pp_tree_claude)"
check "and main's number is yellow and bold again (#f9e2af): its key works" "249;226;175 1" "$(e2e_number_style main)"

echo "-- popup: 4) x asks before killing a pane, Esc cancels"
keys Tab; e2e_wait_cursor 'claude' 1500   # guarantee the *cursor itself* is on the agent row --
                                          # "claude" being visible anywhere (e.g. the preview) is not enough
keys x
e2e_wait_screen 'kill-pane' 1500
pp_confirm=$(screen | grep -m1 'kill-pane')
echo "$pp_confirm" | grep -qE 'kill-pane main:[0-9]+ \(claude\)' && ok "confirmation names the tmux command and the agent" || bad "confirmation names the tmux command and the agent" "$pp_confirm"
# The agent is alone in its window, so closing its pane closes the window too, and the
# confirmation says so in a sentence of its own.
check "the confirmation says the window closes with its last pane" "1" "$(screen | grep -c "It is the window's last pane, so the window closes too\.")"
check "the confirmation shows no dash, arrow or curly quote" "0" "$(screen | grep -cE "$(pp_banned)")"
keys Escape
e2e_wait_screen '^ 1-9 session' 2000
check "pane survives a cancelled close" "1" "$(tm list-panes -a -F '#{pane_id}' | grep -c "^$pp_pane\$")"

echo "-- popup: 5) ? shows help, q closes it"
keys '?'
e2e_wait_screen 'act on the row' 1500
# The modal help's groups, each title right inside the box's left border, and the
# legend's last line: the whole help is on the 120x40 client at once.
pp5_missing=""
for pp5_group in move go 'act on the row' mouse legend; do
  screen | grep -qF "│ $pp5_group " || pp5_missing="$pp5_missing [$pp5_group]"
done
[ -z "$pp5_missing" ] \
  && ok "help overlay shows its five groups: move, go, act on the row, mouse, legend" \
  || bad "help overlay shows its five groups: move, go, act on the row, mouse, legend" "missing:$pp5_missing $(screen | sed -n '2,6p')"
check "the help's last legend line is on screen" "1" "$(screen | grep -c "the way tmux writes it")"
check "the legend says what * and - mean" "1" "$(screen | grep -c '\* current  ·  - previous')"
check "the legend says what colour the tree line is" "1" "$(screen | grep -cF "tree line: white on each session's open window, blue down your own session")"
check "the help says what Tab does: next up" "1" "$(screen | grep -cF 'next up: who to deal with next (what needs you, else every agent)')"
check "the help says what prefix+O does: the same next up, from anywhere in tmux" "1" "$(screen | grep -cF 'the same next up from anywhere in tmux, straight there without the popup')"
# prefix+O is on the line under Tab's, so it reads as the same action.
pp5_tab=$(screen | grep -n 'Tab / Shift-Tab' | head -1 | cut -d: -f1)
pp5_chord=$(screen | grep -n 'prefix+O' | head -1 | cut -d: -f1)
check "the help puts prefix+O on the line under Tab" "$((pp5_tab + 1))" "$pp5_chord"
check "the help shows no dash, arrow or curly quote" "0" "$(screen | grep -cE "$(pp_banned)")"
# Every key is the help's while it is up, so the tree's numbers beside its box are dim,
# and yellow again once it closes.
check "under the help main's number is dim: the keys are the help's" "127;132;156 0" "$(e2e_number_style main)"
keys q
e2e_wait_screen '^ 1-9 session' 2000 && ok "q closes the help overlay back to the tree" || bad "q closes the help overlay back to the tree" "$(screen | head -3)"
check "with the help closed main's number is yellow and bold again" "249;226;175 1" "$(e2e_number_style main)"
# The options overlay, o: the effective configuration, and at its end what taw accepts but cannot
# honour inside tmux. Its first screen and its last, each without a dash.
keys o
e2e_wait_screen ' settings ' 1500
pp5_opts_top=$(screen)
keys PageDown PageDown PageDown
e2e_wait_screen 'no first-run flow' 1500
pp5_opts_tail=$(screen)
check "the options overlay names what it does not honour" "1" "$(printf '%s\n' "$pp5_opts_tail" | grep -c '\[onboarding\]: no first-run flow')"
check "the options overlay shows no dash, arrow or curly quote" "0" "$(printf '%s\n%s\n' "$pp5_opts_top" "$pp5_opts_tail" | grep -cE "$(pp_banned)")"
keys q
e2e_wait_screen '^ 1-9 session' 2000 && ok "q closes the options overlay back to the tree" || bad "q closes the options overlay back to the tree" "$(screen | head -3)"

echo "-- popup: 6) taw notification show lands on the status row, not in the popup"
taw notification show "pp-toast-check" --body "hi" >/dev/null 2>&1
e2e_wait_screen 'pp-toast-check' 1500
check "toast lands on the last (status) row" "1" "$(screen | sed -n '40p' | grep -c pp-toast-check)"
check "toast does not draw anywhere inside the popup" "0" "$(screen | sed -n '1,39p' | grep -c pp-toast-check)"

echo "-- popup: 7) prefix chord (C-b O) goes next up and closes the popup"
keys C-b O
check "C-b O closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
check "C-b O switches to the blocked agent's pane" "$pp_pane" "$(tm display-message -p -t main '#{pane_id}')"

echo "-- popup: 7b) tmux's prefix2 starts the chord too: C-a O goes next up, from the tree and from the filter"
# herdr v0.9.2 takes several prefix keys (#4653); tmux's second one is prefix2, and the popup reads it
# when it opens. With none set C-a O is nothing to the popup, which the first half shows (after a
# positive wait for the popup, so a popup that never came up cannot pass it). Then `set -g prefix2 C-a`:
# C-a O leaves the popup for the blocked agent, and so does it from a filter, where C-a would otherwise
# move the cursor to the start of the query.
# The popup has left the screen and put the status line back, so the next open starts clean.
pp7b_closed() { e2e_wait_popup_closed && e2e_wait_bar_restored main; }
tm select-window -t main:0 2>/dev/null
tm switch-client -t main 2>/dev/null
pp7b_home=$(tm display-message -p -t main '#{pane_id}')
e2e_ui_open
e2e_wait_screen 'Tab next up' 3000 || bad "check 7b: the popup comes up" "$(screen | sed -n '1,3p;39p')"
keys C-a O
pp7b_waited=0
while [ "$pp7b_waited" -lt 1200 ]; do command sleep 0.1; pp7b_waited=$((pp7b_waited + 100)); popup_open || break; done
check "with no prefix2, C-a O leaves the popup open" "1" "$(popup_open && echo 1 || echo 0)"
check "and goes nowhere" "$pp7b_home" "$(tm display-message -p -t main '#{pane_id}')"
keys q
pp7b_closed >/dev/null
tm set-option -g prefix2 C-a
e2e_ui_open
e2e_wait_screen 'Tab next up' 3000 || bad "check 7b: the popup comes up again" "$(screen | sed -n '1,3p;39p')"
keys C-a O
check "with prefix2 C-a, C-a O closes the popup" "1" "$(pp7b_closed && echo 1 || echo 0)"
check "and goes to the blocked agent's pane" "$pp_pane" "$(tm display-message -p -t main '#{pane_id}')"
tm select-window -t main:0 2>/dev/null
e2e_ui_open
e2e_wait_screen 'Tab next up' 3000 || bad "check 7b: the popup comes up for the filter" "$(screen | sed -n '1,3p;39p')"
keys /
e2e_wait_screen 'search…' 1500
keys -l "ab"
e2e_wait_screen '^ /ab▏' 2000 || bad "check 7b: the filter reads ab" "$(screen | sed -n '39p')"
keys C-a O
check "C-a O from the filter closes the popup" "1" "$(pp7b_closed && echo 1 || echo 0)"
check "and goes to the blocked agent's pane" "$pp_pane" "$(tm display-message -p -t main '#{pane_id}')"
tm select-window -t main:0 2>/dev/null
e2e_ui_open
e2e_wait_screen 'Tab next up' 3000 || bad "check 7b: the popup comes up for the first prefix" "$(screen | sed -n '1,3p;39p')"
keys C-b O
pp7b_closed >/dev/null
check "tmux's own C-b O goes there too, with prefix2 set" "$pp_pane" "$(tm display-message -p -t main '#{pane_id}')"
tm set-option -gu prefix2
# A popup a failed check left open would be the next section's to find: put it away.
popup_open && { taw popup close >/dev/null 2>&1; pp7b_closed >/dev/null; }

echo "-- popup: 8) q closes the popup outright"
tm switch-client -t main 2>/dev/null
e2e_ui_open
e2e_wait_screen 'Tab next up' 3000
keys q
check "q closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
check "no display-popup left open" "" "$(tm display-message -p '#{popup_id}' 2>/dev/null)"

echo "-- popup: 9) n opens the new-session picker; Enter creates and runs the hooks"
pp_tmp="$E2E_TMP/popup-picker-$$"; mkdir -p "$pp_tmp"
pp_tmp="$(cd "$pp_tmp" && pwd -P)"
mkdir -p "$pp_tmp/proj"
pp_hook_log="$pp_tmp/hook.log"
cat > "$pp_tmp/post-hook.sh" <<HOOK
#!/bin/sh
printf '%s %s\n' "\$1" "\$2" >> "$pp_hook_log"
HOOK
chmod +x "$pp_tmp/post-hook.sh"
printf '[sessions]\nname = "directory"\npost_create_hook = "%s/post-hook.sh"\n' "$pp_tmp" > "$TAW_CONFIG_DIR/config.toml"
tm switch-client -t main 2>/dev/null
e2e_ui_open
e2e_wait_screen 'Tab next up' 3000
keys n
e2e_wait_screen 'new session' 1500 && ok "n opens the ' new session ' picker" || bad "n opens the ' new session ' picker" "$(screen | head -3)"
keys C-u; command sleep 0.2   # clear the picker's pre-filled "~/" before typing an absolute path
keys -l "$pp_tmp/proj"
e2e_wait_screen 'new-session -s "proj"' 2000
pp_outcome=$(screen | grep -m1 'Enter:')
echo "$pp_outcome" | grep -qF 'Enter: new-session -s "proj"' && ok "outcome line names the new-session command" || bad "outcome line names the new-session command" "$pp_outcome"
keys Enter
check "picker Enter closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
check "the new session exists" "0" "$(tm has-session -t proj 2>/dev/null; echo $?)"
check "post_create_hook recorded \"<session> <dir>\"" "proj $pp_tmp/proj" "$(cat "$pp_hook_log" 2>/dev/null)"
rm -f "$TAW_CONFIG_DIR/config.toml"
tm kill-session -t proj 2>/dev/null

echo "-- popup: 10) Tab cycles between two working agents when nothing needs you and none is idle"
pp_pane_c=$(e2e_start_agent claude)
e2e_wait_opt "$pp_pane_c" @agent_kind claude 4000 >/dev/null
e2e_wait_opt "$pp_pane_c" @agent_state idle 7000 >/dev/null
pp_fpid_c=$(tm display-message -p -t "$pp_pane_c" '#{pane_pid}')
pp_apid_c=$(pgrep -P "$pp_fpid_c" -f claude 2>/dev/null | head -1); pp_apid_c="${pp_apid_c:-$pp_fpid_c}"
kill -USR1 "$pp_apid_c" 2>/dev/null
e2e_wait_opt "$pp_pane_c" @agent_state working 3000 >/dev/null

pp_pane_d=$(e2e_start_agent claude)
e2e_wait_opt "$pp_pane_d" @agent_kind claude 4000 >/dev/null
e2e_wait_opt "$pp_pane_d" @agent_state idle 7000 >/dev/null
pp_fpid_d=$(tm display-message -p -t "$pp_pane_d" '#{pane_pid}')
pp_apid_d=$(pgrep -P "$pp_fpid_d" -f claude 2>/dev/null | head -1); pp_apid_d="${pp_apid_d:-$pp_fpid_d}"
kill -USR1 "$pp_apid_d" 2>/dev/null
e2e_wait_opt "$pp_pane_d" @agent_state working 3000 >/dev/null
# The original blocked agent (from checks 1-9) is still around and still needs you
# (blocked outranks working), so it -- not either working pane -- would be the only
# stop of Tab's walk; take it out of the picture first.
tm kill-window -t "$pp_pane" 2>/dev/null

tm switch-client -t main 2>/dev/null
e2e_ui_open
e2e_wait_screen 'Tab next up' 3000
# Poll e2e_popup_cursor_row for up to <timeout_ms> until it differs from (or,
# with want_same, equals) <target>; echoes the settled value and returns 0, or
# returns 1 on timeout. Neither Tab press here has fixed target text to hand
# e2e_wait_cursor -- both agents are labelled "claude" -- so this polls the
# actual row content by value instead of a regex.
poll_cursor_until() {
  local target="$1" want_same="$2" timeout_ms="${3:-2000}"; local waited=0 now
  while [ "$waited" -lt "$timeout_ms" ]; do
    now=$(e2e_popup_cursor_row)
    if [ "$want_same" = same ]; then
      [ "$now" = "$target" ] && { echo "$now"; return 0; }
    else
      [ "$now" != "$target" ] && { echo "$now"; return 0; }
    fi
    command sleep 0.1; waited=$((waited + 100))
  done
  echo "$now"; return 1
}
# The cursor opens on main's header (check 1). Neither agent needs you and neither is
# idle, so the rest is the walk: every agent row, in the order drawn.
# Three Tabs prove the full walk and its wrap: header -> C -> D -> back to C.
pp_start=$(e2e_popup_cursor_row)
keys Tab
if pp_after1=$(poll_cursor_until "$pp_start" different 2000); then
  ok "Tab moves off the header onto the first agent"
else
  bad "Tab moves off the header onto the first agent" "still: [$pp_after1]"
fi
keys Tab
if pp_after2=$(poll_cursor_until "$pp_after1" different 2000); then
  ok "a second Tab moves onto the other agent"
else
  bad "a second Tab moves onto the other agent" "after1=[$pp_after1] after2=[$pp_after2]"
fi
keys Tab
if pp_after3=$(poll_cursor_until "$pp_after2" different 2000); then
  [ "$pp_after3" = "$pp_after1" ] \
    && ok "a third Tab wraps back to the first agent" \
    || bad "a third Tab wraps back to the first agent" "after1=[$pp_after1] after3=[$pp_after3]"
else
  bad "a third Tab wraps back to the first agent" "cursor did not move: [$pp_after3]"
fi
taw popup close >/dev/null 2>&1
tm kill-window -t "$pp_pane_c" 2>/dev/null
tm kill-window -t "$pp_pane_d" 2>/dev/null

echo "-- popup: 11) search preset ([keys] preset = \"search\"): fzf's own keys, nothing else"
# The search preset is a pure fzf search. Typing
# filters; the arrows, ctrl-j / ctrl-k and ctrl-n / ctrl-p move; Enter goes; Esc quits; every
# other chord it had does nothing now. Two idle agents in main, their windows renamed so that
# one query names both and nothing else; srch11a's window is the older, so its row is first.
pp_s_a=$(e2e_start_agent claude); tm rename-window -t "$pp_s_a" srch11a
pp_s_b=$(e2e_start_agent claude); tm rename-window -t "$pp_s_b" srch11b
e2e_wait_opt "$pp_s_a" @agent_state idle 7000 >/dev/null
e2e_wait_opt "$pp_s_b" @agent_state idle 7000 >/dev/null
printf '[keys]\npreset = "search"\n' > "$TAW_CONFIG_DIR/config.toml"
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
# Samples the popup's own rows -- the status line below them carries the bar's clock -- across
# a bounded window, failing the moment they differ from <before> and showing the lines that
# did; passing only once the whole window has elapsed with no change. Check 2's idiom for the
# whole popup rather than its cursor row, since "nothing happened" gives nothing to poll for.
watch_rows_unchanged() {
  local label="$1" before="$2" window_ms="${3:-800}"; local waited=0 now
  while [ "$waited" -lt "$window_ms" ]; do
    command sleep 0.1; waited=$((waited + 100))
    now=$(screen | sed -n '1,39p')
    [ "$now" != "$before" ] && { bad "$label" "$(diff <(printf '%s\n' "$before") <(printf '%s\n' "$now") | head -6)"; return; }
  done
  ok "$label"
}
# The search preset's hint line. An overlay has a hint line of its own, and every one of them
# ends in Esc close or Esc cancel, where the tree's says Esc quit.
pp_s_hint='type filter (fzf syntax)  ·  4a session 4 row a  ·  ↑/↓ ctrl-n/p move  ·  Enter go  ·  right-click menu  ·  Esc quit'
pp_s_overlay_open() { screen | sed -n '39p' | grep -qE 'Esc (close|cancel)'; }
e2e_ui_open; e2e_wait_popup_open
e2e_wait_screen '^ type filter' 3000 || bad "check 11: the search preset's hint line appears" "$(screen | sed -n '39p')"
# The query row is the tree panel's first line inside its border: row 2, right under
# the border on row 1 (the popup has no frame).
check "the query row is drawn with its placeholder" "1" "$(screen | sed -n '2p' | grep -c 'search…')"
check "the hint line names fzf's keys, the menu and quit, and nothing else" "$pp_s_hint" "$(screen | sed -n '39p' | sed 's/^ //; s/ *$//')"
keys -l "^pp-other"                   # a prefix anchor, as in check 3
e2e_wait_screen '> \^pp-other▏' 2000 || bad "check 11: the query reads ^pp-other" "$(screen | sed -n '2p')"
check "typing narrows at once, no / needed" "0" "$(pp_tree_claude)"
keys C-u
e2e_wait_screen 'search…' 2000
keys -l "srch11"
e2e_wait_cursor ':srch11a' 2000 \
  && ok "the cursor goes to the first match" \
  || bad "the cursor goes to the first match" "$(e2e_popup_cursor_row)"
keys C-n
e2e_wait_cursor ':srch11b' 2000 \
  && ok "ctrl-n moves the cursor down one match" \
  || bad "ctrl-n moves the cursor down one match" "$(e2e_popup_cursor_row)"
keys C-p
e2e_wait_cursor ':srch11a' 2000 \
  && ok "ctrl-p moves it back up" \
  || bad "ctrl-p moves it back up" "$(e2e_popup_cursor_row)"
check "neither key is typed into the query" "1" "$(screen | sed -n '2p' | grep -c '> srch11▏')"
# Enter is for the row under the cursor: had a key opened something over the tree, it would
# take the Enter for itself.
if pp_s_overlay_open; then bad "no overlay opened on the way to Enter" "$(screen | sed -n '39p')"; keys Escape; e2e_wait_still; else ok "no overlay opened on the way to Enter"; fi
keys Enter
check "Enter closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
pp_s_i=0
while [ "$pp_s_i" -lt 25 ] && [ "$(tm display-message -p '#{pane_id}')" != "$pp_s_a" ]; do
  command sleep 0.2; pp_s_i=$((pp_s_i + 1))
done
check "Enter lands on the agent under the cursor" "$pp_s_a" "$(tm display-message -p '#{pane_id}')"
e2e_wait_bar_restored main
# Chords the modal preset binds do nothing in the search preset, from main's header:
# ctrl-o opened a menu, ctrl-t the flat list, ctrl-r a rename, ctrl-x a kill confirmation, and
# Tab moved to an agent. Each key gets its own baseline, and one that did open something is
# put away before the next, so every key is watched from the tree.
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
e2e_ui_open; e2e_wait_popup_open
e2e_wait_cursor '^│[0-9 ] [* -] main( |$)' 2000 \
  && ok "the popup opens on main's header" \
  || bad "the popup opens on main's header" "$(e2e_popup_cursor_row)"
# A digit is text here, and typing a session's number puts the cursor on it, so the
# number is drawn as the modal preset draws it, in the yellow of a key, bold (#f9e2af).
check "a session's number is yellow and bold in the search preset: typing it reaches it" "249;226;175 1" "$(e2e_number_style main)"
e2e_wait_still || bad "check 11: the screen settles before the watch" "$(screen | sed -n '38,39p')"
# What an overlay draws: a box's title, a confirmation's or a prompt's tmux command, a menu's
# entries. Counted on the baseline too, so that text a preview happens to show is not taken
# for one.
pp_s_overlay='╭ (help|settings|confirm|new session) |kill-|rename|new worktree'
# The highlighted row's screen line number (e2e_popup_cursor_line): a move between two rows
# that read alike, and whose previews do too, shows only in the highlight.
for pp_s_k in C-o C-t C-r C-x Tab; do
  pp_s_before=$(screen | sed -n '1,39p')
  pp_s_line=$(e2e_popup_cursor_line)
  keys "$pp_s_k"
  watch_rows_unchanged "$pp_s_k changes nothing on screen" "$pp_s_before"
  check "$pp_s_k leaves the cursor where it was" "$pp_s_line" "$(e2e_popup_cursor_line)"
  check "$pp_s_k opens no menu, prompt, confirmation or help" \
    "$(printf '%s\n' "$pp_s_before" | grep -cE "$pp_s_overlay")" "$(screen | sed -n '1,39p' | grep -cE "$pp_s_overlay")"
  if pp_s_overlay_open; then keys Escape; e2e_wait_still; fi
done
check "the popup is still open after all five" "1" "$(popup_open && echo 1 || echo 0)"
taw popup close >/dev/null 2>&1; e2e_wait_popup_closed; e2e_wait_bar_restored main
rm -f "$TAW_CONFIG_DIR/config.toml"
tm kill-window -t "$pp_s_a" 2>/dev/null
tm kill-window -t "$pp_s_b" 2>/dev/null
tm kill-session -t pp-other 2>/dev/null

echo "-- popup: 12) a number moves the cursor to that session and switches nothing"
pp12_pane=$(e2e_start_agent claude)
e2e_wait_opt "$pp12_pane" @agent_kind claude 4000 >/dev/null
tm new-session -d -s pp-two -c "$DIR/home" 2>/dev/null
tm new-window -t pp-two -c "$DIR/home" 2>/dev/null      # pp-two now has windows 0 and 1; 1 is active
tm switch-client -t main 2>/dev/null
pp12_n=$(tm list-sessions -F '#S' | grep -n '^pp-two$' | cut -d: -f1)
# Labels stop at nine: earlier sections can leave scratch sessions behind, so kill every
# session this suite made other than main, pp-other and pp-two when pp-two would be past 9.
if [ "$pp12_n" -gt 9 ]; then
  tm list-sessions -F '#S' | grep -vE '^(main|pp-other|pp-two)$' | while read -r pp12_s; do tm kill-session -t "$pp12_s" 2>/dev/null; done
  pp12_n=$(tm list-sessions -F '#S' | grep -n '^pp-two$' | cut -d: -f1)
fi
e2e_ui_open; e2e_wait_screen 'Tab next up' 3000
keys "$pp12_n"
if e2e_wait_cursor "pp-two" 2000; then ok "number $pp12_n puts the cursor on pp-two's header"; else bad "number puts the cursor on the session" "$(e2e_popup_cursor_row)"; fi
check "the popup is still open after a number" "1" "$(popup_open && echo 1 || echo 0)"
check "the client did not switch on a number" "main" "$(tm display-message -p '#{client_session}')"
# The hint's Enter entry is the constant "Enter go" on every row; it does not
# name a target.
pp12_hint=$(screen | sed -n '39p')
echo "$pp12_hint" | grep -qF 'Enter go' && ok "the hint says Enter go on a header too" || bad "the hint says Enter go on a header too" "$pp12_hint"
echo "$pp12_hint" | grep -q 'pp-two' && bad "the hint names no target" "$pp12_hint" || ok "the hint names no target"

echo "-- popup: 13) Enter on a header is switch-client only"
keys Enter
check "Enter on a header closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
check "the client is on pp-two" "pp-two" "$(tm display-message -p '#{client_session}')"
check "pp-two's active window is still 1" "1" "$(tm list-windows -t pp-two -F '#{window_index} #{window_active}' | awk '$2==1{print $1}')"
tm switch-client -t main 2>/dev/null

echo "-- popup: 14) the cursor opens on the pane you came from, and h/l keep it"
tm select-window -t "$pp12_pane" 2>/dev/null           # open the popup from the agent's pane
e2e_ui_open; e2e_wait_screen 'Tab next up' 3000
e2e_wait_cursor 'claude' 2000 && ok "cursor opens on the agent's row when opened from it" || bad "cursor opens on the agent's row" "$(e2e_popup_cursor_row)"
keys h; e2e_wait_cursor '[* -] main' 2000 && ok "h folds to the session header" || bad "h folds to the session header" "$(e2e_popup_cursor_row)"
keys l; e2e_wait_cursor 'claude' 2000 && ok "l returns to the agent row" || bad "l returns to the agent row" "$(e2e_popup_cursor_row)"
e2e_popup_cursor_row | grep -q 'fake task' && ok "the agent's own title is on its row" || bad "agent title on its row" "$(e2e_popup_cursor_row)"
taw popup close >/dev/null 2>&1; command sleep 0.3
tm select-window -t main:0 2>/dev/null

echo "-- popup: 15) prefix+O walks what needs you in order, then says so"
# pp15_b lives in its own session (pp-b), not main: two hot
# agents *in different sessions* are needed here, so Tab's walk and prefix+O's walk are proven to cross a
# session boundary, not just move between two windows of one session.
tm new-session -d -s pp-b -c "$DIR/home" 2>/dev/null
pp15_a=$(e2e_start_agent claude); e2e_wait_opt "$pp15_a" @agent_kind claude 4000 >/dev/null; e2e_wait_opt "$pp15_a" @agent_state idle 7000 >/dev/null
pp15_b=$(e2e_start_agent claude pp-b); e2e_wait_opt "$pp15_b" @agent_kind claude 4000 >/dev/null; e2e_wait_opt "$pp15_b" @agent_state idle 7000 >/dev/null
tm kill-window -t "$pp12_pane" 2>/dev/null
for pp15_p in "$pp15_a" "$pp15_b"; do
  pp15_fpid=$(tm display-message -p -t "$pp15_p" '#{pane_pid}')
  pp15_apid=$(pgrep -P "$pp15_fpid" -f claude 2>/dev/null | head -1); kill -USR2 "${pp15_apid:-$pp15_fpid}" 2>/dev/null
  e2e_wait_opt "$pp15_p" @agent_state blocked 3000 >/dev/null
done
tm select-window -t main:0 2>/dev/null
# Inside the popup first: Tab visits the two blocked agents top to bottom and wraps.
e2e_ui_open; e2e_wait_screen 'Tab next up' 3000
pp15_start=$(e2e_popup_cursor_row)
keys Tab; pp15_t1=$(poll_cursor_until "$pp15_start" different 2000)
keys Tab; pp15_t2=$(poll_cursor_until "$pp15_t1" different 2000)
keys Tab; pp15_t3=$(poll_cursor_until "$pp15_t2" different 2000)
[ "$pp15_t1" != "$pp15_start" ] && [ "$pp15_t2" != "$pp15_t1" ] && [ "$pp15_t3" = "$pp15_t1" ] \
  && ok "Tab walks both blocked agents top to bottom and wraps" \
  || bad "Tab walks both blocked agents and wraps" "start=[$pp15_start] t1=[$pp15_t1] t2=[$pp15_t2] t3=[$pp15_t3]"
taw popup close >/dev/null 2>&1; command sleep 0.3
# pp15_a must land first regardless of pp-b's own position in tmux's session order: the walk
# starts from main:0 (select-window, above), which is *in pp15_a's own session*, so pp15_a -- the
# next hot pane in that same session -- is the nearest hot pane in tmux order no matter where
# session "pp-b" itself sorts; only once pp15_a is exhausted can the walk ever leave session main.
# The client's actual current pane, not session main's own remembered one -- the two must
# be queried differently once prefix+O can cross into a different session: main:0's own
# "-t main" query would keep reporting main's last-selected window even after the client
# itself has moved on to session pp-b, since -t main asks main's own state, not the
# attached client's. Plain, untargeted display-message resolves the sole attached client's
# current pane directly, which is what actually moved (the same untargeted query check 12
# already trusts for "#{client_session}").
prefix O; e2e_wait_opt "$pp15_a" @agent_seen 1 3000 >/dev/null
check "first prefix+O goes to the first blocked agent in tmux order" "$pp15_a" "$(tm display-message -p '#{pane_id}')"
prefix O; e2e_wait_opt "$pp15_b" @agent_seen 1 3000 >/dev/null
check "second prefix+O crosses into the next session" "$pp15_b" "$(tm display-message -p '#{pane_id}')"
tm kill-window -t "$pp15_a" 2>/dev/null; tm kill-window -t "$pp15_b" 2>/dev/null
tm select-window -t main:0 2>/dev/null
prefix O
e2e_wait_screen 'taw: no agents' 2000 && ok "with no agent at all prefix+O says so" || bad "prefix+O says there are no agents" "$(screen | tail -1)"
tm kill-session -t pp-two 2>/dev/null
tm kill-session -t pp-b 2>/dev/null

echo "-- popup: 15b) next up: Tab walks what needs you, and with nothing that does every agent"
# One rule for Tab and prefix+O: the agents that need you while any does, and
# with none every agent, idle and working alike, one step a press, wrapping.
# Three agents in main, their windows renamed so each row can be found by name: nu-a needs you,
# nu-b is idle and nu-c is working. Every agent pane the sections above left is swept first, server-wide,
# so these three are the whole census.
while read -r nu_stray; do
  [ -n "$nu_stray" ] && tm kill-window -t "$nu_stray" 2>/dev/null
done < <(tm list-panes -a -F '#{pane_id} #{@agent_kind}' | awk 'NF>1{print $1}')
# Sends the fixture agent in <pane> a signal: USR1 toggles idle and working (and takes a blocked
# agent to idle), USR2 makes it ask for permission.
nu_signal() {
  local pane="$1" sig="$2" fpid apid
  fpid=$(tm display-message -p -t "$pane" '#{pane_pid}')
  apid=$(pgrep -P "$fpid" -f claude 2>/dev/null | head -1)
  kill "$sig" "${apid:-$fpid}" 2>/dev/null
}
# Answers the permission prompt of <pane>'s agent and lets it start another turn: blocked, then
# (its turn over) done or idle, then working. The second signal waits for the first to be read,
# or the two would be one.
nu_answer() {
  local pane="$1" waited=0
  nu_signal "$pane" -USR1
  while [ "$waited" -lt 4000 ] && [ "$(tm display-message -p -t "$pane" '#{@agent_state}')" = blocked ]; do
    command sleep 0.1; waited=$((waited + 100))
  done
  command sleep 0.3
  nu_signal "$pane" -USR1
  e2e_wait_opt "$pane" @agent_state working 4000 >/dev/null
}
# Three agents in main, nu-a blocked, nu-b idle and nu-c working, and the client on main:0.
nu_setup() {
  nu_a=$(e2e_start_agent claude); tm rename-window -t "$nu_a" nu-a
  nu_b=$(e2e_start_agent claude); tm rename-window -t "$nu_b" nu-b
  nu_c=$(e2e_start_agent claude); tm rename-window -t "$nu_c" nu-c
  for nu_p in "$nu_a" "$nu_b" "$nu_c"; do
    e2e_wait_opt "$nu_p" @agent_kind claude 4000 >/dev/null
    e2e_wait_opt "$nu_p" @agent_state idle 7000 >/dev/null
  done
  nu_signal "$nu_a" -USR2; e2e_wait_opt "$nu_a" @agent_state blocked 3000 >/dev/null
  nu_signal "$nu_c" -USR1; e2e_wait_opt "$nu_c" @agent_state working 3000 >/dev/null
  tm switch-client -t main 2>/dev/null
  tm select-window -t main:0 2>/dev/null
}
# Puts the popup away whatever the check found, and the three agents with it.
nu_teardown() {
  taw popup close >/dev/null 2>&1; e2e_wait_popup_closed >/dev/null
  tm kill-window -t "$nu_a" 2>/dev/null; tm kill-window -t "$nu_b" 2>/dev/null; tm kill-window -t "$nu_c" 2>/dev/null
  tm select-window -t main:0 2>/dev/null
}
nu_setup
e2e_ui_open; e2e_wait_screen 'Tab next up' 3000
e2e_wait_screen '[0-9]+:nu-c +working' 3000 || bad "check 15b: the popup draws all three agents" "$(screen | sed -n '1,10p')"
e2e_wait_cursor '^[│ ]*[0-9]? [* -] main\b' 2000 \
  && ok "the popup opens on main's header, your own row" \
  || bad "the popup opens on main's header, your own row" "$(e2e_popup_cursor_row)"
# What needs you first: the one blocked agent, with an idle and a working one below it.
keys Tab
e2e_wait_cursor ':nu-a' 2000 \
  && ok "Tab goes to the agent that needs you" \
  || bad "Tab goes to the agent that needs you" "$(e2e_popup_cursor_row)"
nu_before=$(e2e_popup_cursor_row)
keys Tab
watch_cursor_unchanged "Tab again stays on it: the idle and the working agent below are not in the walk" "$nu_before"
# Answered, it is working, and nothing needs you: every agent is the walk, from the cursor down.
nu_answer "$nu_a"
e2e_wait_screen '[0-9]+:nu-a +working' 4000 || bad "check 15b: the popup shows nu-a working" "$(screen | sed -n '1,10p')"
keys Tab
e2e_wait_cursor ':nu-b' 2000 \
  && ok "with nothing needing you Tab goes on to the next agent, the idle nu-b" \
  || bad "with nothing needing you Tab goes on to the next agent, the idle nu-b" "$(e2e_popup_cursor_row)"
keys Tab
e2e_wait_cursor ':nu-c' 2000 \
  && ok "then to the working nu-c: every agent is a stop" \
  || bad "then to the working nu-c: every agent is a stop" "$(e2e_popup_cursor_row)"
keys Tab
e2e_wait_cursor ':nu-a' 2000 \
  && ok "and round to nu-a, past main's header" \
  || bad "and round to nu-a, past main's header" "$(e2e_popup_cursor_row)"
keys BTab
e2e_wait_cursor ':nu-c' 2000 \
  && ok "Shift-Tab walks it back" \
  || bad "Shift-Tab walks it back" "$(e2e_popup_cursor_row)"
# An idle agent that starts working is still a stop.
nu_signal "$nu_b" -USR1; e2e_wait_opt "$nu_b" @agent_state working 3000 >/dev/null
e2e_wait_screen '[0-9]+:nu-b +working' 4000 || bad "check 15b: the popup shows nu-b working" "$(screen | sed -n '1,10p')"
keys BTab
e2e_wait_cursor ':nu-b' 2000 \
  && ok "nu-b, working now, is still a stop" \
  || bad "nu-b, working now, is still a stop" "$(e2e_popup_cursor_row)"
nu_teardown

echo "-- popup: 15c) next up: prefix+O walks the same way, outside the popup and inside it"
# The same three agents and the same rule, from the pane you are in. Waits until the client is
# looking at <pane>, up to <ms> (default 3000).
nu_wait_on() {
  local pane="$1" ms="${2:-3000}" waited=0
  while [ "$waited" -lt "$ms" ]; do
    [ "$(tm display-message -p '#{pane_id}')" = "$pane" ] && return 0
    command sleep 0.1; waited=$((waited + 100))
  done
  return 1
}
# prefix+O, then a bounded watch that the client stays on <pane>: nothing to poll for, so it
# passes only once the window has gone by with no move (check 2's idiom for the cursor).
nu_stays_on() {
  local label="$1" pane="$2" waited=0
  prefix O
  while [ "$waited" -lt 1200 ]; do
    command sleep 0.1; waited=$((waited + 100))
    [ "$(tm display-message -p '#{pane_id}')" != "$pane" ] && { bad "$label" "moved to $(tm display-message -p '#{pane_id}'), not staying on $pane"; return; }
  done
  ok "$label"
}
nu_setup
prefix O
nu_wait_on "$nu_a" \
  && ok "prefix+O goes to the agent that needs you" \
  || bad "prefix+O goes to the agent that needs you" "on $(tm display-message -p '#{pane_id}'), not $nu_a"
nu_stays_on "prefix+O again stays on it: the idle and the working agent are not in the walk" "$nu_a"
check "open-target names the group of the pane it went to: needs_you" "needs_you" "$(jget "$(taw notification open-target 2>&1)" target)"
# Answered, in the background, and nothing needs you: every agent is the walk, from the pane you
# are in, in tmux order.
tm select-window -t main:0 2>/dev/null
nu_answer "$nu_a"
prefix O
nu_wait_on "$nu_a" \
  && ok "with nothing needing you prefix+O goes to the next agent, working or not: nu-a" \
  || bad "with nothing needing you prefix+O goes to the next agent, working or not: nu-a" "on $(tm display-message -p '#{pane_id}'), not $nu_a"
prefix O
nu_wait_on "$nu_b" \
  && ok "and on to the idle nu-b" \
  || bad "and on to the idle nu-b" "on $(tm display-message -p '#{pane_id}'), not $nu_b"
check "open-target names the group of the pane it went to: rest, the working nu-c" "rest" "$(jget "$(taw notification open-target 2>&1)" target)"
# The chord inside the popup takes the same walk, from the pane the popup was opened from.
tm select-window -t main:0 2>/dev/null
e2e_ui_open; e2e_wait_screen 'Tab next up' 3000
keys C-b O
check "C-b O closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
nu_wait_on "$nu_a" \
  && ok "and in the popup it goes to the next agent too: nu-a" \
  || bad "and in the popup it goes to the next agent too: nu-a" "on $(tm display-message -p '#{pane_id}'), not $nu_a"
# An idle agent that starts working is still a stop.
nu_signal "$nu_b" -USR1; e2e_wait_opt "$nu_b" @agent_state working 3000 >/dev/null
prefix O
nu_wait_on "$nu_b" \
  && ok "nu-b, working now, is still a stop" \
  || bad "nu-b, working now, is still a stop" "on $(tm display-message -p '#{pane_id}'), not $nu_b"
prefix O
nu_wait_on "$nu_c" \
  && ok "and on to nu-c" \
  || bad "and on to nu-c" "on $(tm display-message -p '#{pane_id}'), not $nu_c"
check "open-target names the group of the pane it went to: rest, the working nu-a" "rest" "$(jget "$(taw notification open-target 2>&1)" target)"
nu_teardown

echo "-- popup: 16) the picker searches zoxide from an empty start"
# Two directories that really exist -- open_or_create needs a real path, and both the
# session's own #{session_path} and zoxide_add's argument get canonicalised (macOS resolves
# /var to /private/var), so a second, resolved form is kept for comparing against those
# specifically. The unresolved form is what actually gets rendered/typed, and it has to stay
# short: the picker draws a candidate on one row, 94 columns wide in this 120-column client, and
# cuts it there, so a path much past 90 characters loses "beta-tool" off its end. The two
# directories are under /tmp for that, not under E2E_TMP: macOS's own $TMPDIR is 49 characters
# before taw-e2e/accept-<pid>/zx/beta-tool is added to it, and an E2E_TMP of a scratch area's
# depth is past the edge on its own.
pp16_zx_root=$(mktemp -d /tmp/taw-zx.XXXXXX 2>/dev/null) || pp16_zx_root="$DIR/zx"
pp16_zx_alpha="$pp16_zx_root/alpha-project"; mkdir -p "$pp16_zx_alpha"
pp16_zx_beta="$pp16_zx_root/beta-tool"; mkdir -p "$pp16_zx_beta"
pp16_zx_beta_real="$(cd "$pp16_zx_beta" && pwd -P)"
pp16_zx_log="$DIR/pp16-zoxide.log"; : > "$pp16_zx_log"
# The popup is a fresh `run-shell` -> `display-popup -E` process each time it opens, which
# inherits its environment from the tmux *server's* own global environment table -- not from
# this script's shell, so a plain `PATH=... taw ...` prefix (the "session open" section's own
# fake zoxide, above, only needs that since it runs taw directly) never reaches it. Verified
# directly that prepending here via `set-environment -g PATH` does reach a display-popup -E
# process; this is the one mechanism that actually works for the popup specifically.
pp16_fakebin="$DIR/pp16-fakebin"; mkdir -p "$pp16_fakebin"
cat > "$pp16_fakebin/zoxide" <<FAKEZOXIDE
#!/bin/sh
case "\$1" in
  add) printf 'add %s\n' "\$2" >> "$pp16_zx_log" ;;
  query)
    if [ "\$2" = "-ls" ]; then
      printf '2580.0 %s\n' "$pp16_zx_alpha"
      printf ' 680.0 %s\n' "$pp16_zx_beta"
    else
      printf '%s\n' "$pp16_zx_alpha"
      printf '%s\n' "$pp16_zx_beta"
    fi
    ;;
esac
FAKEZOXIDE
chmod +x "$pp16_fakebin/zoxide"
pp16_path_saved=$(tm show-environment -g PATH 2>/dev/null | sed 's/^PATH=//')
tm set-environment -g PATH "$pp16_fakebin:$pp16_path_saved"
# No project_dirs configured (the common setup): zoxide is
# the picker's only source here, so the two listed candidates can only have come from it.
printf '[sessions]\nname = "directory"\n' > "$TAW_CONFIG_DIR/config.toml"

tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
e2e_ui_open; e2e_wait_screen 'Tab next up' 3000
keys n
e2e_wait_screen 'new session' 1500
check "the popup is still open, picker over the tree" "1" "$(popup_open && echo 1 || echo 0)"
screen | grep -q 'type a project, or a path' && ok "the picker's placeholder shows when nothing is typed" || bad "picker placeholder shown" "$(screen | grep -m1 '>')"
# With zero candidates the picker draws only its placeholder, never an "Enter:" line at all --
# an absent line must not read as "correctly has no ~/": confirm one exists first (bounded
# poll) and fail outright if it never appears, rather than letting an empty $pp16_outcome pass
# the substring check on nothing.
if e2e_wait_screen 'Enter:' 2000; then
  pp16_outcome=$(screen | grep -m1 'Enter:')
  # shellcheck disable=SC2088  # the literal two characters "~/", searched for verbatim, not $HOME
  if echo "$pp16_outcome" | grep -qF '~/'; then
    bad "the outcome line does not start from ~/" "$pp16_outcome"
  else
    ok "the outcome line does not start from ~/"
  fi
else
  bad "the outcome line does not start from ~/" "no Enter: line appeared at all"
fi
pp16_alpha_row=$(screen | grep -F "$pp16_zx_alpha" | grep -v 'Enter:' | head -1)
pp16_beta_row=$(screen | grep -F "$pp16_zx_beta" | grep -v 'Enter:' | head -1)
if [ -n "$pp16_alpha_row" ] && [ -n "$pp16_beta_row" ]; then
  ok "both zoxide directories are listed with nothing typed"
else
  bad "both zoxide directories are listed" "alpha=[$pp16_alpha_row] beta=[$pp16_beta_row]"
fi
# Neither row's own text before the path (the highlight marker or its blank space) may ever
# contain a digit (a label digit in front of the path would be typed into it, and creation
# would fail). A missing row is this check's own failure too, not something to
# skip: an empty $pp16_alpha_row/$pp16_beta_row strips to an empty prefix, which contains no
# digit either, and would otherwise pass on a candidate that was never listed at all.
if [ -z "$pp16_alpha_row" ] || [ -z "$pp16_beta_row" ]; then
  bad "no leading number on either candidate row" "a candidate row is missing: alpha=[$pp16_alpha_row] beta=[$pp16_beta_row]"
else
  pp16_alpha_prefix="${pp16_alpha_row%%"$pp16_zx_alpha"*}"
  pp16_beta_prefix="${pp16_beta_row%%"$pp16_zx_beta"*}"
  if echo "$pp16_alpha_prefix" | grep -qE '[0-9]' || echo "$pp16_beta_prefix" | grep -qE '[0-9]'; then
    bad "no leading number on either candidate row" "alpha prefix=[$pp16_alpha_prefix] beta prefix=[$pp16_beta_prefix]"
  else
    ok "no leading number on either candidate row"
  fi
fi

keys -l "beta"
e2e_wait_screen "» .*beta-tool" 2000 && ok "typing 'beta' ranks beta-tool first" || bad "typing 'beta' ranks beta-tool first" "$(screen | grep -m1 'Enter:')"

keys Enter
check "Enter on the zoxide candidate closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
check "a session exists at the beta-tool directory" "$pp16_zx_beta_real" "$(tm list-sessions -F '#{session_path}' | grep -F "$pp16_zx_beta_real" | head -1)"
check "a session named after the directory's basename exists" "0" "$(tm has-session -t beta-tool 2>/dev/null; echo $?)"
check "that session's path is the beta-tool directory" "$pp16_zx_beta_real" "$(tm display-message -p -t beta-tool '#{session_path}' 2>/dev/null)"
check "zoxide add recorded the new directory" "add $pp16_zx_beta_real" "$(cat "$pp16_zx_log" 2>/dev/null)"

echo "-- popup: 16b) reopening the picker still switches to filesystem completion on a path prefix"
tm switch-client -t main 2>/dev/null
# The picker lists the directories under ~/, the home of the server it runs under, which is the run's
# own and empty: this makes the one that is listed (before, it listed the runner's real home).
mkdir -p "$DIR/shell-home/pp16-home-dir"
e2e_ui_open; e2e_wait_screen 'Tab next up' 3000
keys n
e2e_wait_screen 'new session' 1500
# shellcheck disable=SC2088  # sent -l (literal): the two keystrokes "~" "/", not $HOME
keys -l "~/"
e2e_wait_screen '» ~/pp16-home-dir' 2000 && ok "typing ~/ switches to filesystem completion" || bad "typing ~/ switches to filesystem completion" "$(screen | grep -m1 'Enter:')"
keys Escape; command sleep 0.3
taw popup close >/dev/null 2>&1; command sleep 0.3
rmdir "$DIR/shell-home/pp16-home-dir"

tm kill-session -t beta-tool 2>/dev/null
tm set-environment -g PATH "$pp16_path_saved"
rm -f "$TAW_CONFIG_DIR/config.toml"
rm -rf "${pp16_zx_root:?}"
tm select-window -t main:0 2>/dev/null

echo "-- popup: 16c) projects in the filter: the directories the picker lists, under the sessions"
# [ui] projects_in_filter: with it on, `/` also lists the directories `n`
# would, in a section under the sessions, and Enter on one opens or creates its session, like
# tmux-session-wizard. Two directories that really exist, under /tmp for the short path check 16
# explains, with names no session's path or name can spell in order (a fuzzy match reads a
# session's whole path, and a scratch path is random letters): the second half of each name
# follows a hyphen, which the only hyphens on a session's path, `taw-e2e` and `accept-`, cannot
# be followed by. The fake zoxide is check 16's: reached through the server's global PATH,
# because the popup is a fresh process that inherits the server's environment, not this shell's.
pf_root=$(mktemp -d /tmp/taw-pf.XXXXXX 2>/dev/null) || pf_root="$DIR/pf"
pf_a="$pf_root/xylo-falcon"; mkdir -p "$pf_a"
pf_b="$pf_root/zeb-marlin"; mkdir -p "$pf_b"
pf_a_real="$(cd "$pf_a" && pwd -P)"
pf_b_real="$(cd "$pf_b" && pwd -P)"
pf_log="$DIR/pf-zoxide.log"; : > "$pf_log"
pf_fakebin="$DIR/pf-fakebin"; mkdir -p "$pf_fakebin"
cat > "$pf_fakebin/zoxide" <<FAKEZOXIDE
#!/bin/sh
case "\$1" in
  add) printf 'add %s\n' "\$2" >> "$pf_log" ;;
  query) printf '%s\n' "$pf_a" "$pf_b" ;;
esac
FAKEZOXIDE
chmod +x "$pf_fakebin/zoxide"
pf_path_saved=$(tm show-environment -g PATH 2>/dev/null | sed 's/^PATH=//')
tm set-environment -g PATH "$pf_fakebin:$pf_path_saved"
# The popup's colours are the default theme's: dim #7f849c (127;132;156).
pf_dim='127;132;156 0'

# Opens the popup on main and waits, bounded, for <regex> on its screen. A popup that did not
# come up is this step's own failure, not something the checks after it read an empty screen
# through: a "nothing is listed" check passes on a screen with no popup in it.
pf_open() {
  local regex="$1" label="$2"
  tm switch-client -t main 2>/dev/null
  tm select-window -t main:0 2>/dev/null
  e2e_ui_open
  e2e_wait_screen "$regex" 8000 && return 0
  bad "$label: the popup came up" "$(screen | sed -n '1,4p')"
  return 1
}
# Leaves the popup the way a user does: Esc takes the query and any overlay away, `q` quits.
# `taw popup close` only if it is still up, and the status line is waited back either way.
pf_close() {
  keys Escape; command sleep 0.2; keys q
  e2e_wait_popup_closed 3000 || { taw popup close >/dev/null 2>&1; e2e_wait_popup_closed; }
  e2e_wait_bar_restored main
}

# --- the modal preset, the switch on
printf '[ui]\nprojects_in_filter = true\n[sessions]\nname = "directory"\n' > "$TAW_CONFIG_DIR/config.toml"
if pf_open 'Tab next up' "the switch on"; then
  screen | grep -q 'open a directory' \
    && bad "no section before the filter is opened" "$(screen | grep -m1 'open a directory')" \
    || ok "no section before the filter is opened"
  keys -l "/"
  e2e_wait_screen 'open a directory' 3000 \
    && ok "/ opens the section under the sessions: open a directory" \
    || bad "/ opens the section under the sessions: open a directory" "$(screen | sed -n '2,8p')"
  if screen | grep -qE "$pf_a +zoxide" && screen | grep -qE "$pf_b +zoxide"; then
    ok "both directories are listed under it, tagged zoxide"
  else
    bad "both directories are listed under it, tagged zoxide" "$(screen | grep -F "$pf_root")"
  fi
  pf_header_line=$(screen | grep -n 'open a directory' | head -1 | cut -d: -f1)
  pf_main_line=$(screen | grep -nE '^│[0-9 ] [* -] main( |$)' | head -1 | cut -d: -f1)
  pf_dir_line=$(screen | grep -nF "$pf_a" | grep -v preview | head -1 | cut -d: -f1)
  if [ -n "$pf_main_line" ] && [ -n "$pf_header_line" ] && [ "$pf_main_line" -lt "$pf_header_line" ] && [ "$pf_dir_line" = "$((pf_header_line + 1))" ]; then
    ok "the header is under the sessions and the first directory is right under it"
  else
    bad "the header is under the sessions and the first directory is right under it" "main=$pf_main_line header=$pf_header_line directory=$pf_dir_line"
  fi
  check "the header is dim, and not bold" "$pf_dim" "$(e2e_style_at "$(screenc)" 'open a directory')"
  e2e_wait_cursor '^[│ ]*[0-9]? [* -] main\b' 2000 \
    && ok "right after / the cursor is still on your own row" \
    || bad "right after / the cursor is still on your own row" "$(e2e_popup_cursor_row)"

  # Nothing running matches the name, so the cursor goes to the directory, and the other is gone.
  keys -l "xylo-falcon"
  if e2e_wait_cursor "$pf_a" 3000; then
    ok "a name no session matches puts the cursor on its directory"
    screen | sed -n '2,38p' | grep -qF "$pf_b" \
      && bad "the other directory is not listed for that name" "$(screen | grep -F "$pf_b")" \
      || ok "the other directory is not listed for that name"
    pf_sessions_before=$(tm list-sessions | wc -l | tr -d ' ')
    keys Enter
    check "Enter closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
    check "Enter created one session, named after the directory" "$((pf_sessions_before + 1))" "$(tm list-sessions | wc -l | tr -d ' ')"
    check "that session is in the directory" "$pf_a_real" "$(tm display-message -p -t xylo-falcon '#{session_path}' 2>/dev/null)"
    check "and the client landed there" "xylo-falcon" "$(tm display-message -p '#{client_session}')"
    check "zoxide add recorded the directory" "add $pf_a_real" "$(cat "$pf_log" 2>/dev/null)"
    e2e_wait_bar_restored main
  else
    bad "a name no session matches puts the cursor on its directory" "$(e2e_popup_cursor_row)"
    pf_close
  fi
fi

# The directory has a session now: it is listed all the same, tagged running, and Enter on it
# switches to that session instead of making a second one.
if pf_open 'Tab next up' "a directory with a session"; then
  keys -l "/"; e2e_wait_screen 'open a directory' 3000
  keys -l "xylo-falcon"
  e2e_wait_screen "$pf_a +running" 3000 \
    && ok "a directory with a session is listed, tagged running" \
    || bad "a directory with a session is listed, tagged running" "$(screen | grep -F "$pf_root")"
  # The query names the session, so the cursor is on the session; Down reaches the directory.
  keys Down
  if e2e_wait_cursor "$pf_a +running" 3000; then
    ok "Down moves from the session into the directory below it"
    pf_sessions_before=$(tm list-sessions | wc -l | tr -d ' ')
    keys Enter
    check "Enter on it closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
    check "and the client switched to the session running there" "xylo-falcon" "$(tm display-message -p '#{client_session}')"
    check "and no second session was made" "$pf_sessions_before" "$(tm list-sessions | wc -l | tr -d ' ')"
    e2e_wait_bar_restored main
  else
    bad "Down moves from the session into the directory below it" "$(e2e_popup_cursor_row)"
    pf_close
  fi
fi
tm kill-session -t xylo-falcon 2>/dev/null

# The help says the filter lists them.
if pf_open 'Tab next up' "the help"; then
  keys -l "?"
  e2e_wait_screen 'filter \(fzf syntax\), by state too; directories to open show below' 3000 \
    && ok "help: the filter's line says directories to open show below" \
    || bad "help: the filter's line says directories to open show below" "$(screen | grep -m1 'filter (fzf')"
  pf_close
fi

# --- the switch off (the default): nothing changes anywhere
printf '[sessions]\nname = "directory"\n' > "$TAW_CONFIG_DIR/config.toml"
if pf_open 'Tab next up' "the switch off"; then
  keys -l "/"
  # The filter is open and drawing before "nothing is listed" means anything.
  if e2e_wait_screen '/▏search…' 3000; then
    screen | grep -q 'open a directory' \
      && bad "the switch off: no section after /" "$(screen | grep -m1 'open a directory')" \
      || ok "the switch off: no section after /"
    keys -l "zeb-marlin"
    e2e_wait_screen 'nothing matches' 3000 \
      && ok "the switch off: a directory's name matches nothing" \
      || bad "the switch off: a directory's name matches nothing" "$(screen | sed -n '2,6p')"
    screen | sed -n '2,38p' | grep -qF "$pf_root" \
      && bad "the switch off: no directory is listed" "$(screen | grep -F "$pf_root")" \
      || ok "the switch off: no directory is listed"
  else
    bad "the switch off: the filter opened" "$(screen | sed -n '38,40p')"
  fi
  pf_close
fi

# --- the search preset: the filter is open from the first frame, and so are the directories
printf '[keys]\npreset = "search"\n[ui]\nprojects_in_filter = true\n[sessions]\nname = "directory"\n' > "$TAW_CONFIG_DIR/config.toml"
if pf_open 'search…' "the search preset"; then
  if e2e_wait_screen 'open a directory' 3000 && screen | grep -qE "$pf_a +zoxide" && screen | grep -qE "$pf_b +zoxide"; then
    ok "the search preset lists zoxide's directories from its first frame"
  else
    bad "the search preset lists zoxide's directories from its first frame" "$(screen | sed -n '2,8p')"
  fi
  keys -l "zeb-marlin"
  if e2e_wait_cursor "$pf_b" 3000; then
    ok "typing a name puts the cursor on its directory"
    keys Enter
    check "Enter closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
    check "the search preset created the session in that directory" "$pf_b_real" "$(tm display-message -p -t zeb-marlin '#{session_path}' 2>/dev/null)"
    check "and the client landed there" "zeb-marlin" "$(tm display-message -p '#{client_session}')"
    e2e_wait_bar_restored main
  else
    bad "typing a name puts the cursor on its directory" "$(e2e_popup_cursor_row)"
    pf_close
  fi
fi

tm kill-session -t zeb-marlin 2>/dev/null
tm kill-session -t xylo-falcon 2>/dev/null
tm switch-client -t main 2>/dev/null
tm set-environment -g PATH "$pf_path_saved"
rm -f "$TAW_CONFIG_DIR/config.toml"
rm -rf "${pf_root:?}"
tm select-window -t main:0 2>/dev/null

echo "-- popup: 16d) the filter's directories stay at the bottom of a tall tree, and a long path keeps its tag and its name"
# 16c's world has a handful of sessions, so the section fits under them.
# Here the tree is taller than the popup's panel, which is when the section could be scrolled
# off the screen after `/`, and the only directory zoxide knows is too deep for its row, so the row
# could be cut at the panel's edge, losing its tag and its own name. The section is pinned to
# the panel's foot, and a path that does not fit loses its middle. The fake zoxide is 16c's
# again, reached through the server's global PATH; the fillers are real sessions in /.
pd_root=$(mktemp -d /tmp/taw-pd.XXXXXX 2>/dev/null) || pd_root="$DIR/pd"
pd_leaf="leaf-of-the-path"
pd_dir="$pd_root/some/deeply/nested/worktrees/$pd_leaf"; mkdir -p "$pd_dir"
pd_fakebin="$DIR/pd-fakebin"; mkdir -p "$pd_fakebin"
cat > "$pd_fakebin/zoxide" <<FAKEZOXIDE
#!/bin/sh
case "\$1" in
  add) : ;;
  query) printf '%s\n' "$pd_dir" ;;
esac
FAKEZOXIDE
chmod +x "$pd_fakebin/zoxide"
pd_path_saved=$(tm show-environment -g PATH 2>/dev/null | sed 's/^PATH=//')
tm set-environment -g PATH "$pd_fakebin:$pd_path_saved"
for pd_n in $(seq 1 44); do tm new-session -d -s "pd-fill-$pd_n" -c / 2>/dev/null; done
printf '[ui]\nprojects_in_filter = true\n[sessions]\nname = "directory"\n' > "$TAW_CONFIG_DIR/config.toml"
if pf_open 'Tab next up' "a tree taller than the popup"; then
  # The tree must really overflow, or the section fitting under it proves nothing.
  pd_shown=$(screen | grep -c 'pd-fill-')
  [ "$pd_shown" -gt 0 ] && [ "$pd_shown" -lt 44 ] \
    && ok "the tree is taller than the popup ($pd_shown of 44 fillers on screen)" \
    || bad "the tree is taller than the popup" "$pd_shown of 44 fillers on screen"
  keys -l "/"
  if e2e_wait_screen 'open a directory' 3000; then
    ok "/ shows the section although the tree fills the panel"
    pd_border=$(screen | grep -n '^╰' | head -1 | cut -d: -f1)
    pd_row=$(screen | grep -n "$pd_leaf" | head -1 | cut -d: -f1)
    pd_header=$(screen | grep -n 'open a directory' | head -1 | cut -d: -f1)
    if [ -n "$pd_row" ] && [ "$pd_row" = "$((pd_header + 1))" ] && [ "$pd_border" = "$((pd_row + 1))" ]; then
      ok "the header and the directory are the panel's last two lines, just above its border"
    else
      bad "the header and the directory are the panel's last two lines, just above its border" "header=$pd_header row=$pd_row border=$pd_border"
    fi
    screen | sed -n "${pd_row}p" | grep -qE "/…/$pd_leaf +zoxide" \
      && ok "a path too deep for its row loses its middle and keeps its last component and its tag" \
      || bad "a path too deep for its row loses its middle and keeps its last component and its tag" "$(screen | sed -n "${pd_row}p")"
    screen | sed -n "${pd_row}p" | grep -qE '^│ +/tmp/taw-pd\.' \
      && ok "and its start" \
      || bad "and its start" "$(screen | sed -n "${pd_row}p")"
    e2e_wait_cursor '^[│ ]*[0-9]? [* -] main\b' 2000 \
      && ok "the cursor is on your own row, on screen above the section" \
      || bad "the cursor is on your own row, on screen above the section" "$(e2e_popup_cursor_row)"
  else
    bad "/ shows the section although the tree fills the panel" "$(screen | sed -n '30,39p')"
  fi
  pf_close
fi
for pd_n in $(seq 1 44); do tm kill-session -t "pd-fill-$pd_n" 2>/dev/null; done
tm switch-client -t main 2>/dev/null
tm set-environment -g PATH "$pd_path_saved"
rm -f "$TAW_CONFIG_DIR/config.toml"
rm -rf "${pd_root:?}"
tm select-window -t main:0 2>/dev/null

echo "-- popup: 16e) the filter's directories sit just under a short tree, not at the panel's foot"
# The section sits just below the sessions, not at the bottom. Two sessions on a tall panel: the
# header is on the second line after the last session's line, with a blank line between them, and
# the directories follow it; the panel's last lines are empty. (16d is the other side of it: a tree
# taller than the panel leaves the section at the foot.) The fake zoxide prints what the real
# `zoxide query` prints, an absolute path a line, and is reached through the server's global PATH
# like 16c's. The second session sorts after main whichever way tmux lists sessions, so it is the
# tree's last line.
pe_root=$(mktemp -d /tmp/taw-pe.XXXXXX 2>/dev/null) || pe_root="$DIR/pe"
pe_a="$pe_root/one-falcon"; mkdir -p "$pe_a"
pe_b="$pe_root/two-marlin"; mkdir -p "$pe_b"
pe_fakebin="$DIR/pe-fakebin"; mkdir -p "$pe_fakebin"
cat > "$pe_fakebin/zoxide" <<FAKEZOXIDE
#!/bin/sh
case "\$1" in
  add) : ;;
  query) printf '%s\n' "$pe_a" "$pe_b" ;;
esac
FAKEZOXIDE
chmod +x "$pe_fakebin/zoxide"
pe_path_saved=$(tm show-environment -g PATH 2>/dev/null | sed 's/^PATH=//')
tm set-environment -g PATH "$pe_fakebin:$pe_path_saved"
tm list-sessions -F '#S' | grep -vx main | while read -r pe_s; do tm kill-session -t "$pe_s" 2>/dev/null; done
# No agent anywhere either (17 sweeps them next, the same way), so each session is one line.
while read -r pe_stray; do
  [ -n "$pe_stray" ] && tm kill-window -t "$pe_stray" 2>/dev/null
done < <(tm list-panes -a -F '#{pane_id} #{@agent_kind}' | awk 'NF>1{print $1}')
tm new-session -d -s pe-second -c / 2>/dev/null
printf '[ui]\nprojects_in_filter = true\n[sessions]\nname = "directory"\n' > "$TAW_CONFIG_DIR/config.toml"
check "the world is two sessions" "2" "$(tm list-sessions | wc -l | tr -d ' ')"
if pf_open 'Tab next up' "a short tree"; then
  keys -l "/"
  if e2e_wait_screen 'open a directory' 3000; then
    pe_header=$(screen | grep -n 'open a directory' | head -1 | cut -d: -f1)
    pe_border=$(screen | grep -n '^╰' | head -1 | cut -d: -f1)
    check "the last session is on the second line above the header" "1" "$(screen | sed -n "$((pe_header - 2))p" | grep -c 'pe-second')"
    check "and the line between them is blank" "1" "$(screen | sed -n "$((pe_header - 1))p" | grep -cE '^│ +│')"
    check "the first directory is right under the header" "$((pe_header + 1))" "$(screen | grep -nF "$pe_a" | grep -v preview | head -1 | cut -d: -f1)"
    check "and the second under that" "$((pe_header + 2))" "$(screen | grep -nF "$pe_b" | grep -v preview | head -1 | cut -d: -f1)"
    # Not on the panel's last lines: at the foot the header would be 3 lines above the border.
    [ -n "$pe_border" ] && [ $((pe_border - pe_header)) -ge 10 ] \
      && ok "the section is nowhere near the panel's foot ($((pe_border - pe_header)) lines above the border)" \
      || bad "the section is nowhere near the panel's foot" "header=$pe_header border=$pe_border"
    check "the lines under the section are empty" "0" "$(screen | sed -n "$((pe_header + 3)),$((pe_border - 1))p" | grep -cE '^│[^│]*[^ │]')"
  else
    bad "/ shows the section under two sessions" "$(screen | sed -n '2,12p')"
  fi
  pf_close
fi
tm kill-session -t pe-second 2>/dev/null
tm switch-client -t main 2>/dev/null
tm set-environment -g PATH "$pe_path_saved"
rm -f "$TAW_CONFIG_DIR/config.toml"
rm -rf "${pe_root:?}"
tm select-window -t main:0 2>/dev/null

echo "-- popup: 17) Enter on your own row just closes the popup"
# Enter goes to the row under the cursor, and on your own row that means the
# popup closes and nothing else happens (no hop to what pinged or to the next
# row that needs you). 17a is the plain case, with nothing needing you
# anywhere; 17b adds an agent blocked in another session. Sweep every agent pane the
# sections above left behind, server-wide, so 17a's "nothing needs you" is true rather than
# assumed.
while read -r pp17_stray; do
  [ -n "$pp17_stray" ] && tm kill-window -t "$pp17_stray" 2>/dev/null
done < <(tm list-panes -a -F '#{pane_id} #{@agent_kind}' | awk 'NF>1{print $1}')
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
pp17_session=$(tm display-message -p '#{client_session}')
pp17_window=$(tm display-message -p '#{window_id}')
pp17_pane=$(tm display-message -p '#{pane_id}')

e2e_ui_open; e2e_wait_screen 'Tab next up' 3000
# Where Enter is pressed, asserted rather than assumed: main:0's shell is not drawn at the
# agents depth, so your own row is main's header.
e2e_wait_cursor '^[│ ]*[0-9]? [* -] main\b' 2000 \
  && ok "the cursor opens on your own row, main's header" \
  || bad "the cursor opens on your own row, main's header" "$(e2e_popup_cursor_row)"
e2e_wait_screen 'Enter go' 2000 \
  && ok "the hint says Enter go on your own row" \
  || bad "the hint says Enter go on your own row" "$(screen | sed -n '39p')"
keys Enter
check "Enter on your own row with nothing hot closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
check "the client stayed on its own session" "$pp17_session" "$(tm display-message -p '#{client_session}')"
check "that session's active window is unchanged" "$pp17_window" "$(tm display-message -p '#{window_id}')"
check "that window's active pane is unchanged" "$pp17_pane" "$(tm display-message -p '#{pane_id}')"

echo "-- popup: 17b) ... and still just closes when an agent needs you"
tm new-session -d -s pp17-b -c "$DIR/home" 2>/dev/null
pp17_agent=$(e2e_start_agent claude pp17-b)
e2e_wait_opt "$pp17_agent" @agent_kind claude 4000 >/dev/null
e2e_wait_opt "$pp17_agent" @agent_state idle 7000 >/dev/null
pp17_fpid=$(tm display-message -p -t "$pp17_agent" '#{pane_pid}')
pp17_apid=$(pgrep -P "$pp17_fpid" -f claude 2>/dev/null | head -1); pp17_apid="${pp17_apid:-$pp17_fpid}"
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
kill -USR2 "$pp17_apid" 2>/dev/null
e2e_wait_opt "$pp17_agent" @agent_state blocked 3000 >/dev/null
e2e_ui_open; e2e_wait_screen 'Tab next up' 3000
# The popup has to know the agent needs you, or "Enter switched nothing" below would pass
# on an Enter that had nowhere to walk to: the tree's border chip counts it.
screen | sed -n '1p' | grep -q '⏸1 needs you' \
  && ok "the popup counts the blocked agent: ⏸1 needs you" \
  || bad "the popup counts the blocked agent: ⏸1 needs you" "$(screen | sed -n '1p')"
e2e_wait_cursor '^[│ ]*[0-9]? [* -] main\b' 2000 \
  && ok "the cursor is on your own row, main's header" \
  || bad "the cursor is on your own row, main's header" "$(e2e_popup_cursor_row)"
e2e_wait_screen 'Enter go' 2000 \
  && ok "the hint still says Enter go" \
  || bad "the hint still says Enter go" "$(screen | sed -n '39p')"
screen | sed -n '39p' | grep -q 'pp17-b' \
  && bad "the hint names no session to walk to" "$(screen | sed -n '39p')" \
  || ok "the hint names no session to walk to"
keys Enter
check "Enter on your own row closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
check "Enter switched nothing even though an agent needs you" "$pp17_session" "$(tm display-message -p '#{client_session}')"
check "the active pane is unchanged" "$pp17_pane" "$(tm display-message -p '#{pane_id}')"
# Landing on the agent would mark it seen through the focus hooks a moment later: allow
# that moment before calling it unseen.
e2e_wait_opt "$pp17_agent" @agent_seen 1 1500 \
  && bad "the blocked agent is still unseen" "@agent_seen became 1" \
  || ok "the blocked agent is still unseen"

echo "-- popup: 18) the sessions depth folds the agents onto their header"
printf '[ui]\ndepth = "sessions"\n' > "$TAW_CONFIG_DIR/config.toml"
# Leave pp17-b on a window that is *not* the agent's, so "Enter switched to the session,
# not to the agent" is a distinguishable outcome rather than the state it was already in.
tm select-window -t pp17-b:0 2>/dev/null
pp18_window=$(tm list-windows -t pp17-b -F '#{window_index} #{window_active}' | awk '$2==1{print $1}')
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
e2e_ui_open; e2e_wait_screen 'Tab next up' 3000
pp18_header=$(screen | grep -m1 'pp17-b')
echo "$pp18_header" | grep -q '⏸1' \
  && ok "the folded header counts its blocked agent" \
  || bad "the folded header counts its blocked agent" "$pp18_header"
echo "$pp18_header" | grep -q 'fake task' \
  && ok "the folded header carries that agent's own title" \
  || bad "the folded header carries that agent's own title" "$pp18_header"
# Where Tab starts from, asserted rather than assumed: a cursor that had already opened on
# pp17-b would make the Tab below pass without Tab having done anything.
pp18_cursor=$(e2e_popup_cursor_row)
echo "$pp18_cursor" | grep -qE '^[│ ]*[0-9]? [* -] main\b' \
  && ok "the cursor opens on your own session's folded header" \
  || bad "the cursor opens on your own session's folded header" "$pp18_cursor"
keys Tab
e2e_wait_cursor 'pp17-b' 2000 \
  && ok "Tab walks from your own row to the header that needs you" \
  || bad "Tab walks from your own row to the header that needs you" "$(e2e_popup_cursor_row)"
keys Enter
check "Enter on a folded header closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
pp18_i=0
while [ "$pp18_i" -lt 25 ] && [ "$(tm display-message -p '#{client_session}')" != "pp17-b" ]; do
  command sleep 0.2; pp18_i=$((pp18_i + 1))
done
check "Enter on a folded header switches to the session" "pp17-b" "$(tm display-message -p '#{client_session}')"
check "pp17-b's active window is the one it was left on" "$pp18_window" \
  "$(tm list-windows -t pp17-b -F '#{window_index} #{window_active}' | awk '$2==1{print $1}')"
if [ "$(tm display-message -p '#{pane_id}')" = "$pp17_agent" ]; then
  bad "Enter on a folded header chose no pane" "landed on the agent's own pane $pp17_agent"
else
  ok "Enter on a folded header chose no pane"
fi
rm -f "$TAW_CONFIG_DIR/config.toml"

echo "-- popup: 19) a number typed in the filter puts the cursor on its session, and hides what does not match"
# A digit in the filter is text like any other character, and the filter also
# matches the numbers drawn, and a query that is exactly a session's number puts the cursor on its
# header; a header's folder, never drawn, is not matched. The digit typed is pp17-b's own number.
# Two sessions of this check's own, both at `/`, a path with no digit in it: pp-plain, with none in
# its name either, which the digit's text has to hide; and one whose name starts with the digit,
# which it has to keep. pp-plain's own number is another digit, so nothing it draws holds the typed
# one.
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
tm new-session -d -s pp-plain -c / 2>/dev/null
# Labels stop at nine (check 12's own guard): drop the scratch sessions earlier sections
# left behind if pp17-b would otherwise sort past the ninth and carry no digit at all -- the
# eighth, here, since the session made below sorts before pp17-b and moves it down one.
pp19_n=$(tm list-sessions -F '#S' | grep -n '^pp17-b$' | cut -d: -f1)
if [ "$pp19_n" -gt 8 ]; then
  tm list-sessions -F '#S' | grep -vE '^(main|pp17-b|pp-plain)$' | while read -r pp19_s; do tm kill-session -t "$pp19_s" 2>/dev/null; done
  pp19_n=$(tm list-sessions -F '#S' | grep -n '^pp17-b$' | cut -d: -f1)
fi
pp19_n=$((pp19_n + 1))
pp19_digit="${pp19_n}x-pp19"
tm new-session -d -s "$pp19_digit" -c / 2>/dev/null
e2e_ui_open; e2e_wait_screen 'Tab next up' 3000
# The number typed is pp17-b's as the popup draws it; and both sessions are confirmed drawn
# before the filter: "kept" or "hidden", read off a session that was never drawn in the
# first place, would prove nothing.
screen | grep -qE "^│${pp19_n} [* -] pp17-b( |\$)" \
  && ok "pp17-b carries number $pp19_n" \
  || bad "pp17-b carries number $pp19_n" "$(screen | grep -m1 ' pp17-b')"
if screen | grep -qF " $pp19_digit" && screen | grep -q ' pp-plain'; then
  ok "both sessions are drawn before the filter"
else
  bad "both sessions are drawn before the filter" "$(screen | sed -n '2,12p')"
fi
keys -l "/"
e2e_wait_screen 'search…' 2000
keys "$pp19_n"
# While the filter is open, its input starts the hint line: `/`, what has been typed, then the
# input's cursor. Only the typed digit draws `/<digit>▏` there.
e2e_wait_screen "^ /${pp19_n}▏" 2000 \
  && ok "the digit lands in the filter's input" \
  || bad "the digit lands in the filter's input" "$(screen | sed -n '39p')"
screen | grep -qF " $pp19_digit" \
  && ok "a session with the digit in its name stays drawn" \
  || bad "a session with the digit in its name stays drawn" "$(screen | sed -n '2,12p')"
if screen | grep -q ' pp-plain'; then
  bad "a session with no digit anywhere is hidden" "$(screen | grep -m1 ' pp-plain')"
else
  ok "a session with no digit anywhere is hidden"
fi
# The digit is pp17-b's number, so the cursor goes to pp17-b's header.
e2e_wait_cursor "^│${pp19_n} [* -] pp17-b( |\$)" 2000 \
  && ok "the typed number puts the cursor on pp17-b's header" \
  || bad "the typed number puts the cursor on pp17-b's header" "$(e2e_popup_cursor_row)"
keys Escape
e2e_wait_screen ' pp-plain' 2000 \
  && ok "Esc clears the query and the session with no digit comes back" \
  || bad "Esc clears the query and the session with no digit comes back" "$(screen | sed -n '2,12p')"
taw popup close >/dev/null 2>&1; e2e_wait_popup_closed; e2e_wait_bar_restored main
tm kill-session -t pp-plain 2>/dev/null
tm kill-session -t "$pp19_digit" 2>/dev/null
tm kill-window -t "$pp17_agent" 2>/dev/null
tm kill-session -t pp17-b 2>/dev/null
tm select-window -t main:0 2>/dev/null

echo "-- popup: 20) the tree line of the session you came from is drawn in the accent colour"
# One idle agent in main, one in a session of its own. Each agent's window is renamed so its
# row can be found by name (the where column reads <index>:<name>), and neither agent needs
# you, so nothing but whose session a row is in can colour its connector. The colours are the
# default theme's: accent #89b4fa (137;180;250), dim #7f849c (127;132;156). e2e_style_at
# follows every escape rather than reading only the one run in front of the glyph
# (e2e_sgr_before), which a bold carried over from the gutter's letter column could otherwise
# fool.
# The styles of both agents' connectors and main's *: each row found by line number (from the
# plain capture, where a pattern anchored on the line's start still works) and then narrowed
# to that one line of the coloured capture with sed -- a pattern matched straight against the
# coloured text would not, since an escape can land between the border and what follows it,
# breaking an anchor like ^ that expects the very next byte to be a digit or a space.
pp20_read() {
  pp20_own_n=$(screen | grep -n ':own20' | head -1 | cut -d: -f1)
  pp20_oth_n=$(screen | grep -n ':oth20' | head -1 | cut -d: -f1)
  pp20_hdr_n=$(screen | grep -nE '^│[0-9 ] \* main( |$)' | head -1 | cut -d: -f1)
  [ -n "$pp20_own_n" ] && [ -n "$pp20_oth_n" ] && [ -n "$pp20_hdr_n" ] || return 1
  pp20_c=$(screenc)
  pp20_own_sgr=$(e2e_style_at "$(printf '%s\n' "$pp20_c" | sed -n "${pp20_own_n}p")" '└')
  pp20_oth_sgr=$(e2e_style_at "$(printf '%s\n' "$pp20_c" | sed -n "${pp20_oth_n}p")" '└')
  pp20_hdr_sgr=$(e2e_style_at "$(printf '%s\n' "$pp20_c" | sed -n "${pp20_hdr_n}p")" '*')
}
tm new-session -d -s pp20-o -c "$DIR/home" 2>/dev/null
pp20_own=$(e2e_start_agent claude main); tm rename-window -t "$pp20_own" own20
pp20_oth=$(e2e_start_agent claude pp20-o); tm rename-window -t "$pp20_oth" oth20
e2e_wait_opt "$pp20_own" @agent_state idle 7000 >/dev/null
e2e_wait_opt "$pp20_oth" @agent_state idle 7000 >/dev/null
# The other session's number, for moving the cursor there below. Labels stop at nine (check
# 12's own guard): drop the scratch sessions earlier sections left if pp20-o is past it.
pp20_n=$(tm list-sessions -F '#S' | grep -n '^pp20-o$' | cut -d: -f1)
if [ "$pp20_n" -gt 9 ]; then
  tm list-sessions -F '#S' | grep -vE '^(main|pp20-o)$' | while read -r pp20_s; do tm kill-session -t "$pp20_s" 2>/dev/null; done
  pp20_n=$(tm list-sessions -F '#S' | grep -n '^pp20-o$' | cut -d: -f1)
fi
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
# The popup first, then the rows: main's own status line lists main's windows by name
# (`1:own20`) until the bar replaces it, so a bare name on screen proves nothing.
e2e_ui_open; e2e_wait_popup_open; e2e_wait_screen '[0-9]+:oth20 +idle' 3000
if pp20_read; then
  ok "both agents' rows and main's header are drawn"
  check "your own session's connector is drawn in the accent colour, bold" "137;180;250 1" "$pp20_own_sgr"
  check "another session's connector stays dim" "127;132;156 0" "$pp20_oth_sgr"
  check "your own session's * is drawn in the accent colour, bold" "137;180;250 1" "$pp20_hdr_sgr"
else
  bad "both agents' rows and main's header are drawn" "own row [$pp20_own_n] other row [$pp20_oth_n] header [$pp20_hdr_n]: $(screen | sed -n '2,8p')"
fi
# The colour follows the session you came from, not the cursor: with the cursor moved onto
# the other session's header by its number, main's line keeps the accent and the line of
# the session under the cursor stays dim.
keys "$pp20_n"
e2e_wait_cursor 'pp20-o' 2000 \
  && ok "number $pp20_n puts the cursor on the other session's header" \
  || bad "the number puts the cursor on the other session's header" "$(e2e_popup_cursor_row)"
if pp20_read; then
  check "with the cursor elsewhere, your own session's connector keeps the accent" "137;180;250 1" "$pp20_own_sgr"
  check "the connector of the session under the cursor stays dim" "127;132;156 0" "$pp20_oth_sgr"
  check "with the cursor elsewhere, your own session's * keeps the accent" "137;180;250 1" "$pp20_hdr_sgr"
else
  bad "the rows are still drawn with the cursor moved" "own row [$pp20_own_n] other row [$pp20_oth_n] header [$pp20_hdr_n]: $(screen | sed -n '2,8p')"
fi
# Your own session's open window is white too, not the accent: with the shells drawn
# (l), main's first row is the window the popup was opened from, main:0's shell, and it hangs on a
# white connector, while its other window, own20, keeps the accent line read above. With the shells
# drawn own20's row is not main's last, so its connector is a tee or a corner.
keys l
e2e_wait_screen '▢ ' 2000
pp20_conn() { local sgr; sgr=$(e2e_style_at "$1" '├'); [ -n "$sgr" ] || sgr=$(e2e_style_at "$1" '└'); printf '%s' "$sgr"; }
if pp20_read; then
  check "your own session's open window hangs on a white connector" "205;214;244 1" "$(pp20_conn "$(printf '%s\n' "$pp20_c" | sed -n "$((pp20_hdr_n + 1))p")")"
  check "and its other window keeps the accent line" "137;180;250 1" "$(pp20_conn "$(printf '%s\n' "$pp20_c" | sed -n "${pp20_own_n}p")")"
else
  bad "your own session's rows are drawn with the shells shown" "own row [$pp20_own_n] header [$pp20_hdr_n]: $(screen | sed -n '2,8p')"
fi
keys q; e2e_wait_popup_closed
tm kill-window -t "$pp20_own" 2>/dev/null
tm kill-session -t pp20-o 2>/dev/null

echo "-- popup: 21) the flat list puts an idle agent above a working one"
# Past the tier, the flat list breaks ties by the newest state change, then by tmux order.
# Both favour the working agent here, so only the tier (idle before
# working) can put the idle one first. The working agent is started first, so tmux order
# lists it first; and it turns working only once both agents read idle and the clock has
# moved past the idle agent's stamp (`@agent_state_since` counts whole seconds), so its
# state change is the newest.
pp21_work=$(e2e_start_agent claude main); tm rename-window -t "$pp21_work" work21
pp21_idle=$(e2e_start_agent claude main); tm rename-window -t "$pp21_idle" idle21
e2e_wait_opt "$pp21_work" @agent_state idle 7000 >/dev/null
e2e_wait_opt "$pp21_idle" @agent_state idle 7000 >/dev/null
pp21_is=$(tm display-message -p -t "$pp21_idle" '#{@agent_state_since}')
pp21_waited=0
while [ "$(date +%s)" -le "${pp21_is:-0}" ] && [ "$pp21_waited" -lt 3000 ]; do
  command sleep 0.1; pp21_waited=$((pp21_waited + 100))
done
pp21_wfpid=$(tm display-message -p -t "$pp21_work" '#{pane_pid}')
pp21_wapid=$(pgrep -P "$pp21_wfpid" -f claude 2>/dev/null | head -1)
kill -USR1 "${pp21_wapid:-$pp21_wfpid}" 2>/dev/null   # the fixture's idle -> working
e2e_wait_opt "$pp21_work" @agent_state working 3000 >/dev/null
pp21_ws=$(tm display-message -p -t "$pp21_work" '#{@agent_state_since}')
pp21_wi=$(tm display-message -p -t "$pp21_work" '#{window_index}')
pp21_ii=$(tm display-message -p -t "$pp21_idle" '#{window_index}')
[ "$pp21_wi" -lt "$pp21_ii" ] \
  && ok "tmux order has the working agent first" \
  || bad "tmux order has the working agent first" "working in window $pp21_wi, idle in window $pp21_ii"
[ "${pp21_ws:-0}" -gt "${pp21_is:-0}" ] \
  && ok "the working agent's state change is the newer one" \
  || bad "the working agent's state change is the newer one" "working since [$pp21_ws], idle since [$pp21_is]"
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
e2e_ui_open; e2e_wait_popup_open; e2e_wait_screen '[0-9]+:idle21 +idle' 3000
keys f
e2e_wait_screen ' flat ╯' 2000 \
  && ok "f turns the tree into the flat list" \
  || bad "f turns the tree into the flat list" "$(screen | sed -n '38p')"
pp21_idle_row=$(screen | grep ':idle21' | head -1)
pp21_work_row=$(screen | grep ':work21' | head -1)
echo "$pp21_idle_row" | grep -q ' idle ' && echo "$pp21_work_row" | grep -q ' working ' \
  && ok "the two rows read idle and working" \
  || bad "the two rows read idle and working" "idle row [$pp21_idle_row] working row [$pp21_work_row]"
pp21_idle_n=$(screen | grep -n ':idle21' | head -1 | cut -d: -f1)
pp21_work_n=$(screen | grep -n ':work21' | head -1 | cut -d: -f1)
[ -n "$pp21_idle_n" ] && [ -n "$pp21_work_n" ] && [ "$pp21_idle_n" -lt "$pp21_work_n" ] \
  && ok "the flat list puts the idle agent above the working one" \
  || bad "the flat list puts the idle agent above the working one" "idle on row [$pp21_idle_n], working on row [$pp21_work_n]"
keys f
e2e_wait_screen ' agents ╯' 2000 \
  && ok "f again turns the flat list back into the tree" \
  || bad "f again turns the flat list back into the tree" "$(screen | sed -n '38p')"

echo "-- popup: 22) f at the sessions depth leaves the screen unchanged"
keys h
e2e_wait_screen ' sessions ╯' 2000 \
  && ok "h steps up to the sessions depth" \
  || bad "h steps up to the sessions depth" "$(screen | sed -n '38p')"
# watch_rows_unchanged is defined at check 11, its first user.
# From a screen that has stopped changing on its own first: a session's git branch is read
# off-thread and lands on its header a moment after the rows are drawn -- the worktree
# section's session, here, whose branch otherwise arrives during the watch below.
e2e_wait_still || bad "check 22: the screen settles before the watch" "$(screen | sed -n '38,39p')"
pp22_before=$(screen | sed -n '1,39p')
keys f
watch_rows_unchanged "f changes nothing on screen at the sessions depth" "$pp22_before"
screen | grep -q '^│[0-9 ] \* main ' \
  && ok "the sessions stay listed" \
  || bad "the sessions stay listed" "$(screen | sed -n '2,6p')"
# f has flipped the layout all the same, out of sight at this depth; check 27
# presses l to see it.
keys q; e2e_wait_popup_closed
tm kill-window -t "$pp21_work" 2>/dev/null
tm kill-window -t "$pp21_idle" 2>/dev/null

echo "-- popup: 23) the preview wraps a long line, and cuts one from a full-screen program"
# A window of main's own, and main's active pane: the popup opens from its shell, which the
# agents depth does not draw, so the cursor sits on main's header -- and a header previews
# its session's active pane, following that pane's alternate-screen flag. The line is ten
# columns short of the pane's width, so the pane shows it on one row of its own and only the
# preview, narrower than the pane by at least the tree panel, can break it.
pp23_pane=$(tm new-window -t main -P -F '#{pane_id}' -c "$DIR/home")
pp23_len=$(( $(tm display-message -p -t "$pp23_pane" '#{pane_width}') - 10 ))
cat > "$DIR/pp23-wide.sh" <<SCRIPT
printf '\\033[2J\\033[H'
i=0; while [ "\$i" -lt $pp23_len ]; do printf W; i=\$((i + 1)); done; printf '\\n'
SCRIPT
tm send-keys -t "$pp23_pane" "sh '$DIR/pp23-wide.sh'" Enter
e2e_wait_pane "$pp23_pane" 'W{10}' 3000
check "the pane shows the $pp23_len-column line on one row" "1" "$(tm capture-pane -p -t "$pp23_pane" | grep -c 'WWWWWWWWWW')"
check "the pane is not in the alternate screen" "0" "$(tm display-message -p -t "$pp23_pane" '#{alternate_on}')"
tm switch-client -t main 2>/dev/null
tm select-window -t "$pp23_pane" 2>/dev/null
# The pane itself shows the line until the popup covers it: wait for the popup, then read
# only rows that start at the preview panel's own left edge (`││`: the tree panel's right
# border, then the preview's left one).
e2e_ui_open; e2e_wait_popup_open
e2e_wait_cursor '^[│ ]*[0-9]? [* -] main\b' 2000 \
  && ok "the cursor opens on main's header, which previews that pane" \
  || bad "the cursor opens on main's header, which previews that pane" "$(e2e_popup_cursor_row)"
# Counted once the preview has drawn the line, polled so a frame caught half-drawn is
# never what gets asserted on.
pp23_waited=0
while [ "$pp23_waited" -lt 3000 ]; do
  pp23_rows=$(screen | sed -n '1,39p' | grep -cE '││W{10}')
  pp23_total=$(screen | sed -n '1,39p' | grep -E '││W' | grep -oE 'W{2,}' | awk '{ n += length($0) } END { print n + 0 }')
  [ "$pp23_rows" -ge 2 ] && [ "$pp23_total" -eq "$pp23_len" ] && break
  command sleep 0.1; pp23_waited=$((pp23_waited + 100))
done
[ "$pp23_rows" -ge 2 ] \
  && ok "a line wider than the preview wraps onto more than one preview row" \
  || bad "a line wider than the preview wraps onto more than one preview row" "preview rows with W: $pp23_rows"
check "the wrapped rows carry the whole line, nothing cut" "$pp23_len" "$pp23_total"
keys q; e2e_wait_popup_closed

# A full-screen program: less -S chops its one long line at the pane's width, in the
# alternate screen, and +G opens on the file's last page, so that line sits right above
# less's prompt, inside the preview's bottom-aligned tail. LESS is emptied so a user's -F
# (quit when the file fits) or -X (no alternate screen) cannot leak in, and LESSHISTFILE=-
# keeps less from writing a history file under the real $HOME.
{ pp23_i=1; while [ "$pp23_i" -le 60 ]; do echo "short $pp23_i"; pp23_i=$((pp23_i + 1)); done
  printf '%0300d\n' 0 | tr 0 L; } > "$DIR/pp23-long.txt"
tm send-keys -t "$pp23_pane" "LESS= LESSHISTFILE=- less -S +G '$DIR/pp23-long.txt'" Enter
e2e_wait_opt "$pp23_pane" alternate_on 1 3000 \
  && ok "less runs in the alternate screen" \
  || bad "less runs in the alternate screen" "alternate_on=$(tm display-message -p -t "$pp23_pane" '#{alternate_on}')"
e2e_wait_pane "$pp23_pane" 'L{10}' 3000
check "less shows the 300-column line on one row, cut at the pane's width" "1" "$(tm capture-pane -p -t "$pp23_pane" | grep -c 'LLLLLLLLLL')"
e2e_ui_open; e2e_wait_popup_open; e2e_wait_screen '││L{10}' 3000
# A wrapped continuation would start at the preview's left edge too, so one such row is
# the whole claim; settle briefly first so a row still being drawn is not counted short.
command sleep 0.3
check "the preview cuts a full-screen program's long line, it does not wrap it" "1" "$(screen | sed -n '1,39p' | grep -cE '││L{10}')"
check "the cut line runs right up to the preview's right border" "1" "$(screen | sed -n '1,39p' | grep -cE '││L+│$')"
keys q; e2e_wait_popup_closed
tm kill-window -t "$pp23_pane" 2>/dev/null
tm select-window -t main:0 2>/dev/null

# Checks 24 to 29 start from a quiet server: every session the sections
# above left behind goes, main alone stays, and so does every agent pane. No header on screen
# is then a repository's, whose branch -- read off-thread -- could land in the middle of a
# watch for nothing to change, and every session number these checks press is at most a
# handful.
tm switch-client -t main 2>/dev/null
tm list-sessions -F '#S' | grep -vx main | while read -r pp_s; do tm kill-session -t "$pp_s" 2>/dev/null; done
while read -r pp_stray; do
  [ -n "$pp_stray" ] && tm kill-window -t "$pp_stray" 2>/dev/null
done < <(tm list-panes -a -F '#{pane_id} #{@agent_kind}' | awk 'NF>1{print $1}')
tm select-window -t main:0 2>/dev/null
# Samples popup_open across a bounded window, failing the moment the popup has gone: a key
# that must not close it gives nothing to poll for.
watch_popup_open() {
  local label="$1" window_ms="${2:-800}"; local waited=0
  while [ "$waited" -lt "$window_ms" ]; do
    popup_open || { bad "$label" "the popup closed within $waited ms"; return; }
    command sleep 0.1; waited=$((waited + 100))
  done
  ok "$label"
}
# Every check below ends with the popup gone and main's status line its own again, whatever
# its keys did: a key acting where it must not -- a confirmation left open, which takes every
# key after it, q included -- would otherwise leave that popup up for the next check.
popup_gone() {
  e2e_wait_popup_closed 2000 || { taw popup close >/dev/null 2>&1; e2e_wait_popup_closed; }
  e2e_wait_bar_restored main
}

echo "-- popup: 24) every session's rows wear their letters, and the cursor's session's are lit"
# The letters a to e name the first five rows of the session
# the cursor is in, whatever put it there, each letter right before its row's connector; no
# number comes first, and nothing puts them away. Every session's rows wear
# their letters, and only the cursor's session's are lit (the theme's yellow, bold); the others
# are dim. A row starts two columns in, its letter under its header's * and
# its connector under the session's name, so the first column holds the session numbers alone. Two idle agents in a session of their own, and one in
# main, where the popup opens. Each agent's window is renamed so its row can be found by name
# (the where column reads <index>:<name>).
tm new-session -d -s pp24 -c "$DIR/home" 2>/dev/null
pp24_own=$(e2e_start_agent claude main); tm rename-window -t "$pp24_own" own24
pp24_a=$(e2e_start_agent claude pp24); tm rename-window -t "$pp24_a" one24
pp24_b=$(e2e_start_agent claude pp24); tm rename-window -t "$pp24_b" two24
for pp24_p in "$pp24_own" "$pp24_a" "$pp24_b"; do e2e_wait_opt "$pp24_p" @agent_state idle 7000 >/dev/null; done
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
# The style of the letter on the row of <window>: the row's first a or b is its letter, as only
# the tree panel's border and two blanks come before it. The default theme's yellow is #f9e2af (249;226;175),
# its dim #7f849c (127;132;156).
pp24_letter_style() { e2e_style_at "$(screenc | grep -F ":$1" | head -1)" "$2"; }
pp24_lit="249;226;175 1"; pp24_dim="127;132;156 0"
e2e_ui_open; e2e_wait_popup_open; e2e_wait_screen '[0-9]+:two24 +idle' 3000
e2e_wait_cursor '^│[0-9] \* main( |$)' 2000 \
  && ok "the popup opens on main's header" \
  || bad "the popup opens on main's header" "$(e2e_popup_cursor_row)"
# Every session's number is a key drawn as one, the theme's yellow, bold, on
# every header: main's, where the cursor is, and pp24's.
check "every session's number is yellow and bold, main's and pp24's" "$pp24_lit $pp24_lit" "$(e2e_number_style main) $(e2e_number_style pp24)"
screen | grep ':own24' | grep -q '^│  a └ ' \
  && ok "main's agent wears a, with no number pressed" \
  || bad "main's agent wears a, with no number pressed" "$(screen | grep ':own24')"
check "main's letter is lit: the cursor is in main" "$pp24_lit" "$(pp24_letter_style own24 a)"
screen | grep ':one24' | grep -q '^│  a ├ ' && screen | grep ':two24' | grep -q '^│  b └ ' \
  && ok "pp24's agents wear a and b too" \
  || bad "pp24's agents wear a and b too" "$(screen | grep -E ':(one|two)24')"
check "pp24's letters are dim" "$pp24_dim $pp24_dim" "$(pp24_letter_style one24 a) $(pp24_letter_style two24 b)"
check "no row's letter is in the first column, the session numbers'" "0" "$(screen | grep -cE '^│[a-e] [├└] ')"
e2e_wait_screen '^ 1-9 session  ·  a-e rows  ·  Tab next up' 2000 \
  && ok "the hint names the letters after the numbers in main, one row: a-e rows" \
  || bad "the hint names the letters after the numbers in main, one row: a-e rows" "$(screen | sed -n '39p')"
keys J
e2e_wait_cursor '^│[0-9] [* -] pp24( |$)' 2000 \
  && ok "J puts the cursor on pp24's header" \
  || bad "J puts the cursor on pp24's header" "$(e2e_popup_cursor_row)"
check "the numbers stay yellow and bold with the cursor on pp24" "$pp24_lit $pp24_lit" "$(e2e_number_style main) $(e2e_number_style pp24)"
e2e_wait_screen '^ 1-9 session  ·  a-e rows  ·  Tab next up' 2000 \
  && ok "the hint is the same in pp24, two rows: a-e rows" \
  || bad "the hint is the same in pp24, two rows: a-e rows" "$(screen | sed -n '39p')"
screen | grep ':one24' | grep -q '^│  a ├ ' \
  && ok "pp24's first agent wears a, under its header's *, before its connector" \
  || bad "pp24's first agent wears a, under its header's *, before its connector" "$(screen | grep ':one24')"
screen | grep ':two24' | grep -q '^│  b └ ' \
  && ok "pp24's second agent wears b" \
  || bad "pp24's second agent wears b" "$(screen | grep ':two24')"
check "pp24's letters are lit now, in the theme's yellow, bold" "$pp24_lit $pp24_lit" "$(pp24_letter_style one24 a) $(pp24_letter_style two24 b)"
screen | grep ':own24' | grep -q '^│  a └ ' \
  && ok "main's agent keeps its letter" \
  || bad "main's agent keeps its letter" "$(screen | grep ':own24')"
check "and it is dim now" "$pp24_dim" "$(pp24_letter_style own24 a)"
# pp24 has two rows, so c names none there and says so on the hint line, moving nothing.
keys c
e2e_wait_screen '^ no row c in this session' 2000 \
  && ok "c, with no third row in pp24, says so" \
  || bad "c, with no third row in pp24, says so" "$(screen | sed -n '39p')"
e2e_wait_cursor '^│[0-9] [* -] pp24( |$)' 2000 \
  && ok "and the cursor stays on pp24's header" \
  || bad "and the cursor stays on pp24's header" "$(e2e_popup_cursor_row)"
keys b
e2e_wait_cursor ':two24' 2000 \
  && ok "b puts the cursor on pp24's second agent" \
  || bad "b puts the cursor on pp24's second agent" "$(e2e_popup_cursor_row)"
check "pp24's letters stay lit after the jump" "$pp24_lit" "$(pp24_letter_style one24 a)"
watch_popup_open "the letter only moves the cursor: the popup stays open"
keys Enter
check "Enter then closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
pp24_i=0
while [ "$pp24_i" -lt 25 ] && [ "$(tm display-message -p '#{pane_id}')" != "$pp24_b" ]; do
  command sleep 0.2; pp24_i=$((pp24_i + 1))
done
check "Enter lands in pp24" "pp24" "$(tm display-message -p '#{client_session}')"
check "Enter lands on that agent's own pane" "$pp24_b" "$(tm display-message -p '#{pane_id}')"
popup_gone

echo "-- popup: 25) the lit letters follow the cursor across a session, and Esc quits"
# j from main's one agent onto pp24's header moves the light over with the cursor. Esc, with
# the letters out or not, closes the popup.
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
e2e_ui_open; e2e_wait_popup_open; e2e_wait_screen '[0-9]+:two24 +idle' 3000
keys j
e2e_wait_cursor ':own24' 2000 \
  && ok "j moves onto main's agent" \
  || bad "j moves onto main's agent" "$(e2e_popup_cursor_row)"
screen | grep ':own24' | grep -q '^│  a └ ' \
  && ok "main's agent wears a while the cursor is in main" \
  || bad "main's agent wears a while the cursor is in main" "$(screen | grep ':own24')"
check "main's letter is lit while the cursor is in main" "$pp24_lit" "$(pp24_letter_style own24 a)"
check "pp24's rows wear theirs, dim, while the cursor is in main" "$pp24_dim $pp24_dim" "$(pp24_letter_style one24 a) $(pp24_letter_style two24 b)"
keys j
e2e_wait_cursor '^│[0-9] [* -] pp24( |$)' 2000 \
  && ok "a second j crosses onto pp24's header" \
  || bad "a second j crosses onto pp24's header" "$(e2e_popup_cursor_row)"
e2e_wait_screen '^ 1-9 session  ·  a-e rows  ·  Tab next up' 2000 \
  && ok "the hint reads the same after the cross: a-e rows" \
  || bad "the hint reads the same after the cross: a-e rows" "$(screen | sed -n '39p')"
screen | grep ':one24' | grep -q '^│  a ├ ' && screen | grep ':two24' | grep -q '^│  b └ ' \
  && ok "pp24's two agents wear a and b" \
  || bad "pp24's two agents wear a and b" "$(screen | grep -E ':(one|two)24')"
check "the light crossed with the cursor: pp24's lit, main's dim" "$pp24_lit $pp24_dim" "$(pp24_letter_style one24 a) $(pp24_letter_style own24 a)"
keys Escape
check "Esc closes the popup, the letters out" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
popup_gone
tm kill-window -t "$pp24_own" 2>/dev/null
tm kill-session -t pp24 2>/dev/null

echo "-- popup: 25b) typing a session's number and a letter in the search reaches that row, in both presets"
# Searching by 4A goes to session 4's agent A, and the numbers and letters stay drawn while the
# search runs. Two sessions of this check's own whose names
# sort before every other (tmux numbers the sessions in name order): 0fg-a, a filler, is 1, and
# 0fg-b, with two idle agents in windows onex and twox, is 2. No session, window or command name
# here holds a 2: a row's own words then hold no 2 before an a (the one 2 they may
# hold is a window's index, after claude's a), so nothing but session 2's address in front of a
# row can match 2a, and a search of a row's own words only would leave the
# cursor off row a. The rows' own words are checked for that before 2 is typed. In each preset (the modal one after `/`) typing 2 keeps the
# numbers yellow and bold, puts the cursor on 0fg-b's header and lights its letters; a then puts
# it on 0fg-b's row a (`2A` in the search preset: either case), and Enter lands on that agent's
# own pane. The modal preset's query is on the hint line (` /2▏`), the search preset's on its
# own line (`> 2▏`). A number confirmed on screen first: "on session 2" read off a world where 0fg-b is
# not session 2 would prove nothing.
tm new-session -d -s 0fg-a -c "$DIR/home" 2>/dev/null
tm new-session -d -s 0fg-b -c "$DIR/home" 2>/dev/null
pp25b_a=$(e2e_start_agent claude 0fg-b); tm rename-window -t "$pp25b_a" onex
pp25b_b=$(e2e_start_agent claude 0fg-b); tm rename-window -t "$pp25b_b" twox
for pp25b_p in "$pp25b_a" "$pp25b_b"; do e2e_wait_opt "$pp25b_p" @agent_state idle 7000 >/dev/null; done
pp25b_letter_style() { e2e_style_at "$(screenc | grep -F ":$1" | head -1)" "$2"; }
pp25b_lit="249;226;175 1"; pp25b_dim="127;132;156 0"
for pp25b_preset in modal search; do
  tm switch-client -t main 2>/dev/null
  tm select-window -t main:0 2>/dev/null
  if [ "$pp25b_preset" = search ]; then
    printf '[keys]\npreset = "search"\n' > "$TAW_CONFIG_DIR/config.toml"
    pp25b_letter=A; pp25b_in='> '
  else
    rm -f "$TAW_CONFIG_DIR/config.toml"
    pp25b_letter=a; pp25b_in='^ /'
  fi
  e2e_ui_open; e2e_wait_popup_open; e2e_wait_screen '[0-9]+:twox +idle' 3000
  if ! screen | grep -qE '^│2 [* -] 0fg-b( |$)'; then
    bad "$pp25b_preset: 0fg-b carries number 2" "$(screen | grep -m1 ' 0fg-b')"
    taw popup close >/dev/null 2>&1; e2e_wait_popup_closed; e2e_wait_bar_restored main
    continue
  fi
  ok "$pp25b_preset: 0fg-b carries number 2"
  # The words the search reads in 0fg-b's agent rows, without their letters: the session's
  # name, what (claude), where (a window index and its name) and the state word.
  # The pane title and the elapsed time after the state word are drawn but not searched, and
  # row b's title, `fake task`, holds an a after window 2's index: each line is cut after its
  # state word. Two such rows, ending idle, then none holds a 2 before an a: only the address
  # can match 2a.
  pp25b_words=$(screen | grep -E ':(onex|twox) ' | sed -E 's/^│ +[a-e] (├|└) //; s/ (blocked|done|idle|working|unknown)( .*)?$/ \1/; s/^/0fg-b /')
  check "$pp25b_preset: 0fg-b's two agent rows are read up to their state word" "2" \
    "$(printf '%s\n' "$pp25b_words" | grep -cE '^0fg-b .* [0-9]+:(onex|twox) +idle$')"
  check "$pp25b_preset: no agent row of 0fg-b holds a 2 before an a in its own words" "0" \
    "$(printf '%s\n' "$pp25b_words" | grep -ciE '2.*a')"
  [ "$pp25b_preset" = modal ] && keys -l "/"
  e2e_wait_screen 'search…' 2000
  keys -l "2"
  e2e_wait_screen "${pp25b_in}2▏" 2000 \
    && ok "$pp25b_preset: 2 lands in the search's input" \
    || bad "$pp25b_preset: 2 lands in the search's input" "$(screen | sed -n '39p')"
  e2e_wait_cursor '^│2 [* -] 0fg-b( |$)' 2000 \
    && ok "$pp25b_preset: typing 2 puts the cursor on session 2's header" \
    || bad "$pp25b_preset: typing 2 puts the cursor on session 2's header" "$(e2e_popup_cursor_row)"
  check "$pp25b_preset: while typing, session 2's number is yellow and bold" "$pp25b_lit" "$(e2e_number_style 0fg-b)"
  screen | grep ':onex' | grep -q '^│  a ├ ' && screen | grep ':twox' | grep -q '^│  b └ ' \
    && ok "$pp25b_preset: while typing, 0fg-b's agents wear a and b" \
    || bad "$pp25b_preset: while typing, 0fg-b's agents wear a and b" "$(screen | grep -E ':(onex|twox)')"
  check "$pp25b_preset: with its number alone typed, session 2's letters are lit" "$pp25b_lit $pp25b_lit" "$(pp25b_letter_style onex a) $(pp25b_letter_style twox b)"
  keys -l "$pp25b_letter"
  e2e_wait_screen "${pp25b_in}2${pp25b_letter}▏" 2000 \
    && ok "$pp25b_preset: 2$pp25b_letter is the query" \
    || bad "$pp25b_preset: 2$pp25b_letter is the query" "$(screen | sed -n '39p')"
  e2e_wait_cursor ':onex' 2000 \
    && ok "$pp25b_preset: 2$pp25b_letter puts the cursor on session 2's row a" \
    || bad "$pp25b_preset: 2$pp25b_letter puts the cursor on session 2's row a" "$(e2e_popup_cursor_row)"
  screen | grep ':onex' | grep -q '^│  a ├ ' \
    && ok "$pp25b_preset: row a still wears its letter" \
    || bad "$pp25b_preset: row a still wears its letter" "$(screen | grep ':onex')"
  # The query is not a number alone, so the letters are text again: b, which it did not
  # match, is drawn unlit.
  check "$pp25b_preset: after 2$pp25b_letter, row b's letter is unlit" "$pp25b_dim" "$(pp25b_letter_style twox b)"
  keys Enter
  check "$pp25b_preset: Enter closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
  pp25b_i=0
  while [ "$pp25b_i" -lt 25 ] && [ "$(tm display-message -p '#{pane_id}')" != "$pp25b_a" ]; do
    command sleep 0.2; pp25b_i=$((pp25b_i + 1))
  done
  check "$pp25b_preset: Enter lands in session 2" "0fg-b" "$(tm display-message -p '#{client_session}')"
  check "$pp25b_preset: Enter lands on row a's own pane" "$pp25b_a" "$(tm display-message -p '#{pane_id}')"
  e2e_wait_bar_restored 0fg-b
done
rm -f "$TAW_CONFIG_DIR/config.toml"
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
tm kill-session -t 0fg-b 2>/dev/null
tm kill-session -t 0fg-a 2>/dev/null

echo "-- popup: 26) each session's open window's rows are lit"
# In every session, the rows of the window it has
# open -- where Enter on its header lands -- are lit whole: the kind column (claude) and the
# place column (<index>:<name>) in the theme's fg, bold, and the state, elapsed time and title
# -- and in the flat list the session name -- in fg at normal weight. Every other row keeps its
# kind in fg, not bold, and its place, state and session name dim. Two idle agents in one session, each in a window of its own; the second agent's
# window is the one that session has open. The default theme's colours: fg #cdd6f4
# (205;214;244), dim #7f849c (127;132;156). fg is also the colour of the space in front of a
# column, so tmux writes no colour there, only the bold: e2e_style_at follows the escapes from
# the top of the capture instead of reading the one run in front of it (e2e_sgr_before).
tm new-session -d -s pp26 -c "$DIR/home" 2>/dev/null
pp26_a=$(e2e_start_agent claude pp26); tm rename-window -t "$pp26_a" one26
pp26_b=$(e2e_start_agent claude pp26); tm rename-window -t "$pp26_b" two26
e2e_wait_opt "$pp26_a" @agent_state idle 7000 >/dev/null
e2e_wait_opt "$pp26_b" @agent_state idle 7000 >/dev/null
tm select-window -t "$pp26_b" 2>/dev/null
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
# The style of <cell> (claude, idle) on the row whose place is <row> (:one26, :two26), read off
# the coloured capture <capture>: e2e_style_at stops at the first <cell>, and every agent row
# says claude and idle, so the rows above that one have theirs upper-cased first. Their
# escapes, which carry a style into the next line, are left as they are.
pp26_cell() {
  local n; n=$(printf '%s\n' "$1" | grep -nF -- "$2" | head -1 | cut -d: -f1)
  [ -n "$n" ] || return 1
  e2e_style_at "$(printf '%s\n' "$1" | awk -v n="$n" -v c="$3" 'NR < n { gsub(c, toupper(c)) } NR <= n')" "$3"
}
e2e_ui_open; e2e_wait_popup_open; e2e_wait_screen '[0-9]+:two26 +idle' 3000
pp26_c=$(screenc)
check "the open window's agent row shows its place in fg, bold" "205;214;244 1" "$(e2e_style_at "$pp26_c" ':two26')"
check "the open window's agent row shows its kind in fg, bold" "205;214;244 1" "$(pp26_cell "$pp26_c" ':two26' claude)"
check "the open window's agent row shows its state in fg, not bold" "205;214;244 0" "$(pp26_cell "$pp26_c" ':two26' idle)"
check "the other window's agent row keeps its place dim" "127;132;156 0" "$(e2e_style_at "$pp26_c" ':one26')"
check "the other window's agent row keeps its kind in fg, not bold" "205;214;244 0" "$(pp26_cell "$pp26_c" ':one26' claude)"
check "the other window's agent row keeps its state dim" "127;132;156 0" "$(pp26_cell "$pp26_c" ':one26' idle)"
# The tree line is the mark you look at: the open window's row hangs on a white
# connector, fg bold, and the other's on a dim one. pp26 is not your session, so no accent comes into
# it. The first agent's row is its session's first child, on a tee connector, and the second's the
# last, on a corner one. As pp26_cell does, the rows above the one asked for have their connectors
# turned into a # first, so that e2e_style_at stops at this row's own.
pp26_conn() {
  local n; n=$(printf '%s\n' "$1" | grep -nF -- "$2" | head -1 | cut -d: -f1)
  [ -n "$n" ] || return 1
  e2e_style_at "$(printf '%s\n' "$1" | awk -v n="$n" 'NR < n { gsub(/├|└/, "#") } NR <= n')" "$3"
}
check "the open window's agent row hangs on a white connector" "205;214;244 1" "$(pp26_conn "$pp26_c" ':two26' '└')"
check "the other window's agent row keeps a dim connector" "127;132;156 0" "$(pp26_conn "$pp26_c" ':one26' '├')"
# The mark follows the window the session has open, not a row's place in the list: with the
# first agent's window selected instead, the popup's next refresh moves it there.
tm select-window -t "$pp26_a" 2>/dev/null
pp26_waited=0
while [ "$pp26_waited" -lt 3000 ] && [ "$(e2e_style_at "$(screenc)" ':one26')" != "205;214;244 1" ]; do
  command sleep 0.1; pp26_waited=$((pp26_waited + 100))
done
pp26_c=$(screenc)
check "with the first window open instead, its row shows its place in fg, bold" "205;214;244 1" "$(e2e_style_at "$pp26_c" ':one26')"
check "with the first window open instead, its row shows its kind in fg, bold" "205;214;244 1" "$(pp26_cell "$pp26_c" ':one26' claude)"
check "and the second window's row goes dim" "127;132;156 0" "$(e2e_style_at "$pp26_c" ':two26')"
check "and the second window's kind is no longer bold" "205;214;244 0" "$(pp26_cell "$pp26_c" ':two26' claude)"
check "with the first window open instead, its row hangs on a white connector" "205;214;244 1" "$(pp26_conn "$pp26_c" ':one26' '├')"
check "and the second window's connector goes dim" "127;132;156 0" "$(pp26_conn "$pp26_c" ':two26' '└')"
# The flat list lights the same rows.
keys f
e2e_wait_screen ' flat ╯' 2000 \
  && ok "f turns the tree into the flat list" \
  || bad "f turns the tree into the flat list" "$(screen | sed -n '38p')"
pp26_c=$(screenc)
check "in the flat list, the open window's row shows its place in fg, bold" "205;214;244 1" "$(e2e_style_at "$pp26_c" ':one26')"
check "in the flat list, the open window's row shows its kind in fg, bold" "205;214;244 1" "$(pp26_cell "$pp26_c" ':one26' claude)"
check "in the flat list, the other window's row keeps its place dim" "127;132;156 0" "$(e2e_style_at "$pp26_c" ':two26')"
check "in the flat list, the other window's row keeps its kind in fg, not bold" "205;214;244 0" "$(pp26_cell "$pp26_c" ':two26' claude)"
check "in the flat list, the open window's row shows its session name in fg" "205;214;244 0" "$(pp26_cell "$pp26_c" ':one26' pp26)"
check "in the flat list, the other window's row keeps its session name dim" "127;132;156 0" "$(pp26_cell "$pp26_c" ':two26' pp26)"
keys q; popup_gone

echo "-- popup: 27) f at the sessions depth flips the layout, and l shows the flat list"
# f flips the layout at every depth. At the sessions depth the rows are the
# headers whatever the layout, so nothing changes on screen there; l, back at the agents
# depth, then shows the flat list. Check 26's two agents, in pp26, are its rows.
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
e2e_ui_open; e2e_wait_popup_open; e2e_wait_screen '[0-9]+:two26 +idle' 3000
check "the popup opens on the tree" "1" "$(screen | sed -n '38p' | grep -c ' agents ╯')"
keys h
e2e_wait_screen ' sessions ╯' 2000 \
  && ok "h steps up to the sessions depth" \
  || bad "h steps up to the sessions depth" "$(screen | sed -n '38p')"
e2e_wait_still || bad "check 27: the screen settles before the watch" "$(screen | sed -n '38,39p')"
pp27_before=$(screen | sed -n '1,39p')
keys f
watch_rows_unchanged "f changes nothing on screen at the sessions depth" "$pp27_before"
keys l
e2e_wait_screen ' flat ╯' 2000 \
  && ok "l then shows the flat list: f flipped the layout" \
  || bad "l then shows the flat list: f flipped the layout" "$(screen | sed -n '38p')"
# The chip is half of it; the rows are the other half: a flat row has no header to sit under,
# so it starts with the number that reaches it, and carries its session's name.
screen | grep ':one26' | grep -qE '^│[0-9] .* pp26 ' \
  && ok "the rows are the flat list's, each with its session's name" \
  || bad "the rows are the flat list's, each with its session's name" "$(screen | grep ':one26')"
# A flat row's number is a key too (it puts the cursor on its row), drawn as a header's is
# yellow, bold, while it works, and with / open too, where typing it reaches its
# row.
check "a flat row's number is yellow and bold: a key that works" "249;226;175 1" "$(e2e_number_style_on '^│[0-9] .*:one26 ')"
keys /
e2e_wait_screen 'search…' 2000 || bad "check 27: / opens the filter over the flat list" "$(screen | sed -n '39p')"
check "with / open the flat row's number stays yellow and bold" "249;226;175 1" "$(e2e_number_style_on '^│[0-9] .*:one26 ')"
keys Escape
e2e_wait_screen '^ 1-9 session' 2000 || bad "check 27: Esc closes the filter" "$(screen | sed -n '39p')"
keys q; popup_gone
tm kill-session -t pp26 2>/dev/null

echo "-- popup: 28) X, s and v do nothing, and c with no row says so"
# Closing a window, splitting a pane and opening a window are tmux's own
# keys, so the popup does not bind X, s, v or c. One
# idle agent, in a session of its own, on purpose: c is a jump letter, and with only one row here it
# names none, so it moves nothing and only says so on the hint line -- a second, three-row session
# after the loop shows what c actually does elsewhere. Tab, with nothing needing you, puts the
# cursor on it (an idle agent is the walk). Each key then gets a bounded watch of its own: the
# popup's rows must not change, and tmux must have just the windows and panes it had right before
# that key. X goes last: a confirmation would take every key pressed after it.
tm new-session -d -s pp28 -c "$DIR/home" 2>/dev/null
pp28_pane=$(e2e_start_agent claude pp28); tm rename-window -t "$pp28_pane" win28
e2e_wait_opt "$pp28_pane" @agent_state idle 7000 >/dev/null
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
pp28_tmux() { tm list-panes -a -F '#{session_name} #{window_id} #{pane_id}' | sort; }
e2e_ui_open; e2e_wait_popup_open; e2e_wait_screen '[0-9]+:win28 +idle' 3000
keys Tab
e2e_wait_cursor ':win28' 2000 \
  && ok "Tab puts the cursor on the agent's row" \
  || bad "Tab puts the cursor on the agent's row" "$(e2e_popup_cursor_row)"
# The preview follows the cursor at the popup's next refresh: wait for the screen to stop
# changing, so that whatever a watch below sees change is the key's doing.
e2e_wait_still || bad "check 28: the screen settles before the watch" "$(screen | sed -n '38,39p')"
# c, a letter with no row in this one-row session: the hint line says so, and nothing else moves.
pp28_was=$(pp28_tmux)
pp28_before=$(screen | sed -n '1,38p')
keys c
e2e_wait_screen '^ no row c in this session' 2000 \
  && ok "c, a letter with no row here, says so on the hint line" \
  || bad "c, a letter with no row here, says so on the hint line" "$(screen | sed -n '39p')"
check "c moves nothing: the popup's rows are as they were" "$pp28_before" "$(screen | sed -n '1,38p')"
check "c leaves tmux's windows and panes as they were" "$pp28_was" "$(pp28_tmux)"
# The line says so for a few seconds; wait it out, so the watches below see only their own keys.
e2e_wait_screen '^ 1-9 session' 6000 || bad "check 28: the hint line comes back after c's answer" "$(screen | sed -n '39p')"
for pp28_k in s v X; do
  pp28_was=$(pp28_tmux)
  pp28_before=$(screen | sed -n '1,39p')
  keys "$pp28_k"
  watch_rows_unchanged "$pp28_k changes nothing on screen" "$pp28_before"
  check "$pp28_k leaves tmux's windows and panes as they were" "$pp28_was" "$(pp28_tmux)"
done
check "X asks nothing: no confirmation to kill the window" "0" "$(screen | grep -c 'kill-window')"
keys q
check "q still closes the popup after c s v X" "1" "$(e2e_wait_popup_closed 2000 && echo 1 || echo 0)"
popup_gone
tm kill-session -t pp28 2>/dev/null

# c's real job: a session with a third row under its header. Fresh, so it is session 2
# (main alone comes before it), reached by Tab the same way as above -- nothing needs you,
# so every agent is the walk and the first after main's header is pp28c's own -- rather than by a number,
# which this section does not need to know.
tm new-session -d -s pp28c -c "$DIR/home" 2>/dev/null
pp28c_a=$(e2e_start_agent claude pp28c); tm rename-window -t "$pp28c_a" one28c
pp28c_b=$(e2e_start_agent claude pp28c); tm rename-window -t "$pp28c_b" two28c
pp28c_c=$(e2e_start_agent claude pp28c); tm rename-window -t "$pp28c_c" three28c
for pp28c_p in "$pp28c_a" "$pp28c_b" "$pp28c_c"; do e2e_wait_opt "$pp28c_p" @agent_state idle 7000 >/dev/null; done
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
pp28c_tmux() { tm list-panes -a -F '#{session_name} #{window_id} #{pane_id}' | sort; }
e2e_ui_open; e2e_wait_popup_open; e2e_wait_screen '[0-9]+:three28c +idle' 3000
keys Tab
e2e_wait_cursor ':one28c' 2000 \
  && ok "Tab puts the cursor on pp28c's first agent" \
  || bad "Tab puts the cursor on pp28c's first agent" "$(e2e_popup_cursor_row)"
e2e_wait_still || bad "check 28: the screen settles before c" "$(screen | sed -n '38,39p')"
pp28c_was=$(pp28c_tmux)
keys c
e2e_wait_cursor ':three28c' 2000 \
  && ok "c moves the cursor to the third row when this session has one" \
  || bad "c moves the cursor to the third row when this session has one" "$(e2e_popup_cursor_row)"
check "c still just moves the cursor: tmux's panes are unchanged" "$pp28c_was" "$(pp28c_tmux)"
keys q; popup_gone
tm kill-session -t pp28c 2>/dev/null

echo "-- popup: 29) a taw ui typed in a pane leaves no bar option behind"
# The popup writes @agent_wizard_bar, the text taw's bar draws from, only when
# the launcher says it put the bar up: `taw ui open` runs it with --bar. Typed in a pane by
# hand, taw ui has no launcher: it runs in that pane, and writing the option there would put it on
# the pane's session as it opened and at every refresh after, with nothing ever to take it
# down. In a session of its own, which has no status-format of its own.
tm new-session -d -s pp29 -c "$DIR/home" 2>/dev/null
pp29_pane=$(tm display-message -p -t pp29 '#{pane_id}')
# How many bar options there are: in each session's own options, then in the global ones, where
# a write with -g would land. Counted by name: `show-options -v` prints nothing for an option
# set to the empty string, so it cannot tell one from an option that was never set.
pp29_bars() {
  { tm list-sessions -F '#S' | while read -r pp29_s; do tm show-options -t "$pp29_s" 2>/dev/null; done
    tm show-options -g 2>/dev/null; } | grep -cE '^@agent_wizard_bar( |$)'
}
pp29_global=$(tm show-options -g status-format 2>/dev/null)
check "no session, nor the global options, carries a bar option before taw ui runs" "0" "$(pp29_bars)"
check "pp29 has no status-format of its own" "0" "$(tm show-options -t pp29 2>/dev/null | grep -c 'status-format')"
tm send-keys -t "$pp29_pane" "$(printf '%q' "$TAW_BIN") ui" Enter
e2e_wait_pane "$pp29_pane" '╭ sessions' 5000 \
  && ok "taw ui typed in the pane draws the tree there" \
  || bad "taw ui typed in the pane draws the tree there" "$(tm capture-pane -p -t "$pp29_pane" | grep -v '^$' | tail -3)"
# The option would land at the popup's first refresh, and again at each one after, half a
# second apart: sampled across three of them.
pp29_waited=0; pp29_seen=0
while [ "$pp29_waited" -lt 1500 ]; do
  pp29_seen=$(pp29_bars)
  [ "$pp29_seen" != 0 ] && break
  command sleep 0.1; pp29_waited=$((pp29_waited + 100))
done
check "while it runs, no session, nor the global options, carries a bar option" "0" "$pp29_seen"
tm send-keys -t "$pp29_pane" q
# taw ui prints its envelope once the popup has gone: text only its end draws in the pane.
e2e_wait_pane "$pp29_pane" '"id":"cli:ui"' 3000 \
  && ok "q ends taw ui in the pane" \
  || bad "q ends taw ui in the pane" "$(tm capture-pane -p -t "$pp29_pane" | grep -v '^$' | tail -3)"
check "after q, no session, nor the global options, carries a bar option" "0" "$(pp29_bars)"
check "pp29 still has no status-format of its own" "0" "$(tm show-options -t pp29 2>/dev/null | grep -c 'status-format')"
check "the global status-format is untouched" "$pp29_global" "$(tm show-options -g status-format 2>/dev/null)"
tm kill-session -t pp29 2>/dev/null
tm select-window -t main:0 2>/dev/null

echo "-- popup: 30) an extra Enter after a kill or a worktree removal leaves the popup open"
# Killing a worktree from the popup must not close it. The
# confirmation takes one key, and a close moves the cursor onto whichever row took the closed
# one's place. An Enter left over -- typed behind the y, or pressed again while git was still
# deleting a large checkout -- went to that row, and the popup closed. For a moment after a
# close Enter does nothing; a moment later it goes again. A worktree of a scratch repo,
# with an agent in its session and an ignored build directory in its checkout, so that git
# takes a while to delete it.
pp30_repo="$E2E_TMP/pp30-repo-$$"; mkdir -p "$pp30_repo"
( cd "$pp30_repo" && git init -q && printf 'target/\n' > .gitignore && git add .gitignore && git -c user.email=e2e@example.invalid -c user.name=e2e commit -q -m init )
printf '[worktrees]\ndirectory = "%s/pp30-wt-%s"\n' "$E2E_TMP" "$$" > "$TAW_CONFIG_DIR/config.toml"
pp30_wid=$(jget "$(taw workspace create --cwd "$pp30_repo" 2>/dev/null)" workspace_id)
pp30_wt=$(taw worktree create --workspace "$pp30_wid" --branch pp30 2>&1)
rm -f "$TAW_CONFIG_DIR/config.toml"
pp30_s=$(jget "$pp30_wt" session_name)
[ -n "$pp30_s" ] && ok "a worktree with a session of its own" || bad "a worktree with a session of its own" "$pp30_wt"
pp30_dir=$(tm display-message -p -t "=$pp30_s:" '#{session_path}')
pp30_agent=$(e2e_start_agent claude "$pp30_s")
e2e_wait_opt "$pp30_agent" @agent_state idle 7000 >/dev/null
( cd "$pp30_dir" && mkdir -p target && seq 1 20000 | sed 's/^/target\/f/' | xargs touch )
check "the build directory is ignored, so the checkout is clean" "" "$(git -C "$pp30_dir" status --porcelain)"
# Walks the cursor from the first session's header down, one row at a time, until the cursor
# row matches <regex>.
pp30_walk() {
  keys 1; command sleep 0.3
  local i=0
  while [ "$i" -lt 60 ]; do
    e2e_popup_cursor_row | grep -qE "$1" && return 0
    keys j; command sleep 0.15; i=$((i + 1))
  done
  return 1
}
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
e2e_ui_open; e2e_wait_popup_open
pp30_walk "^│. [* -] $pp30_s( |\$)" \
  && ok "the cursor reaches the worktree session's header" \
  || bad "the cursor reaches the worktree session's header" "$(e2e_popup_cursor_row)"
keys x
e2e_wait_screen "kill-session $pp30_s\\?" 2000 \
  && ok "x asks before kill-session" \
  || bad "x asks before kill-session" "$(screen | grep -m1 'kill-')"
keys y Enter                                     # one send-keys: the Enter lands right behind the y
pp30_i=0
while [ "$pp30_i" -lt 30 ] && tm has-session -t "=$pp30_s" 2>/dev/null; do
  command sleep 0.1; pp30_i=$((pp30_i + 1))
done
check "y kills the worktree's session" "0" "$(tm has-session -t "=$pp30_s" 2>/dev/null && echo 1 || echo 0)"
watch_popup_open "an Enter right behind the y leaves the popup open" 1500
popup_open && keys q
popup_gone
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
e2e_ui_open; e2e_wait_popup_open
pp30_walk '◌ pp30 ' \
  && ok "the worktree, parked now, has a row the cursor reaches" \
  || bad "the worktree, parked now, has a row the cursor reaches" "$(e2e_popup_cursor_row)"
keys x
e2e_wait_screen 'git worktree remove pp30\?' 2000 \
  && ok "x asks before git worktree remove" \
  || bad "x asks before git worktree remove" "$(screen | grep -m1 'worktree remove')"
keys Enter; command sleep 0.3; keys Enter        # the second while git is still deleting
e2e_wait_screen '^ removed worktree pp30' 10000 \
  && ok "the removal finishes and says so" \
  || bad "the removal finishes and says so" "$(screen | sed -n '39p')"
watch_popup_open "an Enter pressed again during the removal leaves the popup open" 1500
check "the worktree's checkout is gone" "0" "$([ -d "$pp30_dir" ] && echo 1 || echo 0)"
keys Enter
check "a moment later Enter goes again, and the popup closes" "1" "$(e2e_wait_popup_closed 2000 && echo 1 || echo 0)"
popup_gone
tm list-sessions -F '#{session_id} #{session_path}' 2>/dev/null | awk -v d="pp30-repo-$$" 'index($2, d) { print $1 }' \
  | while read -r pp30_id; do tm kill-session -t "$pp30_id" 2>/dev/null; done
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null

echo "-- popup: 30b) herdr's text keys in the filter, with tmux's prefix kept"
# herdr v0.9.1's text fields: alt-backspace cuts the word before the cursor, ctrl-y puts the
# last cut back, ctrl-f moves a character, ctrl-d deletes the next one, alt-d cuts to the end of the next word.
# They go through the real client and tmux, so this is where an Alt that arrives as Escape is
# proved, and tmux's own prefix, ctrl-b: it starts the popup's prefix chord, so it moves no cursor
# and the key after it is the chord's, not text. The filter draws its cursor where it is, between
# the characters, so each key is read from where the cursor stands after it.
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
e2e_ui_open; e2e_wait_popup_open
keys /
e2e_wait_screen 'search…' 1500
keys -l "one two three"
e2e_wait_screen '^ /one two three▏' 2000 || bad "check 30b: the query reads one two three" "$(screen | sed -n '39p')"
keys C-a
e2e_wait_screen '^ /▏one two three' 2000 \
  && ok "ctrl-a draws the cursor at the start of the line" \
  || bad "ctrl-a draws the cursor at the start of the line" "$(screen | sed -n '39p')"
keys Right Right
e2e_wait_screen '^ /on▏e two three' 2000 \
  && ok "Right moves it on, and the line shows it" \
  || bad "Right moves it on, and the line shows it" "$(screen | sed -n '39p')"
keys C-e Left
e2e_wait_screen '^ /one two thre▏e' 2000 \
  && ok "ctrl-e and Left: the end, and one back from it" \
  || bad "ctrl-e and Left: the end, and one back from it" "$(screen | sed -n '39p')"
keys C-e
e2e_wait_screen '^ /one two three▏' 2000 || bad "check 30b: ctrl-e is back at the end" "$(screen | sed -n '39p')"
keys M-BSpace
e2e_wait_screen '^ /one two ▏' 2000 \
  && ok "alt-backspace cuts the word before the cursor" \
  || bad "alt-backspace cuts the word before the cursor" "$(screen | sed -n '39p')"
keys C-y
e2e_wait_screen '^ /one two three▏' 2000 \
  && ok "ctrl-y puts the cut word back" \
  || bad "ctrl-y puts the cut word back" "$(screen | sed -n '39p')"
keys C-a C-f C-d
keys -l "Y"
e2e_wait_screen '^ /oY▏e two three' 2000 \
  && ok "ctrl-a, ctrl-f and ctrl-d: one character forward, the next one deleted" \
  || bad "ctrl-a, ctrl-f and ctrl-d: one character forward, the next one deleted" "$(screen | sed -n '39p')"
keys C-b
keys -l "z"                                      # the chord's: nothing is typed
keys -l "Q"
e2e_wait_screen '^ /oYQ▏e two three' 2000 \
  && ok "ctrl-b is tmux's prefix: it moves no cursor, and the key after it is swallowed" \
  || bad "ctrl-b is tmux's prefix: it moves no cursor, and the key after it is swallowed" "$(screen | sed -n '39p')"
keys C-a M-d
e2e_wait_screen '^ /▏ two three' 2000 \
  && ok "alt-d cuts to the end of the word after the cursor and leaves the space" \
  || bad "alt-d cuts to the end of the word after the cursor and leaves the space" "$(screen | sed -n '39p')"
keys C-y
e2e_wait_screen '^ /oYQe▏ two three' 2000 \
  && ok "ctrl-y puts it back where the cursor is" \
  || bad "ctrl-y puts it back where the cursor is" "$(screen | sed -n '39p')"
keys Escape
popup_open && keys q
popup_gone
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null

# A repository, a workspace for it and a worktree with a session of its own, for the next two
# sections: prints the session names, the repository's and the worktree's, and the checkout.
pp30x_make() {
  local dir="$1" branch="$2" repo wid wt
  repo="$E2E_TMP/$dir"; mkdir -p "$repo"
  ( cd "$repo" && git init -q && git -c user.email=e2e@example.invalid -c user.name=e2e commit -q --allow-empty -m init )
  printf '[worktrees]\ndirectory = "%s/%s-wt"\n' "$E2E_TMP" "$dir" > "$TAW_CONFIG_DIR/config.toml"
  wid=$(jget "$(taw workspace create --cwd "$repo" 2>/dev/null)" workspace_id)
  wt=$(taw worktree create --workspace "$wid" --branch "$branch" 2>&1)
  rm -f "$TAW_CONFIG_DIR/config.toml"
  printf '%s %s %s' "$(tm display-message -p -t "\$${wid#w}" '#{session_name}' 2>/dev/null)" "$(jget "$wt" session_name)" \
    "$(tm display-message -p -t "=$(jget "$wt" session_name):" '#{session_path}' 2>/dev/null)"
}
# Walks the cursor from the first session's header down until the cursor row matches <regex>.
pp30x_walk() {
  keys 1; command sleep 0.3
  local i=0
  while [ "$i" -lt 60 ]; do
    e2e_popup_cursor_row | grep -qE "$1" && return 0
    keys j; command sleep 0.15; i=$((i + 1))
  done
  return 1
}

echo "-- popup: 30c) a worktree git will not remove is asked about: only y removes it"
# herdr v0.9.1's remove-anyway question: git refuses a checkout with an untracked file, and the
# popup asks remove anyway in place of showing git's reason. Enter keeps it, and y, past the moment
# a close rests the keys, removes it, its branch kept.
read -r pp30c_src pp30c_s pp30c_dir <<<"$(pp30x_make "r30c-$$" pp30c)"
[ -n "$pp30c_s" ] && ok "a worktree with a session of its own" || bad "a worktree with a session of its own" "$pp30c_src $pp30c_s"
printf 'unsaved\n' > "$pp30c_dir/notes.txt"
tm kill-session -t "=$pp30c_s" 2>/dev/null          # parked: a row of its own
e2e_ui_open; e2e_wait_popup_open
pp30x_walk '◌ pp30c ' \
  && ok "the parked worktree has a row the cursor reaches" \
  || bad "the parked worktree has a row the cursor reaches" "$(e2e_popup_cursor_row)"
keys x
e2e_wait_screen 'git worktree remove pp30c\?' 2000 \
  && ok "x asks before git worktree remove" \
  || bad "x asks before git worktree remove" "$(screen | grep -m1 'worktree remove')"
keys y
e2e_wait_screen 'remove anyway\? uncommitted changes are lost' 5000 \
  && ok "git refuses, and the popup asks remove anyway" \
  || bad "git refuses, and the popup asks remove anyway" "$(screen | sed -n '36,39p')"
check "the question says why, in git's own terms" "1" "$(screen | grep -c 'git will not remove pp30c: it has modified or untracked files')"
check "and ends in y / N" "1" "$(screen | grep -c '│y / N  *│')"
e2e_wait_screen '^ y confirm  ·  n, Enter or Esc cancel' 1500 \
  && ok "the hint line names only y as confirming" \
  || bad "the hint line names only y as confirming" "$(screen | sed -n '39p')"
keys Enter
command sleep 0.4
check "Enter keeps the worktree: the question is gone" "0" "$(screen | grep -c 'remove anyway')"
popup_open && ok "and the popup is still open" || bad "and the popup is still open" "$(screen | sed -n '39p')"
check "and the checkout, with its file, is still there" "1" "$([ -f "$pp30c_dir/notes.txt" ] && echo 1 || echo 0)"
command sleep 1                                      # past the moment after a close, in which a y is not an answer
keys x
e2e_wait_screen 'git worktree remove pp30c\?' 2000
keys y
e2e_wait_screen 'remove anyway\? uncommitted changes are lost' 5000 \
  && ok "asked again" || bad "asked again" "$(screen | sed -n '36,39p')"
command sleep 1
keys y
e2e_wait_screen '^ removed worktree pp30c' 10000 \
  && ok "y removes it, and the popup says so" \
  || bad "y removes it, and the popup says so" "$(screen | sed -n '39p')"
check "the checkout is gone" "0" "$([ -d "$pp30c_dir" ] && echo 1 || echo 0)"
check "the branch is kept" "1" "$(git -C "$E2E_TMP/r30c-$$" branch --list pp30c | grep -c pp30c)"
popup_open && keys q
popup_gone
tm kill-session -t "=$pp30c_src" 2>/dev/null
rm -rf "$E2E_TMP/r30c-$$" "$E2E_TMP/r30c-$$-wt"
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null

echo "-- popup: 30d) x on a session with a worktree session open asks whether to close it too"
# herdr v0.9.1's group close: the plain confirmation becomes a question with a default answer:
# y closes the session and its worktree sessions, Enter just this session, Esc nothing.
read -r pp30d_a pp30d_a_wt _ <<<"$(pp30x_make "r30d1-$$" pp30d)"
read -r pp30d_b pp30d_b_wt _ <<<"$(pp30x_make "r30d2-$$" pp30d)"
[ -n "$pp30d_a_wt" ] && [ -n "$pp30d_b_wt" ] && ok "two sessions, each with a worktree session open" || bad "two sessions, each with a worktree session open" "$pp30d_a $pp30d_a_wt $pp30d_b $pp30d_b_wt"
has() { tm has-session -t "=$1" 2>/dev/null && echo 1 || echo 0; }
e2e_ui_open; e2e_wait_popup_open
pp30x_walk "^│. [* -] $pp30d_a( |\$)" \
  && ok "the cursor reaches the first session's header" \
  || bad "the cursor reaches the first session's header" "$(e2e_popup_cursor_row)"
keys x
e2e_wait_screen "kill-session $pp30d_a\\? It has 1 worktree session open \\($pp30d_a_wt\\)\\. Close them too\\?" 2000 \
  && ok "x names the worktree session and asks whether to close it too" \
  || bad "x names the worktree session and asks whether to close it too" "$(screen | grep -m1 'kill-session')"
check "with y / N under it" "1" "$(screen | grep -c '│y / N  *│')"
e2e_wait_screen '^ y close them too  ·  n or Enter just this one  ·  Esc cancel' 1500 \
  && ok "the hint line names each answer" \
  || bad "the hint line names each answer" "$(screen | sed -n '39p')"
keys Escape
command sleep 0.4
check "Esc closes nothing: the session is there" "1" "$(has "$pp30d_a")"
check "and so is its worktree session" "1" "$(has "$pp30d_a_wt")"
keys x
e2e_wait_screen "Close them too\\?" 2000
keys Enter
command sleep 0.8
check "Enter closes just the session" "0" "$(has "$pp30d_a")"
check "and leaves its worktree session open" "1" "$(has "$pp30d_a_wt")"
popup_open && ok "the popup stays open" || bad "the popup stays open" "$(screen | sed -n '39p')"
command sleep 1                                      # past the rest after a close
pp30x_walk "^│. [* -] $pp30d_b( |\$)" \
  && ok "the cursor reaches the second session's header" \
  || bad "the cursor reaches the second session's header" "$(e2e_popup_cursor_row)"
keys x
e2e_wait_screen "Close them too\\?" 2000
keys y
command sleep 0.8
check "y closes the session" "0" "$(has "$pp30d_b")"
check "and its worktree session" "0" "$(has "$pp30d_b_wt")"
e2e_wait_screen "^ closed $pp30d_b and its 1 worktree session" 3000 \
  && ok "and says so" || bad "and says so" "$(screen | sed -n '39p')"
check "closing sessions removes no worktree: the checkout is still there" "1" "$([ -d "$E2E_TMP/r30d2-$$-wt" ] && echo 1 || echo 0)"
popup_open && keys q
popup_gone
tm kill-session -t "=$pp30d_a_wt" 2>/dev/null
rm -rf "$E2E_TMP/r30d1-$$" "$E2E_TMP/r30d1-$$-wt" "$E2E_TMP/r30d2-$$" "$E2E_TMP/r30d2-$$-wt"
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null

echo "-- popup: 30e) ctrl-d: fzf's end of input on the search preset's empty query, nothing that closes in the modal preset"
# fzf's ctrl-d is delete-char/eof. The search preset's query is fzf's own: with text it deletes the
# character under the cursor (and finds none at the end of the line), on an empty query it quits as
# Esc does. The modal preset's filter never closes on it, empty or not. Through the real client, so
# the byte a terminal sends for ctrl-d is what the popup reads.
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
e2e_ui_open; e2e_wait_popup_open
keys /
e2e_wait_screen 'search…' 1500 || bad "check 30e: the modal filter opens empty" "$(screen | sed -n '39p')"
keys C-d
watch_popup_open "ctrl-d on the modal filter's empty query leaves the popup open" 1200
check "and the filter is still open and empty" "1" "$(screen | sed -n '39p' | grep -c 'search…')"
keys -l "ab"
e2e_wait_screen '^ /ab▏' 2000 || bad "check 30e: the modal filter reads ab" "$(screen | sed -n '39p')"
keys C-a C-d
e2e_wait_screen '^ /▏b' 2000 \
  && ok "in the modal filter ctrl-d deletes the character under the cursor" \
  || bad "in the modal filter ctrl-d deletes the character under the cursor" "$(screen | sed -n '39p')"
keys Escape
e2e_wait_screen 'Tab next up' 2000
keys C-d
watch_popup_open "ctrl-d with no filter open leaves the popup open" 1200
popup_open && keys q
popup_gone

printf '[keys]\npreset = "search"\n' > "$TAW_CONFIG_DIR/config.toml"
e2e_ui_open; e2e_wait_popup_open
e2e_wait_screen '^ type filter' 3000 || bad "check 30e: the search preset's hint line appears" "$(screen | sed -n '39p')"
keys -l "ab"
e2e_wait_screen '> ab▏' 2000 || bad "check 30e: the query reads ab" "$(screen | sed -n '2p')"
keys C-d
watch_popup_open "ctrl-d at the end of a query with text leaves the popup open" 1200
check "and deletes nothing, there being nothing under the cursor" "1" "$(screen | sed -n '2p' | grep -c '> ab▏')"
keys C-a C-d
e2e_wait_screen '> ▏b' 2000 \
  && ok "ctrl-d deletes the character under the cursor" \
  || bad "ctrl-d deletes the character under the cursor" "$(screen | sed -n '2p')"
keys C-d
e2e_wait_screen 'search…' 2000 \
  && ok "and the press after it deletes the last character, leaving the query empty" \
  || bad "and the press after it deletes the last character, leaving the query empty" "$(screen | sed -n '2p')"
watch_popup_open "the press that emptied the query left the popup open" 800
keys C-d
check "on an empty query ctrl-d closes the popup, as fzf's end of input does" "1" "$(e2e_wait_popup_closed 2000 && echo 1 || echo 0)"
popup_gone
rm -f "$TAW_CONFIG_DIR/config.toml"
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null

echo "-- popup: 30f) x on a session's last pane asks whether to close its worktree sessions too"
# Closing the last pane of a session's last window closes the session, so x on it asks what x on the
# session's header asks (30d): y closes the session and its worktree sessions, Enter just the pane
# (and the session with it), Esc nothing. A plain shell is a row only when every pane is listed.
read -r pp30f_a pp30f_a_wt _ <<<"$(pp30x_make "r30f1-$$" pp30f)"
read -r pp30f_b pp30f_b_wt _ <<<"$(pp30x_make "r30f2-$$" pp30f)"
[ -n "$pp30f_a_wt" ] && [ -n "$pp30f_b_wt" ] && ok "two sessions, each with a worktree session open" || bad "two sessions, each with a worktree session open" "$pp30f_a $pp30f_a_wt $pp30f_b $pp30f_b_wt"
has() { tm has-session -t "=$1" 2>/dev/null && echo 1 || echo 0; }
printf '[ui]\ndepth = "all"\n' > "$TAW_CONFIG_DIR/config.toml"
e2e_ui_open; e2e_wait_popup_open
pp30x_walk "^│. [* -] $pp30f_a( |\$)" \
  && ok "the cursor reaches the first session's header" \
  || bad "the cursor reaches the first session's header" "$(e2e_popup_cursor_row)"
keys j                                               # its only pane
command sleep 0.3
keys x
e2e_wait_screen "Close them too\\?" 2000 \
  && ok "x on its only pane asks whether to close the worktree session too" \
  || bad "x on its only pane asks whether to close the worktree session too" "$(screen | sed -n '14,24p')"
check "the question names the pane and says the session goes with it" "1" \
  "$(screen | grep -c "kill-pane $pp30f_a:0 (.*It is the session's last pane, so the session closes with it\\.")"
check "and names the worktree session" "1" "$(screen | grep -c "($pp30f_a_wt)\\. Close them too")"
check "with y / N under it" "1" "$(screen | grep -c '│y / N  *│')"
keys Escape
command sleep 0.4
check "Esc closes nothing: the session is there" "1" "$(has "$pp30f_a")"
check "and so is its worktree session" "1" "$(has "$pp30f_a_wt")"
keys x
e2e_wait_screen "Close them too\\?" 2000
keys Enter
command sleep 0.8
check "Enter closes just the pane, and the session with it" "0" "$(has "$pp30f_a")"
check "and leaves its worktree session open" "1" "$(has "$pp30f_a_wt")"
popup_open && ok "the popup stays open" || bad "the popup stays open" "$(screen | sed -n '39p')"
command sleep 1                                      # past the rest after a close
pp30x_walk "^│. [* -] $pp30f_b( |\$)" \
  && ok "the cursor reaches the second session's header" \
  || bad "the cursor reaches the second session's header" "$(e2e_popup_cursor_row)"
keys j
command sleep 0.3
keys x
e2e_wait_screen "Close them too\\?" 2000
keys y
command sleep 0.8
check "y closes the session" "0" "$(has "$pp30f_b")"
check "and its worktree session" "0" "$(has "$pp30f_b_wt")"
e2e_wait_screen "^ closed $pp30f_b and its 1 worktree session" 3000 \
  && ok "and says so" || bad "and says so" "$(screen | sed -n '39p')"
popup_open && keys q
popup_gone
rm -f "$TAW_CONFIG_DIR/config.toml"
tm kill-session -t "=$pp30f_a_wt" 2>/dev/null
rm -rf "$E2E_TMP/r30f1-$$" "$E2E_TMP/r30f1-$$-wt" "$E2E_TMP/r30f2-$$" "$E2E_TMP/r30f2-$$-wt"
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null

echo "-- popup: 30g) a group close from a popup inside one of its own sessions closes them all, and with detach-on-destroy on its client goes to the session the question named"
# With tmux's default detach-on-destroy, closing the session a client is attached to takes the client
# with it, and a popup dies with its client: a group close that killed that session first never reached
# the rest. The popup's own session is closed last, all of them in one tmux command, and before it the
# client is moved to the session the question names, so detach-on-destroy finds it elsewhere and it
# stays attached. A second client of the server, in an outer window of its own so that the harness's
# client stays on main, sits in the worktree session and opens the popup there; its cursor goes to the
# parent's header, x, y. Each client is known by its terminal: the tty of the outer pane it runs in.
read -r pp30g_src pp30g_wt _ <<<"$(pp30x_make "r30g-$$" pp30g)"
[ -n "$pp30g_wt" ] && ok "a session with a worktree session open" || bad "a session with a worktree session open" "$pp30g_src $pp30g_wt"
has() { tm has-session -t "=$1" 2>/dev/null && echo 1 || echo 0; }
pp30g_client_session() { tm list-clients -F '#{client_tty} #{client_session}' | awk -v t="$1" '$1 == t { print $2 }'; }
# The sentence of the popup's confirm box, its wrapped lines joined: the box is cut out of each screen
# line by the columns of its top border (characters, so in a UTF-8 locale).
pp30g_question() {
  local LC_ALL=C.UTF-8 line pre rest left=-1 width=0 seg out=""
  while IFS= read -r line; do
    if [ "$left" -lt 0 ]; then
      case "$line" in
        *"╭ confirm "*) pre=${line%%╭ confirm *}; left=${#pre}; rest=${line:left}; rest=${rest%%╮*}; width=$(( ${#rest} + 1 )) ;;
      esac
      continue
    fi
    seg=${line:left:width}
    case "$seg" in ╰*|"") break ;; esac
    seg=${seg#│}; seg=${seg%│}
    out="$out $seg"
  done <<<"$(screen)"
  printf '%s\n' "$out" | tr -s ' ' | sed 's/^ //; s/ $//'
}
pp30g_host_tty=$(outer display-message -p -t host '#{pane_tty}')
tm switch-client -c "$pp30g_host_tty" -t main   # making the worktree moved it
check "the harness's own client is on main" "main" "$(pp30g_client_session "$pp30g_host_tty")"
tm set -g detach-on-destroy on
outer new-session -d -s host2 -x 120 -y 40 "tmux -L $SOCK attach -t '=$pp30g_wt'"   # quoted: zsh reads a bare =word as a command
pp30g_tty=$(outer display-message -p -t host2 '#{pane_tty}')
pp30g_up=0
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30; do
  [ "$(pp30g_client_session "$pp30g_tty")" = "$pp30g_wt" ] && { pp30g_up=1; break; }
  command sleep 0.1
done
check "a second client is attached to the worktree session" "1" "$pp30g_up"
E2E_HOST=host2
# Opened as the plugin's key does, in the worktree session's pane: the client it is for is the one
# most recently used in that session, the second.
tm run-shell -b -t "=$pp30g_wt:" "$(printf '%q' "$TAW_BIN") ui open >/dev/null 2>&1"
e2e_wait_popup_open
popup_open && ok "the popup opens on the second client" || bad "the popup opens on the second client" "$(screen | sed -n '1,3p')"
check "and not on the harness's client" "0" "$(outer capture-pane -p -t host | grep -c '╭ sessions')"
pp30x_walk "^│. [* -] $pp30g_src( |\$)" \
  && ok "the cursor reaches the parent session's header" \
  || bad "the cursor reaches the parent session's header" "$(e2e_popup_cursor_row)"
keys x
e2e_wait_screen "Close them too\\?" 2000 \
  && ok "x asks whether to close the worktree session too" \
  || bad "x asks whether to close the worktree session too" "$(screen | sed -n '14,24p')"
pp30g_q=$(pp30g_question)
pp30g_to=$(printf '%s\n' "$pp30g_q" | sed -n 's/.* If you close them too, you go to \([^ ;]*\); if not, you stay\..*/\1/p')
[ -n "$pp30g_to" ] \
  && ok "the question says y takes this client to another session and Enter leaves it where it is" \
  || bad "the question says y takes this client to another session and Enter leaves it where it is" "[$pp30g_q]"
check "and the session it names is neither of the two that close" "0" \
  "$(case "$pp30g_to" in "$pp30g_src"|"$pp30g_wt") echo 1 ;; *) echo 0 ;; esac)"
keys y
for _ in $(seq 1 100); do                            # up to 20 s: the gate runs under load
  [ "$(has "$pp30g_src")$(has "$pp30g_wt")" = 00 ] && [ -n "$pp30g_to" ] \
    && [ "$(pp30g_client_session "$pp30g_tty")" = "$pp30g_to" ] && break
  command sleep 0.2
done
check "y closes the parent session" "0" "$(has "$pp30g_src")"
check "and the worktree session the popup was in" "0" "$(has "$pp30g_wt")"
check "the popup's client stays attached, on the session the question named" "$pp30g_to" "$(pp30g_client_session "$pp30g_tty")"
check "and its terminal is still there" "1" "$(outer has-session -t host2 2>/dev/null && echo 1 || echo 0)"
E2E_HOST=host
check "the harness's own client is untouched: still on main" "main" "$(pp30g_client_session "$pp30g_host_tty")"
tm set -g detach-on-destroy off
outer kill-session -t host2 2>/dev/null
rm -rf "$E2E_TMP/r30g-$$" "$E2E_TMP/r30g-$$-wt"
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null

echo "-- popup: 30h) the word keys: herdr's boundary in the filter, fzf's in the search preset's query"
# alt-b, alt-f and alt-d share one rule for where a word ends. herdr's, in the modal filter: a run of
# letters, digits and _ or of punctuation, so the dashes of tmux-agent-wizard are runs of their own.
# fzf's, in the search preset's query (a pure fzf search): only letters and digits are words, so
# alt-d cuts tmux, then -agent, then -wizard, as fzf 0.73.1 does with the same keys.
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
e2e_ui_open; e2e_wait_popup_open
keys /
e2e_wait_screen 'search…' 1500
keys -l "tmux-agent-wizard"
e2e_wait_screen '^ /tmux-agent-wizard▏' 2000 || bad "check 30h: the filter reads tmux-agent-wizard" "$(screen | sed -n '39p')"
keys C-a M-f
e2e_wait_screen '^ /tmux▏-agent-wizard' 2000 \
  && ok "filter: alt-f goes to the end of the first word" \
  || bad "filter: alt-f goes to the end of the first word" "$(screen | sed -n '39p')"
keys M-f
e2e_wait_screen '^ /tmux-▏agent-wizard' 2000 \
  && ok "filter: the dash is a stop of its own" \
  || bad "filter: the dash is a stop of its own" "$(screen | sed -n '39p')"
keys C-a M-d
e2e_wait_screen '^ /▏-agent-wizard' 2000 \
  && ok "filter: alt-d cuts tmux" \
  || bad "filter: alt-d cuts tmux" "$(screen | sed -n '39p')"
keys M-d
e2e_wait_screen '^ /▏agent-wizard' 2000 \
  && ok "filter: and then the dash alone" \
  || bad "filter: and then the dash alone" "$(screen | sed -n '39p')"
keys Escape
popup_open && keys q
popup_gone

printf '[keys]\npreset = "search"\n' > "$TAW_CONFIG_DIR/config.toml"
e2e_ui_open; e2e_wait_popup_open
e2e_wait_screen '^ type filter' 3000 || bad "check 30h: the search preset's hint line appears" "$(screen | sed -n '39p')"
keys -l "tmux-agent-wizard"
e2e_wait_screen '> tmux-agent-wizard▏' 2000 || bad "check 30h: the query reads tmux-agent-wizard" "$(screen | sed -n '2p')"
keys C-a M-f
e2e_wait_screen '> tmux▏-agent-wizard' 2000 \
  && ok "search preset: alt-f goes to the end of the first word" \
  || bad "search preset: alt-f goes to the end of the first word" "$(screen | sed -n '2p')"
keys M-f
e2e_wait_screen '> tmux-agent▏-wizard' 2000 \
  && ok "search preset: and on to the end of agent, the dash skipped" \
  || bad "search preset: and on to the end of agent, the dash skipped" "$(screen | sed -n '2p')"
keys C-a M-d
e2e_wait_screen '> ▏-agent-wizard' 2000 \
  && ok "search preset: alt-d cuts tmux" \
  || bad "search preset: alt-d cuts tmux" "$(screen | sed -n '2p')"
keys M-d
e2e_wait_screen '> ▏-wizard' 2000 \
  && ok "search preset: then -agent, as fzf's kill-word does" \
  || bad "search preset: then -agent, as fzf's kill-word does" "$(screen | sed -n '2p')"
keys M-d
e2e_wait_screen 'search…' 2000 \
  && ok "search preset: then -wizard, and the query is empty" \
  || bad "search preset: then -wizard, and the query is empty" "$(screen | sed -n '2p')"
popup_open && keys Escape
popup_gone
rm -f "$TAW_CONFIG_DIR/config.toml"
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null

echo "-- popup: 30i) x on the last pane of a session nothing is cut from says the session closes with it"
# Closing the last pane of a session's last window closes the session, and the question says so (herdr
# asks before it closes the last tab of a workspace, #4379 and #4409); taw says that too, not only
# that the window closed. Nothing is cut from either session here, so it is the plain question, y /
# n. A session with a second window loses only the window, and still says only that. A plain shell
# is a row only when every pane is listed.
pp30i_one="pp30i-one-$$"; pp30i_two="pp30i-two-$$"
pp30i_has() { tm has-session -t "=$1" 2>/dev/null && echo 1 || echo 0; }
tm new-session -d -s "$pp30i_one" -c "$DIR/home"
tm new-session -d -s "$pp30i_two" -c "$DIR/home"
tm new-window -d -t "$pp30i_two:" -c "$DIR/home"
printf '[ui]\ndepth = "all"\n' > "$TAW_CONFIG_DIR/config.toml"
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
e2e_ui_open; e2e_wait_popup_open
pp30x_walk "^│. [* -] $pp30i_one( |\$)" \
  && ok "the cursor reaches the one-window session's header" \
  || bad "the cursor reaches the one-window session's header" "$(e2e_popup_cursor_row)"
keys j                                               # its only pane
command sleep 0.3
keys x
e2e_wait_screen "kill-pane $pp30i_one:0" 2000 \
  && ok "x on its only pane asks first" \
  || bad "x on its only pane asks first" "$(screen | sed -n '14,24p')"
check "the question says the session closes with its last pane" "1" \
  "$(screen | grep -c "kill-pane $pp30i_one:0 (.*It is the session's last pane, so the session closes with it\\.")"
check "it says nothing of the window alone" "0" "$(screen | grep -c "It is the window's last pane")"
check "no worktree session is asked about" "0" "$(screen | grep -c 'Close them too')"
check "with y / n under it" "1" "$(screen | grep -c '│y / n  *│')"
check "and no dash, arrow or curly quote" "0" "$(screen | grep -cE "$(pp_banned)")"
keys Escape
command sleep 0.4
check "Esc closes nothing: the session is there" "1" "$(pp30i_has "$pp30i_one")"
keys x
e2e_wait_screen "kill-pane $pp30i_one:0" 2000
keys y
command sleep 0.8
check "y closes the pane, and the session with it" "0" "$(pp30i_has "$pp30i_one")"
popup_open && ok "the popup stays open" || bad "the popup stays open" "$(screen | sed -n '39p')"
command sleep 1                                      # past the rest after a close
pp30x_walk "^│. [* -] $pp30i_two( |\$)" \
  && ok "the cursor reaches the two-window session's header" \
  || bad "the cursor reaches the two-window session's header" "$(e2e_popup_cursor_row)"
keys j                                               # its first window's pane
command sleep 0.3
keys x
e2e_wait_screen "kill-pane $pp30i_two:0" 2000 \
  && ok "x on a pane with a window beside it asks first" \
  || bad "x on a pane with a window beside it asks first" "$(screen | sed -n '14,24p')"
check "and says only that the window closes" "1" \
  "$(screen | grep -c "kill-pane $pp30i_two:0 (.*It is the window's last pane, so the window closes too\\.")"
check "not the session" "0" "$(screen | grep -c "so the session closes with it")"
keys Escape
popup_open && keys q
popup_gone
rm -f "$TAW_CONFIG_DIR/config.toml"
tm kill-session -t "=$pp30i_one" 2>/dev/null
tm kill-session -t "=$pp30i_two" 2>/dev/null
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null

echo "-- popup: 31) a popup too narrow for both panels shows the tree alone; the preview returns when both fit"
# In a very small terminal the list of agents must stay usable. Below 100 columns the tree
# takes the whole width and no preview is drawn; from 100 up the preview is beside the tree. The
# client is resized while the popup is open, as a window is (wide, then small), across the
# threshold itself (99 and 100), and once more with the popup opened at the small size. One idle
# agent, in main, so the tree has a row to reach; main is the first session, so its rows are on top.
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
pp31_pane=$(e2e_start_agent claude); tm rename-window -t "$pp31_pane" nar31
e2e_wait_opt "$pp31_pane" @agent_state idle 7000 >/dev/null
# Resizes the client and waits until tmux reports the new width: <cols> <rows>.
pp31_size() {
  outer resize-window -t host -x "$1" -y "$2" >/dev/null 2>&1
  local waited=0
  while [ "$waited" -lt 3000 ] && [ "$(tm display-message -p '#{client_width}' 2>/dev/null)" != "$1" ]; do
    command sleep 0.1; waited=$((waited + 100))
  done
}
# Waits until the popup shows <n> preview panels (0 or 1) and its hint line: a resize takes a moment
# to reach the popup and the popup a moment to draw it.
pp31_wait_previews() {
  local want="$1" waited=0
  while [ "$waited" -lt 3000 ]; do
    [ "$(screen | grep -c '╭ preview')" = "$want" ] && screen | grep -q 'Tab next up' && return 0
    command sleep 0.1; waited=$((waited + 100))
  done
  return 1
}
# How many panels the popup's top border draws: one when the tree has the whole width, two beside
# the preview.
pp31_panels() { screen | sed -n '1p' | grep -o '╭' | wc -l | tr -d ' '; }
pp31_size 120 40
e2e_ui_open; e2e_wait_popup_open; e2e_wait_screen 'Tab next up' 3000
pp31_wait_previews 1 \
  && ok "at 120 columns the preview is beside the tree" \
  || bad "at 120 columns the preview is beside the tree" "$(screen | sed -n '1,3p')"
check "two panels then" "2" "$(pp31_panels)"
pp31_size 99 30
pp31_wait_previews 0 \
  && ok "made 99 columns wide the popup draws no preview" \
  || bad "made 99 columns wide the popup draws no preview" "$(screen | sed -n '1,3p')"
check "the tree is the one panel, across the whole width" "1" "$(pp31_panels)"
check "the hint line is under the tree" "1" "$(screen | grep -c 'Tab next up')"
screen | grep -q ':nar31' \
  && ok "the agent's row is listed" \
  || bad "the agent's row is listed" "$(screen | sed -n '1,6p')"
keys Tab
e2e_wait_cursor ':nar31' 2000 \
  && ok "Tab reaches the agent in the narrow popup: there is a list to navigate" \
  || bad "Tab reaches the agent in the narrow popup" "$(e2e_popup_cursor_row)"
pp31_size 100 30
pp31_wait_previews 1 \
  && ok "at exactly 100 columns both panels fit again" \
  || bad "at exactly 100 columns both panels fit again" "$(screen | sed -n '1,3p')"
check "two panels at 100" "2" "$(pp31_panels)"
pp31_size 90 30
pp31_wait_previews 0 \
  && ok "and at 90 columns the tree is alone again" \
  || bad "and at 90 columns the tree is alone again" "$(screen | sed -n '1,3p')"
taw popup close >/dev/null 2>&1; e2e_wait_popup_closed
# Opened at the small size rather than resized to it.
e2e_ui_open; e2e_wait_popup_open; e2e_wait_screen 'Tab next up' 3000
pp31_wait_previews 0 \
  && ok "a popup opened at 90 columns is the tree alone" \
  || bad "a popup opened at 90 columns is the tree alone" "$(screen | sed -n '1,3p')"
check "one panel" "1" "$(pp31_panels)"
taw popup close >/dev/null 2>&1; e2e_wait_popup_closed
e2e_wait_bar_restored main
tm kill-window -t "$pp31_pane" 2>/dev/null
tm select-window -t main:0 2>/dev/null

echo "-- popup: 32) taw ui open --new-session starts on the new-session picker, and Esc closes the popup; the picker n opens goes back to the tree"
# The popup `taw open` opens when it has nothing to open. Without the flag the popup starts on
# the tree; the tree's hint is waited for first, so that "no picker" is read from a popup that
# has drawn, not from one that has not yet.
outer resize-window -t host -x 120 -y 40 >/dev/null 2>&1
tm switch-client -t main 2>/dev/null
tm select-window -t main:0 2>/dev/null
e2e_ui_open; e2e_wait_screen 'Tab next up' 3000
check "without the flag the popup starts on the tree" "0" "$(screen | grep -c 'new session')"
# The picker n opens inside that popup has the tree behind it, and Esc goes back to it.
keys n
check "n opens the picker over the tree" "1" "$(e2e_wait_screen 'new session' 2000 && echo 1 || echo 0)"
keys Escape
e2e_wait_screen 'Tab next up' 2000
check "Esc on the picker n opened goes back to the tree, the popup still open" "0:1" "$(screen | grep -c 'new session'):$(popup_open && echo 1 || echo 0)"
keys Escape
check "Esc on the tree closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
e2e_wait_bar_restored main
for pp32_preset in modal search; do
  printf '[keys]\npreset = "%s"\n' "$pp32_preset" > "$TAW_CONFIG_DIR/config.toml"
  e2e_ui_open --new-session
  if e2e_wait_screen 'new session' 3000; then
    ok "$pp32_preset preset: the picker is open from the first frame"
  else
    bad "$pp32_preset preset: the picker is open from the first frame" "$(screen | sed -n '1,4p')"
  fi
  check "$pp32_preset preset: the tree is under it" "1" "$(popup_open && echo 1 || echo 0)"
  screen | grep -q 'type a project, or a path' \
    && ok "$pp32_preset preset: nothing is typed, and it says what to type" \
    || bad "$pp32_preset preset: nothing is typed, and it says what to type" "$(screen | grep -m1 '>')"
  # The popup started on the picker, so there is no tree anyone asked to see behind it: one Esc
  # closes the popup, as it does in tmux-session-wizard's t.
  keys Escape
  check "$pp32_preset preset: one Esc closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
  e2e_wait_bar_restored main
  rm -f "$TAW_CONFIG_DIR/config.toml"
done

echo "-- sandbox: 📦 on the row, the preview's title, /sbx keeps the boxed agent's session, /sandbox finds nothing (no word is drawn for the box), the legend, n, w, the help and o unchanged, restore, outside tmux"
# No runtime runs: the fake is compiled from tests/fixtures/sandbox/fake-runtime.rs, and the section's
# profile runs it by its absolute path, so no docker on any PATH is ever reached, a restored pane's
# included. The fake writes Claude's SessionStart input into the folder it is given with -v, as Claude's
# hook does inside a sandbox. Everything the section makes, it removes.
sb_dir="$DIR/sandbox"; mkdir -p "$sb_dir/state" "$TAW_CONFIG_DIR/sandbox"
sb_id=4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d
sb_start='{"session_id":"'"$sb_id"'","transcript_path":"/home/agent/.claude/projects/-work/'"$sb_id"'.jsonl","cwd":"/work","hook_event_name":"SessionStart","source":"startup"}'
if rustc --edition 2021 -o "$sb_dir/docker" "$E2E_REPO/tests/fixtures/sandbox/fake-runtime.rs" 2>"$sb_dir/rustc.log"; then
  ok "sandbox: the fake runtime builds"
else
  bad "sandbox: the fake runtime builds" "$(head -3 "$sb_dir/rustc.log")"
fi
# The profile names the fake by its path: no docker on any PATH is reached, a restored pane's included.
# Beside the report file it names the sandbox's own agent, as taw's default profile does, so that only
# that agent's hook inside writes the report (the fake, which stands in for it, is that agent).
cat > "$TAW_CONFIG_DIR/sandbox/box" <<SH
#!/bin/sh
if [ -n "\${TAW_SANDBOX_REPORT:-}" ]; then
  set -- -v "\$TAW_SANDBOX_REPORT:/run/taw" -e TAW_REPORT_FILE=/run/taw/session.json -e "TAW_REPORT_AGENT=\$TAW_SANDBOX_AGENT" box-image "\$@"
else
  set -- box-image "\$@"
fi
exec "$sb_dir/docker" run --rm -it "\$@"
SH
chmod +x "$TAW_CONFIG_DIR/sandbox/box"
# What the fake needs, for every pane the server starts from here, the restored one included.
sb_vars="FAKE_RUNTIME_LOG FAKE_RUNTIME_STATE FAKE_RUNTIME_FIXTURES FAKE_RUNTIME_STOP FAKE_RUNTIME_REPORT"
tm set-environment -g FAKE_RUNTIME_LOG "$sb_dir/calls.log"
tm set-environment -g FAKE_RUNTIME_STATE "$sb_dir/state"
tm set-environment -g FAKE_RUNTIME_FIXTURES "$E2E_REPO/tests/fixtures/sandbox"
tm set-environment -g FAKE_RUNTIME_STOP "$sb_dir/stop"
tm set-environment -g FAKE_RUNTIME_REPORT "$sb_start"
sb_runs() { tr '\036' '\n' < "$sb_dir/calls.log" 2>/dev/null | tr '\037' ' ' | grep -c ' run --rm -it '; }

tm new-session -d -s sbx -c "$DIR/home"
sb_pane=$(tm display-message -p -t sbx '#{pane_id}')
tm send-keys -t "$sb_pane" -l "$(printf '%q' "$TAW_BIN") sandbox claude --profile box"; tm send-keys -t "$sb_pane" Enter
e2e_wait_opt "$sb_pane" @agent_container docker 10000 && ok "sandbox: taw sees the agent in its container" || bad "sandbox: taw sees the agent in its container" "$(tm capture-pane -p -t "$sb_pane" | tail -3)"
check "sandbox: the pane is marked with its profile" "box" "$(tm show-options -pqv -t "$sb_pane" @agent_sandbox)"
e2e_wait_opt "$sb_pane" @agent_session_id "$sb_id" 10000 && ok "sandbox: the session reported from inside is the pane's" || bad "sandbox: the session reported from inside is the pane's"
tr '\036' '\n' < "$sb_dir/calls.log" 2>/dev/null | tr '\037' ' ' | grep -q " -e TAW_REPORT_AGENT=claude box-image claude " \
  && ok "sandbox: the report's writer inside is told it is the sandbox's own agent" \
  || bad "sandbox: the report's writer inside is told it is the sandbox's own agent" "$(tr '\036' '\n' < "$sb_dir/calls.log" 2>/dev/null | tr '\037' ' ' | tail -1)"

# A plain agent in main, for the filter to hide: with the boxed one alone, a filter that kept every agent
# row would look the same. It is ended after the legend, so that what follows is as it was without it.
sb_plain=$(e2e_start_agent claude)
e2e_wait_opt "$sb_plain" @agent_kind claude 4000 >/dev/null || bad "sandbox: main's plain claude is up" "$(tm display-message -p -t "$sb_plain" '#{@agent_kind}' 2>/dev/null)"
# The popup, opened from the sandboxed pane, so the cursor is on its row; wide, so the preview's
# title has room for the whole path and the sandbox's words.
outer resize-window -t host -x 200 -y 40 >/dev/null 2>&1
tm switch-client -t sbx 2>/dev/null
e2e_ui_open; e2e_wait_screen 'Tab next up' 3000
sb_tree() { screen | grep '^│' | sed 's/││.*//'; }
sb_tree | grep -q 'claude 📦' && ok "sandbox: the row has 📦 after the name" || bad "sandbox: the row has 📦 after the name" "$(sb_tree | grep claude)"
screen | sed -n '1p' | grep -q ' preview · .* · 📦 in a sandbox: docker, profile box ' && ok "sandbox: the preview's title names the runtime and the profile" || bad "sandbox: the preview's title names the runtime and the profile" "$(screen | sed -n '1p')"
keys f; e2e_wait_screen ' flat ╯' 2000
sb_tree | grep -q 'claude 📦' && ok "sandbox: the flat list has 📦 after the name" || bad "sandbox: the flat list has 📦 after the name" "$(sb_tree | grep claude)"
keys f; e2e_wait_screen ' agents ╯' 2000
# What the filter has to hide: main's header and its plain claude are drawn before it, and both go.
sb_hides() { printf '%s %s' "$(sb_tree | grep -c ' main ')" "$(sb_tree | grep claude | grep -vc '📦')"; }
sb_hides_before=$(sb_hides)
keys /; e2e_wait_screen 'search…' 1500
# The boxed agent's session is called sbx: its name is a word the screen draws (on the header above the row), so it finds the row.
keys -l "sbx"
e2e_wait_screen '/sbx▏' 2000 || bad "sandbox: the query reads sbx" "$(screen | tail -2)"
check "sandbox: /sbx keeps the agent in a container and only it" "1" "$(sb_tree | grep -c '📦')"
# The box count alone is 1 whether or not the filter hid anything: a plain agent row kept or gone is told apart here.
# Both before the filter and after it are read, so that a tree that drew nothing to hide is not a pass.
sb_hid=hid; read -r sb_hb_main sb_hb_plain <<< "$sb_hides_before"
{ [ "$sb_hb_main" -ge 1 ] && [ "$sb_hb_plain" -ge 1 ]; } || sb_hid="the tree drew no main or no plain claude before the filter [$sb_hides_before]"
[ "$(sb_hides)" = "0 0" ] || sb_hid="the filter kept main or its plain claude [$(sb_hides)]"
check "sandbox: /sbx hid main and its plain claude, which the tree drew before it" "hid" "$sb_hid"
check "sandbox: under /sbx the tree is sbx's header and its agent and nothing else" "2" "$(sb_tree | grep -c '[[:alnum:]]')"
# The box is drawn as 📦 and no word: the positive checks above ran first, so this finding nothing is not a tree that drew nothing.
keys BSpace BSpace BSpace; keys -l "sandbox"
e2e_wait_screen '/sandbox▏' 2000 || bad "sandbox: the query reads sandbox" "$(screen | tail -2)"
screen | grep -q 'nothing matches "sandbox"' \
  && ok "sandbox: /sandbox finds nothing, the box has no word on the screen" \
  || bad "sandbox: /sandbox finds nothing, the box has no word on the screen" "$(screen | grep -m2 -e 'nothing matches' -e 📦)"
# A key sent right after Esc reaches the popup as Alt and that key, so each Esc is let land first.
# The popup is closed with Esc alone, one at a time while it is open (Esc on the tree closes it): a
# q after an Esc that already closed it would be typed into the pane under it. "Drawn something" is
# read off the popup's own 39 rows (the client is 40 rows, the last is the status line): a redraw
# outside them, the bar's clock, is not the Esc landing, and the next Esc would follow it too soon.
sb_close() {
  local _
  for _ in 1 2 3 4; do
    popup_open || return 0
    sb_was=$(screen | sed -n '1,39p'); keys Escape
    sb_waited=0   # until the Esc has drawn something
    while [ "$sb_waited" -lt 3000 ] && [ "$(screen | sed -n '1,39p')" = "$sb_was" ]; do command sleep 0.05; sb_waited=$((sb_waited + 50)); done
  done
  ! popup_open
}
keys Escape; e2e_wait_screen 'Tab next up  ·  Enter go' 3000; keys '?'
e2e_wait_screen 'in a container that taw started, or saw start' 3000 && ok "sandbox: the legend says what 📦 claims" || bad "sandbox: the legend says what 📦 claims" "$(screen | grep -m1 '📦  ')"
sb_close || bad "sandbox: the popup closed after the legend" "$(screen | head -3)"
tm kill-pane -t "$sb_plain" 2>/dev/null

# n, w, the help and o are today's with a [sandbox] table set, and ctrl-s in n and w does what it
# does without one. Both sides read a config.toml, so that the table is all that differs.
outer resize-window -t host -x 120 -y 40 >/dev/null 2>&1
tm switch-client -t main 2>/dev/null
# A state's age read as T, and the spaces after it as one: "now" is a column wider than "5m".
sb_screen() { screen | sed -n '1,39p' | sed -E 's/(now|[0-9]+[smhd])( +|$)/T /g'; }
# It runs in a $(...), so a popup that stays open is written to a file, and checked after the overlays.
sb_overlay() {   # what the popup's 39 rows show after the keys given
  e2e_ui_open; e2e_wait_screen 'Tab next up' 3000
  for sb_key in "$@"; do keys "$sb_key"; e2e_wait_still >/dev/null; done
  sb_screen
  sb_close && e2e_wait_popup_closed >/dev/null || echo "after keys [$*]: $(screen | head -3 | tr '\n' '|')" >> "$sb_dir/stuck"
}
printf '[ui.sound]\nenabled = false\n' > "$TAW_CONFIG_DIR/config.toml"
sb_n_plain=$(sb_overlay n); sb_w_plain=$(sb_overlay w); sb_help_plain=$(sb_overlay '?'); sb_o_plain=$(sb_overlay o)
sb_ns_plain=$(sb_overlay n C-s); sb_ws_plain=$(sb_overlay w C-s)
# Each of them drew over the tree, so that an equal pair below is two overlays, not two trees.
sb_tree_plain=$(sb_overlay); sb_drawn=0
for sb_seen in "$sb_n_plain" "$sb_w_plain" "$sb_help_plain" "$sb_o_plain" "$sb_ns_plain" "$sb_ws_plain"; do
  [ -n "$sb_seen" ] && [ "$sb_seen" != "$sb_tree_plain" ] && sb_drawn=$((sb_drawn + 1))
done
check "sandbox: n, w, the help, o and ctrl-s in n and w each drew over the tree" "6" "$sb_drawn"
printf '[ui.sound]\nenabled = false\n\n[sandbox]\nprofile = "box"\n\n[sandbox.agents]\ncodex = "default"\n' > "$TAW_CONFIG_DIR/config.toml"
check "sandbox: n is today's with a [sandbox] table set" "$sb_n_plain" "$(sb_overlay n)"
check "sandbox: w is today's with a [sandbox] table set" "$sb_w_plain" "$(sb_overlay w)"
check "sandbox: the help is today's with a [sandbox] table set" "$sb_help_plain" "$(sb_overlay '?')"
check "sandbox: o is today's with a [sandbox] table set" "$sb_o_plain" "$(sb_overlay o)"
check "sandbox: ctrl-s in n does what it does without the table" "$sb_ns_plain" "$(sb_overlay n C-s)"
check "sandbox: ctrl-s in w does what it does without the table" "$sb_ws_plain" "$(sb_overlay w C-s)"
check "sandbox: every overlay's popup closed" "" "$(cat "$sb_dir/stuck" 2>/dev/null)"
rm -f "$TAW_CONFIG_DIR/config.toml"

# A restart, from the popup's side: the layout saved, the session gone, restored; the row comes
# back with its 📦, and the fake is given the agent's resume arguments.
# The restore is the plugin's, run by the server: a taw run here has this script's home, where the
# server's panes have a home of their own, and a path in tilde form typed into one would miss.
taw session save --name sbx-trip >/dev/null 2>&1
tm kill-session -t sbx
tm run-shell "$(printf '%q' "$TAW_BIN") session restore --name sbx-trip >/dev/null 2>&1"
sb_back=$(tm display-message -p -t sbx '#{pane_id}' 2>/dev/null)
# Why a restored pane is not in its container: the typed line failed (the pane is back at the shell, no
# run made), or the fake never got its variable (the fake is the pane's command, runs are counted).
sb_why() {
  printf 'pane command [%s], fake runs %s, the pane'"'"'s last lines:\n' "$(tm display-message -p -t "$sb_back" '#{pane_current_command}' 2>/dev/null)" "$(sb_runs)"
  tm capture-pane -p -t "$sb_back" 2>/dev/null | grep -v '^ *$' | tail -10
}
e2e_wait_opt "$sb_back" @agent_container docker 15000 && ok "sandbox: the restored pane's agent is in its container again" || bad "sandbox: the restored pane's agent is in its container again" "$(sb_why)"
check "sandbox: the restored pane is marked with its profile" "box" "$(tm show-options -pqv -t "$sb_back" @agent_sandbox)"
tr '\036' '\n' < "$sb_dir/calls.log" | tr '\037' ' ' | grep -q "box-image claude --resume $sb_id" && ok "sandbox: the restore ran the profile with the agent's resume arguments" || bad "sandbox: the restore ran the profile with the agent's resume arguments" "$(tr '\036' '\n' < "$sb_dir/calls.log" | tr '\037' ' ' | tail -1; sb_why)"
outer resize-window -t host -x 200 -y 40 >/dev/null 2>&1
tm switch-client -t sbx 2>/dev/null
e2e_ui_open; e2e_wait_screen 'Tab next up' 3000
sb_tree | grep -q 'claude 📦' && ok "sandbox: the restored row has its 📦" || bad "sandbox: the restored row has its 📦" "$(sb_tree | grep claude)"
keys q; e2e_wait_popup_closed
outer resize-window -t host -x 120 -y 40 >/dev/null 2>&1
tm switch-client -t main 2>/dev/null

# Outside tmux: a window of the outer server with no TMUX and no TMUX_PANE is a terminal outside
# the inner tmux. taw says so in one line and runs the profile there.
sb_before_runs=$(sb_runs)
outer new-window -d -t host -n plain "env -u TMUX -u TMUX_PANE HOME='$DIR/home' HISTFILE=/dev/null FAKE_RUNTIME_LOG='$sb_dir/calls.log' FAKE_RUNTIME_STATE='$sb_dir/state' FAKE_RUNTIME_FIXTURES='$E2E_REPO/tests/fixtures/sandbox' FAKE_RUNTIME_STOP='$sb_dir/stop' $(printf '%q' "$TAW_BIN") sandbox claude --profile box; sleep 30"
sb_waited=0
while [ "$sb_waited" -lt 5000 ] && ! outer capture-pane -p -t host:plain | grep -q 'not in tmux'; do command sleep 0.1; sb_waited=$((sb_waited + 100)); done
outer capture-pane -p -t host:plain | grep -q 'taw sandbox: not in tmux, so this agent gets no popup row and no restore' && ok "sandbox: outside tmux it says so in one line" || bad "sandbox: outside tmux it says so in one line" "$(outer capture-pane -p -t host:plain | head -3)"
# The line comes before the profile runs the fake, which records its run a moment later.
sb_waited=0
while [ "$sb_waited" -lt 5000 ] && [ "$(sb_runs)" -le "$sb_before_runs" ]; do command sleep 0.1; sb_waited=$((sb_waited + 100)); done
check "sandbox: outside tmux it runs the profile in that terminal" "$((sb_before_runs + 1))" "$(sb_runs)"

# Everything the section made goes: the fakes stop, the sessions, the window, the files, the variables.
: > "$sb_dir/stop"
outer kill-window -t host:plain 2>/dev/null
tm kill-session -t sbx 2>/dev/null
for sb_var in $sb_vars; do tm set-environment -gu "$sb_var"; done
rm -rf "$sb_dir" "$TAW_CONFIG_DIR/sandbox" "$TAW_STATE_DIR/sandbox"
taw session delete sbx-trip >/dev/null 2>&1

echo "-- taw open: from a shell inside tmux, a directory, a relative path, a zoxide term, --json and the picker"
# `taw open` is tmux-session-wizard's `t <path or zoxide term>`. It is typed into a pane's shell
# here, as a person types it, inside a client: a script's own `taw` would run outside tmux.
# zoxide is a fake, first on the pane shell's PATH, that records how it is called and answers
# from a file, so not one `zoxide add` reaches a real database.
oo_tmp="$E2E_TMP/taw-open-$$"; mkdir -p "$oo_tmp"
oo_tmp="$(cd "$oo_tmp" && pwd -P)"   # resolved, to match the session_path tmux reports
mkdir -p "$oo_tmp/code/oo-app" "$oo_tmp/code/oo-sibling" "$oo_tmp/code/oo-found" "$oo_tmp/code/oo-picked" \
  "$oo_tmp/code/oo-json" "$oo_tmp/code/oo-alias" "$oo_tmp/code/oo-outside" "$oo_tmp/code/oo-cold"
oo_fakebin="$oo_tmp/fakebin"; mkdir -p "$oo_fakebin"
oo_zx_log="$oo_tmp/zoxide.log"; : > "$oo_zx_log"
oo_zx_answer="$oo_tmp/zoxide.answer"; : > "$oo_zx_answer"
cat > "$oo_fakebin/zoxide" <<FAKEZOXIDE
#!/bin/sh
{ for a in "\$@"; do printf '%s\037' "\$a"; done; printf '\n'; } >> '$oo_zx_log'
case "\$1" in
  query)
    if [ -s '$oo_zx_answer' ]; then cat '$oo_zx_answer'; else echo 'zoxide: no match found' >&2; exit 1; fi ;;
esac
exit 0
FAKEZOXIDE
chmod +x "$oo_fakebin/zoxide"
# One call of the fake as a line of arguments separated by |
oo_zx_call() { sed -n "${1}p" "$oo_zx_log" | tr '\037' '|' | sed 's/|$//'; }
oo_out="$oo_tmp/out"; oo_err="$oo_tmp/err"; oo_status="$oo_tmp/status"; oo_done="$oo_tmp/done"
oo_pane=$(tm new-window -t main -P -F '#{pane_id}' -d)
tm send-keys -t "$oo_pane" -l "export PATH='$oo_fakebin':\$PATH"; tm send-keys -t "$oo_pane" Enter
# oo_type <cwd> <command line>: types `cd <cwd> && <command line>` into the pane's shell, its
# output and error going to $oo_out and $oo_err and its exit status to $oo_status, and
# `$oo_done` made when it has finished. Not waited for: `taw open` with nothing to open
# stays until the popup closes.
oo_type() {
  rm -f "$oo_done" "$oo_status"; : > "$oo_out"; : > "$oo_err"
  tm send-keys -t "$oo_pane" -l "cd '$1' && $2 >'$oo_out' 2>'$oo_err'; echo \$? >'$oo_status'; : >'$oo_done'"
  tm send-keys -t "$oo_pane" Enter
}
oo_wait_done() {
  local waited=0
  while [ ! -e "$oo_done" ] && [ "$waited" -lt 15000 ]; do command sleep 0.1; waited=$((waited + 100)); done
  [ -e "$oo_done" ]
}
# oo_open <cwd> <taw open arguments...>: typed, and waited for.
oo_open() { local cwd="$1"; shift; oo_type "$cwd" "'$TAW_BIN' open $*"; oo_wait_done; }
oo_client() { tm list-clients -F '#{client_session}'; }
outer resize-window -t host -x 120 -y 40 >/dev/null 2>&1
tm switch-client -t main 2>/dev/null

oo_app="$oo_tmp/code/oo-app"
oo_open "$oo_tmp" "$oo_app" || bad "taw open <dir> finished" "no marker after 15 s"
check "taw open <dir>: exit status 0" "0" "$(cat "$oo_status" 2>/dev/null)"
check "taw open <dir>: nothing is printed on success" "" "$(cat "$oo_out")$(cat "$oo_err")"
check "taw open <dir>: this client is on the new session, named for the directory" "oo-app" "$(oo_client)"
check "taw open <dir>: the session is rooted at the directory" "$oo_app" "$(tm display-message -p -t oo-app '#{session_path}' 2>/dev/null)"
# Named as session open names it: the directory's own name, here for a directory made by each.
# With the fake zoxide first on PATH: session open runs `zoxide add`, and the real one would
# write this directory into the database of whoever runs the suite.
oo_so=$(PATH="$oo_fakebin:$PATH" taw session open "$oo_tmp/code/oo-sibling" 2>&1)
check "session open names a directory the same way (its own name)" "oo-sibling" "$(jget "$oo_so" session)"

tm switch-client -t main 2>/dev/null
oo_open "$oo_tmp" "$oo_app"
check "a second taw open of the same directory switches to that session" "oo-app" "$(oo_client)"
check "... and does not make another" "1" "$(tm list-sessions -F '#{session_name}' | grep -c '^oo-app$')"

tm switch-client -t main 2>/dev/null
oo_open "$oo_app" ../oo-sibling
check "taw open ../dir: the client is on the session of the sibling directory" "oo-sibling" "$(oo_client)"
check "taw open ../dir: rooted at the sibling directory" "$oo_tmp/code/oo-sibling" "$(tm display-message -p -t oo-sibling '#{session_path}' 2>/dev/null)"

# A term that is not a directory is asked of zoxide, as z asks it, and what it finds is opened
# like any directory, and told to zoxide again.
printf '%s\n' "$oo_tmp/code/oo-found" > "$oo_zx_answer"; : > "$oo_zx_log"
tm switch-client -t main 2>/dev/null
oo_open "$oo_tmp/code" found
check "taw open <term>: exit status 0, nothing printed" "0:" "$(cat "$oo_status" 2>/dev/null):$(cat "$oo_out")$(cat "$oo_err")"
check "taw open <term>: the client is on zoxide's match" "oo-found" "$(oo_client)"
check "zoxide is asked as z asks it: query --exclude <here> -- <term>" "query|--exclude|$oo_tmp/code|--|found" "$(oo_zx_call 1)"
check "zoxide is told about the directory it found" "add|$oo_tmp/code/oo-found" "$(oo_zx_call 2)"
: > "$oo_zx_log"; tm switch-client -t main 2>/dev/null
oo_open "$oo_tmp/code" oo found
check "two terms are two keywords" "query|--exclude|$oo_tmp/code|--|oo|found" "$(oo_zx_call 1)"

# Nothing found: one line on stderr, exit 1, and nobody is moved.
: > "$oo_zx_answer"; tm switch-client -t main 2>/dev/null
oo_open "$oo_tmp/code" nothing like it
check "an unknown term: exit status 1" "1" "$(cat "$oo_status" 2>/dev/null)"
check "an unknown term: one plain line on stderr, nothing on stdout" "taw open: no directory matches nothing like it|" "$(cat "$oo_err")|$(cat "$oo_out")"
check "an unknown term: the client stays where it was" "main" "$(oo_client)"

# --json is the envelope session open prints, for scripts.
oo_open "$oo_tmp" --json "$oo_tmp/code/oo-json"
oo_j=$(cat "$oo_out")
check "--json: the envelope names taw open" "cli:open" "$(jget "$oo_j" id)"
check "--json: its result is a session_open" "session_open" "$(jget "$oo_j" type)"
check "--json: the session is named for the directory, and was made" "oo-json:true" "$(jget "$oo_j" session):$(jget "$oo_j" created)"
check "--json: the client was switched, as session open does" "true:oo-json" "$(jget "$oo_j" switched):$(oo_client)"
tm switch-client -t main 2>/dev/null

# The alias the README suggests for anyone used to tmux-session-wizard's t.
tm send-keys -t "$oo_pane" -l "alias t='$TAW_BIN open'"; tm send-keys -t "$oo_pane" Enter
oo_type "$oo_tmp" "t '$oo_tmp/code/oo-alias'"; oo_wait_done
check "alias t='taw open': t <dir> opens it" "0:oo-alias" "$(cat "$oo_status" 2>/dev/null):$(oo_client)"
tm switch-client -t main 2>/dev/null

# With nothing to open the popup opens on its new-session picker, and stays until it closes.
# The popup is a fresh process that gets the server's own PATH, not the pane shell's, so the fake
# zoxide goes on the server's for as long as the picker is used: what the popup opens is told to
# zoxide, and that must not be the machine's own.
oo_path_saved=$(tm show-environment -g PATH 2>/dev/null | sed 's/^PATH=//')
tm set-environment -g PATH "$oo_fakebin:$oo_path_saved"
tm select-window -t "$oo_pane" 2>/dev/null
oo_type "$oo_tmp" "'$TAW_BIN' open"
if e2e_wait_screen 'new session' 3000; then
  ok "taw open with no terms opens the popup on the new-session picker"
else
  bad "taw open with no terms opens the popup on the new-session picker" "$(screen | sed -n '1,4p')"
fi
check "... over the tree" "1" "$(popup_open && echo 1 || echo 0)"
screen | grep -q 'type a project, or a path' && ok "... with nothing typed" || bad "... with nothing typed" "$(screen | grep -m1 '>')"
keys C-u; command sleep 0.2
keys -l "$oo_tmp/code/oo-picked"
e2e_wait_screen 'new-session -s "oo-picked"' 2000 && ok "typing a directory says what Enter would do" || bad "typing a directory says what Enter would do" "$(screen | grep -m1 'Enter:')"
keys Enter
check "Enter closes the popup" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
oo_wait_done
check "taw open returned, with exit status 0 and nothing printed" "0:" "$(cat "$oo_status" 2>/dev/null):$(cat "$oo_out")$(cat "$oo_err")"
check "the picked directory has its session, and the client is on it" "oo-picked:oo-picked" "$(tm display-message -p -t oo-picked '#{session_name}' 2>/dev/null):$(oo_client)"
check "the popup told zoxide about it, and the fake heard" "add|$oo_tmp/code/oo-picked" "$(grep -F "$oo_tmp/code/oo-picked" "$oo_zx_log" | tail -1 | tr '\037' '|' | sed 's/|$//')"
e2e_wait_bar_restored main
tm switch-client -t main 2>/dev/null; tm select-window -t "$oo_pane" 2>/dev/null
oo_type "$oo_tmp" "'$TAW_BIN' open"
check "the popup opens on the picker again" "1" "$(e2e_wait_screen 'new session' 3000 && echo 1 || echo 0)"
keys Escape
check "one Esc closes the popup: nothing was picked, and there is no tree to go back to" "1" "$(e2e_wait_popup_closed && echo 1 || echo 0)"
oo_wait_done
check "... taw open exits 0, prints nothing, and the client is where it was" "0::main" "$(cat "$oo_status" 2>/dev/null):$(cat "$oo_out")$(cat "$oo_err"):$(oo_client)"
e2e_wait_bar_restored main
tm set-environment -g PATH "$oo_path_saved"
tm kill-window -t "$oo_pane" 2>/dev/null
for oo_s in oo-app oo-sibling oo-found oo-picked oo-json oo-alias; do tm kill-session -t "$oo_s" 2>/dev/null; done
tm switch-client -t main 2>/dev/null

echo "-- taw open: from a plain terminal, outside tmux, a client attaches to the session, and a first open starts tmux"
# TMUX is unset here (the harness unsets it), so `taw open` is outside tmux, and the terminal is a
# pty from `script` (e2e_terminal, in the harness, says why its input is held open by a `cat` of a
# fifo that this shell keeps a write end of, until the end of this section, and how that `cat` is
# ended). A `script` that is killed is not waited for: bash would wait for the whole pipeline.
oo_fifo="$oo_tmp/tty-in"; mkfifo "$oo_fifo"; exec 7<>"$oo_fifo"
oo_terminal() { e2e_terminal "$oo_fifo" "$@"; oo_term_pid=$E2E_TERM_PID; }
# Waits, at most <ms>, for tmux -L <socket> to list a client that is not one of <known ttys>,
# printing that client's session.
oo_new_client() {
  local socket="$1" known="$2" ms="$3" waited=0 line
  while [ "$waited" -lt "$ms" ]; do
    line=$(tmux -L "$socket" list-clients -F '#{client_tty} #{client_session}' 2>/dev/null | grep -vF -e "$known" | head -1)
    [ -n "$line" ] && { printf '%s' "${line#* }"; return 0; }
    command sleep 0.1; waited=$((waited + 100))
  done
  return 1
}
oo_hosts=$(tm list-clients -F '#{client_tty}' | head -1)
oo_terminal env -u TMUX -u TMUX_PANE PATH="$oo_fakebin:$PATH" "$TAW_BIN" open "$oo_tmp/code/oo-outside"
check "outside tmux, taw open <dir> leaves a client attached to the new session" "oo-outside" "$(oo_new_client "$SOCK" "$oo_hosts" 8000)"
check "... rooted at the directory" "$oo_tmp/code/oo-outside" "$(tm display-message -p -t oo-outside '#{session_path}' 2>/dev/null)"
check "... and one client more than before, this terminal's" "2" "$(tm list-clients | wc -l | tr -d ' ')"
kill "$oo_term_pid" 2>/dev/null
oo_waited=0; while [ "$(tm list-clients | wc -l | tr -d ' ')" -gt 1 ] && [ "$oo_waited" -lt 5000 ]; do command sleep 0.1; oo_waited=$((oo_waited + 100)); done
oo_terminal env -u TMUX -u TMUX_PANE PATH="$oo_fakebin:$PATH" "$TAW_BIN" open "$oo_tmp/code/oo-outside"
check "outside tmux, taw open <dir> of a directory that has its session attaches to that one" "oo-outside" "$(oo_new_client "$SOCK" "$oo_hosts" 8000)"
check "... without making another" "1" "$(tm list-sessions -F '#{session_name}' | grep -c '^oo-outside$')"
kill "$oo_term_pid" 2>/dev/null
oo_waited=0; while [ "$(tm list-clients | wc -l | tr -d ' ')" -gt 1 ] && [ "$oo_waited" -lt 5000 ]; do command sleep 0.1; oo_waited=$((oo_waited + 100)); done
check "the harness's own client is untouched by all that" "main" "$(tm list-clients -F '#{client_session}')"
tm kill-session -t oo-outside 2>/dev/null

# With no terms there is no popup to open outside tmux: it says what to type, and exits 2.
oo_msg=$(taw open 2>&1 >/dev/null); oo_rc=$?
check "outside tmux, taw open with no terms: says what to type" "taw open: name a directory or a zoxide term (the picker needs tmux)" "$oo_msg"
check "... and exits 2, the code for a command line that cannot be run as typed" "2" "$oo_rc"
# --json attaches nothing, in a script with no terminal to give it.
oo_before=$(tm list-clients | wc -l | tr -d ' ')
oo_j=$(PATH="$oo_fakebin:$PATH" taw open --json "$oo_tmp/code/oo-outside")
check "--json outside tmux: the envelope for a script" "cli:open:session_open:true" "$(jget "$oo_j" id):$(jget "$oo_j" type):$(jget "$oo_j" created)"
check "--json outside tmux: no client was attached" "$oo_before" "$(tm list-clients | wc -l | tr -d ' ')"
tm kill-session -t oo-outside 2>/dev/null; tm switch-client -t main 2>/dev/null

# A first open with no tmux running at all starts one: a second, private server that nothing has
# started, and a tmux that reads no config file, or the machine's own would load into it.
oo_cold="taw-e2e-open-cold-$$"
printf '#!/bin/sh\nHISTFILE=/dev/null HOME=%s ZDOTDIR=%s SHELL=/bin/sh exec tmux -f /dev/null "$@"\n' "$DIR/shell-home" "$DIR/shell-home" > "$oo_tmp/tmux-quiet"; chmod +x "$oo_tmp/tmux-quiet"
check "the cold socket has no server to begin with" "1" "$(tmux -L "$oo_cold" list-sessions >/dev/null 2>&1; echo $?)"
oo_terminal env -u TMUX -u TMUX_PANE PATH="$oo_fakebin:$PATH" TAW_TMUX_SOCKET_NAME="$oo_cold" TAW_TMUX_BIN="$oo_tmp/tmux-quiet" "$TAW_BIN" open "$oo_tmp/code/oo-cold"
check "with no tmux running, taw open starts it and attaches this terminal to the session" "oo-cold" "$(oo_new_client "$oo_cold" "none-known" 8000)"
kill "$oo_term_pid" 2>/dev/null
oo_cold_file=$(tmux -L "$oo_cold" display-message -p '#{socket_path}' 2>/dev/null)   # taken away with the server
tmux -L "$oo_cold" kill-server 2>/dev/null
e2e_remove_socket "$oo_cold_file"
exec 7>&-
# The three terminals' `cat`s see the end of the file now, and one that does not is ended by its pid.
# A run must not leave any of them behind for good (ppid 1): nothing may be left of them here.
e2e_end_tracked
oo_waited=0; while [ "$(e2e_running "cat $oo_fifo")" -gt 0 ] && [ "$oo_waited" -lt 3000 ]; do command sleep 0.1; oo_waited=$((oo_waited + 100)); done
check "no cat of this run's terminal input is left running" "0" "$(e2e_running "cat $oo_fifo")"

echo "-- taw pane clear: the key, pressed in a real client"
# herdr's keys.clear_pane is unbound until it is set. The plugin binds nothing for an empty
# @agent-wizard-clear-key; a key in it, and the plugin sourced again as TPM's prefix + I does, binds the
# key, and pressing it clears the pane the client is looking at.
pc_key=K
pc_tmp="$E2E_TMP/pane-clear-$$"; mkdir -p "$pc_tmp"
pc_log="$pc_tmp/keys.log"; : > "$pc_log"
# Polls until a pane's first row is <row> and no other row has anything on it, which is what a shell's
# redraw leaves, up to 5 seconds.
pc_wait_top() {
  local pane="$1" row="$2" n=0 text
  while [ "$n" -lt 50 ]; do
    text=$(tm capture-pane -p -t "$pane")
    if [ "$(printf '%s\n' "$text" | head -1)" = "$row" ] && [ -z "$(printf '%s\n' "$text" | tail -n +2 | tr -d ' \n')" ]; then return 0; fi
    command sleep 0.1; n=$((n+1))
  done
  return 1
}
check "the clear key is bound to nothing until it is set" "0" "$(tm list-keys -T prefix "$pc_key" 2>/dev/null | grep -c 'pane clear')"
tm set-option -g @agent-wizard-clear-key "$pc_key"
tm run-shell "$E2E_REPO/tmux-agent-wizard.tmux"
check "with the option set, and the plugin sourced again, the key is bound to pane clear" "1" "$(tm list-keys -T prefix "$pc_key" 2>/dev/null | grep -c 'pane clear')"

# A bash at its prompt, with a line half typed: the key redraws it at the top, and the history goes.
pc_bash=$(tm new-window -t main -P -F '#{pane_id}' "exec env HOME='$pc_tmp' HISTFILE=/dev/null INPUTRC=/dev/null BASH_SILENCE_DEPRECATION_WARNING=1 PS1='READY> ' '$(command -v bash)' --norc --noprofile -i")
e2e_wait_pane "$pc_bash" '^READY>' 5000
# shellcheck disable=SC2016  # the pane's shell expands this, not this one
tm send-keys -t "$pc_bash" 'for i in $(seq 0 59); do echo line$i; done' Enter
e2e_wait_pane "$pc_bash" '^line59$' 5000
tm send-keys -t "$pc_bash" -l 'echo kept'
e2e_wait_pane "$pc_bash" '^READY> echo kept$' 5000
tm select-window -t "$pc_bash"
[ "$(tm display-message -p -t "$pc_bash" '#{history_size}')" -gt 0 ] && ok "the bash has history to clear" || bad "the bash has history to clear" "$(tm display-message -p -t "$pc_bash" '#{history_size}')"
check "the client is looking at that pane" "$pc_bash" "$(tm display-message -p '#{pane_id}')"
prefix "$pc_key"
pc_wait_top "$pc_bash" "READY> echo kept" && ok "prefix + the clear key redraws a bash at its prompt, and the line being typed, at the top" || bad "prefix + the clear key redraws a bash at its prompt at the top" "$(tm capture-pane -p -t "$pc_bash" | head -3)"
e2e_wait_opt "$pc_bash" history_size 0 4000 && ok "... and the history is gone, the old screen with it" || bad "... and the history is gone" "history_size=$(tm display-message -p -t "$pc_bash" '#{history_size}')"
keys Enter
e2e_wait_pane "$pc_bash" '^kept$' 5000 && ok "... and the line kept is the shell's own, and runs" || bad "... and the line kept runs" "$(tm capture-pane -p -t "$pc_bash" | head -4)"

# A program in front: the key clears the history and sends the program nothing. It logs every byte
# it reads, and has been sent one for certain.
cat > "$pc_tmp/logger.sh" <<LOGGER
#!/bin/sh
i=0; while [ \$i -lt 60 ]; do echo "line\$i"; i=\$((i+1)); done
stty raw -echo
printf 'LOGGING\\r\\n'
exec cat >> '$pc_log'
LOGGER
chmod +x "$pc_tmp/logger.sh"
pc_prog=$(tm new-window -t main -P -F '#{pane_id}' "exec '$pc_tmp/logger.sh'")
e2e_wait_pane "$pc_prog" '^LOGGING' 5000
tm send-keys -t "$pc_prog" -l x
pc_n=0; while [ "$(cat "$pc_log")" != x ] && [ "$pc_n" -lt 50 ]; do command sleep 0.1; pc_n=$((pc_n+1)); done
check "the program has read the byte it was sent" "x" "$(cat "$pc_log")"
pc_screen=$(tm capture-pane -p -t "$pc_prog")
tm select-window -t "$pc_prog"
prefix "$pc_key"
e2e_wait_opt "$pc_prog" history_size 0 4000 && ok "with a program in front the key clears the history" || bad "with a program in front the key clears the history" "history_size=$(tm display-message -p -t "$pc_prog" '#{history_size}')"
command sleep 0.5
check "... leaves its screen as it is" "$pc_screen" "$(tm capture-pane -p -t "$pc_prog")"
check "... and sends it nothing" "x" "$(cat "$pc_log")"

tm kill-window -t "$pc_bash" 2>/dev/null; tm kill-window -t "$pc_prog" 2>/dev/null
tm unbind-key -T prefix "$pc_key" 2>/dev/null; tm set-option -gu @agent-wizard-clear-key 2>/dev/null
tm select-window -t main:0 2>/dev/null

outer resize-window -t host -x 180 -y 45 >/dev/null 2>&1

echo "-- remote: taw app lists a machine, goes into it and back, the doorway, and the machine dropping off"
# Machines end to end, on the real stack with no real ssh. "This Mac" is the harness's own server; "studio" is a
# second private server of its own, whose far `taw` is the binary under test, reached through a stand-in for ssh
# that this section writes (TAW_SSH_BIN names it; the PATH's ssh stays the net's refusing stub, which the closing
# check of the run looks in). The app is the program of a pane of the harness's outer server, so its screen is read
# with capture-pane and driven with send-keys; studio's tmux, attached by the app through the stand-in, draws in
# the same pane. Everything this section makes is noted and ended by what was noted (rm_cleanup), a failed step
# included. Every step waits for what it expects (rm_see: bounded, with the screen printed on a miss), and a step
# that says "nothing happened" has such a wait first.
rm_dir="$DIR/remote"; mkdir -p "$rm_dir/calls" "$rm_dir/far-bin" "$rm_dir/studio/home" "$rm_dir/studio/config" "$rm_dir/studio/state"
rm_far="taw-e2e-remote-$$"                  # studio's tmux socket name
rm_app_s="rm-app-$$"; rm_hand_s="rm-hand-$$" # the outer sessions that host the app and a hand-attached client
rm_far_hand_s="rm-farhand-$$"                 # the outer session of a client attached by hand to studio's own server
rm_dead=0                                   # 1 once a step the next ones stand on did not happen
rm_far_file=""; rm_config_saved=""
rm_q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
# A call to studio's server is made as the far side makes it, under env -i with the far environment: a pane or window it
# makes gets the PATH of the client that asks for it, so that client must not carry this run's.
rm_far_tm() { env -i "${rm_far_env[@]}" "PATH=$rm_far_path" "$rm_dir/tmux-quiet" -L "$rm_far" "$@"; }
rm_screen() { outer capture-pane -p -t "=$rm_app_s:" 2>/dev/null; }
rm_styled() { outer capture-pane -p -e -t "=$rm_app_s:" 2>/dev/null; }
rm_hand_screen() { outer capture-pane -p -t "=$rm_hand_s:" 2>/dev/null; }
rm_hand_has() { rm_hand_screen | grep -qF -- "$1"; }
rm_hand_cursor_has() { outer capture-pane -p -e -t "=$rm_hand_s:" 2>/dev/null | grep -F '48;2;69;71;90m' | grep -qF -- "$1"; }
rm_has() { rm_screen | grep -qF -- "$1"; }
rm_press() { outer send-keys -t "=$rm_app_s:" "$@"; }
rm_wait() { local ms="$1" waited=0; shift; while [ "$waited" -lt "$ms" ]; do "$@" && return 0; command sleep 0.1; waited=$((waited+100)); done; return 1; }
# Tests that read something afresh each time they are tried, for rm_wait.
rm_far_clients_are() { [ "$(rm_far_clients)" = "$1" ]; }
rm_selected_has() { rm_selected | grep -qF -- "$1"; }
# A row's name part: its words after the list's left border, before the state word (the first of the words taw prints
# there: idle, working, blocked, done, unknown, see AgentState in src/store.rs) and what comes after it.
rm_row_name() { local r=${1#*│} w; for w in idle working blocked "done" unknown; do r=${r%%"$w"*}; done; printf '%s' "$r"; }
rm_selected_is_not() { [ "$(rm_selected)" != "$1" ]; }
rm_colours_are() { [ "$(rm_colours "$1")" = "$2" ]; }
# A line of the list with studio's dot and name and the words of a machine that is not live.
rm_studio_header_is_away() { rm_screen | grep -F -- '● studio' | grep -qE 'not answering|connecting'; }
rm_dump() { rm_screen | grep -v '^ *$' | head -30; }
# One line when a step is left out because an earlier one it stands on did not happen (a broken stand-in would
# otherwise cost a wait at every step after it).
rm_live() { [ "$rm_dead" = 0 ] && return 0; echo "  SKIP  $1: an earlier step it stands on did not happen"; return 1; }
# rm_see <label> <text on the app's screen> [ms]: a positive wait. A miss is a failure, and the steps after it stand down.
rm_see() {
  [ "$rm_dead" = 0 ] || return 1
  if rm_wait "${3:-15000}" rm_has "$2"; then ok "$1"; return 0; fi
  bad "$1" "never on the screen: [$2]; the screen was:
$(rm_dump)"
  rm_dead=1; return 1
}
# The plain text of the row the cursor is on: the line drawn with the selection's background.
rm_selected() { rm_styled | grep -F -m1 '48;2;69;71;90m' | sed $'s/\x1b\\[[0-9;]*[A-Za-z]//g'; }
rm_far_clients() { rm_far_tm list-clients -F '#{client_pid}' 2>/dev/null | grep -c .; }
# What studio's server has under each client: the pane that client is looking at.
rm_far_client_pane() { rm_far_tm list-clients -F '#{pane_id}' 2>/dev/null | head -1; }
rm_apps_are() { [ "$(rm_apps)" = "$1" ]; }
rm_apps() { local f c=0; for f in "$TAW_STATE_DIR"/apps/*.json; do [ -f "$f" ] && c=$((c+1)); done; echo "$c"; }
# The apps this section started, by the pid their records are named for: noted whenever a stage has started one.
rm_app_pids=" "
# Everything the section starts is noted for the run's own take-down (e2e_track: its pid and its whole command line)
# as soon as it is known, so that a run that stops inside the section leaves none of it: rm_track <kind> <pid>.
RM_TRACKED=(); rm_seen=$'\n'
rm_track() {
  local cmd; cmd=$(e2e_command_of "$2")
  [ -n "$cmd" ] || return 0
  case "$rm_seen" in *$'\n'"$2 $cmd"$'\n'*) return 0 ;; esac
  rm_seen="$rm_seen$2 $cmd"$'\n'; RM_TRACKED+=("$1 $2 $cmd"); e2e_track "$2" "$cmd"
}
# Every call of the stand-in, whatever it is doing now: its pid is noted as soon as it starts (before it reads its
# verb) and `exec` keeps it, so the pid is noted again with each new command line it takes.
rm_track_calls() {
  local f pid
  for f in "$rm_dir"/calls/*.pid; do
    [ -f "$f" ] || continue
    pid=$(cat "$f")
    # A pid noted long ago may be another process's by now: only a line that is the stand-in's, the far taw's (`taw machine `)
    # or the attach of the hand (tmux on studio's socket, whose line holds `-L <socket> `) counts.
    case "$(e2e_command_of "$pid")" in *"$rm_dir"*|*"taw machine "*|*"-L $rm_far "*) rm_track far "$pid" ;; esac
  done
}
# The apps (a record is named for its pid), their children, and the far side's feed and attach.
rm_note_apps() {
  local f p c
  for f in "$TAW_STATE_DIR"/apps/*.json; do
    [ -f "$f" ] || continue
    p=${f##*/}; p=${p%.json}
    case "$rm_app_pids" in *" $p "*) continue ;; esac
    rm_app_pids="$rm_app_pids$p "
    rm_track app "$p"
    for c in $(ps -Ao pid=,ppid= | awk -v p="$p" '$2 == p { print $1 }'); do rm_track app "$c"; done
  done
  rm_track_calls
}

# The environment of studio's side: its own tmux (the private socket name, a tmux that reads no config), its own
# config and state folders, and a home of its own that no shell reads a startup file or writes a history in.
rm_h="$rm_dir/studio/home"
printf '#!/bin/sh\nHISTFILE=/dev/null HOME=%s ZDOTDIR=%s SHELL=/bin/sh exec tmux -f /dev/null "$@"\n' "$rm_h" "$rm_h" > "$rm_dir/tmux-quiet"; chmod +x "$rm_dir/tmux-quiet"
mkdir -p "$rm_dir/studio/state/agent-detection" && : > "$rm_dir/studio/state/agent-detection/last-check"
ln -sf "$TAW_BIN" "$rm_dir/far-bin/taw"
rm_far_env=(
  HOME="$rm_h" ZDOTDIR="$rm_h" HISTFILE=/dev/null
  XDG_CONFIG_HOME="$rm_h/.config" XDG_DATA_HOME="$rm_h/.local/share" XDG_STATE_HOME="$rm_h/.local/state"
  CLAUDE_CONFIG_DIR="$rm_h/.claude" CODEX_HOME="$rm_h/.codex"
  TAW_TMUX_SOCKET_NAME="$rm_far" TAW_TMUX_BIN="$rm_dir/tmux-quiet" TMUX_TMPDIR="$TMUX_TMPDIR"
  TAW_CONFIG_DIR="$rm_dir/studio/config" TAW_STATE_DIR="$rm_dir/studio/state"
  ALL_PROXY="$ALL_PROXY" all_proxy="$ALL_PROXY" NO_PROXY= no_proxy=
)
[ -z "${TAW_NOTIFY_CMD:-}" ] || rm_far_env+=(TAW_NOTIFY_CMD="$TAW_NOTIFY_CMD")
[ -z "${TAW_SOUND_CMD:-}" ] || rm_far_env+=(TAW_SOUND_CMD="$TAW_SOUND_CMD")
# The far PATH: the nets of the run (an agent or ssh reached from there is logged and refused), the folder of the
# `taw` under test, tmux, the system's.
rm_far_path="${AGENT_NET_DIR:+$AGENT_NET_DIR/bin:}${TAW_DESKTOP_NET:+$TAW_DESKTOP_NET/bin:}$rm_dir/far-bin:$(dirname "$(command -v tmux)"):/usr/bin:/bin"

# The stand-in for ssh: tests/common/machine.rs's StandIn, in shell. It records each call's arguments and its own
# pid (the far side keeps that pid: `exec`), takes the options up to the destination, and runs the command words
# on studio's side as the far `taw` of the binary under test. Like the Rust one it has no `--` case: a `--` word is
# skipped as any other flag (the real ssh ends its options there, and taw puts none before the destination).
# A file `refuse` makes every call fail as a refused connection does (status 255).
{
  printf '#!/bin/sh\n'
  printf 'dir=%s\n' "$(rm_q "$rm_dir")"
  printf 'far_path=%s\n' "$(rm_q "$rm_far_path")"
  printf 'far_run() {\n  exec env -i'
  for rm_w in "${rm_far_env[@]}"; do printf ' %s' "$(rm_q "$rm_w")"; done
  # shellcheck disable=SC2016  # the stand-in's own $far_path, expanded when it runs
  printf ' "PATH=%s" "$@"\n}\n' '$far_path'
  cat <<'STANDIN'
n=1
tty=
while ! mkdir "$dir/calls/$n.d" 2>/dev/null; do n=$((n + 1)); done
printf '%s\0' "$@" > "$dir/calls/$n"
echo $$ > "$dir/calls/$n.pid"
while [ $# -gt 0 ]; do
  case $1 in
    -o | -p | -l | -i | -F | -J) shift 2 ;;
    -n) exec </dev/null; shift ;;
    -t) tty=tty; shift ;;
    -*) shift ;;
    *) break ;;
  esac
done
dest=${1:-}
[ $# -gt 0 ] && shift
if [ "$dest" != studio ]; then
  echo "ssh: Could not resolve hostname $dest: nodename nor servname provided, or not known" >&2
  exit 255
fi
if [ -f "$dir/refuse" ]; then
  echo "ssh: connect to host $dest port 22: Connection refused" >&2
  exit 255
fi
if [ "$tty" = tty ] && [ -n "${TERM:-}" ]; then
  far_run "TERM=$TERM" sh -c "$*"
fi
far_run sh -c "$*"
STANDIN
} > "$rm_dir/ssh"
chmod +x "$rm_dir/ssh"
rm_calls() { local f c=0; for f in "$rm_dir"/calls/[0-9]*; do case "$f" in *.pid|*.d) ;; *) [ -f "$f" ] && c=$((c+1)) ;; esac; done; echo "$c"; }
# The end of the section, on every path: what it made is ended by what was noted, never by name.
rm_cleanup() {
  local e rest
  outer send-keys -t "=$rm_app_s:" C-c 2>/dev/null
  outer send-keys -t "=$rm_hand_s:" C-c 2>/dev/null
  command sleep 0.3
  outer kill-session -t "=$rm_app_s" 2>/dev/null
  outer kill-session -t "=$rm_hand_s" 2>/dev/null
  outer kill-session -t "=$rm_far_hand_s" 2>/dev/null
  rm_track_calls
  for e in ${RM_TRACKED[@]+"${RM_TRACKED[@]}"}; do
    rest=${e#* }
    [ "$(e2e_command_of "${rest%% *}")" = "${rest#* }" ] && kill "${rest%% *}" 2>/dev/null
  done
  tmux -L "$rm_far" kill-server 2>/dev/null
  e2e_remove_socket "$rm_far_file"
  rm -f "$TAW_CONFIG_DIR/config.toml"
  [ -z "$rm_config_saved" ] || mv "$rm_config_saved" "$TAW_CONFIG_DIR/config.toml"
}

# --- the net is in front, and the stand-in is not it
case "$(command -v ssh)" in "${AGENT_NET_DIR:-/nonexistent}"/bin/ssh) ok "ssh on the PATH is the net's refusing stub" ;; *) bad "ssh on the PATH is the net's refusing stub" "$(command -v ssh)" ;; esac

# --- studio: a session with an agent pane, and a plain second session
# Studio's server starts under env -i with the far side's own environment (the one the stand-in gives the far taw),
# so that nothing of this run's (its config folders, its socket name, its zoxide data) is in a pane or job of studio's.
env -i "${rm_far_env[@]}" "PATH=$rm_far_path" "$rm_dir/tmux-quiet" -L "$rm_far" new-session -d -s garden -x 120 -y 40 -c "$rm_h"
# No later client copies anything of its own environment (ssh agent socket, display) into a session of studio's.
rm_far_tm set-option -g update-environment ''
rm_far_file=$(rm_far_tm display-message -p '#{socket_path}' 2>/dev/null)
e2e_track_socket "$rm_far_file"
check "studio's server named its socket file" "1" "$([ -S "$rm_far_file" ] && echo 1 || echo 0)"
rm_far_env_seen=$(rm_far_tm show-environment -g 2>/dev/null)
check "studio's server has the far side's socket name in its environment" "TAW_TMUX_SOCKET_NAME=$rm_far" "$(printf '%s\n' "$rm_far_env_seen" | grep '^TAW_TMUX_SOCKET_NAME=')"
check "... and nothing of this run's: no zoxide data folder, no TMUX, a Claude config folder that is studio's" "0 0 CLAUDE_CONFIG_DIR=$rm_h/.claude" "$(printf '%s\n' "$rm_far_env_seen" | grep -c '^_ZO_DATA_DIR=') $(printf '%s\n' "$rm_far_env_seen" | grep -c '^TMUX=') $(printf '%s\n' "$rm_far_env_seen" | grep '^CLAUDE_CONFIG_DIR=')"
# Noted for the run's own take-down as well (e2e_down ends it by this pid and this command line, and removes the
# socket file noted above), so that a run that stops half way in this section leaves no server of studio's.
rm_far_pid=$(rm_far_tm display-message -p '#{pid}' 2>/dev/null)
rm_track server "$rm_far_pid"
rm_far_agent=$(rm_far_tm new-window -d -t garden: -n work -P -F '#{pane_id}')
# An agent as the far taw lists one: the pane's own options, set as `taw agent start` sets them, with no agent
# program or watcher behind them (the feed reads the options, and no pane of studio is classified by a watcher here).
rm_far_tm set-option -p -t "$rm_far_agent" @agent_kind claude
rm_far_tm set-option -p -t "$rm_far_agent" @agent_state idle
rm_far_tm set-option -p -t "$rm_far_agent" @agent_name gardener
# Made as garden is, under env -i with the far environment, never from a client of this run's shell. The guard is
# rm_far_tm under env -i (what the check below proves); the env -i here and update-environment '' are defence in depth,
# which no check can prove without starting a client from the runner's shell, and the section never does that.
env -i "${rm_far_env[@]}" "PATH=$rm_far_path" "$rm_dir/tmux-quiet" -L "$rm_far" new-session -d -s orchard -c "$rm_h"
# What a pane of orchard has (a pane command is not a login shell, so PATH is as the server gave it): printed as set or
# unset, never as a value.
rm_far_tm new-window -d -t orchard: -n probe "{ [ -n \"\${TAW_TMUX_SOCKET_NAME:-}\" ] && echo far=set || echo far=unset; [ -n \"\${SSH_AUTH_SOCK+x}\" ] && echo ssh=set || echo ssh=unset; [ \"\$PATH\" = $(rm_q "$rm_far_path") ] && echo path=far || echo path=other; } > $(rm_q "$rm_dir/orchard-env.part"); mv $(rm_q "$rm_dir/orchard-env.part") $(rm_q "$rm_dir/orchard-env")"
rm_far_tm bind-key -n F5 run-shell "env -u TMUX -u TMUX_PANE ${rm_far_env[*]} $(rm_q "$TAW_BIN") ui open >/dev/null 2>&1"
rm_wait 5000 test -f "$rm_dir/orchard-env"
check "a pane of orchard has the far variable, no ssh agent socket, and the far PATH" "far=set ssh=unset path=far" "$(tr '\n' ' ' < "$rm_dir/orchard-env" 2>/dev/null | sed 's/ $//')"

# --- this Mac: studio in [machines], and the numbering to compare at the end
[ ! -f "$TAW_CONFIG_DIR/config.toml" ] || { rm_config_saved="$rm_dir/config.saved"; mv "$TAW_CONFIG_DIR/config.toml" "$rm_config_saved"; }
printf '[machines]\nstudio = "studio"\n' > "$TAW_CONFIG_DIR/config.toml"
# A claude on this Mac's own server as well, so that the walk has a stop on each side and a machine dropping off
# leaves one for Tab to go to (the fake claude of the harness, idle).
rm_mac_agent=$(e2e_start_agent claude)
e2e_wait_opt "$rm_mac_agent" @agent_state idle 7000 >/dev/null
rm_sessions_before=$(tm list-sessions -F '#{session_id} #{session_name}')

# --- (a) the list with the one machine
outer new-session -d -s "$rm_app_s" -x 160 -y 40 "exec env -u TMUX -u TMUX_PANE TAW_SSH_BIN=$(rm_q "$rm_dir/ssh") $(rm_q "$TAW_BIN") app 2>$(rm_q "$rm_dir/app-err")"
rm_see "(a) the app's list has studio's header" " studio"
rm_see "(a) ... and studio's agent row" "gardener"
rm_see "(a) ... and this Mac's own agent row" "claude"
rm_note_apps
# The words a machine that is away is answered with, read from the source that says them (src/machine/words.rs,
# `not_connected`), and not typed again here.
rm_words=$(sed -n 's/.*"{name} \(is not connected\) (last seen.*/\1/p' "$E2E_REPO/src/machine/words.rs" | head -1)
check "the not-connected words were read off the source" "is not connected" "$rm_words"
# A row drawn greyed has one colour on its words: the number of distinct foreground colours in the part of the
# line of the row of <text> that is the list's own (after its left border, up to the row's state word `idle`).
rm_colours() {
  local line
  line=$(rm_styled | grep -F -m1 -- "$1"); line=${line#*│}; line=${line%%idle*}
  printf '%s' "$line" | grep -o '38;2;[0-9;]*' | sort -u | wc -l | tr -d ' '
}
# Tab (the walk) until the cursor is on the row of <text>, at most <n> presses, each waited for by a change of the row.
rm_tab_to() {
  local text="$1" n="$2" i=0 row
  while [ "$i" -lt "$n" ]; do
    row=$(rm_selected); case "$row" in *"$text"*) return 0 ;; esac
    rm_press Tab
    rm_wait 3000 rm_selected_is_not "$row"
    i=$((i+1))
  done
  row=$(rm_selected); case "$row" in *"$text"*) return 0 ;; esac
  return 1
}

# --- (b) Tab onto studio's agent row, Enter: studio's tmux is on the screen, at that agent's pane
if rm_live "(b) and (c), the walk into studio and back"; then
if rm_tab_to gardener 4; then ok "(b) Tab walks to studio's agent row"; else bad "(b) Tab walks to studio's agent row" "the cursor row: [$(rm_selected)]"; rm_dead=1; fi
rm_colours_alive=$(rm_colours gardener)
if [ "$rm_colours_alive" -gt 1 ]; then ok "(b) a live row of studio is drawn in more than one colour (the control of the greyed check below)"; else bad "(b) a live row of studio is drawn in more than one colour" "colours: $rm_colours_alive"; fi
[ "$rm_dead" != 0 ] || rm_press Enter
rm_see "(b) Enter puts studio's tmux on the screen" "[garden]"
rm_track_calls
# The attach of the hand is tracked: noted by its pid, and that pid's line holds studio's socket name.
rm_attach_noted=$(printf '%s\n' ${RM_TRACKED[@]+"${RM_TRACKED[@]}"} | awk '$1 == "far" { print $2 }' | while read -r rm_p; do case "$(e2e_command_of "$rm_p")" in *"-L $rm_far "*attach-session*) echo 1 ;; esac; done | head -1)
check "(b) the attach of the hand is tracked by its pid, whose command line holds studio's socket name" "1" "$rm_attach_noted"
[ "$rm_dead" != 0 ] || rm_wait 10000 rm_far_clients_are 1
check "(b) studio's server has one client, the app's" "1" "$(rm_far_clients)"
check "(b) ... looking at the pane of the agent row" "$rm_far_agent" "$(rm_far_client_pane)"

# --- (c) the popup key of studio's session takes the app back
[ "$rm_dead" != 0 ] || rm_press F5
[ "$rm_dead" != 0 ] || rm_wait 15000 rm_has "Enter go in"
rm_see "(c) the popup key brings the app's list back" " taw app"
[ "$rm_dead" != 0 ] || rm_wait 10000 rm_far_clients_are 0
check "(c) studio's server has no client of the app left" "0" "$(rm_far_clients)"
check "(c) the cursor is back on the row the app went in from" "gardener" "$(rm_selected | grep -o gardener)"
fi

# --- (d) this Mac's session numbers are as they were
check "(d) this Mac's sessions have the same ids and names" "$rm_sessions_before" "$(tm list-sessions -F '#{session_id} #{session_name}')"

# --- (e) the doorway: a client attached by hand to this Mac's server, the popup (the plugin's key, prefix A),
# Enter on studio's session, and that terminal's client becomes the app, inside studio. The app of (a) to (d) is
# still up and keeps studio's feed, from which the popup reads studio's rows (tests/app_doorway.rs, on the real
# plugin's own key). The shell of the hand-attached client has no variable naming this Mac's server, as a person's
# terminal has none: the doorway's command carries the server itself.
rm_e_dead=0
if rm_live "(e) the doorway"; then
rm_hosts=$(tm list-clients -F '#{client_pid}')
rm_sock_path=$(tm display-message -p '#{socket_path}')
outer new-session -d -s "$rm_hand_s" -x 160 -y 40 "env -u TMUX -u TMUX_PANE -u TAW_TMUX_SOCKET_NAME -u TAW_TMUX_SOCKET_PATH TAW_SSH_BIN=$(rm_q "$rm_dir/ssh") tmux -S $(rm_q "$rm_sock_path") attach -t '=main'; echo \$? > $(rm_q "$rm_dir/hand-status")"
rm_new_client() { tm list-clients -F '#{client_pid}' | grep -vxF -e "$rm_hosts" | head -1; }
rm_new_client_is_there() { [ -n "$(rm_new_client)" ]; }
rm_hand_client_is_gone() { ! tm list-clients -F '#{client_pid}' | grep -qxF "$rm_hand_pid"; }
rm_wait 8000 rm_new_client_is_there
rm_hand_pid=$(rm_new_client)
[ -z "$rm_hand_pid" ] || rm_track hand "$rm_hand_pid"
# The client of that terminal, known by its tty, and the session it is on (by id): the popup is opened from it.
rm_hand_tty=$(outer display-message -p -t "=$rm_hand_s:" '#{pane_tty}')
rm_hand_client_session() { tm list-clients -F '#{client_tty} #{session_id}' | awk -v t="$rm_hand_tty" '$1 == t { print $2 }'; }
rm_hand_from=$(rm_hand_client_session)
rm_hand_is_back() { [ -n "$rm_hand_from" ] && [ "$(rm_hand_client_session)" = "$rm_hand_from" ]; }
rm_note_apps
check "(e) a client of this Mac's server was attached by hand" "1" "$([ -n "$rm_hand_pid" ] && echo 1 || echo 0)"
[ -n "$rm_hand_pid" ] || rm_e_dead=1
if [ "$rm_e_dead" = 0 ]; then
# The way back is checked against this id at the end: an empty one would match a terminal gone back to the shell.
check "(e) the session that client is on is known, by its id" "1" "$([ -n "$rm_hand_from" ] && echo 1 || echo 0)"
outer send-keys -t "=$rm_hand_s:" C-b A
rm_wait 8000 rm_hand_has garden
check "(e) the popup of that client has studio's session" "1" "$(rm_hand_has garden && echo 1 || echo 0)"
rm_hand_has garden || rm_e_dead=1
fi
if [ "$rm_e_dead" = 0 ]; then
outer send-keys -t "=$rm_hand_s:" 2
rm_wait 5000 rm_hand_cursor_has garden
outer send-keys -t "=$rm_hand_s:" Enter
rm_wait 15000 rm_hand_has "[garden]"
rm_note_apps
check "(e) Enter on studio's session puts studio's tmux on that terminal" "1" "$(rm_hand_has "[garden]" && echo 1 || echo 0)"
rm_hand_has "[garden]" || rm_e_dead=1
rm_wait 8000 rm_hand_client_is_gone
check "(e) the client of this Mac's server is gone: that terminal is the app now" "" "$(tm list-clients -F '#{client_pid}' | grep -xF "$rm_hand_pid")"
check "(e) the doorway attached studio's session by its id" "1" "$(for f in "$rm_dir"/calls/[0-9]*; do case "$f" in *.pid|*.d) continue ;; esac; tr '\0' ' ' < "$f"; echo; done | grep -c "'machine' 'attach' '\\$")"
fi
if [ "$rm_e_dead" = 0 ]; then
# Studio's client ends, as a person leaving studio's tmux would: the app is on its list, with this Mac's own
# session on it (a name nothing else on the screen could spell, and the newest session), and Ctrl-C ends it:
# the doorway's app gives the terminal back to tmux, a client of the session the popup was opened from.
rm_pin="rm-pin-$$"
tm new-session -d -s "$rm_pin"
rm_far_tm detach-client -s garden
rm_wait 15000 rm_hand_has "$rm_pin"
check "(e) with studio's client gone, the app's list is on that terminal, with this Mac's own session" "1" "$(rm_hand_has "$rm_pin" && echo 1 || echo 0)"
outer send-keys -t "=$rm_hand_s:" C-c
rm_wait 15000 rm_hand_is_back
check "(e) Ctrl-C ends the app and the terminal is a tmux client again, of the session the popup was opened from (not the newest)" \
  "$rm_hand_from" "$(rm_hand_client_session)"
# A person detaching that client gets the shell back, with the status of a clean detach.
[ -z "$(rm_hand_client_session)" ] || tm detach-client -t "$rm_hand_tty"
rm_wait 8000 test -s "$rm_dir/hand-status"
check "(e) a detach of that client gives the terminal back to the shell, status 0" "0" "$(cat "$rm_dir/hand-status" 2>/dev/null)"
tm kill-session -t "$rm_pin" 2>/dev/null
check "(e) this Mac's sessions are as they were" "$rm_sessions_before" "$(tm list-sessions -F '#{session_id} #{session_name}')"
fi
[ "$rm_e_dead" = 0 ] || echo "  SKIP  (e) the steps after the one that did not happen"
fi

# --- (g) n on studio's row: a session made on studio itself, with no client of studio's moved. A client of studio's
# own is attached by hand to garden first (a person at that machine), because `taw open` without --no-switch would
# move a client of the server it runs on. The folder is typed as `~/<name>`, which is studio's own home (the far
# taw expands it), and its session is named for it: the app goes in once its row is in the list (the cursor stayed
# on the row n was pressed on), which is studio's tmux on the screen at that session. Studio's client ends then
# (as (e) does), the session is ended, and the list is back for the step after this one.
rm_g_name="rmnew"; rm_g_hand=""   # short: the status line of studio's tmux cuts a session's name at 10 characters; studio's home is this run's alone
rm_g_hand_at() { rm_far_tm list-clients -F '#{client_pid} #{session_name}' 2>/dev/null | awk -v p="$rm_g_hand" '$1 == p { print $2 }'; }
rm_g_app_at() { rm_far_tm list-clients -F '#{client_pid} #{session_name}' 2>/dev/null | awk -v p="$rm_g_hand" '$1 != p { print $2 }'; }
rm_g_session_there() { rm_far_tm list-sessions -F '#{session_name}' 2>/dev/null | grep -qxF "$rm_g_name"; }
rm_g_asked() { local n=$(($1 + 1)) c=0 f; while [ "$n" -le "$(rm_calls)" ]; do f="$rm_dir/calls/$n"; tr '\0' ' ' < "$f" | grep -qF "'open' '--json' '--no-switch' '--' " && c=$((c+1)); n=$((n+1)); done; echo "$c"; }
rm_g_row_gone() { ! rm_has "$rm_g_name"; }
if rm_live "(g) n on studio's row"; then
mkdir -p "$rm_h/$rm_g_name"
rm_g_cmd="exec env -i"
for rm_w in "${rm_far_env[@]}"; do rm_g_cmd="$rm_g_cmd $(rm_q "$rm_w")"; done
rm_g_cmd="$rm_g_cmd $(rm_q "PATH=$rm_far_path") TERM=xterm-256color $(rm_q "$rm_dir/tmux-quiet") -L $rm_far attach-session -t '=garden'"
outer new-session -d -s "$rm_far_hand_s" -x 160 -y 40 "$rm_g_cmd"
rm_wait 8000 rm_far_clients_are 1
rm_g_hand=$(rm_far_tm list-clients -F '#{client_pid}' 2>/dev/null | head -1)
[ -z "$rm_g_hand" ] || rm_track hand "$rm_g_hand"
check "(g) a client of studio's own is attached by hand, to garden" "garden" "$(rm_g_hand_at)"
[ "$(rm_g_hand_at)" = garden ] || rm_dead=1
if [ "$rm_dead" = 0 ]; then
  rm_tab_to gardener 4
  check "(g) the cursor is on a live row of studio" "gardener" "$(rm_selected | grep -o gardener)"
  rm_g_calls=$(rm_calls)
  rm_press n
  rm_see "(g) n on a row of studio asks for a folder on studio" "new session on studio"
fi
if [ "$rm_dead" = 0 ]; then
  # shellcheck disable=SC2088  # typed as it is, a tilde: studio's taw expands it
  rm_press -l "~/$rm_g_name"
  rm_press Enter
  rm_wait 15000 rm_g_session_there
  check "(g) studio's own server has the session" "1" "$(rm_g_session_there && echo 1 || echo 0)"
  check "(g) studio's taw was asked as the app asks it: open --json --no-switch -- <the folder>" "1" "$(rm_g_asked "$rm_g_calls")"
  rm_see "(g) the app goes into the session: studio's tmux is on the screen, at it" "[$rm_g_name]"
fi
if [ "$rm_dead" = 0 ]; then
  rm_wait 10000 rm_far_clients_are 2
  check "(g) studio's server has two clients: the one attached by hand and the app's" "2" "$(rm_far_clients)"
  check "(g) the client attached by hand has not moved: it is still on garden" "garden" "$(rm_g_hand_at)"
  check "(g) the app's client is on the new session" "$rm_g_name" "$(rm_g_app_at)"
  rm_far_tm detach-client -s "$rm_g_name"
  rm_see "(g) with the app's client gone, the app's list is back" " taw app"
fi
check "(g) the client attached by hand never moved, to the end" "garden" "$(rm_g_hand_at)"
# Studio is as it was: the session ended, the hand's terminal ended, and the list without the row (a wait for the
# toast to go as well, which names the session).
rm_far_tm kill-session -t "=$rm_g_name" 2>/dev/null
outer kill-session -t "=$rm_far_hand_s" 2>/dev/null
[ "$rm_dead" != 0 ] || rm_wait 12000 rm_g_row_gone
[ "$rm_dead" != 0 ] || check "(g) studio's list no longer names the session" "1" "$(rm_g_row_gone && echo 1 || echo 0)"
rm_wait 8000 rm_far_clients_are 0
check "(g) studio's server has no client left" "0" "$(rm_far_clients)"
fi

# --- (f) the machine dropping off while the app's list is open. The feed of studio (the stand-in's call that runs
# `machine feed`, by the pid it noted) is ended and the stand-in is told to refuse from then on, as a machine that
# went away does: the connection ends at once and the first try again is refused, which is the faster of the two
# ways the app drops a machine (a silent stream is dropped after 15 s, src/machine/conn.rs `quiet`). Ended by the
# pid of the call, after seeing that pid is the feed of this run.
rm_feed_pid() {
  local n=1 f
  while [ "$n" -le "$(rm_calls)" ]; do
    f="$rm_dir/calls/$n"
    if tr '\0' ' ' < "$f" | grep -qF "'machine' 'feed'"; then cat "$f.pid" 2>/dev/null; return 0; fi
    n=$((n+1))
  done
}
rm_feed=$(rm_feed_pid)
rm_track far "$rm_feed"
rm_attaches() { local n=1 c=0 f; while [ "$n" -le "$(rm_calls)" ]; do f="$rm_dir/calls/$n"; tr '\0' ' ' < "$f" | grep -qF "'machine' 'attach'" && c=$((c+1)); n=$((n+1)); done; echo "$c"; }
if rm_live "(f) the machine dropping off"; then
  rm_tab_to gardener 4
  check "(f) the list is open with the cursor on a live row of studio" "gardener" "$(rm_selected | grep -o gardener)"
  # The pid's own command line is the feed's (the program is a `taw`, its words `machine feed`), not a shell that
  # carries those words in its arguments: killing a shell would leave the feed, and the drop would be the slow one.
  rm_feed_cmd=$(e2e_command_of "$rm_feed")
  if [ "$(basename "${rm_feed_cmd%% *}")" = taw ] && case "${rm_feed_cmd#* }" in 'machine feed'*) true ;; *) false ;; esac; then ok "(f) the pid the stand-in noted is studio's feed"; else bad "(f) the pid the stand-in noted is studio's feed" "pid [$rm_feed] is [$rm_feed_cmd]"; rm_dead=1; fi
fi
if rm_live "(f) the drop"; then
  rm_attaches_before=$(rm_attaches)
  : > "$rm_dir/refuse"
  [ "$(e2e_command_of "$rm_feed")" = "$rm_feed_cmd" ] && kill "$rm_feed" 2>/dev/null
  # At most 8 s: a feed that ended is dropped at once (src/machine/conn.rs, end of file), while one that only went
  # quiet is dropped after 15 s, so a drop that took the slow way fails here.
  # The list's header of studio says it is away (its dot and name with the words of a machine that is not live):
  # taw names no machine on the top line while a header names it.
  if rm_wait 8000 rm_studio_header_is_away; then ok "(f) studio's header says it is away"; else bad "(f) studio's header says it is away" "never on the screen; the screen was:
$(rm_dump)"; rm_dead=1; fi
  rm_wait 2000 rm_colours_are gardener 1
  check "(f) studio's rows are greyed: the row of the agent is drawn in one colour" "1" "$(rm_colours gardener)"
  # Tab skips them: from the cursor on a greyed row it goes to this Mac's agent, the one stop left.
  rm_press Tab
  if rm_wait 2000 rm_selected_has claude; then ok "(f) Tab from a greyed row goes on to this Mac's agent"; else bad "(f) Tab from a greyed row goes on to this Mac's agent" "the cursor row: [$(rm_selected)]"; fi
  rm_mac_row=$(rm_row_name "$(rm_selected)")
  # Tab once more, from this Mac's last stop, which is also its first: with studio's rows live it would go to
  # studio's agent, and with them skipped it comes back to this Mac's first stop, the same row. `j` and `k` show
  # where the cursor is, in order, so that the screen has been drawn after the Tab before it is read: `j` goes
  # down onto the row under this Mac's agent, studio's session (greyed, a row the number key skips and the arrow
  # keys do not; it is where Enter is pressed next), and `k` goes back up to where Tab left the cursor.
  rm_press Tab
  rm_press j
  rm_wait 2000 rm_selected_has "2   garden"
  check "(f) the cursor goes down from there onto studio's session" "1" "$(rm_selected | grep -cF '2   garden')"
  rm_press k
  rm_wait 2000 rm_selected_has claude
  # The row's name part only (the age and the preview beside it change on their own): the same words, and they name claude.
  rm_landed_row=$(rm_row_name "$(rm_selected)")
  check "(f) Tab from this Mac's last stop lands on this Mac's first stop: the same row's name part, word for word, and it is claude's" "1 $rm_mac_row" "$(case "$rm_mac_row" in *claude*) echo 1 ;; *) echo 0 ;; esac) $rm_landed_row"
  rm_press j
  rm_wait 2000 rm_selected_has "2   garden"
  # Enter on a greyed row says the not-connected words (with "(last seen", which only the toast has), and runs nothing.
  rm_press Enter
  if rm_wait 5000 rm_has "studio $rm_words (last seen"; then ok "(f) Enter on a greyed row says studio is not connected"; else bad "(f) Enter on a greyed row says studio is not connected" "the screen was:
$(rm_dump)"; fi
  check "(f) ... and attached nothing" "$rm_attaches_before" "$(rm_attaches)"
fi

# --- the end: everything this section made is ended by what was noted, and none of it is left
rm_cleanup
rm_wait 8000 rm_apps_are 0
[ "$rm_dead" != 0 ] || check "the section noted at least the two apps it started (the list's and the doorway's)" "1" "$([ "$(echo "$rm_app_pids" | wc -w | tr -d ' ')" -ge 2 ] && echo 1 || echo 0)"
# Each process noted (rm_track) by its pid and its whole command line: left is the pid still having that line.
rm_left() {
  local e k rest c=0
  for e in ${RM_TRACKED[@]+"${RM_TRACKED[@]}"}; do
    k=${e%% *}; rest=${e#* }
    case " $1 " in *" $k "*) [ "$(e2e_command_of "${rest%% *}")" = "${rest#* }" ] && c=$((c+1)) ;; esac
  done
  echo "$c"
}
rm_left_is() { [ "$(rm_left "$1")" = "$2" ]; }
rm_wait 5000 rm_left_is app 0
check "no taw app of this section is left (an app whose terminal is gone spins)" "0" "$(rm_left app)"
rm_wait 5000 rm_left_is "far hand" 0
check "no process of the stand-in is left, and no client of the hand-attached terminal" "0" "$(rm_left "far hand")"
rm_wait 5000 rm_left_is server 0
check "studio's server is gone" "1 0" "$(tmux -L "$rm_far" list-sessions >/dev/null 2>&1; echo $?) $(rm_left server)"
check "... and its socket file" "0" "$([ -S "$rm_far_file" ] && echo 1 || echo 0)"
check "no record of an app of this section is left" "0" "$(rm_apps)"
tm kill-window -t "$rm_mac_agent" 2>/dev/null
check "this Mac's sessions are as they were at the end" "$rm_sessions_before" "$(tm list-sessions -F '#{session_id} #{session_name}')"
rm -rf "$rm_dir"
# end of the remote section

# Nothing but the harness's own two servers has a socket in the tmux directory this run made for itself:
# a tmux that named no socket would have started its server on `default` there (and not on the one of
# whoever runs this), and a section that left a server, or the file of one, behind would show as well.
check "the run's own tmux directory holds the harness's two sockets and nothing else" "$(printf '%s\n%s\n' "$OUTER" "$SOCK" | sort)" "$(private_tmux_names | sort)"

# Nothing the run started ran an agent, a container runtime or ssh from the PATH of whoever runs it: the net
# e2e_up put first on PATH logged no call (scripts/agent-net.sh).
check "no agent, container runtime or ssh ran from the run's own PATH" "" "$(agent_net_hits)"
# Nor did anything show a notification or play a sound to whoever runs it, from a section with no
# recorder of its own: the desktop net e2e_up put first on PATH logged no call (scripts/desktop-net.sh).
check "no notification or sound reached the desktop of whoever runs it" "" "$(desktop_net_hits)"

echo
echo "== acceptance: $pass passed, $fail failed =="
[ "$fail" -eq 0 ]
