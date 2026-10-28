#!/usr/bin/env bash
# taw's sandbox profile "default": a coding agent in a Docker container.
# taw runs it as: default <agent> [the agent's own arguments], in the project directory.
# taw replaces this file when it updates. To change it, make a copy of your own:
#   taw sandbox new <name>     (taw sandbox new default: your copy is used instead of this)
# TAW_SANDBOX_PRINT=1 prints the command this would run, and runs nothing.
#
# What it does: the agent runs in a container, as you, without asking before each command.
# It uses your own agent settings. It reaches the internet and your local network, and
# nothing on this machine. It has no Docker. Each recipe below changes one of those.
set -euo pipefail
unset ask_first settings block_local_network open_ports docker runtime tools pass

# Recipes: uncomment a line to use it. taw's README says what each one does.
# Have the agent ask before each command, as it does outside:
#   ask_first=yes
# Give it a separate set of settings, with yours read-only inside:
#   settings=own
# Block your local network too, so that it reaches only the internet:
#   block_local_network=yes
# Leave ports on this machine open to it, for example a local model proxy on 8317:
#   open_ports="8317"
# Let it look at Docker without touching it (docker ps, logs, inspect), through a proxy:
#   docker=read-only
# Let it use all of Docker. WARNING: this is the same as giving it this whole machine. It
# can start a container that reads and writes your entire disk, so the sandbox then keeps
# nothing out:
#   docker=full
# Use Podman instead of Docker:
#   runtime=podman
# Tools beyond what the project's mise.toml installs, as mise spells them:
#   tools="node@22 uv"
# Variables of yours to pass in, by name; their values come from your environment:
#   pass="OPENAI_API_KEY GH_TOKEN"

# The profile's name, for what it says: taw gives it; run by hand, it is the file's name.
profile="${TAW_SANDBOX_PROFILE:-$(basename "$0")}"
say() { printf 'taw sandbox %s: %s\n' "$profile" "$*" >&2; }
# How a start is going, once its first lines are said: the line as it is, no name in front.
tell() { printf '%s\n' "$*" >&2; }
if [ "$#" -eq 0 ]; then
  say "usage: $(basename "$0") <agent> [the agent's own arguments]"
  exit 2
fi

# Each agent: how it is installed inside, the folders of its settings it shares (and the variable
# that names them, when one can), and its switch for running without asking. The README's
# recipes say how each one logs in.
agent="$1"; shift
settings_from=""
case "$agent" in
  claude)   install=claude settings_from=CLAUDE_CONFIG_DIR
            if [ -n "${CLAUDE_CONFIG_DIR:-}" ]; then folders=("$CLAUDE_CONFIG_DIR")
            else folders=("$HOME/.claude" "$HOME/.claude.json"); fi
            skip=(--dangerously-skip-permissions) ;;
  codex)    install=npm:@openai/codex settings_from=CODEX_HOME
            folders=("${CODEX_HOME:-$HOME/.codex}")
            skip=(--dangerously-bypass-approvals-and-sandbox) ;;
  gemini)   install=npm:@google/gemini-cli
            folders=("$HOME/.gemini")
            skip=(--yolo) ;;
  opencode) install=opencode
            folders=("$HOME/.config/opencode" "$HOME/.local/share/opencode")
            skip=(--auto) ;;
  copilot)  install=npm:@github/copilot settings_from=COPILOT_HOME
            folders=("${COPILOT_HOME:-$HOME/.copilot}")
            skip=(--allow-all) ;;
  cursor-agent) install=installer:https://cursor.com/install settings_from=CURSOR_CONFIG_DIR
            folders=("${CURSOR_CONFIG_DIR:-$HOME/.cursor}")
            skip=(--force) ;;
  amp)      install=npm:@sourcegraph/amp
            folders=("$HOME/.config/amp")
            skip=(--dangerously-allow-all) ;;
  qwen)     install=npm:@qwen-code/qwen-code settings_from=QWEN_HOME
            folders=("${QWEN_HOME:-$HOME/.qwen}")
            skip=(--yolo) ;;
  droid)    install=installer:https://app.factory.ai/cli
            folders=("$HOME/.factory")
            skip=(--auto high) ;;
  kimi)     install=installer:https://code.kimi.com/kimi-code/install.sh settings_from=KIMI_CODE_HOME
            folders=("${KIMI_CODE_HOME:-$HOME/.kimi-code}")
            skip=(--auto) ;;
  pi)       install=npm:@mariozechner/pi-coding-agent settings_from=PI_CODING_AGENT_DIR
            folders=("${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}")
            skip=() ;;
  letta)    install=npm:@letta-ai/letta-code
            folders=("$HOME/.letta")
            skip=(--yolo) ;;
  *) say "no recipe for $agent; add one to a copy (taw sandbox new)"
     exit 1 ;;
esac
# With ask_first the agent asks as it does outside: its switch goes. One with none (pi never asks)
# runs as it always does.
asks=no
if [ "${ask_first:-no}" != no ] && [ -n "${skip[*]:-}" ]; then
  skip=()
  asks=yes
fi
agent_name="${TAW_SANDBOX_AGENT:-$agent}"

runtime="${runtime:-docker}"
case "$runtime" in
  docker|podman) ;;
  *) say "runtime=$runtime is not one this profile knows (docker or podman)"
     exit 1 ;;
esac
# pass is names, written out: a pattern would be matched against the project's files.
case "${pass:-}" in
  *[*?[]*)
    say "pass takes the names of variables, not patterns; write each name out in its line in this profile"
    exit 1 ;;
esac
for variable in ${pass:-}; do
  case "$variable" in
    TMUX|TMUX_*)
      say "pass names $variable, and tmux's variables never go in: whoever holds its socket can type into every pane you have"
      exit 1 ;;
    *[!A-Za-z0-9_]*|[0-9]*)
      say "pass takes the names of variables, not their values; fix its line in this profile"
      exit 1 ;;
  esac
done
case "${settings:-shared}" in
  shared|own) ;;
  *) say "settings=$settings is not one of this profile's recipes (own)"
     exit 1 ;;
esac
# open_ports is port numbers, written out: a pattern would be matched against the project's files.
case "${open_ports:-}" in
  *[!0-9\ ]*)
    say "open_ports takes port numbers; fix its line in this profile"
    exit 1 ;;
