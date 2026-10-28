#!/usr/bin/env bash
# Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
# TPM entry point for tmux-agent-wizard. Gates on the tmux version first, so a
# tmux too old never starts a download, then resolves the taw binary: with none,
# it downloads one in the background (scripts/auto-install.sh, unless
# @agent-wizard-auto-install is off) and loads again once it is there, and it
# refreshes a downloaded one after an update. Then it sets @agent-wizard* option
# defaults, binds the popup and notification keys (and the clear key, when one
# is set), stops an outer agent's session markers and the handles of the
# terminal that started tmux reaching new panes, installs the
# watch/seen/autosave hooks (idempotently), and fires the one-shot startup
# commands.

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/helpers.sh
source "$CURRENT_DIR/scripts/helpers.sh"

tmux_option_keys="@agent-wizard"
tmux_option_height="@agent-wizard-height"
tmux_option_width="@agent-wizard-width"
tmux_option_notification_key="@agent-wizard-notification-key"
tmux_option_bin="@agent-wizard-bin"
tmux_option_watch="@agent-wizard-watch"
tmux_option_popup_title="@agent-wizard-popup-title"
tmux_option_popup_border="@agent-wizard-popup-border"
tmux_option_clear_key="@agent-wizard-clear-key"
tmux_option_auto_install="@agent-wizard-auto-install"
tmux_option_herdr_env="@agent-wizard-herdr-env"
tmux_option_path="@agent-wizard-path"
# Set while scripts/auto-install.sh is fetching taw in the background. taw's own
# option, not one the user sets, so not a tmux_option_ like those above.
installing_flag="@agent_wizard_installing"

default_keys="A"
# 100/100: `taw ui open` sizes the popup itself, capped at these
# percentages: the full client by default, leaving only the one row its own
# geometry protects for the status line.
default_height="100"
default_width="100"
default_notification_key="O"
default_watch="on"
default_popup_title=" taw "
default_popup_border=""
# Unbound until it is set, as herdr's keys.clear_pane is.
default_clear_key=""
default_auto_install="on"
# Off unless `on`: HERDR_ENV=1 tells every herdr-aware program, herdr's own agent skill
# included, that it runs inside herdr.
default_herdr_env="off"
# The HERDR_PANE_ID the plugin hands out. tmux gives every pane the one global value, so it is
# a fixed word that taw's pane verbs read as the pane they run in ($TMUX_PANE). tmux reads a
# leading % as a pane id, so no session or window name can have meant it. It is also the
# plugin's own mark: an off reload removes the three only while HERDR_PANE_ID is still this.
herdr_calling_pane="%calling"
# On unless `off`: taw is callable by name in every pane the server creates from then on.
default_path="on"
herdr_env_names=(HERDR_ENV HERDR_BIN_PATH HERDR_PANE_ID)

# Hooks that (re)attach a pipe-pane watcher to the pane the event fired for.
watch_hooks=(after-new-window after-split-window after-new-session)
# Hooks that mark a pane "seen" (drives done -> idle, and suppresses
# notifications for the pane the user is currently looking at). pane-focus-in
# fires on every supported tmux (3.2+), including 3.6b; it is only dropped from
# bare `show-hooks -g` there, not from actually firing. install_agent_wizard_hooks
# adds tmux 3.6's client-focus-in on top of this list where it exists.
seen_hooks=(after-select-pane after-select-window client-session-changed pane-focus-in)
# Hooks that debounce a session snapshot. after-new-window/after-split-window
# overlap watch_hooks above (each gets its own appended hook entry: a `watch
# attach` one and a `session autosave` one; taw_hook_entries names both
# when re-sourcing regardless of which loop installed them). Split so only the
# close-triggered half gets `--on-close`: opening a pane is never a shutdown
# risk, so there is nothing there to wait 5s and check tmux is still around for.
autosave_open_hooks=(after-new-window after-split-window)
autosave_close_hooks=(after-kill-pane session-closed)
autosave_hooks=("${autosave_open_hooks[@]}" "${autosave_close_hooks[@]}")

