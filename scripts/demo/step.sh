#!/usr/bin/env bash
# Moves the demo's stand-in agents along. start.sh binds these to hidden keys
# (Alt-b or Ctrl-g, Alt-f or Ctrl-y, Alt-r) so the recording can trigger each beat exactly when it
# wants; run by hand inside the demo server they do the same.
#
#   step.sh block    web's Codex stops at an approval prompt
#   step.sh finish   infra's Claude Code finishes its review
#   step.sh reset    both back to working
pid_of() {
  tmux display-message -p -t "$(tmux show-option -gqv "@demo_pane_$1")" '#{pane_pid}'
}

case "${1:-}" in
block) kill -USR2 "$(pid_of web)" ;;
finish) kill -HUP "$(pid_of infra)" ;;
reset)
  kill -USR1 "$(pid_of web)"
  kill -USR1 "$(pid_of infra)"
  ;;
*)
  echo "usage: step.sh block|finish|reset" >&2
  exit 2
  ;;
esac