esac
# The start script blocks the local network for yes alone, and the first lines say what it does.
case "${block_local_network:-no}" in
  yes|no) ;;
  *) say "block_local_network=$block_local_network is not one of this profile's recipes (yes)"
     exit 1 ;;
esac
case "${docker:-none}" in
  none|read-only|full) ;;
  *) say "docker=$docker is not one of this profile's recipes (read-only or full)"
     exit 1 ;;
esac
# Either Docker recipe needs the Docker CLI inside, at an exact version: mise asks GitHub what a
# loose one means, GitHub answers a machine without a login 60 times an hour, and a start after
# that would fail. An exact version downloads from Docker at once.
[ "${docker:-none}" = none ] || tools="${tools:+$tools }docker-cli@29.9.1"

# A runtime's image names are lowercase, with one dash where the profile's name has a run of
# dashes and underscores.
image="taw-sandbox-$(printf '%s' "$profile" | tr '[:upper:]' '[:lower:]' | sed -E 's/[-_]+/-/g')"

# The project: the folder taw gives, or this one. It is shared read-write, so it is never your
# home folder or a folder that holds it: the sandbox would share all of it, your keys and your
# shell's startup files included.
asked="${TAW_SANDBOX_DIR:-$PWD}"
if ! dir="$(cd "$asked" 2>/dev/null && pwd -P)"; then
  say "the project folder $asked is not there; cd into a project first"
  exit 1
fi
home="$(cd "$HOME" 2>/dev/null && pwd -P || printf '%s' "$HOME")"
# What a folder is to your home folder: home, above (a folder that holds it), or nothing. Compared
# as folders, not as names: a link to the home, its name in another case on a disk that does not
# tell case apart, or the data volume's own path to it (/System/Volumes/Data/...) is the same folder.
home_or_above() {
  local up="$home"
  if [ "$1" -ef "$home" ]; then
    echo home
    return
  fi
  while [ "$up" != / ] && [ "$up" != . ]; do
    up="$(dirname "$up")"
    if [ "$1" -ef "$up" ]; then
      echo above
      return
    fi
  done
}
case "$(home_or_above "$dir")" in
  home) say "~ is your home folder, not a project, and the sandbox would share all of it; cd into a project first"
        exit 1 ;;
  above) say "$dir holds your home folder, not a project, and the sandbox would share all of it; cd into a project first"
         exit 1 ;;
esac
# git finds the checkout from the folder, as taw's own git does: the variables that point git at
# another repository are left out (the list taw's Rust git clears, REDIRECTING_VARIABLES in
# src/git.rs). A dotfiles setup's GIT_DIR would otherwise have that repository shared read-write,
# and its GIT_WORK_TREE would have the project refused as your home folder.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_NAMESPACE GIT_CONFIG GIT_IMPLICIT_WORK_TREE GIT_GRAFT_FILE GIT_NO_REPLACE_OBJECTS GIT_REPLACE_REF_BASE GIT_PREFIX GIT_SHALLOW_FILE
# In git, the project is the checkout the folder is in, shared whole with the folder as the
# working directory, since git fails inside a subfolder shared alone; so its top is refused as the
# folder is. git spells the top as the disk has it, and the folder's place in it, so the working
# directory is spelled as the project is, inside its mount.
top="$(git -C "$dir" rev-parse --show-toplevel 2>/dev/null || true)"
project="$dir" workdir="$dir"
if [ -n "$top" ]; then
  case "$(home_or_above "$top")" in
    home) say "this folder is in a git checkout of your whole home folder, and the sandbox would share all of it; cd into a project with a git checkout of its own first"
          exit 1 ;;
    above) say "this folder is in a git checkout of $top, which holds your home folder, and the sandbox would share all of it; cd into a project with a git checkout of its own first"
           exit 1 ;;
  esac
  prefix="$(git -C "$dir" rev-parse --show-prefix)"
  prefix="${prefix%/}"
  project="$top" workdir="$top${prefix:+/$prefix}"
fi

# Nor a folder whose files run on this machine, or reach into it: tmux's socket folder, through which
# anything can type into every pane you have; taw's own folders, whose config holds the hooks taw
# runs here and whose profiles taw sandbox runs; and the folders of your programs, their settings,
# data and state (~/.config and ~/.local, and the XDG folders when they are set). A project folder or
# a checkout's top that is one of them, or holds one, is refused as the home is: compared as folders.
# One inside taw's folders or tmux's is refused too, since what the sandbox writes there runs here or
# types into your panes; one inside the others is a project like any other, yours to look over.
xdg_or() {
  case "${1:-}" in
    /*) printf '%s' "$1" ;;
    *) printf '%s' "$2" ;;
  esac
}
if [ -n "${TMUX:-}" ]; then
  sockets="$(dirname "${TMUX%%,*}")"
else
  sockets="${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)"
fi
# The state folder taw itself used, from the report folder it gives, which is
# <state>/sandbox/panes/<server>/<pane>; a report folder of another shape is not taw's, and names no
# state folder.
panes=""
[ -z "${TAW_SANDBOX_REPORT:-}" ] || panes="$(dirname "$(dirname "$TAW_SANDBOX_REPORT")")"
case "$panes" in
  */sandbox/panes) taw_state="$(dirname "$(dirname "$panes")")" ;;
  *) taw_state="${TAW_STATE_DIR:-$(xdg_or "${XDG_STATE_HOME:-}" "$HOME/.local/state")/tmux-agent-wizard}" ;;
