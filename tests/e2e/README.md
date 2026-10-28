# End-to-end test harness

`mise run check` (unit + bats) proves the code in isolation. This directory
proves the **wired-up binary and tmux plugin behave end to end**: real watchers
attached by real hooks, the popup rendering on a real client, notifications,
sessions, worktrees and plugins: things a unit test cannot reach.

Everything here is **isolated from your real tmux**. Each run creates private
tmux servers named `taw-e2e-<slice>-<pid>`; the user's default socket is never
addressed. Their sockets are in a tmux directory of the run's own, which `e2e_up`
makes (`scripts/private-tmux-dir.sh`: a fresh short directory in `/tmp`, which is
`$TMUX_TMPDIR` for the run, with `$TMUX` unset), so a tmux that named no socket would
find no server there or start one of its own, never yours: `e2e_down` says so, ends it
and removes the directory. `scripts/check.sh`, the bats tests and `scripts/cli-sweep.sh`
run the same way, and the gate fails for anything a step leaves running when it ends (a child
of a test that nothing waited for): it names it and ends it as soon as the step is over, before
it could outlive the directory. It cannot see a process that a test puts in a process group of
its own (`process_group(0)`, `set -m`, `setsid`), which is not in the step's group: a test that
starts one ends it itself, as the suite's own do. taw's config/state go to a per-run temp dir under
`${TMPDIR:-/tmp}/taw-e2e`. Nothing reads or writes `~/.config`, `~/.claude`, or
the real `~/.local/state/tmux-agent-wizard`. The shells the two servers run keep no
history file (a shell that is typed into saves what it was typed when its server is
killed, to `$HISTFILE`, which is `~/.bash_history` where nothing says otherwise) and have
a home of their own, `shell-home` in the run's directory, for `HOME` and `ZDOTDIR`, so none
of them reads your startup files either. `e2e_up` sets those three on the two commands that
start the servers (a `set-environment` afterwards cannot reach a server's first pane) and
does not export them, so a script that sources the harness keeps the home it was run with.
`tests/shell_isolation.rs` fails when a server is started without them. And the harness unsets
`GIT_DIR`, `GIT_WORK_TREE` and git's other variables that point it at another repository (git
sets `GIT_DIR` for the command of a `rebase -x`), so the repositories a section makes with `git
init` are its own, and never the one being rebased (`tests/git_env.rs` holds every script that
runs git to that).

A run reaches no network either: its state directory has a fresh marker of the last manifest
check (so neither a watcher nor the plugin's startup one-shot asks herdr.dev for anything), and
`ALL_PROXY` names a port nothing listens on (`E2E_PROXY` puts a listener of a test's own there:
`tests/offline.rs` does, to see that nothing is asked). And it leaves nothing in the tmux socket
directory: `e2e_down` removes the socket file of each server it started, by the exact path the
server said it listens on (tmux leaves the file of a server that has ended where it was). Nor does
it leave a process running that it started and did not wait for: such a process (the `cat` that
holds a client's terminal input open, from `e2e_terminal`) is noted with `e2e_track`, and `e2e_down`
ends it by its pid, if the pid still has the command line it was noted with. `tests/harness_cleanup.rs`
shows that, and that the `cat` leaves by itself when the run lets go of its fifo (a run must not
leave any behind for good).

Nor does a run start an agent or a container runtime that is installed on your machine: `e2e_up`
puts a net of refusing stubs first on the run's `PATH`, in its tmux directory (`scripts/agent-net.sh`:
one for every name in `scripts/agent-net-names.txt`, every agent and runtime taw knows by name and
every agent a test names that taw does not know),
after the fake agents of `tests/fixtures/fake-agents`. A section that ran `codex` or `docker` from
your `PATH` reaches a stub, which notes the call and refuses it (exit 127), and the run's last check
fails, naming each call. `mise run check` puts the same net first on the gate's `PATH`.

Nor does a run show a notification or play a sound to whoever runs it. The notifications it checks
go to a recorder of its own (`$TAW_NOTIFY_CMD`, which `e2e_up` sets), and `e2e_up` puts a second net
first on the run's `PATH` (`scripts/desktop-net.sh`: one stub for every name in
`scripts/desktop-net-names.txt`, `osascript`, `afplay` and the other notifiers and players), with
`$TAW_SOUND_CMD` naming a refusing recorder of the net's: a section that reached any of them is noted
and refused, and the run's last check fails, naming each call. `mise run check` puts the same net
first on the gate's `PATH`.

