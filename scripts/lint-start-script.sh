#!/bin/sh
# Lints the sandbox's start script, which taw's default profile holds in a heredoc that shellcheck
# reads as text: as the POSIX sh script it is, the one the sandbox's image runs first.
set -eu
cd "$(dirname "$0")/.."
script="$(sed -n "/^  cat <<'START'\$/,/^START\$/p" src/sandbox/default-profile.sh | sed '1d;$d')"
case "$script" in
  '#!/bin/sh'*) ;;
  *) echo "lint-start-script: no start script in src/sandbox/default-profile.sh" >&2
     exit 1 ;;
esac
printf '%s\n' "$script" | shellcheck -s sh -