esac
taw_config="${TAW_CONFIG_DIR:-$(xdg_or "${XDG_CONFIG_HOME:-}" "$HOME/.config")/tmux-agent-wizard}"
# Each: the folder, what it is, and for taw's and tmux's what the sandbox could do from inside it.
running=(
  "$sockets" "tmux's socket folder" "from the sandbox anything could type into every pane you have"
  "$taw_state" "taw's state folder" "the sandbox could change the profile taw runs here and every pane's report"
  "$taw_config" "taw's config folder" "the sandbox could change your profiles and the hooks taw runs here"
  "$HOME/.config" "the folder of your programs' settings" ""
  "$HOME/.local" "the folder of your own programs and their data" ""
  "$HOME/.local/bin" "the folder of your own programs" ""
  "$HOME/.local/share" "the folder of your programs' data" ""
  "$HOME/.local/state" "the folder of your programs' state" ""
)
case "${XDG_CONFIG_HOME:-}" in /*) running+=("$XDG_CONFIG_HOME" "the folder of your programs' settings" "") ;; esac
case "${XDG_DATA_HOME:-}" in /*) running+=("$XDG_DATA_HOME" "the folder of your programs' data" "") ;; esac
case "${XDG_STATE_HOME:-}" in /*) running+=("$XDG_STATE_HOME" "the folder of your programs' state" "") ;; esac
# A folder as it is said: your home written as ~, however it is spelled here.
said_as() {
  local base
  # shellcheck disable=SC2088  # a tilde to read, not to expand
  for base in "$home" "$HOME"; do
    case "$1" in
      "$base") printf '~'; return ;;
      "$base"/*) printf '~%s' "${1#"$base"}"; return ;;
    esac
  done
  printf '%s' "$1"
}
# Whether the folder $1 is the folder $2 or holds it, compared as folders: the walk goes up $2's own
# path, as the disk spells it, to /. A folder that is not there is held by nothing.
holds() {
  local up
  up="$(unset CDPATH; cd "$2" 2>/dev/null && pwd -P)" || return 1
  while :; do
    if [ "$1" -ef "$up" ]; then
      return 0
    fi
    if [ "$up" = / ]; then
      return 1
    fi
    up="$(dirname "$up")"
  done
}
# Whether the folder $1 is inside the folder $2, compared as folders: the walk holds() does,
# reversed, up $1's own path. A folder that is not there is in nothing.
inside() {
  local up
  up="$(unset CDPATH; cd "$1" 2>/dev/null && pwd -P)" || return 1
  while [ "$up" != / ]; do
    up="$(dirname "$up")"
    if [ "$up" -ef "$2" ]; then
      return 0
    fi
  done
  return 1
}
# Which of taw's folders or tmux's the folder $1 is or is in, and what the sandbox could do there, as
# its refusal says it; nothing for none.
taws_own() {
  local i=0
  while [ "$i" -lt "${#running[@]}" ]; do
    if [ -n "${running[i+2]}" ]; then
      if [ "$1" -ef "${running[i]}" ]; then
        printf 'is %s, and %s' "${running[i+1]}" "${running[i+2]}"
        return
      fi
      if inside "$1" "${running[i]}"; then
        printf 'is in %s (%s), and %s' "${running[i+1]}" "$(said_as "${running[i]}")" "${running[i+2]}"
        return
      fi
    fi
    i=$((i + 3))
  done
}
# What of those the folder $1 is, or holds, as its refusal says it; nothing for none.
runs_here() {
  local i=0
  while [ "$i" -lt "${#running[@]}" ]; do
    if holds "$1" "${running[i]}"; then
      if [ "$1" -ef "${running[i]}" ]; then
        printf 'is %s' "${running[i+1]}"
      else
        printf 'holds %s (%s)' "${running[i+1]}" "$(said_as "${running[i]}")"
      fi
      return
    fi
    i=$((i + 3))
  done
}
own="$(taws_own "$dir")"
if [ -n "$own" ]; then
  say "$(said_as "$dir") $own; open a project outside it"
  exit 1
fi
runs="$(runs_here "$dir")"
if [ -n "$runs" ]; then
  say "$(said_as "$dir") $runs, and the sandbox would share it; open a project inside it, or make a profile of your own that shares only what it needs"
  exit 1
fi
if [ -n "$top" ]; then
  # git spells the top as the disk has it, which can name one of taw's folders that the folder's
  # own spelling (another case, a link) did not.
  own="$(taws_own "$top")"
  if [ -n "$own" ]; then
    say "this folder is in a git checkout of $(said_as "$top"), which $own; open a project outside it"
    exit 1
  fi
  runs="$(runs_here "$top")"
  if [ -n "$runs" ]; then
    say "this folder is in a git checkout of $(said_as "$top"), which $runs, and the sandbox would share it; open a project with a git checkout of its own, or make a profile of your own that shares only what it needs"
    exit 1
  fi
fi

# Names: taw-<the project folder's name>-<a short random id>, labelled with the profile and the
# pane, so a leftover shows with: docker ps --filter label=taw.profile
folder_name="$(printf '%s' "$(basename "$dir")" | LC_ALL=C tr -c 'A-Za-z0-9_.-' '-')"
name="taw-$folder_name-$(LC_ALL=C od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"
run=(--name "$name" --label "taw.profile=$profile")
[ -z "${TAW_SANDBOX_PANE:-}" ] || run+=(--label "taw.pane=$TAW_SANDBOX_PANE")

# Docker's own detach keys, ctrl-p then ctrl-q, hold back every ctrl-p the agent should see.
run+=(--detach-keys "ctrl-],ctrl-],ctrl-]" --shm-size 1g)

# As root only long enough to set the network rules; then the start script becomes you, with no
# capabilities, and nothing in the container can gain one back. NET_ADMIN sets the rules, CHOWN
# hands you the volumes' folders, SETUID and SETGID become you, and SETPCAP lets the start script
# empty the bounding set, so that no capability can ever come back.
run+=(--cap-drop ALL --cap-add NET_ADMIN --cap-add SETUID --cap-add SETGID --cap-add CHOWN --cap-add SETPCAP)
run+=(--security-opt no-new-privileges)
# Podman takes the same arguments, and this one, so that files the agent writes are yours.
[ "$runtime" != podman ] || run+=(--userns keep-id)

# The project, read-write at its own path, so file links, errors and each project's history read
# the same on both sides. In git, the hooks folder and config of every repository it holds (its
# own, each submodule's, nested ones too) are read-only over the project's own, and so is each
# config.worktree: a hook, or a config line such as core.fsmonitor or core.hooksPath, written by
# the agent would run on your machine the next time you used git there. The cost: git config
# writes fail inside, so git push -u pushes but cannot record the upstream. A repository kept
# outside the checkout (a worktree's main one, a submodule's) is mounted at its own path too. A
# repository with no hooks folder gets an empty one at the start (make_git_parts), or the agent
# could make one.
run+=(--workdir "$workdir" -v "$project:$project")
git_parts=()
readonly_git() {
  local part
  for part in hooks config; do
    run+=(-v "$1/$part:$1/$part:ro")
    git_parts+=("$1/$part")
  done
}
if [ -n "$top" ]; then
  # Without your CDPATH, which cd would search for a folder such as .git, saying where it went.
  repository="$(unset CDPATH; cd "$workdir" && cd "$(git rev-parse --git-common-dir)" && pwd -P)"
  case "$repository/" in
    "$project"/*) ;;
    *) run+=(-v "$repository:$repository") ;;
  esac
  readonly_git "$repository"
  # The repositories it holds are found by what makes one (HEAD, objects and config), each
  # config.worktree by its name; find follows no link, and runs nothing.
  while IFS= read -r found; do
    case "$found" in
      */config.worktree) run+=(-v "$found:$found:ro") ;;
      *) held="$(dirname "$found")"
         if [ "$held" != "$repository" ] && [ -f "$held/HEAD" ] && [ -d "$held/objects" ]; then
           readonly_git "$held"
         fi ;;
    esac
  done < <(find "$repository" -name objects -prune -o \( -name config -o -name config.worktree \) -type f -print \
             2>/dev/null | LC_ALL=C sort)
