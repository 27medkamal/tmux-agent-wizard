---
title: CLI
description: "The taw command line: every verb answers in JSON, for your scripts and for agents that drive other agents, plus a skill any agent can read."
---

# CLI

`taw agent start`, `taw agent prompt --wait`, `taw agent wait --state blocked`, all JSON, plus an agent skill (`taw skill`).

```sh
taw --help          # full command tree
taw agent            # print just that group's help
```

Every command prints a JSON envelope to stdout, except eight. `taw skill` and
`taw config default` print raw text, because they exist to be redirected into a file.
`taw open` is for people at a shell: it prints nothing when it works (`--json` gives it the
envelope), and a line on stderr when it does not. So is `taw sandbox [<agent>]`, the start: it
becomes the profile when it works, and prints one line on stderr when it does not (`taw sandbox
new` and `taw sandbox list` print the envelope). `taw app` is a screen and prints none. `taw machine feed` prints JSON lines, for the app to read. `taw machine attach` becomes tmux on that machine, and `taw machine attach --new` starts a session on one that has none (a target and `--new` are exclusive, and it needs a terminal, refused with status 2 without one; the app runs it for Enter on a machine's `no tmux session here` row, and `taw app --go studio:new` is the same from the popup's doorway), and `taw machine check` prints one line a machine (`--json` gives the envelope). `taw machine list`, `taw machine add` and `taw machine remove` print the envelope.

`taw --machine NAME <command>` runs a command of the list on a machine under `[machines]` and prints its answer as it was given; `machine_unreachable` is the error code when no answer came, and the ids are that machine's (see "Remote machines").

```json
{"id":"cli:pane:list","result":{"type":"pane_list","panes":[…]}}
{"id":"cli:pane:get","error":{"code":"not_found","message":"pane 'w9:p9' not found"}}
```

Groups: `agent`, `pane`, `workspace`, `tab`, `worktree`, `layout`, `notification`,
`hooks`, `session`, `integration`, `plugin`, `manifest`, `terminal`, `watch`, `seen`,
`status`, `open`, `ui`, `popup`, `config`, `skill`, `sandbox`, `app`, `machine`, `image`.

`taw image paste` prints the envelope, with the pane the image went to.

Targets accept a public id (`w3`, `w3:t7`, `w3:p31`), a raw tmux id (`$3`, `@7`,
`%31`), a tmux target (`session:window.pane`), or, for agent commands, a live
agent name. One edge case worth knowing: a tmux session literally *named* like a
public id (say, a session named `w5`) still resolves the bare string `w5` as the
public id of session `$5`, never as that session by name; address it as `$N`
(its raw tmux id) instead. See `docs/ARCHITECTURE.md` "ID mapping" for why.

The verbs behind the two bound keys: `taw ui open` reads the client's size and status
line, computes the popup's geometry, and runs `display-popup` for `taw ui` (which is
the TUI itself, and runs in whatever terminal you start it in). When the launcher swapped
the status line for taw's bar, that popup command also carries `--bar`, so only that
popup keeps `@agent_wizard_bar` fresh: a `taw ui` run any other way (typed in a pane, or
opened without a swap) never touches it. `taw notification
open-target` goes next up, reporting `{"pane_id": …, "target": "needs_you" | "idle" |
"rest"}` (`target` is the group of the pane it went to: the next agent that needs you after the
current pane, in tmux order, else the next agent of any kind) or a `not_found` when no
pane holds an agent.
`taw session open <path>` switches to the session already rooted at `<path>`, or
creates one: the picker's `Enter`, as a command. `taw open [terms]` is that for a person at
a shell, taking a directory or zoxide terms and attaching the terminal outside tmux. It
runs the create hooks as `session open` does, so a pre-create hook that refuses stops it
with nothing created (`hook_refused` under `--json`). `--no-switch` makes or finds the session and
moves no client and attaches no terminal, with or without `--json`; it needs a directory or a term. `--json` prints the
envelope `session open` prints, plus `dir`, the absolute folder it opened. With no terms it runs
`taw ui open --new-session`, which opens the popup on the new-session picker
(`taw ui --new-session` does the same for the popup itself), and Esc closes it. The
picker `n` opens inside the popup is different: Esc there goes back to the tree.
`taw session candidates [--json]` tells what that picker lists before anything is typed, on the
machine it runs on: zoxide's folders in zoxide's order, then the folders under `[sessions]
project_dirs`, a folder once, and at most 200. `--json` answers
`{"candidates": [{"kind": "zoxide", "name": "~/web", "dir": "/home/me/web"}, ...]}`: `kind` is
`session` for a folder with a session running in it (the row then has `session`, that session's
name), else `zoxide` or `project`; `name` is the folder as the picker shows it and `dir` the same
folder, absolute. Without `--json` it prints a line for each folder. It asks nothing, waits at
most five seconds for zoxide, and a machine with no zoxide, no project folders or no tmux server
answers `{"candidates": []}`.

