#!/usr/bin/env bash
# shellcheck disable=SC2015  # `cond && ok || bad` is intentional; ok/bad always return 0
# Popup smoke test: open the attention popup on an isolated server, confirm it
# renders and closes. Prints the captured popup for eyeballing.
#
# "Rendered" is the tree panel's own title plus the hint line the popup draws
# under it, so a popup that opened but drew nothing still fails. (The window
# title `taw` would not do: tmux draws that itself, around whatever is inside.)
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=tests/e2e/harness.sh
source "$HERE/harness.sh"
# Set before e2e_up, so that a run that fails while it comes up still takes down what it started.
trap e2e_down EXIT
e2e_up smoke || exit 1
prefix A
if e2e_wait_screen ' sessions ' 5000 && screen | grep -q 'Tab next up'; then echo "PASS: popup rendered"; else echo "FAIL: popup did not render"; screen | head -20; exit 1; fi
keys q; command sleep 1
echo "-- popup after quit (should be a shell prompt):"; screen | tail -3
# Nothing but the harness's own two servers has a socket in the tmux directory this run made for itself.
if [ "$(private_tmux_names | sort)" = "$(printf '%s\n%s\n' "$OUTER" "$SOCK" | sort)" ]; then echo "PASS: no other tmux server in the run's own directory"; else echo "FAIL: the run's own tmux directory holds: $(private_tmux_names | tr '\n' ' ')"; exit 1; fi