# Hooks that fire one herdr plugin lifecycle event, as "<hook> <event> <args…>".
#
# tmux owns the UI here, so a workspace/tab/pane change the user makes with a tmux key
# (prefix c, prefix x, prefix ,) never passes through taw; the hook is the only place it
# is observable. Exactly one hook per event, and the taw verbs that cause these changes
# deliberately emit nothing, so nothing fires twice: `taw tab close` runs `kill-window`,
# which fires `window-unlinked` in this same server. See src/cli/events.rs for the full
# source table (the other ten events come from the verb, or the pane watcher, that owns
# the change and holds the record to describe it).
#
# Every id and label goes through `#{q:…}` and is left UNQUOTED. tmux expands the format,
# then hands the string to `$SHELL -c`: a tmux session id is `$3`, which the shell would
# expand to a positional parameter, and a window name is arbitrary text that could carry
# spaces or `$(…)`. `#{q:…}` escapes it for exactly that shell, delivering the value as
# one argv entry. (`#{q:…}` needs tmux 3.1; the plugin's floor is 3.2.)
event_hooks=(
  # The three "created" events. `window-linked` and `session-created` are *notify* hooks
  # and set `#{hook_window}`/`#{hook_session}`, symmetric with `window-unlinked` for
  # tab.closed. `after-split-window` sets no `hook_*` at all (a command hook's format
  # context is the command's own target, which for a split is the pane it just made), so
  # `pane.created` reads `#{pane_id}`. (`after-new-window`/`after-new-session` are not
  # used: they set nothing either, and the notify hooks above already cover both.)
  #
  # A new session fires `session-created` *and* `window-linked`, so creating a workspace
  # delivers workspace.created plus one tab.created for the tab it came with. That is
  # herdr's own cascade, not a double-emit.
  "session-created workspace.created --workspace #{q:hook_session}"
  "window-linked tab.created --tab #{q:hook_window} --workspace #{q:hook_session}"
  "after-split-window pane.created --pane #{q:pane_id} --workspace #{q:session_id}"
  "window-unlinked tab.closed --tab #{q:hook_window} --workspace #{q:hook_session}"
  "session-window-changed tab.focused --tab #{q:window_id} --workspace #{q:hook_session}"
  "after-rename-window tab.renamed --tab #{q:window_id} --workspace #{q:session_id} --label #{q:window_name}"
  "session-closed workspace.closed --workspace #{q:hook_session}"
  "client-session-changed workspace.focused --workspace #{q:session_id}"
  "after-rename-session workspace.renamed --workspace #{q:session_id} --label #{q:session_name}"
  "window-layout-changed layout.updated --tab #{q:hook_window} --workspace #{q:session_id}"
  "pane-exited pane.exited --pane #{q:hook_pane} --workspace #{q:session_id}"
  "after-select-pane pane.focused --pane #{q:pane_id} --workspace #{q:session_id}"
  # `pane.closed` takes no identifier: tmux names no killed pane (`after-kill-pane`
  # leaves every `hook_*` empty on 3.6b, and there is no pane-removed hook at all), so
  # `taw plugin event pane.closed` recovers the id by diffing the live panes against the
  # previous set. It is installed on the hooks where a pane can *appear* as well as
  # disappear, because the diff is only as good as the set it compares against: a pane
  # created since the last diff would otherwise be missing from it and its close would go
  # unnoticed. The creation entries emit nothing; they keep the roster current.
  #
  # Deliberately NOT on `window-layout-changed`: that fires on every frame of an
  # interactive resize, and none of these hooks do.
  # taw reads the live pane set itself (`list-panes -a`). Passing it in from a tmux
  # format is not an option: the only format that enumerates panes is
  # `#{S:#{W:#{P:…}}}`, whose loop modifiers need a current client, and on a server with
  # none attached tmux answers "no current client" and defers that error onto the next
  # unrelated command; it surfaced as a `send-keys` failure three commands later.
  "after-kill-pane pane.closed"
  "window-unlinked pane.closed"
  "after-split-window pane.closed"
  "after-new-window pane.closed"
  "after-new-session pane.closed"
)