`taw sandbox [<agent>] [--profile <name>] [-- <agent args>]` runs an agent in a container, in the
pane it is typed in; `taw sandbox new <name> [--from <profile>]` copies a profile into your folder
and prints its path, and `taw sandbox list` prints the profiles taw can run, and the ones it
would run but refuses, with why (see "Agents in a sandbox").

`taw agent start <name> --kind <kind> --pane <pane> [--timeout <ms>] [--sandbox [--profile <name>]]
[-- <agent args>]` types the agent's command into an existing pane, and only once that pane's shell is at its
prompt: nothing in front of the terminal but the shell, something drawn, its screen
unchanged for 300ms. A pane already at its prompt is typed into at once. A shell that is
still starting would hand the keystrokes to whatever reads the terminal first, such as a
question its startup asks. `--timeout` (default 30000, at most 300000; with `--sandbox`, 300000)
is one budget for the whole verb (a look at the pane already under way when it runs out is
finished, a second or two on a busy machine): the wait for the shell counts against it, and the agent has what
is left. A shell gets 15 seconds of it, or all of it when that is less; one that has not come
to its prompt by then fails the verb with the error code `shell_not_ready`, nothing was
typed, and the pane is not marked as holding an agent. A pane in copy mode (or any other mode)
or with its input turned off (`select-pane -d`) fails it at once with the same code and the
reason ("pane w1:p3 is in copy mode; leave it and try again"), since tmux would give what is
typed to the mode, or to nothing; the pane is left in its mode, where you may be reading the
scrollback, and is not marked either. One put in a mode after the command is typed and before
its Enter goes is refused the same way, and the message adds that the command may be left on its
line, which taw does not clear without taking the pane out of its mode. The command goes as text,
and its Enter after it, each only while the shell the wait found at its prompt is still in front of
the pane, which tmux is asked in the call that sends, as a restore sends its own: a program started
in the pane in between (one you start, or one a key binding runs, such as a history search) is sent
nothing, and the verb fails with `shell_not_ready` and says so ("pane w1:p3 had another program in
front of its shell when the command was to be typed, so nothing was typed"), adding that the
command may be left on its line when the program came in front after the command (or after part of
a very long one) and before its Enter. A key binding of your shell's that runs a program of its own directly on every key typed (a
zsh widget or a bash `bind -x` that runs it as a command, not inside `$(...)`) has that program in
front for a moment, and an Enter that comes then is held back the same way. A prompt such as starship's
or oh-my-zsh's, and the bindings of atuin and fzf (C-r, Up, C-t, Alt-c and Tab, none of which the typed
line sends), are not that. Never typed into,
because taw cannot tell it from a shell that is not ready: a pane whose process is not itself a shell
(ssh, `docker exec`, a wrapper that does not exec); a shell that draws nothing, such as an
empty prompt (`PS1=''`) in a blank pane; and a question the shell asks itself with a
`read` of its own (zsh's `compinit` question about insecure directories, the oh-my-zsh
update prompt). The check runs `ps -o pgid=,tpgid=,comm=` and `pgrep -g <group>` (the members of the terminal's foreground group, a process's own ancestors included; macOS needs `-a` for that), which macOS and
Linux (procps) have; a `ps` with no `tpgid` column cannot tell, and then nothing is typed.

