---
title: Troubleshooting
description: What each tmux-agent-wizard install and runtime message means, and how to fix it.
---

# Troubleshooting

**"tmux-agent-wizard: could not install taw, see .../bin/install.log"** The first load
found no `taw` and could not get one: there is no release for this machine, or the
download failed or stalled (curl gives up after 15 seconds without a connection, or 3
minutes in all), or its checksum did not match, and building it from source failed or
was not possible (no `cargo`). `bin/install.log` in the plugin's directory says which.
Run `scripts/install.sh --download` there to try again, or `scripts/install.sh` to
build it yourself with Rust (`mise.toml` pins the version). After an update the same
failure says `could not update taw, the current one still works` and keeps the `taw`
you had.

**"tmux-agent-wizard: still downloading taw"** The plugin was loaded again (TPM's
`prefix + I`, or a `source-file`) while its first download was running, and it waits
for that one rather than start another; the status line says `taw is ready` when it
is done. A download stopped in the middle can leave the flag behind:
`tmux set-option -gu @agent_wizard_installing` clears it, and the next load tries
again.

**"tmux-agent-wizard: taw binary not found, bin/taw links to a build that is not
there"** `bin/taw` links to a build of your own (`scripts/install.sh` with no flag and
`mise run build` make that link), and the build is gone, after a `cargo clean`, say.
The plugin never downloads over a link of yours: build again with
`scripts/install.sh` or `mise run build`. To take the download instead, remove
`bin/taw` and load the plugin again.

**"The dots are all the same colour."** They are, in `dots` mode: that is what the
mode means: one `●` per state, distinguished by colour alone (yellow blocked, blue
working, green done, grey idle, magenta unknown). Two fixes, and they stack:

- `[ui] status_indicators = "symbols"` (the default) gives each state its own
    glyph instead: `⏸` `⏵` `✓` `○` `?`. `?` in the popup shows the whole legend,
    including `▢` for a pane with no agent and `◌` for a closed worktree. A row of a machine that is not connected is drawn dim, with no state colour, after every connected row, and counts for nothing in the state counts.

- `[theme] name = "terminal"` drops the built-in themes' RGB values and uses your
    terminal's own ANSI colours. Worth trying whenever the popup looks washed out or
    wrong against your colour scheme: every other theme names exact hex values, which
    need a terminal doing truecolor and a `$TERM` that says so
    (`tmux-256color` plus `set -ga terminal-overrides ',*:Tc'` is the usual fix).

**"An agent looks stuck in one state."** Ask what matched:

```sh
taw agent explain <pane-or-agent>    # the rule that produced the current state
taw manifest show claude             # every rule in effect, with its priority
```

