#!/usr/bin/env bash
# Builds the demo world the README recording is made in: a throwaway tmux server
# (socket taw-demo) holding six small projects under a home folder of their own, eight
# stand-in agents among them, the plugin loaded from this checkout, and a parked
# worktree. taw's config and state, and the projects' $HOME, live under one temporary
# directory, so nothing touches your own tmux server, config or taw state.
#
#   scripts/demo/start.sh            # set up, then attach
#   scripts/demo/start.sh --detach   # set up only
#
# With TAW_DEMO_FAR set, a second machine is made as well (far.sh): the machine called home-server, a
# second throwaway tmux server with agents of its own, listed in [machines] and reached through a
# stand-in ssh. The stills (stills.tape) are made in that world; the GIF's has no second machine.
#
# With TAW_DEMO_STORY set (the README's GIF, make-gif.py), the world is the one the GIF tells its story in:
# the popup lists agents only (depth "agents"), no pane runs a dev server, an emulator or a plain shell, a
# second agent is stopped at a prompt (docs' Claude Code asking permission, beside web's Codex asking approval), and [machines] lists a machine that does
# not answer (work-laptop) beside home-server. TAW_DEMO_FAR is set with it, so home-server exists.
#
# Needs a built taw (target/release/taw or bin/taw; TAW_DEMO_BIN to point elsewhere) and
# a C compiler (cc) for the one-file stand-in of a container runtime's command line.
set -euo pipefail
# The projects are git repositories made here: none of git's variables that point it at another
# repository (git runs the command of a `rebase -x` with GIT_DIR set) may turn them on one.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_NAMESPACE GIT_CONFIG GIT_IMPLICIT_WORK_TREE GIT_GRAFT_FILE GIT_NO_REPLACE_OBJECTS GIT_REPLACE_REF_BASE GIT_PREFIX GIT_SHALLOW_FILE

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
SOCK="${TAW_DEMO_SOCKET:-taw-demo}"
# The projects live under the world's own home folder (ROOT/home/code), so the popup
# shows them as ~/code/api and the like.
ROOT="${TAW_DEMO_ROOT:-/tmp/taw-demo}"
ROOT="${ROOT%/}"

BIN="${TAW_DEMO_BIN:-}"
for candidate in "$BIN" "$REPO/target/release/taw" "$REPO/bin/taw"; do
  if [ -n "$candidate" ] && [ -x "$candidate" ]; then
    BIN="$candidate"
    break
  fi
done
[ -x "$BIN" ] || { echo "no taw binary: build one first (mise run build)" >&2; exit 1; }
command -v cc >/dev/null 2>&1 || { echo "no C compiler (cc): the container stand-in needs one" >&2; exit 1; }

# The newest bash around runs the agents: 4+ ticks the spinner smoothly.
AGENT_BASH=bash
for b in /opt/homebrew/bin/bash /usr/local/bin/bash; do
  if [ -x "$b" ]; then
    AGENT_BASH="$b"
    break
  fi
done

FAR_SOCK="${TAW_DEMO_FAR_SOCKET:-$SOCK-home}"

# The GIF's world (see the head of this file): agents only, no dev servers or shells, a second blocked agent.
STORY="${TAW_DEMO_STORY:-}"
DEPTH=all
if [ -n "$STORY" ]; then
  DEPTH=agents
  TAW_DEMO_FAR=1
fi

t() { tmux -L "$SOCK" "$@"; }

t kill-server 2>/dev/null || true
if [ -n "${TAW_DEMO_FAR:-}" ]; then
  tmux -L "$FAR_SOCK" kill-server 2>/dev/null || true
fi
rm -rf "$ROOT"
mkdir -p "$ROOT/home/code" "$ROOT/state" "$ROOT/config" "$ROOT/bin"

