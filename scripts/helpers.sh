#!/usr/bin/env bash
# Shared helpers for tmux-agent-wizard.tmux. Mirrors the get/set option
# pattern from tmux-session-wizard's src/helpers.sh; behaviour here is
# tmux-agent-wizard's own.

# Prints the value of global tmux option $1, or $2 when it is unset/empty.
get_tmux_option() {
  local option="$1"
  local default_value="$2"
  local option_value
  option_value=$(tmux show-option -gqv "$option")
  if [ -z "$option_value" ]; then
    echo "$default_value"
  else
    echo "$option_value"
  fi
}

# Sets global tmux option $1 to $2, but only when it is not already set, so it
# never overrides a value the user configured in their tmux.conf.
set_tmux_option() {
  local option="$1"
  local default_value="$2"
  local option_value
  option_value=$(tmux show-option -gqv "$option")
  if [ -z "$option_value" ]; then
    tmux set-option -g "$option" "$default_value"
  fi
}

# Succeeds when the tmux SERVER this plugin is being sourced into reports a
# version >= $1.$2. Reads `#{version}` from the running server, not whatever
# `tmux` is first on PATH: a newer tmux earlier on PATH must not let the plugin
# load against an older running server. `#{version}` exists on 3.2+ (the floor);
# only when it yields nothing (no server, or a pre-3.2 server without the
# format) does it fall back to the binary's own `tmux -V`. Handles every format
# tmux prints: "3.6b", "next-3.8", "3.2a", plain "3.2" (and the "tmux " prefix
# `-V` adds).
tmux_version_at_least() {
  local want_major="$1" want_minor="$2"
  local raw major minor
  raw=$(tmux display-message -p '#{version}' 2>/dev/null)
  if [ -z "$raw" ]; then
    raw=$(tmux -V 2>/dev/null)
  fi
  raw="${raw#tmux }"
  raw="${raw#next-}"
  major="${raw%%.*}"
  minor="${raw#*.}"
  major="${major%%[!0-9]*}"
  minor="${minor%%[!0-9]*}"
  case "$major" in '' | *[!0-9]*) return 1 ;; esac
  case "$minor" in '' | *[!0-9]*) return 1 ;; esac
  if [ "$major" -gt "$want_major" ]; then
    return 0
  fi
  [ "$major" -eq "$want_major" ] && [ "$minor" -ge "$want_minor" ]
}