fi

# node_modules and .venv: a volume of the container's own at each, at the project's top level
# always and at any deeper ones found at start (down to 5 levels), kept between starts: packages
# built inside are Linux builds, and yours are this machine's, so one shared folder breaks one side.
own=()
volume_for() { printf 'taw-%s-%s' "$1" "$(printf '%s' "$2" | cksum | cut -d ' ' -f 1)"; }
packages_volume() {
  local kind
  kind="$(basename "$1")"
  run+=(-v "$(volume_for "${kind#.}" "$1"):$1")
  own+=("$1")
}
packages_volume "$project/node_modules"
packages_volume "$project/.venv"
while IFS= read -r found; do
  case "$found" in "$project/node_modules"|"$project/.venv") continue ;; esac
  packages_volume "$found"
done < <(find "$project" -maxdepth 5 \( -name .git -o -name node_modules -o -name .venv \) -prune \
           \( -name node_modules -o -name .venv \) -type d -print 2>/dev/null | LC_ALL=C sort)

# The caches: one volume for mise's tools and the agents, one for ~/.cache (npm, pip, uv and cargo
# pointed there), shared by every sandbox, so each download happens once. They hold downloads,
# not your files.
run+=(-v taw-sandbox-mise:/opt/taw/mise -v "taw-sandbox-cache:$HOME/.cache")
own+=(/opt/taw/mise "$HOME/.cache")

# Your agent's settings, shared (the default): its own folders at their paths, read-write, one
# setup in both places. With settings=own, a set of its own instead, in taw's state folder: for
# Claude, yours read-only inside it (settings, instructions, skills, plugins and hooks), your
# conversations read-write so --resume works on both sides, and a login of its own, made once
# inside; for another agent, folders of its own and nothing of yours. What is not there yet is
# made when the agent starts (make_settings_folders).
to_make=()
if [ "${settings:-shared}" = own ]; then
  # taw's state folder, as taw finds it: an XDG_STATE_HOME that is not a full path is ignored.
  state_home="${XDG_STATE_HOME:-}"
  case "$state_home" in
    /*) ;;
    *) state_home="$HOME/.local/state" ;;
  esac
  own_set="${TAW_STATE_DIR:-$state_home/tmux-agent-wizard}/sandbox/own/$profile/$agent_name"
  if [ "$agent" = claude ]; then
    claude_home="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
    run+=(-v "$own_set/claude:$claude_home")
    for part in settings.json CLAUDE.md skills plugins hooks; do
      [ ! -e "$claude_home/$part" ] || run+=(-v "$claude_home/$part:$claude_home/$part:ro")
    done
    run+=(-v "$claude_home/projects:$claude_home/projects")
    to_make+=("$own_set/claude" "$claude_home/projects")
    # Claude keeps its login beside its folder, unless a variable names the folder.
    if [ -z "${CLAUDE_CONFIG_DIR:-}" ]; then
      run+=(-v "$own_set/claude.json:$HOME/.claude.json")
      to_make+=("$own_set/claude.json")
    fi
  else
    for folder in ${folders[@]+"${folders[@]}"}; do
      mine="$own_set/$(printf '%s' "${folder#"$HOME"/}" | tr '/' '_')"
      run+=(-v "$mine:$folder")
      to_make+=("$mine")
    done
  fi
else
  for folder in ${folders[@]+"${folders[@]}"}; do
    run+=(-v "$folder:$folder")
    to_make+=("$folder")
  done
fi

# The agent inside is the version this machine runs, when it runs one; an agent its own
# installer puts in takes no version.
version=""
case "$install" in
  installer:*) ;;
  *) if command -v "$agent" >/dev/null 2>&1; then
       version="$("$agent" --version </dev/null 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -n 1 || true)"
     fi ;;
esac
run+=(-e "TAW_SB_INSTALL=$install" -e "TAW_SB_VERSION=${version:-latest}")

# This machine, every address it has, for the rules the start script sets inside: refused, with
# the open_ports recipe's ports let through. Its loopback is the container's own. A machine without
# the tool that lists them hands in none, and the start goes on: the start script still refuses the
# names Docker gives this machine inside.
machine_addresses() {
  case "$(uname -s)" in
    Darwin) { ifconfig -a 2>/dev/null || true; } | awk '$1 == "inet" || $1 == "inet6" {print $2}' ;;
    *) { ip -o addr show 2>/dev/null || true; } | awk '{print $4}' ;;
  esac | sed -e 's|%.*||' -e 's|/.*||' | { grep -v -E '^(127\.|::1$)' || true; } \
    | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ $//'
}
run+=(-e "TAW_SB_BLOCK=$(machine_addresses)" -e "TAW_SB_OPEN_PORTS=${open_ports:-}")
run+=(-e "TAW_SB_BLOCK_LOCAL=${block_local_network:-no}")

# What is passed in: your home's path and the variable naming the agent's settings folder, when
# it is set (so that the agent finds them where they are mounted), the terminal, the language,
# the time zone, your git name and email (so its commits are yours), the names in the pass line,
# and nothing else. Everything passed in is readable by the agent and by anything it runs. It
# never gets TMUX, TMUX_PANE or the tmux socket: whoever holds the socket can type into every
# pane you have.
tz="${TZ:-}"
if [ -z "$tz" ] && [ -L /etc/localtime ]; then
  tz="$(readlink /etc/localtime | sed 's|.*/zoneinfo/||')"
fi
git_name="$(git -C "$dir" config --get user.name 2>/dev/null || true)"
git_email="$(git -C "$dir" config --get user.email 2>/dev/null || true)"
run+=(-e "HOME=$HOME")
if [ -n "$settings_from" ] && [ -n "${!settings_from:-}" ]; then
  run+=(-e "$settings_from=${!settings_from}")
fi
run+=(-e TERM -e COLORTERM -e LANG)
[ -z "$tz" ] || run+=(-e "TZ=$tz")
[ -z "$git_name" ] || run+=(-e "GIT_AUTHOR_NAME=$git_name" -e "GIT_COMMITTER_NAME=$git_name")
[ -z "$git_email" ] || run+=(-e "GIT_AUTHOR_EMAIL=$git_email" -e "GIT_COMMITTER_EMAIL=$git_email")
for variable in ${pass:-}; do run+=(-e "$variable"); done
run+=(-e "TAW_SB_PROFILE=$profile" -e "TAW_SB_UID=$(id -u)" -e "TAW_SB_GID=$(id -g)")
run+=(-e "TAW_SB_TOOLS=${tools:-}")
# The mount points under your home, one to a line: Docker makes the folders between your home and
# each as root, and the start script hands them to you. The run's options are pairs, a flag and
# its value, and a mount's value is its source, its place inside and its options, split by colons.
mounts_under_home() {
  local i=0 place
  while [ "$i" -lt "${#run[@]}" ]; do
    if [ "${run[$i]}" = -v ]; then
      place="${run[$((i + 1))]#*:}"
      place="${place%%:*}"
      case "$place" in
        "$HOME"/*) printf '%s\n' "$place" ;;
      esac
    fi
    i=$((i + 2))
  done
}
run+=(-e "TAW_SB_MOUNTS=$(mounts_under_home)")
# The volumes' folders, one to a line, which the start script hands to you.
run+=(-e "TAW_SB_OWN=$(printf '%s\n' "${own[@]}")")

# docker=full: the socket. Whoever can use it can do anything as root on this machine. The start
# script is told, to give you the socket's group (inside it is root's, mode 660) and nothing else.
[ "${docker:-none}" != full ] || run+=(-v /var/run/docker.sock:/var/run/docker.sock -e TAW_SB_DOCKER=full)

# The report folder: the agent's taw hook writes the conversation's id there, and taw reads it
# from outside, which is how restore brings the conversation back. Only this agent's own hook
# writes it: the agent's name goes in TAW_REPORT_AGENT, and another agent's hook inside (an
# imported copy, or one run as a tool) finds a name that is not its own and writes nothing.
if [ -n "${TAW_SANDBOX_REPORT:-}" ]; then
  run+=(-v "$TAW_SANDBOX_REPORT:/run/taw" -e TAW_REPORT_FILE=/run/taw/session.json
    -e "TAW_REPORT_AGENT=$agent_name")
fi

# The sandbox's own networks, which block_local_network leaves open: the project's Compose
# services, when they run (the sandbox joins their network, so the agent reaches its own database
# by name, and nothing of this machine), and the Docker proxy's, below. Asked of the runtime on a
# real start only.
allow=()
subnets_of() {
  "$runtime" network inspect --format '{{range .IPAM.Config}}{{.Subnet}} {{end}}' "$1" 2>/dev/null || true
}
join_networks() {
  local network
  network="$("$runtime" ps --filter "label=com.docker.compose.project.working_dir=$project" --format json 2>/dev/null \
    | sed -n 's/.*"Networks":"\([^",]*\).*/\1/p' | head -n 1 || true)"
  if [ -n "$network" ]; then
    run+=(--network "$network")
    # shellcheck disable=SC2207  # the subnets, one word each
    allow+=($(subnets_of "$network"))
  fi
}

# docker=read-only: a small proxy beside the sandbox, on a network only the two share, which passes
# only read requests on to Docker: docker ps, logs and inspect work, and run, exec, stop and rm are
# refused. Reading shows a lot: docker inspect shows how every container was started, the variables
# it was given included. It starts last, once the image is built and the watcher runs, so a start
# that stops on the way leaves no proxy behind; the watcher removes it and its network, by their
# name, when the profile has gone, whatever it was doing.
proxy="$name-docker"
# Named in full: Podman, given a short name and no terminal to ask in, may refuse it.
proxy_image="docker.io/tecnativa/docker-socket-proxy:0.3.0"
start_proxy() {
  if ! "$runtime" network create "$proxy" >/dev/null \
     || ! "$runtime" run -d --rm --name "$proxy" --network "$proxy" --label "taw.profile=$profile" \
          -v /var/run/docker.sock:/var/run/docker.sock:ro \
          -e CONTAINERS=1 -e IMAGES=1 -e INFO=1 -e NETWORKS=1 -e VOLUMES=1 -e POST=0 \
          "$proxy_image" >/dev/null; then
    say "the read-only Docker proxy did not start, so the agent was not started"
    exit 1
  fi
  # The sandbox is on the proxy's network as well as on Compose's, when it joined that: docker run
  # takes two since Docker 25.
  run+=(--network "$proxy" -e "DOCKER_HOST=tcp://$proxy:2375")
  # shellcheck disable=SC2207  # the subnets, one word each
  allow+=($(subnets_of "$proxy"))
}

# The image: built once on this machine from mise's, at the tag a taw release was tested with,
# with git, iptables (the tool that sets network rules) and the start script below, and without
# mise's own config, which installs node and python in every sandbox; built again
# when this part of the file changes. One small image serves every project: the project's
# mise.toml installs its own tools inside.
image_recipe() {
  cat <<'IMAGE'
FROM ghcr.io/jdx/mise:2026.4.6
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends git iptables iproute2 util-linux ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
COPY taw-sandbox-start /usr/local/bin/taw-sandbox-start
RUN chmod 755 /usr/local/bin/taw-sandbox-start
RUN rm -f /mise/config.toml
ENTRYPOINT ["/usr/local/bin/taw-sandbox-start"]
IMAGE
}

# The start script, the program the image runs first.
start_script() {
  cat <<'START'
#!/bin/sh
# taw's sandbox start script, the program the image runs first. As root only long enough to set
# the network rules and hand the volumes' folders to you; then it becomes you, with no
# capabilities, installs what the agent and the project need, and starts the agent.
set -eu

profile="${TAW_SB_PROFILE:-default}"
say() { printf 'taw sandbox %s: %s\n' "$profile" "$*" >&2; }
# How the start is going, as the profile says it: no name in front.
tell() { printf '%s\n' "$*" >&2; }

# The files it reads that only a container has, and the volume every sandbox shares: these five
# name others, for taw's tests. Taken now, as every TAW_SB_ variable is gone before the agent starts.
resolv="${TAW_SB_RESOLV:-/etc/resolv.conf}"
if_inet6="${TAW_SB_IF_INET6:-/proc/net/if_inet6}"
memory_events="${TAW_SB_MEMORY_EVENTS:-/sys/fs/cgroup/memory.events}"
mise_dir="${TAW_SB_MISE_DIR:-/opt/taw/mise}"
docker_mode="${TAW_SB_DOCKER:-}"
docker_sock="${TAW_SB_DOCKER_SOCK:-/var/run/docker.sock}"

# One rule, for one address or network: iptables for IPv4, ip6tables for IPv6, and no IPv6 rule
# in a container that has no IPv6.
rule() {
  case "$1" in
    *:*) has_ipv6 || return 0
         shift; ip6tables "$@" ;;
    *) shift; iptables "$@" ;;
  esac
}
has_ipv6() {
  awk '$1 != "00000000000000000000000000000001" {found = 1} END {exit !found}' "$if_inet6" 2>/dev/null
}

