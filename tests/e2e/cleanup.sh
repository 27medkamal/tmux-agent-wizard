#!/usr/bin/env bash
# Remove every throwaway tmux server this harness (or the bats/cargo suites)
# can leave behind: any socket whose name starts with a taw test prefix. The
# user's real "default" socket and any non-taw socket are never touched.
#
#   tests/e2e/cleanup.sh          # kill + remove taw-e2e-* / taw-test-* / taw-bats-*
#   tests/e2e/cleanup.sh --all    # also taw-* (all this project's test prefixes)
#
# Then the tmux socket directories of their own that the gate, the harness, the
# bats tests, the sweep and the library's own tests make (scripts/private-tmux-dir.sh:
# /tmp/taw-check.*, /tmp/taw-e2e-tmux.*, /tmp/taw-bats-tmux.*, /tmp/taw-sweep-tmux.*,
# /tmp/taw-test-tmux.*), which a run that crashed leaves behind. There only what is dead is taken away, in a
# directory that is the user's own: a socket that refuses a connection. A socket
# a server answers on is a run that is still going, perhaps somebody else's gate:
# it is named and left alone, never killed. No directory is removed, empty or not:
# a run that is going has none of its servers up at times (the gate's, while it
# formats and lints), and tmux takes /tmp for a $TMUX_TMPDIR that is gone, which
# would turn that run's net off. $TAW_CLEANUP_BASE is where those directories are
# looked for (/tmp unless a test says otherwise).
set -u
dir="${TMUX_TMPDIR:-${TMPDIR:-/tmp}}/tmux-$(id -u)"
[ -d "$dir" ] || dir="/tmp/tmux-$(id -u)"

shopt -s nullglob
if [ -d "$dir" ]; then
  if [ "${1:-}" = "--all" ]; then
    globs=("$dir"/taw-*)
  else
    globs=("$dir"/taw-e2e-* "$dir"/taw-test-* "$dir"/taw-bats-*)
  fi
  n=0
  # An empty array under set -u stops bash 3.2 (macOS's /bin/bash): written so that it is no error.
  for path in ${globs[@]+"${globs[@]}"}; do
    s="$(basename "$path")"
    [ "$s" = default ] && continue
    tmux -L "$s" kill-server 2>/dev/null
    rm -f "$path" 2>/dev/null && n=$((n+1))
  done
  echo "removed $n test socket(s) from $dir (kept: default and non-taw sockets)"
else
  echo "no tmux socket dir at $dir"
fi

base="${TAW_CLEANUP_BASE:-/tmp}"
uid="$(id -u)"
dead=0
empty=0
for private in "$base"/taw-check.* "$base"/taw-e2e-tmux.* "$base"/taw-bats-tmux.* "$base"/taw-sweep-tmux.* "$base"/taw-test-tmux.*; do
  # Only a directory, not a link to one, and the user's own.
  [ -d "$private" ] && [ ! -L "$private" ] && [ -O "$private" ] || continue
  sockets="$private/tmux-$uid"
  if [ -d "$sockets" ] && [ ! -L "$sockets" ]; then
    for path in "$sockets"/*; do
      [ -S "$path" ] || continue
      # list-sessions starts nothing, and fails only where no server answers.
      if tmux -S "$path" list-sessions >/dev/null 2>&1; then
        echo "left alone, a server answers on it (a run that is still going?): $path"
      else
        rm -f "$path" && dead=$((dead+1))
      fi
    done
  fi
  [ -n "$(ls -A "$sockets" 2>/dev/null)" ] || empty=$((empty+1))
done
echo "removed $dead dead socket(s) from the private tmux directories in $base, and left the $empty of them that hold no socket as they are (a run may still be using them)"