export HOME="$ROOT/home"
export TAW_STATE_DIR="$ROOT/state"
export TAW_CONFIG_DIR="$ROOT/config"
# For the OpenCode stand-in, which reports its state the way OpenCode's plugin does.
export TAW_DEMO_TAW="$BIN"
export PS1='\[\e[38;5;110m\]\w\[\e[0m\] $ '
cd "$HOME"
CODE="$HOME/code"

cat >"$TAW_CONFIG_DIR/config.toml" <<EOF
[update]
manifest_check = false

[ui.toast]
done_delay_seconds = 1

[worktrees]
directory = "$CODE/worktrees"

[ui]
depth = "$DEPTH"

[sessions]
project_dirs = ["~/projects"]

# Calmer rows for the screenshots, and a narrower list: an agent row is its state, its agent and its own
# title, with no window, state word or age; a session row is its name and its branch, and `main` is hidden.
# A title that is only the agent's own name (Codex, Gemini CLI and OpenCode set one) is hidden too.
# The popup's defaults are unchanged, and the session numbers, the row letters and the tree lines stay.
[ui.sidebar.agents]
rows = [["state_icon", "agent", { token = "terminal_title_stripped", rules = [
  { equals = "codex", hide = true },
  { equals = "Gemini CLI", hide = true },
  { equals = "OpenCode", hide = true },
] }]]

[ui.sidebar.sessions]
rows = [["workspace", { token = "branch", rules = [{ equals = "main", hide = true }] }]]
EOF
if [ -n "${TAW_DEMO_FAR:-}" ]; then
  # The machine called home-server: what ssh home-server runs is the stand-in in far.sh.
  printf '\n[machines]\nhome-server = "home-server"\n' >>"$TAW_CONFIG_DIR/config.toml"
  # work-laptop is a machine that does not answer: the stand-in ssh (far.sh) knows no destination but home-server.
  [ -z "$STORY" ] || printf 'work-laptop = "work-laptop"\n' >>"$TAW_CONFIG_DIR/config.toml"
fi

# Each project is a small git repo, so its session header shows a branch.
project() {
  local dir="$CODE/$1"
  mkdir -p "$dir"
  git -C "$dir" init -q -b "$2"
  printf '# %s\n' "$1" >"$dir/README.md"
  git -C "$dir" add README.md
  git -C "$dir" -c commit.gpgsign=false -c user.name=demo -c user.email=demo@example.invalid commit -q -m init
}
project api main
project web feat/signup-validation
project docs main
project infra main
project mobile main
project data main

# What the project picker (`n`) offers besides the running sessions: folders under ~/projects that no
# session is in yet (the project folders), and directories zoxide remembers, kept in a database of
# the world's own. Without zoxide installed the picker lists the project folders alone.
export _ZO_DATA_DIR="$ROOT/zoxide"
mkdir -p "$_ZO_DATA_DIR" "$HOME/projects/blog" "$HOME/projects/dotfiles" "$HOME/projects/recipes" "$HOME/work/billing" "$HOME/work/ledger" "$HOME/work/reports"
if command -v zoxide >/dev/null 2>&1; then
  for dir in "$HOME/work/ledger" "$HOME/work/billing" "$HOME/work/reports" "$CODE/web" "$CODE/api"; do
    zoxide add "$dir"
  done
fi

# Two stand-ins are programs named for what tmux shows in front of the pane (standin.c):
# `docker`, for the sandboxed agent, and `opencode`.
cc -O1 -o "$ROOT/bin/docker" "$HERE/standin.c"
cp "$ROOT/bin/docker" "$ROOT/bin/opencode"