# This machine refused, every address it has (handed in, and the names Docker gives it here), with
# the name servers first and the open_ports recipe's ports let through. With block_local_network,
# the local networks refused too, the sandbox's own networks excepted.
set_rules() {
  machine="${TAW_SB_BLOCK:-} $(getent ahosts host.docker.internal gateway.docker.internal 2>/dev/null | awk '{print $1}') $(ip route show default 2>/dev/null | awk '{print $3}')"
  # shellcheck disable=SC2013  # a name server is one word
  for server in $(awk '$1 == "nameserver" {print $2}' "$resolv" 2>/dev/null); do
    rule "$server" -A OUTPUT -d "$server" -p udp --dport 53 -j ACCEPT || return 1
    rule "$server" -A OUTPUT -d "$server" -p tcp --dport 53 -j ACCEPT || return 1
  done
  # shellcheck disable=SC2086  # the addresses, one word each, each once, in the order given
  for address in $(printf '%s\n' $machine | awk 'NF && !seen[$0]++'); do
    for port in ${TAW_SB_OPEN_PORTS:-}; do
      rule "$address" -A OUTPUT -d "$address" -p tcp --dport "$port" -j ACCEPT || return 1
    done
    rule "$address" -A OUTPUT -d "$address" -j REJECT || return 1
  done
  [ "${TAW_SB_BLOCK_LOCAL:-no}" = yes ] || return 0
  for network in ${TAW_SB_ALLOW:-}; do
    rule "$network" -A OUTPUT -d "$network" -j ACCEPT || return 1
  done
  for network in 10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 169.254.0.0/16 fc00::/7 fe80::/10; do
    rule "$network" -A OUTPUT -d "$network" -j REJECT || return 1
  done
}