The verb returns once the agent is really there, and not when its command is typed. The pane's
own shell must be no longer in front of the terminal, and either the process in front is named
for the agent (a program called `claude`), or the agent's own integration has reported it
(`pane report-agent-session`, or a state a hook wrote), or the pane's watcher has read a state
for it off the screen with its kind's rules, which is any state but `unknown`. A pane still
shows what its last agent left (an agent that died at a permission prompt leaves the prompt), and
what the command starts can draw only below the line it was typed on, so the verb records that
line (`@agent_typed_line`) once the shell has echoed the command on it, before it sends Enter (a
shell that has not echoed it within a second has the line recorded where the command was typed),
and the watcher reads only the rows below it while it is on the screen (a row is that line when it
has its shape: the prompt as it was drawn, or another put in its place no wider than it or than 16
columns, the whole command, and after it a right prompt only if the prompt row showed one before the
command was typed; a row of the agent's own that names the model, a tip, or its footer saying to
`Run: mise upgrade claude`, is not): the state counts once something is drawn there. The rows a
shell rewrites when the command is entered (a transient prompt, as powerlevel10k offers) are that
line and the rows above it (the rewritten row holds the command, even when it keeps a right prompt,
as zsh does unless `TRANSIENT_RPROMPT` is set, or when it takes a command that had wrapped back to
one row), and what the command prints before the agent draws (an install's progress) is read for
what it is, which is `unknown`, with no notification, until a rule of the agent's kind matches, for a
kind whose rules know its idle screen (an agent whose rules know only its work and its questions, such
as OpenCode or Gemini, reads idle at any frame no rule knows, there as anywhere). The title the last
agent left is not read as the new one's until tmux reports another, even when the new one sets the
same title (Codex sets its folder's name): an agent whose idle only its title shows (Amp, Hermes, and
Codex with `[detection] supplements = false`) reads idle at any frame no rule knows once it has drawn,
as OpenCode does. A watcher started again (a plugin reload or an upgrade) reads an agent that drew
before it as it did, so a `done` stays. Once the agent's own drawing has pushed the line off the
screen or cleared it, and once the agent exits, the whole screen is read again; a read that does not find the
line reads nothing of the screen and keeps what the pane says, and the next read, made at once, takes
the line for gone if it does not find it either. An
agent that runs under `node` or a wrapper script is told by its own integration's report, or by
the watcher, which the tmux plugin attaches to every pane and which takes the 3 seconds a new
agent reads `unknown`. What the pane recorded of its last agent (its report, its state, its
session and its name, and the pid `agent start` recorded for it) is taken off before the command
is typed, so a report the last agent left is not this agent saying it started, and a second start
in the pane of an agent that died is a new agent to the watcher, with the same 3 seconds when the
watcher's first read after the start finds nothing in front of the pane naming it yet, and with none
when its process names it at once, so that it is read from the rows below its line from that first
read on. The pid
of the agent's process is recorded as soon as the job in front of the pane names the agent (by the
names or the arguments of its processes); for an agent nothing names, it is the process in front
that the verb's last two looks agree on, recorded once the wait is over (when the wait's last two did
not agree, it looks again for up to 1.5 s, three times at the least however long a look takes on a busy
machine, and past that until two agree or until the sixth, but begins none once `--timeout` has run
out, and then records the pid only if its last two looks agree), so a helper your shell runs before the command (a preexec hook's) is
not taken for the agent, and a verb that ends in `timeout` leaves the pid of what came up for the
watcher. A shell that is back at its prompt before the
agent is there (the command was not found, or the agent exited at once) fails the verb at once
with the error code `agent_not_started`, and the pane's `@agent_kind_hint` and recorded pid go
back to what they were; the budget running out is the error code `timeout`. A shell whose prompt
starts at the left edge of the screen is taken for one that is still working, and ends in
`timeout`; a question the shell asks of its own after the command was typed (a `read`) cannot be
told from its prompt, and is taken for it. A name that another pane's live agent already has
fails the verb with `invalid_argument`, as `agent rename` does, before the wait for the shell and
before anything is typed; a pane whose own shell is at its prompt holds no live agent, whatever it
records.
`taw agent wait` and `agent prompt --wait` end with `agent_not_running` when the agent has gone
or another has taken its pane. For an agent started this way, whose pane is named by its hint
alone, that includes a process that has exited (the pid recorded for it is dead), and for any agent
a pane whose own shell is back at its prompt with nothing in front that identifies as the agent,
which is what ends a wait for an agent you started by hand when no watcher takes its record off.
A turn such an agent died in the middle of ends as `done`, or `idle` when you were looking, and a
wait that wanted that status returns it. An agent reading `idle` is not permission to approve anything,
and a wait watches the agent's state, not its background jobs: automation that trusts a mistaken
`idle` can type into an approval dialog, and an agent at its prompt reads `idle` while the commands,
monitors and subagents it started keep running.
`taw agent prompt` and `taw agent send-keys` refuse a pane that no longer hosts its agent before
anything is typed, since the text would land at the shell's prompt and a shell may run it: with
herdr's `agent_not_ready` for a pane that hosts no agent or whose agent has gone, and
`agent_not_found` for a target that resolves to no pane. A pane hosts its agent only while the job in
front of its shell is still that agent: the processes in front of it, with their arguments, have to
name the agent (`node /path/to/bin/codex` is codex), so a shell at its prompt, a shell you started
inside the pane (`sh`, `ssh`, `nix develop`, `sudo -s`), a pager, an editor or `cat` hosts none,
whatever the pane records: an agent you started by hand that has exited is refused, and so is a
pane where you have started something else since. The send itself is guarded, in the one tmux call that
makes it, on the command in front of the pane being the one that was checked, so an agent that exits
between the check and the send is typed nothing either. A pane in copy mode (or any other mode) or
with its input off is refused with `shell_not_ready` and the reason, as `agent start` refuses one,
and left in its mode: tmux would give the text to the mode, and the agent would never see it. The
guard asks that too, so a pane you put in copy mode between the check and the send is refused the
same way, and a prompt whose text had gone before the pane went into the mode says the prompt may be
left on the agent's line.

`taw watch status` lists every pane with whether it is piped and whether its watcher is
alive, and marks each live watcher whose `taw` is not the one installed (`binary`, with
`binary_note` for an old one, the count of those `--restart` would start in `old` and the
command to run in `restart`). `taw watch attach-all --restart` attaches a watcher to every pane
that has none, as `attach-all` does, and starts each old watcher again as well, counting the
ones it could not start in `failed` (see Troubleshooting).

Verbs added for the last herdr socket methods (all return the JSON envelope, `--pretty`
accepted): `taw layout export [tab]` / `apply <tab> <layout>` / `set-split-ratio <pane> <ratio>`
(a tab's `#{window_layout}` as herdr's split tree; `apply` needs a single-pane tab);
`taw terminal title set <text>` / `clear` (the outer terminal window title via tmux
`set-titles`, `clear` restores the previous string); `taw popup close` (closes the agents
popup on the caller's client); `taw session snapshot` (the live workspaces/tabs/panes/agents
snapshot); `taw config reload` and `taw manifest reload` (force every live watcher to re-read
`config.toml` and the local manifest overrides; both are also picked up automatically when
their files change); `taw config reset-keys` (drops the `[keys]` table, other bytes untouched);
`taw agent view set --source <id> [--label <l>] --filter <json> [--sort <json>]` / `clear`
(a declarative projection of the popup's agent rows, stored in `@agent_wizard_view`).

## Clearing a pane

`taw pane clear [<pane>]` (also `--pane <id>` and `--current`; the pane it runs in when
none is named) is herdr's `pane.clear`, which herdr offers over its socket and as a key
(`keys.clear_pane`), unbound until you set it. It empties a pane's scrollback and, where a
shell is waiting for you, its screen:

- **On the alternate screen** (what a full-screen program such as Vim or `less` draws on)
    it changes nothing, as herdr's does not.

- **Anywhere else it clears the pane's history.** A program in front of the pane, an
    agent, an editor or a build, keeps its screen and is sent nothing.

- **At a shell's prompt it clears the screen too.** When bash, zsh or fish is at its
    prompt, with nothing else in front of it, the shell is sent Ctrl-L: it clears its screen
    and draws its prompt and the line you were typing at the top, the line herdr keeps, and
    the old screen, which tmux moves into the history as the shell clears it, goes from the
    history as well. The verb returns once the shell has drawn it, normally in a fraction of a
    second, and says the screen was cleared only when it shows it (the old screen gone into
    the history, or the cursor's row moved up); a shell that shows neither within three seconds
    did not answer, and the note says so. A screen that is clear already, the prompt at the top
    and nothing else on it, is not asked for anything, and the reply says it is cleared. Any
    other shell, such as a plain `sh`, which would read the key as text, is sent none. On macOS
    `/bin/sh` is a bash and tmux calls it one, so a pane running it is sent the key.

- **In a window with `synchronize-panes` on** the key is not sent: tmux would copy it to
    every pane of the window, a program in front of one of them included. The history is
    cleared and the screen stays, and the reply's note says why.

tmux can clear a pane's history but not its screen, which is the program's own, so asking
the shell is the one way to a clean screen that does not reset the pane's terminal and undo
the modes a program set. The reply is the usual envelope, with a note when something was
left alone: `{"id":"cli:pane:clear","result":{"type":"ok","pane_id":"w1:p2","history_cleared":true,"screen_cleared":true}}`.

`set -g @agent-wizard-clear-key C-k` binds it on the prefix, for the pane you press it in
(`prefix + C-k`); a space-separated list binds each, and with the option empty nothing is
bound. For a key without the prefix bind it yourself, as `bind -n M-k run-shell -b "taw pane
clear '#{pane_id}'"` (with the full path to `taw` if tmux cannot find it): `run-shell` has no
`$TMUX_PANE`, so the pane is named in the binding.

What it cannot tell, and what it does about it:

- A shell is at its prompt when the process table says nothing but that shell has the
    terminal and its screen has held still for 300ms, which is the test `taw agent start`
    uses. A shell that is waiting in a builtin of its own (`read`, zsh's `select`) passes it
    and is sent Ctrl-L as input, which the terminal echoes as `^L`.

- A bash in readline's vi mode takes Ctrl-L at its prompt as a character, and inserts a
    literal `^L` into the line; zsh in vi mode does not. Nothing outside the shell says
    which mode it is in, so the verb looks for a `^L` that is on the screen now and was
    not, wherever it landed (the end of the line, the middle, wrapped past the right edge,
    after wide characters, on a blank last row), and takes it out again with one Backspace
    and leaves the screen alone (`screen_cleared` is `false`, with a note; if the Backspace
    does not take it out, the note says that). To have bash clear in vi mode, bind it in
    `~/.inputrc`, after `set editing-mode vi`: `set keymap vi-insert` and then
    `"\C-l": clear-screen`.

- A shell that has Ctrl-L bound to something else gets that.
- bash and zsh are tried here. fish is matched by name and sent the same key, but real
    fish's Ctrl-L has not been tried.