# agent_cmd <kind> <role> <task> <state>: the command line of a stand-in agent. It waits a
# moment, so the pane's kind is named for taw before the agent draws anything.
agent_cmd() {
  printf "sleep 0.4; exec %q %q %q %q %q %q" "$AGENT_BASH" "$HERE/agent.sh" "$1" "$2" "$3" "$4"
}
# The OpenCode stand-in, run by the program called opencode.
opencode_cmd() {
  printf "sleep 0.4; exec env TAW_DEMO_STANDIN=%q %q" \
    "exec $(printf '%q ' "$AGENT_BASH" "$HERE/agent.sh" "$@")" "$ROOT/bin/opencode"
}
# The one in a "container": the same stand-in, run by the stand-in `docker`, with the
# arguments taw reads to find the agent (docker run ... image claude).
sandboxed_cmd() {
  local inner
  inner="exec $(printf '%q ' "$AGENT_BASH" "$HERE/agent.sh" "$@")"
  printf "sleep 0.4; exec env TAW_DEMO_STANDIN=%q %q run -it --rm --name data-claude --network none -v %q claude-sandbox:latest claude" \
    "$inner" "$ROOT/bin/docker" "$CODE/data:/work"
}

# A program that prints its start-up lines (JavaScript for node: console.log calls) and stays
# up, as a dev server or an emulator does.
serve() {
  printf 'node -e %q' "$1 setInterval(() => {}, 1e6)"
}

# api: Claude Code fixing a flaky test, the pane the popup is opened from; a second window
# with a Codex review.
pane=$(t -f "$HERE/tmux.conf" new-session -d -P -F '#{pane_id}' -s api -n code -c "$CODE/api" -x 110 -y 33 \
  "$(agent_cmd claude api 'Fix flaky auth test' working)")
t set-option -p -t "$pane" @agent_kind_hint claude
pane=$(t new-window -d -P -F '#{pane_id}' -t api: -n review -c "$CODE/api" \
  "$(agent_cmd codex review 'Review the retry change' idle)")
t set-option -p -t "$pane" @agent_kind_hint codex
[ -n "$STORY" ] || t new-window -d -t api: -n server -c "$CODE/api" \
  "$(serve "console.log('api listening on http://localhost:3000'); console.log('GET /health 200 2ms');")"

# web: Codex adding signup validation, over the dev server (in the GIF's world, asking to run the tests).
web_state=working
[ -z "$STORY" ] || web_state=blocked
pane=$(t new-session -d -P -F '#{pane_id}' -s web -n signup -c "$CODE/web" \
  "$(agent_cmd codex web 'Add signup form validation' "$web_state")")
t set-option -p -t "$pane" @agent_kind_hint codex
t set-option -g @demo_pane_web "$pane"
if [ -z "$STORY" ]; then
  t split-window -v -l 6 -t "$pane" -c "$CODE/web" \
    "node -e \"console.log('  VITE v6.2.0  ready in 412 ms'); console.log(''); console.log('  Local:   http://localhost:5173/'); setInterval(() => {}, 1e6)\""
  t select-pane -t "$pane"
  t new-window -d -t web: -n shell -c "$CODE/web"
fi

# docs: OpenCode, idle, and Claude Code with a finished changelog (in the GIF's world, asking permission first).
pane=$(t new-session -d -P -F '#{pane_id}' -s docs -n guide -c "$CODE/docs" \
  "$(opencode_cmd opencode docs 'Write the migration guide' idle)")
t set-option -p -t "$pane" @agent_kind_hint opencode
changelog_state=idle
[ -z "$STORY" ] || changelog_state=blocked
pane=$(t new-window -d -P -F '#{pane_id}' -t docs: -n changelog -c "$CODE/docs" \
  "$(agent_cmd claude changelog 'Draft the v2 changelog' "$changelog_state")")
t set-option -p -t "$pane" @agent_kind_hint claude
[ -n "$STORY" ] || t new-window -d -t docs: -n preview -c "$CODE/docs" \
  "$(serve "console.log('Serving docs on http://127.0.0.1:8000/');")"

# infra: Claude Code reviewing a plan, and a shell.
pane=$(t new-session -d -P -F '#{pane_id}' -s infra -n review -c "$CODE/infra" \
  "$(agent_cmd claude infra 'Review the terraform plan' working)")