## Scripts

| Script | What it does |
|---|---|
| `harness.sh` | Sourceable library. `e2e_up <slice>` brings up an inner server (plugin loaded, fixture agents on PATH, and a zoxide database of its own, so no `zoxide add` reaches the runner's) plus an outer server holding an attached client; `e2e_down` tears them down. Helpers: `tm`, `outer`, `taw`, `keys`, `screen`, `screenc`, `prefix`, `e2e_start_agent`, `e2e_start_replay`, `e2e_wait_opt`, `e2e_wait_screen`, `e2e_wait_pane`, `e2e_wait_cursor`, `e2e_popup_cursor_line`, `e2e_popup_cursor_row`, `e2e_sgr_before`, `e2e_style_at`, `e2e_wait_still`, `e2e_status_position`, `e2e_ui_open`, `popup_open`, `e2e_wait_popup_open`, `e2e_wait_popup_closed`, `e2e_wait_bar_restored`, `e2e_terminal` (a client's terminal, from outside tmux), `e2e_track`, `e2e_end_tracked`, `e2e_command_of`, `e2e_running`. |
| `acceptance.sh` | Mechanical, self-checking run over the whole feature set (config, manifests, workspace/tab/pane CRUD, watcher lifecycle, a watcher on an old taw marked by `watch status` and started again by `attach-all --restart`, notifications, sessions, worktrees, plugins, `ui open`'s bar on the status line at the top and at the bottom (and the line's own text back on close, a dead launcher's record recovered) with a frameless full-size popup, `session open` hooks/zoxide, notification `open-target` walking next up, the done-toast delay, `[hooks]` (an `agent.done` command, a `notification` command after the toast's delay, a `pre_close` refusal that keeps a session open, `taw hooks list/test/log`), identity across every bundled manifest, and the attention popup itself: attention walk, filter (`ctrl-n`/`ctrl-p` moving through it too), close confirmation, help and its groups (with `prefix+O` on the line under Tab's), toast, the `prefix+O` chord (and, with `set -g prefix2 C-a`, `C-a O` as well, from the tree and from the filter), the project picker, both key presets (the search preset's fzf keys, and the chords it does not have doing nothing), numbers, the own-row cursor, the constant `Enter go` hint, Enter on your own row just closing, with nothing hot and with an agent blocked elsewhere, Enter on a header, the sessions depth's folded counts and titles, a digit in the filter matching as plain text, h/l keeping the row, agent titles, the prefix+O walk and its "no agents" message, Tab and prefix+O (outside the popup and inside it) walking what needs you, and with nothing that does every agent, idle and working alike, your own session's tree line in the accent colour, idle above working in the flat list, `f` changing nothing on screen at the sessions depth, the preview wrapping a long line but cutting a full-screen program's, every session's number in yellow, bold, while its key works (dim while `/` or the help is open and in the search preset, where it is a label only), the letters `a` to `e` beside the first five rows of every session, under its header's `*` (the first column is the session numbers' alone), lit in the session the cursor is in, the light following it there with `J` and `j`, `b` jumping to the second row, `c` saying `no row c in this session` where it names no row, and Esc quitting with them out, each session's open window's rows lit whole (kind, place and tree line in white, bold, the rest in white) in the tree and the flat list, `f` at the sessions depth still flipping the layout for `l`, `X`/`s`/`v` doing nothing and `c` doing nothing only where it names no row (moving the cursor where it does), a `taw ui` typed in a pane leaving no bar option behind, `x` on the last pane of a session nothing is cut from saying the session closes with it (and only the window, when the session has another), and an extra `Enter` after a kill or a worktree removal (typed right behind the confirmation's `y`, or pressed again while git deletes a large checkout) leaving the popup open, with an `Enter` a moment later still going, and a popup narrower than 100 columns drawing the tree alone, resized across the threshold while it is open and opened at the small size), `taw ui open --new-session` starting on the new-session picker in both key presets (one Esc closes the popup, where Esc on the picker `n` opens goes back to the tree), and `taw open` (typed into a pane's shell inside a client: a directory, a relative path, a zoxide term as `z` asks for it, `--json`, the alias `t`, and the picker with a directory picked, or cancelled with one Esc; outside tmux, with a terminal from `script`: a client attached to the new session, to one that already exists, and to the first session of a tmux that was not running), and the filter's directories (`[ui] projects_in_filter`, against a fake zoxide: `/` listing them with a dim header and their tags, one blank line under the last session of a short tree and pinned to the bottom of the panel on a tree taller than the popup, a path too deep for its row losing its middle and keeping its tag and its last component, the cursor going to a directory when no session matches, Enter creating its session and the client landing there, a directory with a session tagged running and switched to rather than made twice, the help naming them, the switch off changing nothing, and the search preset listing them from its first frame), and the clear key (`@agent-wizard-clear-key` set and the plugin sourced again, bound to nothing before) pressed through the client: a bash at its prompt redrawn at the top with its line kept and its history gone, and a program in front losing its history, keeping its screen and being sent nothing, and the remote section (`taw app` in a real terminal with a second private server as the machine `studio`: its rows, going in, the popup key back, this server's session numbers unchanged, the doorway from a hand-attached client, and the machine dropping off with its rows greyed, skipped by Tab and answered with the not-connected words). Prints a PASS/FAIL table and exits non-zero on any failure. |
| `smoke.sh` | Opens the agents popup and confirms it renders and closes. |
| `vm-fresh-install.sh` | Creates a throwaway OrbStack Ubuntu machine, builds taw from source as the README's build-it-yourself path does, from nothing, runs the unit suite and `acceptance.sh` inside it, then deletes the machine (`--keep` to leave it). |
| `vm-download-install.sh` | Creates a throwaway OrbStack Ubuntu machine, builds the static release asset inside it and lays it out as a release, then installs the plugin the way TPM does with neither taw nor cargo on the PATH. Passes when the first load says it is downloading, installs a static `bin/taw` that runs, says it is ready and binds `prefix + A`. Nothing comes from GitHub. Deletes the machine (`--keep` to leave it). |
| `cleanup.sh` | Removes every throwaway `taw-e2e-*` / `taw-test-*` / `taw-bats-*` tmux socket left by a crashed run (`--all` also removes `taw-*`). Never touches `default` or non-`taw` sockets. Then, in the tmux socket directories of their own that a crashed gate, harness run, bats test or sweep left (`/tmp/taw-check.*`, `/tmp/taw-e2e-tmux.*`, `/tmp/taw-bats-tmux.*`, `/tmp/taw-sweep-tmux.*`, and `/tmp/taw-test-tmux.*` of the library's own tests, the user's own), it removes only what is dead: a socket no server answers on. A socket a server answers on is a run still going, perhaps someone else's, and is named and left alone; and no directory is removed, empty or not (a run that is going may have no server up yet, and tmux falls back to `/tmp` for a `$TMUX_TMPDIR` that is gone). |