# An exit is not a close, and the roster `pane.closed` is read off cannot tell them apart: a pane
# whose process ended is gone from `list-panes` as a killed one is. So the `pane-exited` hook marks
# the pane in `@agent_wizard_exited` with its own tmux commands (not in the job it starts, which
# is a process that has not begun when a detection that fires beside it, `window-unlinked` for the
# last pane of a window, has read the panes), and every detection leaves a marked pane out of what
# it reports (see `detect_closed` in src/cli/events.rs). tmux runs a notify hook's own commands from
# the server's queue in the loop pass that sees the event, ahead of any client's commands
# (`server_loop`: `cmdq_next(NULL)`, then each client's queue; the same in 3.2 and 3.6b), so a
# command that finds the pane gone finds its mark. Always, and not only while a roster exists: a
# detection that is about to write the first one may already have listed the pane. The mark is
# the last 400 characters of the marks and the new one (`=-400:`, which 3.2 has), so that a server
# nobody listens to never collects more than that; the detections take off the ones they are done
# with.
exited_mark_command="set-option -gF @agent_wizard_exited '#{=-400:#{@agent_wizard_exited}#{hook_pane} }'"

# Session markers an agent leaves in the environment of every process it starts. A tmux
# server started from inside Claude Code, Codex or OMP has them in its global environment
# and hands them to every new pane: Claude Code then believes it is a child session and
# will not start normally, and a Codex or OMP launched there reads as a nested one.
# Follows herdr's `apply_pane_launch_env` (src/pane.rs, v0.9.3), which strips the same six
# from every pane it launches (first in master's 8ac9542757 (#4461, #4104 in v0.9.2's changelog)).
# taw does not launch panes, so the six are removed from the tmux server's global environment
# instead.
outer_agent_env_markers=(
  CODEX_THREAD_ID
  OMPCODE
  CLAUDECODE
  CLAUDE_CODE_CHILD_SESSION
  CLAUDE_CODE_SESSION_ID
  CLAUDE_CODE_MESSAGING_TOKEN
)

# Handles of the terminal that started the server: iTerm2's, WezTerm's, Kitty's, Windows
# Terminal's, screen's and Zellij's. They name that terminal, not the pane, yet a tmux
# server keeps them in its global environment and hands them to every new pane, even one
# you reach from another terminal later: `update-environment` governs only the names it
# lists (`SSH_AUTH_SOCK`, `DISPLAY` and a few more), and none of these is among them.
# Follows herdr's `apply_pane_terminal_env` (src/pane.rs, v0.9.3), which strips these ten, and
# `TMUX` and `TMUX_PANE`, from every pane it launches. Here they are removed from the tmux
# server's global environment, as above; `TMUX` and `TMUX_PANE` are not here, because tmux
# sets both for every pane itself.
outer_terminal_env_markers=(
  ITERM_SESSION_ID
  LC_TERMINAL
  LC_TERMINAL_VERSION
  WEZTERM_PANE
  KITTY_WINDOW_ID
  WT_SESSION
  STY
  ZELLIJ
  ZELLIJ_SESSION_NAME
  ZELLIJ_PANE_ID
)

set_default_agent_wizard_options() {
  set_tmux_option "$tmux_option_keys" "$default_keys"
  set_tmux_option "$tmux_option_height" "$default_height"
  set_tmux_option "$tmux_option_width" "$default_width"
  set_tmux_option "$tmux_option_notification_key" "$default_notification_key"
  set_tmux_option "$tmux_option_bin" ""
  set_tmux_option "$tmux_option_watch" "$default_watch"
  set_tmux_option "$tmux_option_popup_title" "$default_popup_title"
  set_tmux_option "$tmux_option_popup_border" "$default_popup_border"
  set_tmux_option "$tmux_option_clear_key" "$default_clear_key"
}

# `set-environment -g -r` marks each present name "remove before starting a process", so
# panes created from now on start without it. Panes that already exist keep the environment
# they were started with. A name that is not in the global environment is left alone rather
# than flagged, so this adds nothing to `show-environment -g` on an ordinary server. A name
# listed in `update-environment` still reaches each session from the terminal that creates
# it or attaches to it, because a session's own value is laid over the global removal
# (tmux 3.6b). Sourcing twice is harmless: a name already flagged is listed as `-NAME`,
# which no longer matches.
clear_outer_environment() {
  local name
  for name in "${outer_agent_env_markers[@]}" "${outer_terminal_env_markers[@]}"; do
    case "$(tmux show-environment -g "$name" 2>/dev/null)" in
      "$name="*) tmux set-environment -g -r "$name" ;;
    esac
  done
}