# Whether a folder is one of the mount points under HOME, or inside one.
mounted() {
  printf '%s\n' "${TAW_SB_MOUNTS:-}" | {
    while IFS= read -r mount; do
      [ -n "$mount" ] || continue
      case "$1/" in "$mount"/*) exit 0 ;; esac
    done
    exit 1
  }
}

if [ "${1:-}" != --as-you ]; then
  if ! set_rules; then
    say "the network rules could not be set, so the agent does not start"
    exit 1
  fi
  mkdir -p "$HOME"
  chown "$TAW_SB_UID:$TAW_SB_GID" "$HOME"
  printf '%s\n' "${TAW_SB_OWN:-}" | while IFS= read -r folder; do
    [ -z "$folder" ] || { mkdir -p "$folder" && chown "$TAW_SB_UID:$TAW_SB_GID" "$folder"; }
  done
  # Docker makes the folders between HOME and a mount point under it as root (~/.local and
  # ~/.local/share for ~/.local/share/opencode), where you could then make nothing else: each is
  # handed to you too, alone, not what is in it; never a mount point, nor a folder inside one.
  printf '%s\n' "${TAW_SB_MOUNTS:-}" | while IFS= read -r mount; do
    case "$mount" in
      "$HOME"/?*) ;;
      *) continue ;;
    esac
    folder="$(dirname "$mount")"
    while [ "$folder" != "$HOME" ] && [ "$folder" != / ] && [ "$folder" != . ]; do
      [ ! -d "$folder" ] || mounted "$folder" || chown "$TAW_SB_UID:$TAW_SB_GID" "$folder"
      folder="$(dirname "$folder")"
    done
  done
  # With docker=full the socket is mounted, but inside it belongs to root (or another group), mode
  # 660: you get that group beside your own, and no other. Every other start gets only your own.
  groups_option="--clear-groups"
  if [ "$docker_mode" = full ] && [ -e "$docker_sock" ]; then
    socket_group="$(stat -c %g "$docker_sock" 2>/dev/null || true)"
    case "$socket_group" in
      ''|*[!0-9]*) ;;
      *) groups_option="--groups $socket_group" ;;
    esac
  fi
  # shellcheck disable=SC2086  # the option and its number, two words or one
  exec setpriv --reuid "$TAW_SB_UID" --regid "$TAW_SB_GID" $groups_option \
    --inh-caps=-all --bounding-set=-all --no-new-privs -- "$0" --as-you "$@"
fi
shift

# As you: the agent at the version asked for, then the project's tools, trusted here only, in the
# volume every sandbox shares, with the caches pointed at ~/.cache. An agent its own installer
# puts in goes into that volume too, beside mise's tools, so it is installed once.
# The image's mise is set up for root: its cache and config are root's, and its shims are first in
# PATH, so as you mise's cache, its config and its shims are moved or left out.
agents="$mise_dir/agents"
export MISE_DATA_DIR="$mise_dir" MISE_YES=1 XDG_CACHE_HOME="$HOME/.cache"
export MISE_CACHE_DIR="$HOME/.cache/mise" MISE_CONFIG_DIR="$mise_dir/config"
clean_path=""
old_ifs="$IFS"
IFS=:
set -f
for dir in $PATH; do
  case "$dir" in
    ''|/mise/shims) ;;
    *) clean_path="${clean_path:+$clean_path:}$dir" ;;
  esac
done
set +f
IFS="$old_ifs"
PATH="$clean_path"
export npm_config_cache="$HOME/.cache/npm" PIP_CACHE_DIR="$HOME/.cache/pip"
export UV_CACHE_DIR="$HOME/.cache/uv" CARGO_HOME="$HOME/.cache/cargo"
export PATH="$agents/.local/bin:$PATH"
agent="$1"
spec=""
case "${TAW_SB_INSTALL:-}" in
  installer:*)
    if ! command -v "$agent" >/dev/null 2>&1; then
      tell "installing $agent with its own installer (the first start only)"
      mkdir -p "$agents"
      curl -fsSL "${TAW_SB_INSTALL#installer:}" | HOME="$agents" bash >&2 || true
      command -v "$agent" >/dev/null 2>&1 || { say "$agent did not install, so it does not start"; exit 1; }
    fi ;;
  *)
    version="${TAW_SB_VERSION:-latest}"
    spec="$TAW_SB_INSTALL@$version"
    if ! mise where "$spec" >/dev/null 2>&1; then
      if [ "$version" = latest ]; then
        tell "installing the latest $agent and the tools in mise.toml (once for each version)"
      else
        tell "installing $agent $version and the tools in mise.toml (once for each version)"
      fi
    fi ;;
esac
if [ -f mise.toml ] || [ -f .mise.toml ]; then
  mise trust --yes . >/dev/null 2>&1 || true
  mise install --yes >&2 || say "the tools in mise.toml did not all install; the agent starts without them"
fi
# An agent installed with npm needs node. The project's own, when its mise.toml names one, is
# found by now and wins; with none, node's latest long-term release goes in once for all projects.
case "${TAW_SB_INSTALL:-}" in
  npm:*)
    if ! mise which npm >/dev/null 2>&1; then
      tell "installing node for $agent (once)"
      mise use -g node@lts >&2 || say "node did not install; $agent may not start"
    fi ;;
esac
tools="${TAW_SB_TOOLS:-}"
unset TAW_SB_PROFILE TAW_SB_UID TAW_SB_GID TAW_SB_INSTALL TAW_SB_VERSION TAW_SB_TOOLS \
  TAW_SB_BLOCK TAW_SB_OPEN_PORTS TAW_SB_BLOCK_LOCAL TAW_SB_ALLOW TAW_SB_OWN TAW_SB_MOUNTS \
  TAW_SB_RESOLV TAW_SB_IF_INET6 TAW_SB_MEMORY_EVENTS TAW_SB_MISE_DIR TAW_SB_DOCKER TAW_SB_DOCKER_SOCK

# The agent, as a child, so that how it ended can be told: a container's memory limit is where
# out-of-memory kills come from. Ctrl-C and Ctrl-\ are caught, not ignored: what the shell ignores,
# what it starts ignores too, and they are the agent's keys. The shell waits on through them.
trap : INT QUIT
status=0
# shellcheck disable=SC2086  # the agent's install, one word or none, and the tools, one word each
mise exec --yes $spec $tools -- "$@" || status=$?
if [ "$status" -eq 137 ] && awk '$1 == "oom_kill" && $2 > 0 {found = 1} END {exit !found}' \
     "$memory_events" 2>/dev/null; then
  say "$agent was killed for lack of memory: give the runtime's VM more (Docker Desktop: Settings, Resources, Memory)"
fi
exit "$status"
START
}

# The image is built when this machine has none, or one from another recipe: its taw.recipe
# label holds the checksum of the two parts above.
ensure_image() {
  local recipe have context
  recipe="$( { image_recipe; start_script; } | cksum | cut -d ' ' -f 1)"
  have="$("$runtime" image inspect --format '{{index .Config.Labels "taw.recipe"}}' "$image" 2>/dev/null || true)"
  [ "$have" != "$recipe" ] || return 0
  if [ -z "$have" ]; then
    tell "building the image $image (the first start only, a minute or two)"
  else
    tell "building the image $image again: its recipe in this profile changed"
  fi
  context="$(mktemp -d "${TMPDIR:-/tmp}/taw-sandbox.XXXXXX")"
  image_recipe > "$context/Dockerfile"
  start_script > "$context/taw-sandbox-start"
  chmod 755 "$context/taw-sandbox-start"
  if ! "$runtime" build --label "taw.recipe=$recipe" -t "$image" "$context"; then
    rm -rf "$context"
    say "the image $image did not build, so the agent was not started"
    exit 1
  fi
  rm -rf "$context"
}

# Its first lines: where it runs and what it reaches, before the agent starts, every time.
first_lines() {
  local shown="$dir" reach inside settings_words asking port ports=""
  # shellcheck disable=SC2088  # a tilde to read, not to expand
  case "$dir" in
    "$home"/*) shown="~${dir#"$home"}" ;;
  esac
  reach="reaches the internet and your local network, not this machine"
  [ "${block_local_network:-no}" = no ] || reach="reaches the internet, not your local network or this machine"
  for port in ${open_ports:-}; do ports="${ports:+$ports, }$port"; done
  [ -z "$ports" ] || reach="$reach (but its ports $ports)"
  inside="no docker inside"
  case "${docker:-none}" in
    read-only) inside="docker inside, read-only" ;;
    full) inside="all of docker inside, which is this whole machine" ;;
  esac
  settings_words="uses your own $agent_name settings"
  if [ "${settings:-shared}" = own ]; then
    settings_words="uses a separate set of $agent_name settings, none of yours inside"
    [ "$agent" != claude ] || settings_words="uses a separate set of $agent_name settings, yours read-only inside"
  fi
  asking="runs commands without asking"
  [ "$asks" = no ] || asking="asks before each command"
  say "$agent_name in $runtime, in $shown"
  printf '  %s; %s\n' "$reach" "$inside" >&2
  printf '  %s; %s\n' "$settings_words" "$asking" >&2
}

# The runtime answers, or it is waited for: after a restart, tmux can be back before Docker
# Desktop is. At most 60 seconds, however long each try takes, a try a second, and then the
# agent is not started.
wait_for_runtime() {
  if ! command -v "$runtime" >/dev/null 2>&1; then
    say "$runtime is not installed (no $runtime on the PATH), so the agent was not started"
    exit 1
  fi
  local bound="${TAW_SANDBOX_WAIT_SECONDS:-60}" tries=0 deadline
  # SECONDS counts whole seconds of the clock, so one more keeps the wait from ending early.
  deadline=$((SECONDS + bound + 1))
  "$runtime" info >/dev/null 2>&1 && return 0
  local hint="start Docker Desktop, OrbStack or Colima"
  [ "$runtime" != podman ] || hint="start it with podman machine start"
  say "waiting for $runtime to answer ($hint)"
  while [ "$tries" -lt "$bound" ] && [ "$SECONDS" -lt "$deadline" ]; do
    sleep 1
    tries=$((tries + 1))
    "$runtime" info >/dev/null 2>&1 && return 0
  done
  say "$runtime did not answer in $bound seconds, so the agent was not started"
  exit 1
}

# The daemon this start goes to is held for its run and the watcher's stop alike: a Docker context
# switched, or Podman's default connection changed, while the agent runs would send the stop to
# another daemon. One your environment names already (DOCKER_HOST, DOCKER_CONTEXT, CONTAINER_HOST
# or CONTAINER_CONNECTION) holds for both as it is.
hold_the_daemon() {
  local held=""
  case "$runtime" in
    docker)
      [ -z "${DOCKER_HOST:-}" ] && [ -z "${DOCKER_CONTEXT:-}" ] || return 0
      held="$("$runtime" context show 2>/dev/null || true)"
      [ -z "$held" ] || export DOCKER_CONTEXT="$held" ;;
    podman)
      [ -z "${CONTAINER_HOST:-}" ] && [ -z "${CONTAINER_CONNECTION:-}" ] || return 0
      held="$("$runtime" system connection list --format '{{if .Default}}{{.Name}}{{end}}' 2>/dev/null | grep -m 1 . || true)"
      [ -z "$held" ] || export CONTAINER_CONNECTION="$held" ;;
  esac
}

# A settings folder that is not there yet is made, so a login made inside stays, and a settings
# file starts as an empty one (Docker would make a folder in its place): your own, or the
# separate set's. Only when the agent starts: print mode makes nothing.
make_settings_folders() {
  local folder
  for folder in ${to_make[@]+"${to_make[@]}"}; do
    if [ ! -e "$folder" ]; then
      case "$folder" in
        *.json) { printf '{}\n' > "$folder"; } 2>/dev/null ;;
        *) mkdir -p "$folder" 2>/dev/null ;;
      esac || {
        say "$folder could not be made for $agent_name's settings, so the agent was not started"
        exit 1
      }
    fi
  done
}

# The repository's hooks folder (or config) that is not there yet is made, empty, so that it is
# read-only inside too. Only when the agent starts: print mode makes nothing.
make_git_parts() {
  local part
  for part in ${git_parts[@]+"${git_parts[@]}"}; do
    if [ ! -e "$part" ]; then
      case "$part" in
        */hooks) mkdir -p "$part" 2>/dev/null ;;
        *) { : > "$part"; } 2>/dev/null ;;
      esac || {
        say "$part could not be made, so the agent was not started"
        exit 1
      }
    fi
  done
}