## Run

```sh
mise run build              # build bin/taw first (the harness will build it if missing)
tests/e2e/acceptance.sh     # the main end-to-end run
tests/e2e/smoke.sh          # popup smoke test
tests/e2e/vm-fresh-install.sh   # full fresh-OS install (needs OrbStack)
tests/e2e/vm-download-install.sh  # the download install on a fresh OS (needs OrbStack)
tests/e2e/cleanup.sh        # sweep leftover test sockets
```

Interactive exploration:

```sh
source tests/e2e/harness.sh
e2e_up demo
pane=$(e2e_start_agent claude)      # fixture agent, detected via the kind hint
tm display-message -p -t "$pane" '#{@agent_kind} #{@agent_state}'
prefix A                            # open the popup on the attached client
screen                              # see what the client shows
e2e_down
```

## Notes

- The `remote` section of `acceptance.sh` reaches its machine through a stand-in for ssh that it writes in shell
  (`TAW_SSH_BIN` names it; the `ssh` on the `PATH` stays the net's refusing stub, and the run's last check still
  finds the net's log empty). The stand-in runs the command it is given on studio's side, as the `taw` under test
  with studio's own tmux socket, config and state folders and home. Like `tests/common/machine.rs`'s, it has no `--`
  case. The app is the program of a pane of the outer server, read with `capture-pane` and driven with `send-keys`.
  To run the section alone, source `harness.sh`, define `ok`, `bad`, `check` and `jget` as the top of `acceptance.sh`
  does, run `trap e2e_down EXIT` and then `e2e_up`, and source the lines from `echo "-- remote` to
  `# end of the remote section`. The section notes every process and socket file it makes for `e2e_down`
  (`e2e_track`, `e2e_track_socket`), so a run that stops inside it leaves none of them.

- **Fixture vs real agents.** The fixture agents in `tests/fixtures/fake-agents`
  use a `#!/usr/bin/env bash` shebang, so `#{pane_current_command}` reads `bash`
  and process-name identity does not resolve; `e2e_start_agent` uses
  `taw agent start --kind <k>`, which sets the spawn-time kind hint (herdr's own
  mechanism) so detection resolves, and which returns only once the pane's watcher has
  read a state off the fixture's screen (the interpreter names no agent, so nothing
  else says it started), about 3.5 seconds after the hint: the 3 a new agent reads
  `unknown`, and a tick. `agent start` refuses a name another live agent has, so each
  agent is started under a name of its own (the kind and the pane's number), and the
  name is taken off again: the popup draws an agent's name where it has one and its
  kind where it has not, and the rows of every section are the kind's. Panes run a
  non-interactive `/bin/sh` with the fixture dir first on PATH, so a bare command
  name is the fixture and never a real agent your login shell would resolve.
- **Signals.** The fixture flips state on `SIGUSR1` (working) and `SIGUSR2`
  (blocked); `acceptance.sh` signals the fixture process under the pane pid.
- **Real agents / popup keys / mouse** are covered by the interactive harness
  but not asserted mechanically here; drive them with `keys`/`screen` as above.
- **`e2e_start_replay <id>`** runs `tests/fixtures/fake-agents/replay` (a POSIX `sh`
  script, deliberately not bash) in a fresh window and sets `@agent_kind_hint`
  itself, without going through `taw agent start`. `#{pane_current_command}` reads
  `sh` for it, which names no bundled manifest, so a pane started this way only
  resolves `@agent_kind` if identity truly comes from the hint
  (`src/detect/identity.rs`), never from the process name, so that it can sweep every
  bundled manifest id in one pass.
- **`e2e_status_position top|bottom`** sets the inner server's `status-position`,
  and **`e2e_ui_open`** runs `taw ui open` the way the plugin's key binding does
  (`run-shell -b "<taw> ui open"`; any arguments are `ui open`'s own, such as `--new-session`). The `-b` is required from a script: `display-popup`
  (and so `ui open`'s own call into it) blocks until the popup closes, so a
  synchronous `run-shell` here would hang the caller for the popup's whole
  lifetime (a real key binding only *feels* instant because nothing is waiting on
  it). `taw popup close` right afterwards is enough to end it; closing a
  backgrounded popup this way logs a benign `tmux_error` from the now-interrupted
  `ui open` call, harmless for a check that already read what it needed before
  closing.
- **`e2e_wait_screen <regex> <timeout_ms>`** polls `screen`'s content for an
  extended-regex match every 50 ms, up to the timeout, the same bounded-poll idiom
  as `e2e_wait_opt`, but for asserting on rendered content (a popup drawn, a
  `display-message` landed) instead of a tmux option. Prefer it over a fixed `sleep`
  before any check that reads `screen`: a fixed sleep either wastes time on a fast
  host or is a latent flake on a slow/contended one.
- **`done` vs `idle`.** A `working -> idle` edge only becomes the `done` state
  (`src/watch/state.rs`'s `promote_done`) when the pane is backgrounded and unseen;
  the same edge on a foregrounded/seen pane stays `idle` and never fires a toast.
  The done-delay check backgrounds the fixture first, then waits for literal state
  `done`, not `idle`.
- **`popup_open`** is true while the popup is on screen: it looks for the tree
  panel's own top border, `╭ sessions`, because a full-size popup has no frame
  (and so no `┌─ taw` title). **`e2e_wait_popup_open`** and
  **`e2e_wait_popup_closed`** poll it; **`e2e_wait_bar_restored <session>`** then
  waits for `ui open` to put that session's status line back, which it does right
  after `display-popup` returns, a moment after the popup has left the screen.
- **`e2e_sgr_before <coloured line> <glyph>`** returns the escapes a `screenc` line
  carries right in front of a glyph: the style that glyph is drawn in. Grep a whole
  coloured line for a colour and it matches the tree panel's own accent-coloured
  border on every row.
- **`e2e_style_at <coloured capture> <text>`** returns the style `<text>` is drawn
  in, as `<fg> <bold>` (`205;214;244 1`), following every escape from the top of
  the capture. tmux writes an escape only where the style changes, so a column
  drawn in the colour of the space before it carries nothing in front of it but
  its bold, which `e2e_sgr_before` alone cannot see past.
- **`e2e_wait_still [quiet_ms] [timeout_ms]`** waits until the popup's own rows
  (lines 1-39 of the suite's 120x40 client) have stopped changing. Run it before
  a watch for nothing to change after a key: a session's git branch, or the
  preview of the row the cursor has just reached, is read off-thread and lands a
  moment after the rows are drawn.
- **`e2e_popup_cursor_line`** returns the screen line number carrying the popup's
  selection highlight (the default theme's `selection_bg`, Catppuccin Mocha's
  `#45475a`), or empty if no row is highlighted right now, for a check that polls
  for the cursor reaching a specific row rather than reading what that row says.
  **`e2e_popup_cursor_row`** returns the plain-text content of that row instead,
  found by cross-referencing a coloured capture (`screenc`, via
  `e2e_popup_cursor_line`) against a plain one (`screen`): `capture-pane -p`
  strips styling, so a bare text search cannot tell "the cursor moved to a
  different, identically-worded row" from "nothing moved". **`e2e_wait_cursor
  <regex> <timeout_ms>`** polls it the same way `e2e_wait_screen` polls `screen`:
  use it after a movement key when the target text (an agent's name, say) could
  already be visible somewhere else on screen: a plain `e2e_wait_screen` would
  pass on that stale content without the cursor having actually arrived.
- **`e2e_start_agent`/`e2e_start_replay` pin `-t main`.** Both create their window
  with an explicit session target, not a bare `new-window -d`: unqualified, that
  follows tmux's own notion of the "current" session for a one-off CLI command,
  which drifts wherever an earlier `switch-client` elsewhere in the suite (`taw
  workspace create` calls one internally) last left it. Before this fix that was
  observed landing a replay window in an unrelated scratch session instead of `main`
  and confusing a much later, unrelated check that assumed a clean `main`.