# Prints the folder that holds taw's own `taw` link (bin in taw's state folder: $TAW_STATE_DIR, or the
# XDG state folder, as taw reads it), and fails when that is not a path the plugin can use: not
# absolute, or with a colon in it (it could not be a PATH entry).
taw_link_dir() {
  local dir="${TAW_STATE_DIR:-}"
  if [ -z "$dir" ]; then
    case "${XDG_STATE_HOME:-}" in
      /*) dir="$XDG_STATE_HOME/tmux-agent-wizard" ;;
      *) dir="${HOME:-}/.local/state/tmux-agent-wizard" ;;
    esac
  fi
  case "$dir" in
    /*) ;;
    *) return 1 ;;
  esac
  case "$dir" in *:*) return 1 ;; esac
  printf '%s\n' "$dir/bin"
}

# Prints PATH $1 with every copy of folder $2 taken out, every other entry as it was: one leading
# and one trailing colon are added and taken off again, so an empty entry of the user's stays.
path_without_dir() {
  local rest=":$1:" dir="$2"
  while case "$rest" in *":$dir:"*) true ;; *) false ;; esac; do
    rest="${rest%%":$dir:"*}:${rest#*":$dir:"}"
  done
  rest="${rest#:}"
  rest="${rest%:}"
  printf '%s\n' "$rest"
}

# Prints the absolute path of every step of following the link $1 all the way, one per line, each with its
# folder made physical (so two paths to one folder compare equal), the last being the file it ends at.
# Fails, after printing the steps so far, when the chain never ends (a loop, or more than 40 links) or ends
# at nothing (a dangling link). Portable: readlink without -f, which macOS lacks, and bash 3.2.
taw_link_chain() {
  local p="$1" n=0 d target
  while :; do
    d=$(unset CDPATH; cd -P "$(dirname "$p")" 2>/dev/null && pwd -P) || return 1
    p="$d/$(basename "$p")"
    printf '%s\n' "$p"
    if [ -L "$p" ]; then
      n=$((n + 1))
      [ "$n" -le 40 ] || return 1
      target=$(readlink "$p") || return 1
      case "$target" in
        /*) p="$target" ;;
        *) p="$d/$target" ;;
      esac
    else
      [ -e "$p" ]
      return
    fi
  done
}

# Succeeds when $1, a `taw` found on PATH, can be the binary the state link points at: it is an executable
# file, following its links all the way ends at a file, and no step of the way is in taw's own link folder
# (the step-4 link of the install page, `~/.local/bin/taw`, is a link to the state link: taken as the
# binary, the state link would point at a link that points back at it, and `taw` would be gone).
taw_candidate_ok() {
  local candidate="$1" link_dir chain step
  [ -x "$candidate" ] || return 1
  chain=$(taw_link_chain "$candidate") || return 1
  if link_dir=$(taw_link_dir) && link_dir=$(unset CDPATH; cd -P "$link_dir" 2>/dev/null && pwd -P); then
    while IFS= read -r step; do
      [ "$(dirname "$step")" != "$link_dir" ] || return 1
    done <<<"$chain"
  fi
  return 0
}

# Prints the first `taw` on PATH that taw_candidate_ok accepts, and fails when there is none. Every
# folder of PATH is tried in turn, not only the first hit: a bad `taw` in front must not hide a good one.
taw_on_path_without_link_dir() {
  local rest="$PATH:" entry
  while [ -n "$rest" ]; do
    entry="${rest%%:*}"
    rest="${rest#*:}"
    case "$entry" in
      /*) ;;
      *) continue ;;
    esac
    if [ -f "$entry/taw" ] && taw_candidate_ok "$entry/taw"; then
      printf '%s\n' "$entry/taw"
      return 0
    fi
  done
  return 1
}

# Resolves the taw binary, in order: $TAW_BIN (tests) -> @agent-wizard-bin ->
# $CURRENT_DIR/bin/taw -> $CURRENT_DIR/target/release/taw -> taw on PATH (taw_on_path_without_link_dir:
# not one in the folder of taw's own link, nor one that leads into it, loops or leads nowhere).
# Prints the resolved path and succeeds; prints nothing and fails when no
# candidate is executable. Requires $CURRENT_DIR to already be set by the
# caller.
resolve_taw_bin() {
  local candidate

  if [ -n "${TAW_BIN:-}" ] && [ -x "$TAW_BIN" ]; then
    echo "$TAW_BIN"
    return 0
  fi

  candidate=$(get_tmux_option "@agent-wizard-bin" "")
  if [ -n "$candidate" ] && [ -x "$candidate" ]; then
    echo "$candidate"
    return 0
  fi

  if [ -x "$CURRENT_DIR/bin/taw" ]; then
    echo "$CURRENT_DIR/bin/taw"
    return 0
  fi

  if [ -x "$CURRENT_DIR/target/release/taw" ]; then
    echo "$CURRENT_DIR/target/release/taw"
    return 0
  fi

  # With the plugin's own folder (taw_link_dir) taken out of PATH: a load made after an earlier one has
  # that folder in front, and its `taw` is only a link to the binary the earlier load resolved. Found
  # through it, bin would be the link, not the binary, so hooks of the earlier load (which name the
  # binary) would stay beside the new ones, and a newer taw elsewhere on PATH would never be linked.
  candidate=$(taw_on_path_without_link_dir) || true
  if [ -n "$candidate" ]; then
    echo "$candidate"
    return 0
  fi

  return 1
}

# Unbinds, from the prefix table, every key whose binding carries the text $1 (a line that only a
# binding this plugin makes has) and is not one of $2 (the keys it is about to bind, space-separated),
# so that a key taken out of an option, or an option emptied, does not stay bound until tmux restarts.
# A binding of the user's own has none of that text and is left alone, and so is a key that tmux
# prints with an escape (`\;`, `\"`): that is how it writes it for its parser, and not the name
# `unbind-key` takes. Run before the new keys are bound.
unbind_stale_keys() {
  local marker="$1" wanted=" $2 " line key i
  local words=()
  while IFS= read -r line; do
    [[ "$line" == *"$marker"* ]] || continue
    # "bind-key [-r] -T prefix <key> <command>", the columns padded with runs of spaces
    read -ra words <<<"$line"
    i=1
    [ "${words[$i]:-}" = "-r" ] && i=$((i + 1))
    [ "${words[$i]:-}" = "-T" ] || continue
    key="${words[$((i + 2))]:-}"
    case "$key" in '' | *\\*) continue ;; esac
    case "$wanted" in *" $key "*) continue ;; esac
    tmux unbind-key -T prefix "$key"
  done < <(tmux list-keys -T prefix 2>/dev/null)
}

# Prints the entries of global hook $1 that an earlier source of this plugin left, one a line as
# "<hook>[<index>]": those whose command references the taw binary at $2, either that literal path
# or (fallback) a bare " taw " invocation, or that sets taw's own option @agent_wizard_exited (the
# mark the pane-exited hook leaves, which runs no taw binary). Entries that do not match (the user's
# own hooks) are not named. Taken before this source adds its own and taken off after (see
# unset_hook_entries), so sourcing the plugin twice, or upgrading to a new binary path, never stacks
# duplicate entries, and a hook is never without one of the plugin's. tmux keeps the surviving
# indices stable across an unset, so every entry named from one snapshot of `show-hooks` is safe to
# unset after others were added.
taw_hook_entries() {
  local hook="$1" bin="$2"
  local raw line rest idx cmd

  raw=$(tmux show-hooks -g "$hook" 2>/dev/null) || return 0
  [ -z "$raw" ] && return 0

  while IFS= read -r line; do
    case "$line" in
    "$hook"'['*']'*) ;;
    *) continue ;;
    esac
    rest="${line#"$hook"[}"
    idx="${rest%%]*}"
    cmd="${rest#*] }"
    if [[ "$cmd" == *"$bin"* ]] || [[ "$cmd" == *" taw "* ]] || [[ "$cmd" == *"@agent_wizard_exited"* ]]; then
      printf '%s[%s]\n' "$hook" "$idx"
    fi
  done <<<"$raw"
}

# Takes off the hook entries named on standard input, one a line as "<hook>[<index>]"
# (taw_hook_entries), and nothing else.
unset_hook_entries() {
  local entry
  while IFS= read -r entry; do
    if [ -n "$entry" ]; then
      tmux set-hook -gu "$entry"
    fi
  done
}