# A container started with a terminal (-t) keeps running when that terminal goes away, since
# Docker passes no signal on through one. This watcher stops it once the docker run below has
# gone: a closed pane, x in the popup, tmux killed. It ignores the hang-up that ends docker run,
# and says nothing. Without it a closed pane leaves its agent running where nobody sees it.
start_watcher() {
  local run_pid=$$
  (
    trap '' HUP INT
    while kill -0 "$run_pid" 2>/dev/null; do sleep 1; done
    "$runtime" stop -t 2 "$name" >/dev/null 2>&1 || true
    if [ "${docker:-none}" = read-only ]; then
      "$runtime" rm -f "$proxy" >/dev/null 2>&1 || true
      "$runtime" network rm "$proxy" >/dev/null 2>&1 || true
    fi
  ) </dev/null >/dev/null 2>&1 &
}

first_lines
if [ "${TAW_SANDBOX_PRINT:-}" = 1 ]; then
  # The runtime is asked nothing: the command is printed without the sandbox's own networks, and a
  # second line, a comment, says what a start adds.
  run+=(-e "TAW_SB_ALLOW=")
  # Each word in single quotes, a quote in it written '\'' and a newline $'\n', so that the command
  # is one line: bash 3.2 and 5 and zsh all read it back as these very words, wherever it is
  # pasted (printf %q quotes as the bash running this does, and macOS's leaves a leading ~ bare).
  quote="'\\''" newline="'\$'\\n''" nl='
