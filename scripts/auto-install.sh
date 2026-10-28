#!/usr/bin/env bash
# Run in the background by tmux-agent-wizard.tmux. Fetches the prebuilt taw
# (scripts/install.sh --download), loads the plugin again so its keys and hooks
# are in place, and says how it went on the status line.
#
#   scripts/auto-install.sh install   # a first load found no taw
#   scripts/auto-install.sh update    # the plugin moved to a new version
#
# The plugin script sets @agent_wizard_installing before it starts this, so a
# second load in the meantime does not start another download; this clears it.
#
# An update that finds nothing newer than the taw it has (the release for the
# plugin's new version is not out yet, and the latest is that taw) changes nothing
# and says nothing; the next load tries again. One that replaces taw starts the
# watchers already running again from the new one (taw watch attach-all --restart),
# unless watching is off.
CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mode="${1:-install}"
log="$CURRENT_DIR/bin/install.log"
marker="$CURRENT_DIR/bin/.taw-release"
mkdir -p "$CURRENT_DIR/bin"

no_build=""
[ "$mode" = update ] && no_build=1
before=$(awk '{print $1; exit}' "$marker" 2>/dev/null)

if TAW_NO_BUILD="$no_build" bash "$CURRENT_DIR/scripts/install.sh" --download >"$log" 2>&1; then
  tmux set-option -gu @agent_wizard_installing
  version=$(awk '{print $1; exit}' "$marker" 2>/dev/null)
  if [ "$mode" = update ] && [ -n "$before" ] && [ "$version" = "$before" ]; then
    exit 0
  fi
  TAW_AFTER_INSTALL=1 bash "$CURRENT_DIR/tmux-agent-wizard.tmux"
  if [ "$mode" = update ]; then
    if [ "$(tmux show-option -gqv @agent-wizard-watch)" != off ]; then
      "$CURRENT_DIR/bin/taw" watch attach-all --restart >/dev/null 2>&1
    fi
    tmux display-message "tmux-agent-wizard: taw updated to ${version:-the new version}"
  else
    key=$(tmux show-option -gqv @agent-wizard)
    key="${key%% *}"
    tmux display-message "tmux-agent-wizard: taw is ready, press prefix + ${key:-A}"
  fi
else
  tmux set-option -gu @agent_wizard_installing
  if [ "$mode" = update ]; then
    tmux display-message "tmux-agent-wizard: could not update taw, the current one still works (see $log)"
  else
    tmux display-message "tmux-agent-wizard: could not install taw, see $log"
  fi
fi