t set-option -p -t "$pane" @agent_kind_hint claude
t set-option -g @demo_pane_infra "$pane"
[ -n "$STORY" ] || t new-window -d -t infra: -n shell -c "$CODE/infra"

# mobile: Gemini CLI at work.
pane=$(t new-session -d -P -F '#{pane_id}' -s mobile -n settings -c "$CODE/mobile" \
  "$(agent_cmd gemini mobile 'Scale the settings text' working)")
t set-option -p -t "$pane" @agent_kind_hint gemini
[ -n "$STORY" ] || t new-window -d -t mobile: -n emulator -c "$CODE/mobile" \
  "$(serve "console.log('Launching emulator phone_api_35 ...'); console.log('Emulator ready');")"

# data: Claude Code in a sandbox, and a shell. The sandbox stand-in is marked the way
# `taw sandbox` marks a pane (@agent_sandbox, and the pid it belongs to).
pane=$(t new-session -d -P -F '#{pane_id}' -s data -n pipeline -c "$CODE/data" \
  "$(sandboxed_cmd claude data 'Backfill the nightly metrics' working)")
t set-option -p -t "$pane" @agent_kind_hint claude
t set-option -p -t "$pane" @agent_sandbox default
t set-option -p -t "$pane" @agent_sandbox_pid "$(t display-message -p -t "$pane" '#{pane_pid}')"
[ -n "$STORY" ] || t new-window -d -t data: -n shell -c "$CODE/data"

# The plugin, from this checkout, with this taw.
t set-option -g @agent-wizard-bin "$BIN"
t run-shell "bash '$REPO/tmux-agent-wizard.tmux'"

# A worktree whose session is closed, parked under api as a row of its own.
if [ -z "$STORY" ]; then
  t run-shell "'$BIN' worktree create --workspace api --branch fix-login >/dev/null 2>&1 || true"
  t kill-session -t "api--fix-login" 2>/dev/null || true
fi

# Hidden keys that move the agents along (scripts/demo/step.sh). The recording uses
# the Ctrl ones: vhs's terminal types a symbol for Alt on macOS rather than sending it.
for key in M-b C-g; do t bind -n "$key" run-shell -b "bash '$HERE/step.sh' block"; done
for key in M-f C-y; do t bind -n "$key" run-shell -b "bash '$HERE/step.sh' finish"; done
t bind -n M-r run-shell -b "bash '$HERE/step.sh' reset"

t select-window -t api:code

if [ -n "${TAW_DEMO_FAR:-}" ]; then
  env ROOT="$ROOT" HERE="$HERE" REPO="$REPO" BIN="$BIN" AGENT_BASH="$AGENT_BASH" FAR_SOCK="$FAR_SOCK" \
    bash "$HERE/far.sh"
fi
# What a plain terminal needs to run taw app against this world: the folders, the tmux server, and the
# stand-in ssh (and a taw) first on the PATH. The recording reads it with `source`.
{
  printf 'export HOME=%q TAW_STATE_DIR=%q TAW_CONFIG_DIR=%q TAW_TMUX_SOCKET_NAME=%q\n' \
    "$HOME" "$TAW_STATE_DIR" "$TAW_CONFIG_DIR" "$SOCK"
  printf 'export _ZO_DATA_DIR=%q\n' "$_ZO_DATA_DIR"
  printf 'export PATH=%q\n' "$ROOT/path:$PATH"
  printf 'export PS1=%q\n' "$PS1"
  # A recording started from inside a tmux pane (or its popup) hands its terminal that pane's TMUX, and
  # taw app and tmux attach both refuse to run in what they take for a nested terminal.
  printf 'unset TMUX TMUX_PANE\n'
} >"$ROOT/env.sh"

if [ "${1:-}" != "--detach" ]; then
  exec tmux -L "$SOCK" attach -t api
fi
