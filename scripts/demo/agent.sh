#!/usr/bin/env bash
# A stand-in coding agent for the demo recording and the cost measurement. It draws
# the screen and sets the terminal title the way the real agent does, so taw's
# detection rules read it like the real thing:
#
#   agent.sh <kind> <role> <task> <state>
#
# kind is claude, codex, gemini or opencode; role picks the conversation (screen.py draws Claude Code's and
# Codex's whole screen; Gemini CLI's and OpenCode's come from screens/<role>-<state>.txt); state is working,
# blocked or idle. Signals move it:
# USR1 working, USR2 blocked, HUP idle (a finished turn). Typing 1 at a blocked
# prompt approves it, as in Claude Code. While working, the spinner line ticks the way
# a real turn does, so a watcher sees a live pane.
kind="$1"
role="$2"
task="$3"
state="${4:-idle}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
started=$SECONDS
spin=(⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏)
tick=0
pass=0

title() { printf '\033]2;%s\007' "$1"; }

# OpenCode's idle state comes from its hook, not its screen: with
# `taw integration install opencode`, its plugin reports every change through
# `taw pane report-agent`. The stand-in does what that plugin does, once per change. taw
# throws away what a pane reported before its watcher took hold of the pane, so the
# stand-in tries again, every pass of the loop below, until the watcher is there.
reported=""
report() {
  [ "$kind" = opencode ] && [ -n "${TAW_DEMO_TAW:-}" ] && [ "$reported" != "$state" ] || return 0
  [ -n "$(tmux show-options -pqv -t "$TMUX_PANE" @agent_watch_pid 2>/dev/null)" ] || return 0
  sleep 2
  reported="$state"
  "$TAW_DEMO_TAW" pane report-agent "$TMUX_PANE" --source integration:opencode \
    --agent opencode --state "$state" >/dev/null 2>&1 &
}

# The whole screen comes from screen.py, drawn at the pane's size: once for each state and size (it is read at
# start and again on SIGWINCH), then each tick fills in the spinner glyph and the seconds. Autowrap and the
# cursor are off, as in the real programs (they draw their own cursor), so a line never wraps.
#
# The stand-in draws on the terminal's alternate screen, which the real Claude Code and Codex do not use: taw's
# popup wraps a preview line that is wider than the preview, except on the alternate screen, where it cuts the
# line at the preview's edge. Every word screen.py draws fits the preview already; what is wider is only the
# input box's rules and the background of a user turn or a diff line, which are cut cleanly there instead of
# wrapping onto rows of their own. taw itself is unchanged.
frame=""
size=""
draw_frame() {
  size="$(stty size 2>/dev/null || echo "30 100")"
  frame="$(python3 "$here/screen.py" "$kind" "$role" "$state" "${size#* }" "${size% *}")"
}

render() {
  [ -n "$frame" ] || draw_frame
  local out="${frame//\{SECS\}/$((SECONDS - started + 12))}"
  out="${out//\{SPIN\}/${glyphs[tick % ${#glyphs[@]}]}}"
  printf '\033[?7l\033[?25l\033[H%s\033[J' "$out"
  case "$kind/$state" in
  claude/working) title "${spin[tick % 10]} $task" ;;
  claude/*) title "✳ $task" ;;
  codex/working) title "${spin[tick % 10]} codex" ;;
  codex/*) title "codex" ;;
  gemini/*) title "Gemini CLI" ;;
  opencode/*) title "OpenCode" ;;
  esac
  report
}

# Claude Code's spinner glyphs, out and back.
glyphs=(· ✢ ✳ ✶ ✻ ✽ ✻ ✶ ✳ ✢)
change() {
  state="$1"
  frame=""
  render
}

# A new size (SIGWINCH, or a size that changed while the signal was missed) draws the screen again on the
# loop's next pass; an idle or blocked screen is drawn again every second as well, so whatever wiped it (a
# client switching to the window, a resize mid-draw) never leaves the pane blank for long.
trap 'frame=""' WINCH
trap 'started=$SECONDS; change working' USR1
trap 'change blocked' USR2
trap 'change idle' HUP
trap 'exit 0' TERM INT

printf '\033[?1049h'
render
while :; do
  # A key, a timeout (a status above 128, which a signal's trap also gives) or the end of
  # the terminal: the HUP trap above keeps the agent alive for the recording's own HUP, so it
  # is this check that ends the agent once its terminal is gone, instead of a loop that spins.
  IFS= read -rsn1 -t 0.3 key
  status=$?
  if [ "$status" -eq 0 ]; then
    if [ "$state" = blocked ] && [ "$key" = 1 ]; then
      # The prompt stays a moment with its choice made, as the real ones do, so the GIF can show the line chosen.
      sleep 0.7
      started=$SECONDS
      change working
    fi
  elif [ "$status" -le 128 ]; then
    exit 0
  fi
  pass=$((pass + 1))
  [ "$(stty size 2>/dev/null || echo "30 100")" = "$size" ] || frame=""
  if [ "$state" = working ]; then
    tick=$((tick + 1))
    render
  elif [ -z "$frame" ] || [ $((pass % 3)) -eq 0 ]; then
    render
  else
    report
  fi
done