# herdr's guide (add-herdr-support) tells an agent to report only where HERDR_ENV=1 and
# HERDR_PANE_ID and HERDR_BIN_PATH are set, by running
# `"$HERDR_BIN_PATH" pane report-agent "$HERDR_PANE_ID" ...`. Command Code, Crush, Kiro CLI,
# Muse Code and Prime Agent do, and no tmux pane has the variables. With
# @agent-wizard-herdr-env on, tmux's global environment gets all three (HERDR_BIN_PATH the
# taw binary, absolute), so panes created from now on have them; existing panes keep what they
# started with. Off (or unset), none is set, and a reload after it was on removes the three,
# but only while HERDR_PANE_ID is still the plugin's own word: a value the user or an outer
# herdr put there is left alone. $1 is the resolved bin.
apply_herdr_environment() {
  local bin="$1" name
  if [ "$(get_tmux_option "$tmux_option_herdr_env" "$default_herdr_env")" = "on" ]; then
    case "$bin" in
      /*) ;;
      *) bin="$(unset CDPATH; cd "$(dirname "$bin")" && pwd)/$(basename "$bin")" ;;
    esac
    tmux set-environment -g HERDR_ENV 1
    tmux set-environment -g HERDR_BIN_PATH "$bin"
    tmux set-environment -g HERDR_PANE_ID "$herdr_calling_pane"
    return 0
  fi
  case "$(tmux show-environment -g HERDR_PANE_ID 2>/dev/null)" in
    "HERDR_PANE_ID=$herdr_calling_pane")
      for name in "${herdr_env_names[@]}"; do
        tmux set-environment -g -u "$name"
      done
      ;;
  esac
}

# taw in every pane, with nothing to set up: tmux's global environment PATH gets, in front, a
# folder of taw's own (bin in its state folder, $TAW_STATE_DIR or the XDG one, as taw reads it)
# that holds one entry, a `taw` link to the resolved binary. Only that link is in it, so the
# folder can never shadow another program, which a user's own build folder (~/.cargo/bin) would.
# Panes created from now on have it; existing panes keep the PATH they started with. Sourcing
# again never adds the folder twice, and moves the link to the binary resolved now. With
# @agent-wizard-path off, a reload after it was on takes the folder out of the global PATH and
# leaves every other entry as it was. A folder that cannot be made, or whose name has a colon,
# leaves PATH alone. $1 is the resolved bin.
apply_path_environment() {
  local bin="$1" dir link path rest new tmp
  dir=$(taw_link_dir) || return 0
  link="$dir/taw"
  path="$(tmux show-environment -g PATH 2>/dev/null)"
  case "$path" in
    PATH=*) path="${path#PATH=}" ;;
    *) return 0 ;;
  esac
  rest=$(path_without_dir "$path" "$dir")
  if [ "$(get_tmux_option "$tmux_option_path" "$default_path")" = "off" ]; then
    [ "$rest" = "$path" ] || tmux set-environment -g PATH "$rest"
    return 0
  fi
  case "$bin" in
    /*) ;;
    *) bin="$(unset CDPATH; cd "$(dirname "$bin")" && pwd)/$(basename "$bin")" ;;
  esac
  # A taw found on PATH by a reload from a pane is this folder's own link: already right.
  # A bin that leads into this folder, loops or leads nowhere is never linked to: the link would point
  # at itself through it.
  if [ "$bin" != "$link" ] && [ "$(readlink "$link" 2>/dev/null)" != "$bin" ]; then
    taw_candidate_ok "$bin" || return 0
    mkdir -p "$dir" 2>/dev/null || return 0
    tmp="$dir/.taw.$$"
    if ! { rm -f "$tmp" && ln -s "$bin" "$tmp" && mv -f "$tmp" "$link"; } 2>/dev/null; then
      rm -f "$tmp" 2>/dev/null
      return 0
    fi
  fi
  [ -e "$link" ] || return 0
  new="$dir"
  [ -z "$rest" ] || new="$dir:$rest"
  [ "$new" = "$path" ] || tmux set-environment -g PATH "$new"
}

# Binds every key in @agent-wizard to the popup, the notification key to
# going to the next agent (next up), and every key in @agent-wizard-clear-key,
# if any, to clearing the pane. $1 is the resolved bin, already double-quoted
# for embedding in a shell command string (protects a bin path containing
# spaces once tmux hands the string to $SHELL -c).
bind_agent_wizard_keys() {
  local bin_q="$1"
  local notif_key key keys clear_keys
  keys=$(get_tmux_option "$tmux_option_keys" "$default_keys")

  # Each kind of binding has a line of text in it that only this plugin writes (what it shows when it
  # fails), which is how a key an option has since dropped is told from one of the user's own:
  # sourcing the plugin again, as TPM's prefix + I does, unbinds the first and leaves the second.
  unbind_stale_keys "taw: could not open the popup" "$keys"
  # Intentionally unquoted: @agent-wizard is a space-separated list of keys.
  # shellcheck disable=SC2086
  for key in $keys; do
    # `display-popup` blocks until the popup closes, so `ui open`'s JSON envelope
    # would print after the user has already switched away -- and tmux shows any
    # run-shell output in a copy-mode view in the pane the key was pressed in.
    # Swallow it like every other binding here; only a failure to open is shown.
    tmux bind "$key" run-shell "$bin_q ui open >/dev/null 2>&1 || tmux display-message 'taw: could not open the popup'"
  done

  notif_key=$(get_tmux_option "$tmux_option_notification_key" "$default_notification_key")
  unbind_stale_keys "taw: next up failed" "$notif_key"
  # The reply is read, not shown, for the same reason. With no agent in any pane the verb
  # fails with a `not_found` error, which is no fault, so the status line says so. Any
  # other failure (tmux refusing a step, the binary not running) says that next up failed
  # and where to look. The command is plain sh, and the \$ and \" are for that shell and
  # its case pattern, not for this one.
  tmux bind "$notif_key" run-shell "out=\$($bin_q notification open-target 2>&1) || case \$out in *'\"code\":\"not_found\"'*) tmux display-message 'taw: no agents';; *) tmux display-message 'taw: next up failed (see taw notification open-target)';; esac"

  # Follows herdr's `keys.clear_pane` (src/config/model.rs, v0.9.3), which is unbound until it is
  # set. Here it is a tmux option holding prefix keys, since a `[keys]` entry binds the popup's
  # own actions. With no key in the option nothing is bound, and a key of the user's own
  # is left alone. A space-separated list, as @agent-wizard is.
  # `run-shell` has no $TMUX_PANE, so the binding names the pane itself, which tmux expands to the
  # pane the key was pressed in. In the background (-b): the verb waits for a shell to answer, and
  # the client's keys are not held back meanwhile. Its reply is read by nobody, so it is dropped
  # like every other binding's here; only a failure is shown.
  clear_keys=$(get_tmux_option "$tmux_option_clear_key" "$default_clear_key")
  unbind_stale_keys "taw: could not clear the pane" "$clear_keys"
  # Intentionally unquoted, as above.
  # shellcheck disable=SC2086
  for key in $clear_keys; do
    tmux bind "$key" run-shell -b "$bin_q pane clear '#{pane_id}' >/dev/null 2>&1 || tmux display-message 'taw: could not clear the pane'"
  done
}

# Every job this plugin hands tmux (a hook's or a startup one-shot) discards its output and ends in
# `|| true`, so that its shell exits 0 whatever taw does. A job that exits non-zero makes tmux print
# `'<command>' returned 1` into the job's pane, or, when that pane has gone with its session, into
# another one it looks for (`cmd_find_from_nothing`, which crashes a tmux 3.6a or 3.6b server while a
# session is being closed: tmux issues 5163 and 4904, fixed in 3.8). The verbs keep their own exit
# codes: only these job strings do not report them. The key bindings above, in bind_agent_wizard_keys,
# that report a failure on purpose (`|| tmux display-message ...`) are not jobs of this kind.
#
# Installs the watch/seen/autosave hooks, and takes off the entries this plugin installed before
# (see taw_hook_entries) so sourcing twice, or upgrading to a different binary path, never stacks
# duplicate hooks. The old entries are named before any new one is added and taken off after all
# are in, so that a hook is never without an entry of the plugin's while a source runs: an event
# that fires in between, a pane's exit say, would otherwise run nothing, with neither its mark nor
# the job that fires `pane.exited`. For the moment that both are there an event runs both, which is
# a duplicate where the other way is a loss.
# $1: resolved bin (unquoted, for the idempotency substring match).
# $2: same bin, double-quoted, for embedding in the hook command string.
install_agent_wizard_hooks() {
  local bin="$1" bin_q="$2"
  local watch hook
  local seen=("${seen_hooks[@]}")

  # tmux 3.6 added client-focus-in (the terminal itself regaining focus, as
  # opposed to a pane gaining it). pane-focus-in still fires on 3.6b (verified,
  # it drives seen-on-focus fine) but it is quietly dropped from bare
  # `show-hooks -g` there (see the bats idempotency test); client-focus-in is the
  # canonical "the user is back on this terminal" signal on 3.6+, enumerates
  # normally, and is added on top where it exists. `set-hook` on an unknown hook
  # name errors on older tmux, so it is gated on the running server's version.
  if tmux_version_at_least 3 6; then
    seen+=(client-focus-in)
  fi

  local spec stale=""
  for hook in "${watch_hooks[@]}" "${seen[@]}" "${autosave_hooks[@]}"; do
    stale+="$(taw_hook_entries "$hook" "$bin")"$'\n'
  done
  for spec in "${event_hooks[@]}"; do
    stale+="$(taw_hook_entries "${spec%% *}" "$bin")"$'\n'
  done

  watch=$(get_tmux_option "$tmux_option_watch" "$default_watch")
  if [ "$watch" != "off" ]; then
    for hook in "${watch_hooks[@]}"; do
      tmux set-hook -ga "$hook" "run-shell -b '$bin_q watch attach \"#{?hook_pane,#{hook_pane},#{pane_id}}\" >/dev/null 2>&1 || true'"
    done
  fi

  for hook in "${seen[@]}"; do
    tmux set-hook -ga "$hook" "run-shell -b '$bin_q seen \"#{?hook_pane,#{hook_pane},#{pane_id}}\" >/dev/null 2>&1 || true'"
  done

  for hook in "${autosave_open_hooks[@]}"; do
    tmux set-hook -ga "$hook" "run-shell -b '$bin_q session autosave >/dev/null 2>&1 || true'"
  done
  for hook in "${autosave_close_hooks[@]}"; do
    tmux set-hook -ga "$hook" "run-shell -b '$bin_q session autosave --on-close >/dev/null 2>&1 || true'"
  done

  # Before the event hooks below, so that the mark comes ahead of the job `pane.exited` starts:
  # the entries of one hook run in the order they were added.
  tmux set-hook -ga pane-exited "$exited_mark_command"

  # `${spec%% *}` is the hook name, `${spec#* }` the event plus its arguments. Left
  # unquoted on purpose: the arguments are `#{q:…}` formats, which hold no spaces until
  # tmux expands them.
  for spec in "${event_hooks[@]}"; do
    tmux set-hook -ga "${spec%% *}" \
      "run-shell -b '$bin_q plugin event ${spec#* } >/dev/null 2>&1 || true'"
  done

  # An entry can be named more than once (a hook two of the lists above carry), and is taken off once.
  printf '%s\n' "$stale" | sort -u | unset_hook_entries
}

# Fires the one-shot startup commands and records the binary version as an
# idempotency marker. $1: resolved bin (unquoted, to run directly for
# --version). $2: same bin, double-quoted, for the run-shell command strings.
run_agent_wizard_startup() {
  local bin="$1" bin_q="$2"
  local watch version

  watch=$(get_tmux_option "$tmux_option_watch" "$default_watch")
  if [ "$watch" != "off" ]; then
    tmux run-shell -b "$bin_q watch attach-all >/dev/null 2>&1 || true"
  fi
  tmux run-shell -b "$bin_q status refresh >/dev/null 2>&1 || true"
  tmux run-shell -b "$bin_q plugin startup >/dev/null 2>&1 || true"
  tmux run-shell -b "$bin_q manifest update --if-stale >/dev/null 2>&1 || true"
  tmux run-shell -b "$bin_q server started >/dev/null 2>&1 || true"

  version=$("$bin" --version 2>/dev/null)
  tmux set-option -g "@agent_wizard_version" "$version"
}

# No taw anywhere: fetch the prebuilt one in the background
# (scripts/auto-install.sh), which loads the plugin again once it is there. The
# plain "not found" message instead when @agent-wizard-auto-install is off, when
# this checkout has no auto-install script, or when this run is that reload and
# the install still left no binary (so it can never loop). A bin/taw that is a
# link is a build of your own (scripts/install.sh with no flag and `mise run
# build` make one; a download never does), and one that leads nowhere is a build
# that has gone: yours to make again, so it says that rather than putting a
# download in its place.
install_taw_in_background() {
  local auto
  if [ -L "$CURRENT_DIR/bin/taw" ]; then
    tmux display-message "tmux-agent-wizard: taw binary not found, bin/taw links to a build that is not there. Run $CURRENT_DIR/scripts/install.sh"
    return 0
  fi
  auto=$(get_tmux_option "$tmux_option_auto_install" "$default_auto_install")
  if [ "$auto" = "off" ] || [ -n "${TAW_AFTER_INSTALL:-}" ] ||
    [ ! -f "$CURRENT_DIR/scripts/auto-install.sh" ]; then
    tmux display-message "tmux-agent-wizard: taw binary not found. Run $CURRENT_DIR/scripts/install.sh"
    return 0
  fi
  if [ -n "$(tmux show-option -gqv "$installing_flag")" ]; then
    tmux display-message "tmux-agent-wizard: still downloading taw"
    return 0
  fi
  tmux set-option -g "$installing_flag" 1
  tmux display-message "tmux-agent-wizard: downloading taw for this machine"
  tmux run-shell -b "bash \"$CURRENT_DIR/scripts/auto-install.sh\" install >/dev/null 2>&1 || true"
}

# A taw this plugin downloaded follows the plugin: when an update (TPM's
# prefix + U) moves the checkout to a new version, the matching binary is
# fetched in the background while the current one keeps working. Only a
# download leaves bin/.taw-release, and a download is a file, never a link, so a
# taw you built or installed yourself is never replaced, even beside a marker an
# earlier download left. The marker's second field is the checkout version the
# download is for: a download of an older version (the latest release, while this
# version has none yet) gives its own, so the next load tries again.
refresh_downloaded_taw() {
  local bin="$1" marker="$CURRENT_DIR/bin/.taw-release" auto fetched_for current
  if [ "$bin" != "$CURRENT_DIR/bin/taw" ] || [ -L "$bin" ] || [ ! -f "$marker" ] ||
    [ -n "${TAW_AFTER_INSTALL:-}" ]; then
    return 0
  fi
  auto=$(get_tmux_option "$tmux_option_auto_install" "$default_auto_install")
  if [ "$auto" = "off" ] || [ -n "$(tmux show-option -gqv "$installing_flag")" ]; then
    return 0
  fi
  fetched_for=$(awk '{print $2; exit}' "$marker")
  current=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$CURRENT_DIR/Cargo.toml" 2>/dev/null | head -n 1)
  if [ -z "$current" ] || [ "$fetched_for" = "$current" ]; then
    return 0
  fi
  tmux set-option -g "$installing_flag" 1
  tmux run-shell -b "bash \"$CURRENT_DIR/scripts/auto-install.sh\" update >/dev/null 2>&1 || true"
}

main() {
  local bin bin_q

  # First, so a tmux too old for the plugin never starts a download it would
  # then refuse.
  if ! tmux_version_at_least 3 2; then
    tmux display-message "tmux-agent-wizard: requires tmux >= 3.2 (found $(tmux -V 2>/dev/null))"
    exit 0
  fi

  if ! bin=$(resolve_taw_bin); then
    install_taw_in_background
    exit 0
  fi

  bin_q="\"$bin\""

  set_default_agent_wizard_options
  clear_outer_environment
  apply_herdr_environment "$bin"
  apply_path_environment "$bin"
  bind_agent_wizard_keys "$bin_q"
  install_agent_wizard_hooks "$bin" "$bin_q"
  run_agent_wizard_startup "$bin" "$bin_q"
  refresh_downloaded_taw "$bin"
}

main
