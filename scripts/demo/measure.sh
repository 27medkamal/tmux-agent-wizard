#!/usr/bin/env bash
# Measures what taw costs while it watches agents: ten agent panes in a throwaway
# tmux server (five Claude Code stand-ins mid-turn, redrawing their spinner line
# like the real thing, three idle, two Codex mid-turn), then the memory and CPU of
# taw's own processes only, the per-pane watchers. The agents' cost is theirs.
#
#   scripts/demo/measure.sh [seconds]    # default 60
#
# Nothing touches your own tmux server or taw state.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
SECONDS_TO_MEASURE="${1:-60}"
SOCK="taw-measure-$$"
ROOT="$(mktemp -d)"

BIN="${TAW_DEMO_BIN:-}"
for candidate in "$BIN" "$REPO/target/release/taw" "$REPO/bin/taw"; do
  if [ -n "$candidate" ] && [ -x "$candidate" ]; then
    BIN="$candidate"
    break
  fi
done
[ -x "$BIN" ] || { echo "no taw binary: build one first (mise run build)" >&2; exit 1; }
BIN="$(cd "$(dirname "$BIN")" && pwd -P)/$(basename "$BIN")"

AGENT_BASH=bash
for b in /opt/homebrew/bin/bash /usr/local/bin/bash; do
  if [ -x "$b" ]; then
    AGENT_BASH="$b"
    break
  fi
done

t() { tmux -L "$SOCK" "$@"; }
cleanup() {
  t kill-server 2>/dev/null || true
  rm -rf "$ROOT"
}
trap cleanup EXIT

mkdir -p "$ROOT/home" "$ROOT/state" "$ROOT/config"
export HOME="$ROOT/home" TAW_STATE_DIR="$ROOT/state" TAW_CONFIG_DIR="$ROOT/config"
printf '[update]\nmanifest_check = false\n' >"$TAW_CONFIG_DIR/config.toml"

agent() {
  printf "exec %q %q %q %q %q %q" "$AGENT_BASH" "$HERE/agent.sh" "$1" "$2" "$3" "$4"
}

t -f /dev/null new-session -d -s measure -x 200 -y 50 "sleep 100000"
t set-option -g @agent-wizard-bin "$BIN"
t run-shell "bash '$REPO/tmux-agent-wizard.tmux'"

add() {
  local pane
  pane=$(t new-window -d -P -F '#{pane_id}' -t measure "$(agent "$1" "$2" "$3" "$4")")
  t set-option -p -t "$pane" @agent_kind_hint "$1"
}
for i in 1 2 3 4 5; do add claude api "Task $i" working; done
for i in 6 7 8; do add claude docs "Task $i" idle; done
for i in 9 10; do add codex infra "Task $i" working; done

echo "warming up for 10s..."
sleep 10

# The watchers are the taw processes tmux started with `pipe-pane`.
watchers() { pgrep -f "$BIN watch run" || true; }

cpu_seconds() {
  local pid="$1" stat ticks
  if [ -r "/proc/$pid/stat" ]; then
    stat=$(cut -d' ' -f14,15 "/proc/$pid/stat")
    ticks=$(getconf CLK_TCK)
    awk -v s="$stat" -v hz="$ticks" 'BEGIN { split(s, a, " "); printf "%.3f", (a[1] + a[2]) / hz }'
  else
    ps -o time= -p "$pid" | awk -F: '{ if (NF == 3) print $1 * 3600 + $2 * 60 + $3; else print $1 * 60 + $2 }'
  fi
}

declare -A before
for pid in $(watchers); do before[$pid]=$(cpu_seconds "$pid"); done
echo "measuring ${#before[@]} watchers for ${SECONDS_TO_MEASURE}s..."
sleep "$SECONDS_TO_MEASURE"

total_cpu=0
rss_list=""
count=0
for pid in "${!before[@]}"; do
  kill -0 "$pid" 2>/dev/null || continue
  after=$(cpu_seconds "$pid")
  used=$(awk -v a="$after" -v b="${before[$pid]}" 'BEGIN { printf "%.3f", a - b }')
  total_cpu=$(awk -v t="$total_cpu" -v u="$used" 'BEGIN { printf "%.3f", t + u }')
  rss_list="$rss_list $(ps -o rss= -p "$pid" | tr -d ' ')"
  count=$((count + 1))
done

echo
echo "$count watchers (one per pane) over ${SECONDS_TO_MEASURE}s:"
# shellcheck disable=SC2086
printf '%s\n' $rss_list | sort -n | awk '
  { v[NR] = $1 / 1024; sum += $1 / 1024 }
  END {
    m = (NR % 2) ? v[(NR + 1) / 2] : (v[NR / 2] + v[NR / 2 + 1]) / 2
    printf "  memory: %.1f MB each (median), %.1f MB for all of them\n", m, sum
  }'
awk -v c="$total_cpu" -v s="$SECONDS_TO_MEASURE" -v n="$count" 'BEGIN {
  printf "  CPU: %.2f%% of one core for all of them, %.3f%% each on average\n", c / s * 100, c / s * 100 / n
}'