'
  for word in "$runtime" run --rm --interactive --tty --init --pull never ${run[@]+"${run[@]}"} "$image" "$agent" ${skip[@]+"${skip[@]}"} "$@"; do
    word="${word//\'/$quote}"
    printf "'%s' " "${word//$nl/$newline}"
  done
  printf '\n'
  if [ -e "$project/compose.yaml" ] || [ -e "$project/compose.yml" ] \
     || [ -e "$project/docker-compose.yml" ] || [ -e "$project/docker-compose.yaml" ]; then
    printf '%s\n' "# a start also joins the network of the project's Compose services, when they run"
  fi
  [ "${docker:-none}" != read-only ] \
    || printf '%s\n' "# a start also runs a read-only Docker proxy, $proxy_image, on a network only the two share"
  exit 0
fi
wait_for_runtime
hold_the_daemon
join_networks
make_settings_folders
make_git_parts
ensure_image
start_watcher
[ "${docker:-none}" != read-only ] || start_proxy
# Last of the runtime's options: the sandbox's own networks, for the start script's rules.
run+=(-e "TAW_SB_ALLOW=${allow[*]+${allow[*]}}")
# The image is this machine's own build, so the run never pulls it: a runtime would look for it
# in a registry, and Podman may ask which one.
exec "$runtime" run --rm --interactive --tty --init --pull never ${run[@]+"${run[@]}"} "$image" "$agent" ${skip[@]+"${skip[@]}"} "$@"
