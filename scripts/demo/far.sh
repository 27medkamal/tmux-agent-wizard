#!/usr/bin/env bash
# The second machine of the GIF (scripts/demo/make-gif.py): a machine called home-server, which is a
# second throwaway tmux server on this computer with a home folder, config and state of its own, and
# stand-in agents of its own (one of them stops at a permission prompt). start.sh runs this when
# TAW_DEMO_FAR is set. Nothing here reaches a real host: `ssh` is a stand-in, a script that runs the
# far taw against the second server where a real ssh would log in (the same idea as
# tests/common/machine.rs).
#
# Needs from start.sh: ROOT, HERE, REPO, BIN, AGENT_BASH, FAR_SOCK (the tmux socket of the machine).
set -euo pipefail
: "${ROOT:?}" "${HERE:?}" "${REPO:?}" "${BIN:?}" "${AGENT_BASH:?}" "${FAR_SOCK:?}"
# The machine's projects are git repositories made here, as start.sh's are: none of git's variables
# that point it at another repository may turn them on one.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_NAMESPACE GIT_CONFIG GIT_IMPLICIT_WORK_TREE GIT_GRAFT_FILE GIT_NO_REPLACE_OBJECTS GIT_REPLACE_REF_BASE GIT_PREFIX GIT_SHALLOW_FILE

FAR="$ROOT/far"
mkdir -p "$FAR/home/code" "$FAR/state" "$FAR/config" "$ROOT/path"
export HOME="$FAR/home"
export TAW_STATE_DIR="$FAR/state"
export TAW_CONFIG_DIR="$FAR/config"
export TAW_DEMO_TAW="$BIN"
# The machine's own server is told apart from the near one by its socket, never by the environment
# the near one was started with.
unset TMUX TMUX_PANE TAW_TMUX_SOCKET_NAME TAW_TMUX_SOCKET_PATH
export PS1='\[\e[38;5;110m\]\w\[\e[0m\] $ '
cd "$HOME"
# The machine's projects live under the near machine's home folder, not under its own: the near popup writes a
# path under its own home as ~, and a path under the far home would show the demo's folder in a picture.
CODE="$ROOT/home/code"
mkdir -p "$CODE"

printf '[update]\nmanifest_check = false\n' >"$TAW_CONFIG_DIR/config.toml"

t() { tmux -L "$FAR_SOCK" "$@"; }
t kill-server 2>/dev/null || true

project() {
  local dir="$CODE/$1"
  mkdir -p "$dir"
  git -C "$dir" init -q -b main
  printf '# %s\n' "$1" >"$dir/README.md"
  git -C "$dir" add README.md
  git -C "$dir" -c commit.gpgsign=false -c user.name=demo -c user.email=demo@example.invalid commit -q -m init
}
project release
project bench

agent_cmd() {
  printf "sleep 0.4; exec %q %q %q %q %q %q" "$AGENT_BASH" "$HERE/agent.sh" "$1" "$2" "$3" "$4"
}

# release: Claude Code, stopped at a permission prompt, and a shell. bench: Codex at work.
pane=$(t -f "$HERE/tmux.conf" new-session -d -P -F '#{pane_id}' -s release -n claude -c "$CODE/release" -x 140 -y 38 \
  "$(agent_cmd claude release 'Cut the 0.9 release' blocked)")
t set-option -p -t "$pane" @agent_kind_hint claude
t new-window -d -t release: -n shell -c "$CODE/release"
pane=$(t new-session -d -P -F '#{pane_id}' -s bench -n codex -c "$CODE/bench" \
  "$(agent_cmd codex bench 'Find out why the nightly build got slower' working)")
t set-option -p -t "$pane" @agent_kind_hint codex
t set-option -g automatic-rename off

t set-option -g @agent-wizard-bin "$BIN"
t run-shell "bash '$REPO/tmux-agent-wizard.tmux'"

# The stand-in for ssh. It drops the options up to the destination, as ssh does, refuses any
# destination but home-server, and runs the command line it was given on this machine's server, in a clean
# environment with the far taw and the tmux of this run first on the PATH.
mkdir -p "$ROOT/path" "$FAR/bin"
ln -sf "$BIN" "$FAR/bin/taw"
ln -sf "$BIN" "$ROOT/path/taw"
TMUX_DIR="$(dirname "$(command -v tmux)")"
cat >"$ROOT/path/ssh" <<SH
#!/bin/sh
# The stand-in for ssh (scripts/demo/far.sh): the machine called home-server is a second tmux server on this computer.
tty=
while [ \$# -gt 0 ]; do
  case \$1 in
    -o | -p | -l | -i | -F | -J) shift 2 ;;
    -n) exec </dev/null; shift ;;
    -t) tty=1; shift ;;
    -*) shift ;;
    *) break ;;
  esac
done
dest=\${1:-}
[ \$# -gt 0 ] && shift
if [ "\$dest" != home-server ]; then
  echo "ssh: Could not resolve hostname \$dest: nodename nor servname provided, or not known" >&2
  exit 255
fi
term=
if [ -n "\$tty" ] && [ -n "\${TERM:-}" ]; then term="TERM=\$TERM"; fi
exec env -i HOME='$FAR/home' TAW_STATE_DIR='$FAR/state' TAW_CONFIG_DIR='$FAR/config' \\
  TAW_TMUX_SOCKET_NAME='$FAR_SOCK' TAW_TMUX_BIN='$TMUX_DIR/tmux' \$term \\
  PATH='$FAR/bin:$TMUX_DIR:/usr/bin:/bin' sh -c "\$*"
SH
chmod +x "$ROOT/path/ssh"