`manifest show` lists the rules in the manifest's own order, each tagged
`source: "manifest"` (herdr's) or `source: "taw"` (a supplement) and carrying the
`priority` that decides which one wins: higher takes it, and a tie goes to the rule
that comes first, which is why supplements are appended last. It also reports
`supplement_rules` and where the manifest itself came from (`bundled`, `remote` or
`override`). `taw manifest list` reports the same per agent, with
`shadowed_by_override` when a local file is winning.

If a pane sits on `working` and nothing is happening, read the screen: Claude Code
holds `working` while a background shell is alive (see the dev-server caveat above).
If it sits on `idle` while an agent is clearly busy, check that `[detection]
supplements` is not `false`, and that no local override is shadowing the bundled
manifest. **An override shadows everything**: a file such as
`~/.config/tmux-agent-wizard/agent-detection/claude.toml` wins over both the bundled
manifest and every remote update, so it freezes detection at whatever the rules were when
you copied it. Delete it to go back; `taw manifest reload` picks up the removal without
restarting anything.

**"A key in the popup does nothing."** Open the popup and read its hint line on the
first frame: an unknown action name, a key spec that does not parse, or two actions
contesting one key is reported there as a message, with `(+N more)` when there is more
than one, and written to the log as well. In every one of those cases the action keeps
its preset key, so the symptom is your *new* key doing nothing rather than the action
disappearing. `taw config check` catches the remaining shape mistake: a `[keys]` value
that is neither a key nor a list of keys. `prefix+…` as a key spec is refused outright:
that chord belongs to tmux, so set `@agent-wizard-notification-key` instead.

**"The clear key cleared the history and left the screen."** The screen is cleared by
asking the shell, and only when bash, zsh or fish is at its prompt with nothing else in
front of it. With an agent, an editor, a build or a plain `sh` in front, or with
`synchronize-panes` on in the window, the key clears the history and sends nothing, and
on the alternate screen it does nothing at all.
`taw pane clear <pane>` says which it was in its reply (`screen_cleared`, and a `note`
when something was left alone). A bash in readline's vi mode is told from a bash that
cleared: it takes Ctrl-L as a character, so see "Clearing a pane" for the two `~/.inputrc`
lines that make it clear.

**"The popup covers my status line,"** **"the popup is the wrong size"** or **"my
popup's border and title are gone."** `taw ui open` reads `status` and
`status-position` at open time, so a status line you turned on after the popup opened
is not accounted for until the next open. If you have lowered
`@agent-wizard-width`/`-height`, remember they are caps in percent: `50` means half
the client, not 50 rows. Both default to `100`. A full-size popup on tmux ≥ 3.3 has no
frame, so `@agent-wizard-popup-title` and `@agent-wizard-popup-border` show only once
you lower a cap; `@agent-wizard-popup-border` is also ignored below tmux 3.3, which
does not have `display-popup -b`.

**"`prefix + O` does nothing."** One case is quiet by design: when no pane holds an
agent, the verb reports `not_found` and the binding puts `taw: no agents` on the status
line. If the verb fails any other way, the status line says `taw: next up failed (see
taw notification open-target)`, and running that command in a shell shows the error.
Otherwise the key always goes somewhere, and it stays where it is when you are
already in the only pane of the group it walks. Inside the popup the same key arrives
through the prefix chord, which needs a `prefix` or `prefix2` the popup can parse.

**"Claude Code, Codex or OMP acts like a nested session in a new pane."** A tmux
server started from inside one of them hands that agent's session variables
(`CLAUDECODE`, `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_SESSION_ID`,
`CLAUDE_CODE_MESSAGING_TOKEN`, `CODEX_THREAD_ID`, `OMPCODE`) to every pane it creates,
so Claude Code will not start normally there and a Codex or OMP launched in the pane
looks like a child of the outer one. Loading the plugin tells tmux to drop those six
from every new pane. A pane that already exists keeps the environment it started with:
open a new one.

**"`ITERM_SESSION_ID`, `LC_TERMINAL`, `KITTY_WINDOW_ID` or another terminal variable is
missing in a new pane."** A tmux server started from a terminal holds that terminal's own
variables and hands them to every pane it creates, even one you reach from another terminal
later, because `update-environment` covers only the names it lists. Loading the plugin tells
tmux to leave ten of them out of every new pane, as it does the six agent variables above:
iTerm2's `ITERM_SESSION_ID`, `LC_TERMINAL` and `LC_TERMINAL_VERSION`, WezTerm's
`WEZTERM_PANE`, Kitty's `KITTY_WINDOW_ID`, Windows Terminal's `WT_SESSION`, screen's `STY`,
and Zellij's `ZELLIJ`, `ZELLIJ_SESSION_NAME` and `ZELLIJ_PANE_ID`. To keep one, name it in
tmux's `update-environment` (`set -ga update-environment LC_TERMINAL` in `tmux.conf`), because
a `set-environment -g` made before the plugin loads, or before it is sourced again (a reload
of `tmux.conf` or `prefix + I`), is dropped with the rest. Each time you attach, tmux then
copies it from that terminal into the session, so the session's new panes have it.

**"I rebuilt `taw` and my open panes still behave as before."** A watcher is started once,
by tmux, and lives as long as its pane, so after `scripts/install.sh` or any rebuild (cargo
replaces `target/release/taw` with a new file) every watcher that is already running keeps
the `taw` it started with. New panes get the new one. `taw watch status` says which is
which: each pane's row has `binary`, `current` or `old` (`null` where no watcher is alive),
and for an old one `binary_note` says why, in words. The result counts in `old` the watchers
`--restart` would start (an old watcher whose pipe is already closed was let go of, and is not
one), and `restart` names the command when there is any, or says that watching is off when it
is:

```sh
taw watch status --pretty
taw watch attach-all --restart    # start every old watcher again
```

A watcher is old when the file it was started from is not the file at that path any more:
a rebuild or a reinstall put a new one there, or took it away. A watcher with no record of
the file it was started from is old too. `--restart` closes each old watcher's pipe, waits a moment for it to leave, and
opens the next from the file the old one started from, so the new watcher runs the `taw`
that is there now whichever `taw` you typed the command with. A watcher that wrote no record
or whose file is gone has no such
file, and starts again from the `taw` you ran, so run the installed one. So does a watcher
whose recorded path is not absolute, or names a file that is not yours: it is not run. Watchers
on the installed `taw` are left alone, nothing happens while `@agent-wizard-watch` is `off`, and
the result says how many were started again, `{"attached": 0, "restarted": 3, "failed": 0,
"note": null}`. A watcher that was let go of and whose new one could not be started is counted
in `failed`, and `note` says so in words: its pane has no watcher now, and `taw watch
attach-all` tries again. The same build copied over the old one, or touched, looks like a new
one, which costs one restart.

**"`taw sandbox default: waiting for docker to answer`"** The runtime is not running yet: start
Docker Desktop, OrbStack or Colima, and the profile goes on by itself within 60 seconds. If it
is still not there, the profile says `docker did not answer in 60 seconds, so the agent was not
started` and starts nothing.

**"`taw sandbox default: no recipe for <agent>`"** taw's own profile knows twelve agents.
`taw sandbox new mine` and one entry in its `case` adds another (`taw skill` tells an agent how).

**"`taw sandbox: the profile max is not executable (chmod +x ...)`"** A file of yours in the
profiles folder needs to be executable. taw never runs its own `default` in its place.

**"`prefix + O` shows the list"** On a slow or loaded machine, `prefix + O` there toward an agent elsewhere can show the list instead: press it again on the list.
