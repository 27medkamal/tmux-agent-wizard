---
title: Hooks
description: "Run a command of yours on any tmux-agent-wizard event, such as an agent going blocked, a session opening or a worktree closing, from one line under [hooks]."
---

# Hooks

A hook is a command of yours that taw runs when something happens: a session is
created, a worktree is made, an agent finishes, a notification is shown. Under
`[hooks]` in `config.toml`, each key is an event name and each value is a command, or a
list of them:

```toml
[hooks]
"worktree.created" = "~/bin/setup-worktree"
"agent.done" = ["~/bin/log-finished", "say done"]
"notification" = "~/bin/push"
```

A command runs through `sh -c`, the way a create hook does, so a leading `~/` is your
home directory and flags and quoting work. It gets no arguments: what happened is in
its environment and on its standard input (see "What a hook is told"). The commands of
a list run side by side, and none waits for another. A hook runs in the background,
so nothing you hook here slows the popup, the watcher or a `taw` command down.

`taw hooks list` prints every name a hook can take, when it fires and what is hooked to
it. `taw hooks test <event>` runs the hooks of one event against a sample and shows what
each one did. They are described at the end of this section.

## Events

The first 19 events are the ones taw already fires at plugins (`docs/COMPAT.md` says
what fires each), so a hook and a plugin hear the same thing at the same moment. Most
come from tmux's own hooks, which the plugin installs, so they happen whether you used
taw or a tmux key. The `agent.*` events are the moments an agent goes into that state,
and `notification` is the moment taw shows a toast. The `sandbox.*` events are the moments
the watcher sees a pane's agent come to run in a container, and the container leave the
pane; they are hooks only, as the `agent.*` events are.

| Event | Fires when |
|---|---|
| `workspace.created` | a tmux session is created, by taw or by hand |
| `workspace.closed` | a tmux session is closed |
| `workspace.focused` | a client switches to another session |
| `workspace.renamed` | a session is renamed |
| `workspace.updated` | a session gains worktree details, or has its metadata reported |
| `tab.created` | a window is created |
| `tab.closed` | a window is closed |
| `tab.focused` | a session shows another window |
| `tab.renamed` | a window is renamed |
| `pane.created` | a pane is split off |
| `pane.closed` | a pane is closed |
| `pane.exited` | the process in a pane ends and the pane stays |
| `pane.focused` | another pane is selected |
| `pane.updated` | a pane is renamed, or has its metadata reported |
| `layout.updated` | a window's layout changes, which includes every step of a resize |
| `worktree.created` | taw creates a worktree |
| `worktree.opened` | taw opens a worktree's session |
| `worktree.removed` | taw removes a worktree |
| `pane.agent_status_changed` | an agent changes state, or a pane gains or loses one |
| `agent.blocked` | an agent becomes blocked and is waiting for you |
| `agent.working` | an agent starts working |
| `agent.idle` | an agent goes idle, at its prompt |
| `agent.done` | an agent finishes while you are not looking |
| `sandbox.started` | a pane's agent is seen running in a container, the moment its marker appears |
| `sandbox.stopped` | the container has left the pane, the moment its marker goes |
| `notification` | taw shows a toast: a request or a done, after its delay, and only while you are not looking at that pane |

`pane.agent_status_changed` and the four `agent.*` events share their moments: a change
into `blocked`, `working`, `idle` or `done` fires the status event and the agent event for
that state, each with its own hooks. A pane getting a different agent in the same state,
an agent leaving its pane and a change into `unknown` fire the status event alone.

Two things follow from how taw reads an agent. When an agent's process ends while it is
working or blocked, its turn is ended first, so `agent.done` (`agent.idle` if you are
looking at the pane) and a done notification fire, and its leaving the pane after that is
the status event alone. And an agent that only its launch hint names, one started through
a wrapper, say, can be read as `unknown` for a moment and then as its state again, which
fires the agent event of the state it comes back to. A new agent is read as `unknown` for
its first few seconds, so its first `agent.*` event comes after them.

An agent whose integration reports its own state (`taw integration install` for `kilo`, `kimi`,
`mastracode`, `omp`, `opencode` or `pi`) fires the same events for what it reports. `taw pane
report-agent` writes the state and nudges the pane's watcher, which takes the report in within a
moment, though the pane prints nothing, and announces the change as it does one it reads off the
screen: the status event, the `agent.*` event of the state and the `notification` hook where a toast
is shown, once for each change. A turn shorter than one tick of the watcher, `working` and then
`idle` reported back to back, is announced as well, its start and its end, from a stamp that `taw
pane report-agent` leaves on the pane for each report of work. The same report again fires nothing,
and OpenCode makes it for every tool it runs. A turn that ends while you are not looking at the pane
is `done`, and `idle` if you are looking at it, as for any agent, and a second `idle` report leaves
an unseen `done` as it is. Its end is announced too: when the pane's own shell is back at its prompt
and the last state taw had heard of the agent was `working` or `blocked`, taw writes the turn's end
(`idle`, or `done` while you are not looking at the pane) as it does for any agent that exits, which
covers an agent whose last report is its end, as `opencode run` ends, so `agent.done` (`agent.idle`
if you are looking at the pane), the status event and a done notification fire once, and the pane is
let go a read later, which is the status event alone. One that had been announced as `idle` or
`done` has no turn to end, and only the status event fires. The integrations of Claude, Codex and
the rest report who the agent is and leave its state to taw, which reads it off the screen. Watchers
started by an older taw announce no reports until they are started again: `taw watch status` marks
them, and `taw watch attach-all --restart`, run with the taw that is installed, starts each of them
again.

`sandbox.started` fires when taw sees a container runtime in front of a pane running an agent,
whether `taw sandbox` started that container or you typed it by hand, and `sandbox.stopped` when
that container leaves the pane, or the pane goes with it still in front (x on its row, a closed
window, a tmux server that ends). A watcher started again (`taw watch attach-all --restart`)
announces no container that was already there, and a runtime that changes between two of the
watcher's reads (a `docker` container ending as a `podman` one starts) is a stop and then a start.

`layout.updated` fires on every step of a resize, so hook it only with something cheap.

## What a hook is told

Every variable below is always set, and empty when the event has nothing to say about
it: a `workspace.created` has no `TAW_AGENT`, and a session that is already closed has
no `TAW_SESSION`.

| Variable | What it holds |
|---|---|
| `TAW_EVENT` | the event's name, `agent.done` for example |
| `TAW_SESSION` | the session's name |
| `TAW_SESSION_ID` | the session's tmux id, `$3` |
| `TAW_DIR` | the directory the event is about: a pane's current directory, a session's directory, a worktree's checkout |
| `TAW_WINDOW` | the window's tmux id, `@7` |
| `TAW_PANE` | the pane's tmux id, `%31` |
| `TAW_AGENT` | the agent in the pane, `claude` for example |
| `TAW_STATE` | the agent's state: `working`, `blocked`, `idle`, `done` or `unknown` |
| `TAW_PREVIOUS_STATE` | the state it was in before, on a state change |
| `TAW_TITLE` | the title of what the event is about: a pane's title (Claude Code writes its task there), else a window's or a session's name |
| `TAW_MESSAGE` | on `notification`, the toast's words on one plain line |
| `TAW_KIND` | on `notification`, `request` (an agent needs you) or `done` |
| `TAW_TARGET` | on `pre_close`, what is being closed: `session`, `window` or `pane` |
| `TAW_SANDBOX` | the sandbox profile taw started the pane's agent through, on every event about a pane in a sandbox; empty when taw only recognised the container, and outside one |
| `TAW_RUNTIME` | the container runtime the pane's agent runs under (`docker`, `podman`, `container`, `nerdctl` or `sbx`), on every event about a pane in a sandbox; empty outside one |
| `TAW_MACHINE` | on `notification`, the machine the toast is about, by its name in `[machines]`; empty for this machine's own agents and for every other event. When it is not empty the hook was told of a toast about another machine, so `TAW_PANE` and the session variables are empty and name nothing on this machine: an empty `TAW_PANE` in `tmux send-keys -t "$TAW_PANE"` means the current pane, so a hook that acts on a pane checks `TAW_MACHINE` first |
| `TAW_TEST` | `1` when the hook is run by `taw hooks test`, empty for a real event |

The ids are tmux's own, so `tmux send-keys -t "$TAW_PANE"` works, and `taw` takes them as
readily as its own `w3:p31`. (Under `taw hooks test` they name nothing: see "Trying a hook".)

The event itself arrives on standard input as one line of JSON: the context a plugin
gets in `HERDR_PLUGIN_CONTEXT_JSON` (the `workspace`, `tab` and `pane` records the CLI
prints, and the rest of the event's own fields), with `"event"` first. `agent.done`
looks like this, cut down:

```json
{"event":"agent.done","workspace":{"workspace_id":"w3"},"tab":null,"pane":{"pane_id":"w3:p31","terminal_id":"%31","cwd":"/home/you/api","agent":"claude","agent_status":"done","terminal_title":"Fix the flaky auth test"},"caller_pane_id":null,"pane_id":"w3:p31","workspace_id":"w3","agent":"claude","agent_status":"done","previous_status":"working","kind_changed":false}
```

A pane's title is whatever the program in it wrote, and a directory is named by whoever
made it, so neither is ever part of the command line. They arrive in the environment and
on standard input, and a title of `$(rm -rf ~)` is a title. Keep it that way in your own
scripts: quote the variables (`"$TAW_TITLE"`) and never `eval` them. An environment value
is cut at 4096 bytes and holds no NUL byte; the JSON is whole.

## How a hook runs

- **In the background.** taw starts the hook and goes on, and does not wait for it to end,
    so a hook that hangs holds nothing up. The event's JSON goes to the hook as it starts,
    and JSON larger than a pipe holds, which a very long pane title can make it, holds the
    caller up until the hook has started.

- **Where.** In the directory the event is about (`TAW_DIR`) when it exists and can be
    entered, else in your home directory, whichever process fired the event.

- **With what environment.** What tmux would give a new pane in the session the event is
    about: tmux's global environment (what `set-environment -g` sets, in `tmux.conf` or
    later) with that session's own environment over it. The session's own is what
    `set-environment -t` sets and what tmux copies into it from the terminal a client
    attached from (`SSH_AUTH_SOCK`, `DISPLAY` and the rest of `update-environment`), so a hook
    that reaches your ssh agent finds the socket of the terminal you attached with last. A
    variable the session or the global environment removes (`set-environment -r`) is taken out
    of what the process that fired the event has. Then `TMUX` for the server that fired the
    event, so a `tmux` command in a hook reaches that server even when it runs on a `-L` or
    `-S` socket, then the `TAW_*` variables, which win over a variable of the same name. It is
    the same whichever process fired the event, the watcher included, and the session's
    environment is read for each event, so a client that attached a moment ago is already in
    it. An event about no session (a notification that is about no pane), or about one that
    is gone (`workspace.closed`), has the global environment alone.

- **For how long.** `[hooks] timeout_seconds` seconds, 30 unless you change it (`0` means
    no limit; up to 3600). A hook still running then is stopped, along with what it started.

- **Its output.** What a hook prints to stdout is thrown away. Of what it writes to stderr
    taw keeps the last 16 KiB, so that a failure can say why, and a hook that prints a great
    deal holds nothing up and fills nothing. If it starts something in the background,
    redirect that process's output (`cmd >/dev/null 2>&1 &`), or taw waits for its stderr to
    close, up to the time limit. A hook whose own shell has ended by then is logged as timed
    out all the same, and what it left running is not stopped: only a hook that is still
    running is.

- **When it fails.** A hook that exits non-zero, runs out of time or cannot be started
    leaves one line in the hooks log, `logs/hooks.log` under the state directory
    (`taw hooks list` prints its path), with the last line it wrote to stderr. A hook that
    worked leaves nothing. One that cannot be started is usually fired by a watcher whose
    `taw` has been moved or replaced since it began, an upgrade doing that: the line names that
    `taw`. `taw watch status` marks the watchers that run an older one, and
    `taw watch attach-all --restart`, run with the current one, starts them again.
    `taw hooks log` prints the end of the file (`--lines N`, 20 by default), and the file is
    moved aside to `hooks.log.1` when it passes 256 KiB.

- **When you change the file.** Watchers read `config.toml` again when it changes
    (`taw config reload` forces it). The popup and the `taw` commands read it each time they
    start.

A hook that does what it hooks, a `tab.created` hook that creates a tab, runs itself again,
and taw does not stop that loop.

## Hooks that can say no

Two hooks are not events: taw waits for them, and a non-zero exit refuses.

| Hook | Asked before |
|---|---|
| `pre_close` | before taw closes a session, a window or a pane (the popup's x, workspace close, tab close, pane close); a non-zero exit refuses |
| `pre_worktree_remove` | before taw removes a worktree (the popup's x on a closed worktree, worktree remove); a non-zero exit refuses |

They get the same environment and JSON as an event hook. `TAW_TARGET` says what
`pre_close` is asked about, and the hook starts in that thing's directory. A worktree's
hook has the worktree as `worktree` in the JSON, its checkout as `TAW_DIR` and its session
as `TAW_SESSION`.

A refusal closes nothing:

- The popup shows `pre-close hook: ` and the last line the hook wrote to stderr, for
    example `pre-close hook: uncommitted changes in /home/you/api`, or
    `pre-close hook refused (exit status 1)` when it wrote nothing. For a worktree it says
    `pre-worktree-remove hook`.

- `taw workspace close`, `tab close`, `pane close` and `worktree remove` fail with the
    error code `hook_refused`. The message names the hook and carries what it wrote to
    stderr, the last 16 KiB of it at most.

- `workspace close --group`, and the popup's `Close them too?` answered with `y`, ask about
    every session in the group before they close any, so one refusal keeps them all open.

They have `[sessions] hook_timeout_seconds` to answer in (10 seconds, the create hooks'
limit, since you are waiting), and one that runs longer is stopped and refuses too. `0`
means no limit here as it does for the create hooks: a hook that never ends holds the close,
and the popup or the command that asked, for as long as it runs. A hook answers when its own
shell ends, and not when the last process it started does: taw reads what the shell wrote
for 0.3 s more and answers, and a process left running is left to run on, so a hook that
says yes still says yes. Redirect a background process's output (`cmd >/dev/null 2>&1 &`)
all the same. Every command of a list is asked in order, and the
first refusal ends it. A close taw is not asked about is not seen: tmux's own `prefix + x`
and `prefix + &`, a process that ends by itself, and `taw plugin pane close`, which closes a
pane a plugin opened. `taw worktree remove` also closes the worktree's session when it is
open, and asks `pre_worktree_remove` about that and not `pre_close`, so a hook that keeps
a session open belongs under both keys.

A `pre_close` that refuses to close anything whose directory has uncommitted changes:

```sh
#!/bin/sh
# ~/bin/keep-dirty-work
[ -n "$TAW_DIR" ] || exit 0
if [ -n "$(git -C "$TAW_DIR" status --porcelain 2>/dev/null)" ]; then
  echo "uncommitted changes in $TAW_DIR" >&2
  exit 1
fi
```

A `pre_worktree_remove`, which starts in the checkout, that refuses to remove a worktree
whose branch is not merged into `main`:

```sh
#!/bin/sh
# ~/bin/is-merged
[ -n "$TAW_TEST" ] && exit 0   # the checkout in a test is an empty directory
if ! git merge-base --is-ancestor HEAD main 2>/dev/null; then
  echo "$(git rev-parse --abbrev-ref HEAD) is not merged into main" >&2
  exit 1
fi
```

## Trying a hook

```sh
taw hooks list             # every name, when it fires, and what is hooked to it
taw hooks test agent.done  # run the hooks of agent.done against a sample
taw hooks log              # the end of the hooks log
```

`taw hooks test <event>` fires a sample of that event, shaped like a real one, at the
event's hooks. It waits for each in turn and prints the command, the directory it started
in, how it ended, what it printed and what it wrote to stderr (the last 16 KiB of each, and
when a hook printed more, the first line says so: `(earlier output cut; the last 16 KiB
follow)`), then the `TAW_*` variables they were all given and the JSON on their standard
input. The sample is always the same story: a session called `taw-hooks-test`, a window
`editor`, and a pane where Claude is working on "Fix the flaky auth test". It runs your
hooks for real, so a hook that sends a message sends it, in a directory taw makes under the
state directory for the purpose and takes away afterwards. A hook that can refuse is tried
with the limit it has, and shown as refusing or not.

The ids in the sample name nothing on any tmux server: the pane is `%999999`, the window
`@999999` and the session `$999999` (`$999998` for the worktree events). Every event is told all three, the ones a real event
has no pane or window for included (a real `workspace.renamed` has no `TAW_PANE`, and an
empty `tmux` target is the current pane, which would be a live one). So a hook that acts on
them (`tmux send-keys -t "$TAW_PANE"`, `taw agent prompt "$TAW_PANE"`) finds no pane and
fails in a test, whatever the event, and does not type into one of yours. `TAW_TEST` is `1`
in a test and empty for a real event, so a hook that acts on the pane can start with:

```sh
[ -n "$TAW_TEST" ] && exit 0
```

The `pre_worktree_remove` sample's checkout is an empty directory and not a git worktree,
so a hook that runs git there fails in a test, and can start with the same line.

`TAW_SESSION` is `=taw-hooks-test:` in a test, and `=taw-hooks-test--feat-x:` for the worktree
events. tmux reads a name in a target as the start of a name when no session is called
exactly that, so `tmux kill-session -t taw-hooks-test` would end a session of yours called
`taw-hooks-testing`. The `=` is tmux's mark for exactly this name, and the `:` after it says
the name is a session's: a command that takes a window (`new-window`, `kill-window`,
`rename-window`) reads a bare name as a window's first and then as the start of a session's,
so it would still reach `taw-hooks-testing` without the `:`. With both, a hook that acts on
`$TAW_SESSION` finds no session, whatever it does with it. One session is still reached: one
of yours called exactly `taw-hooks-test`, which is what the `[ -n "$TAW_TEST" ] && exit 0`
line above is for. A real event, and a test with `--pane`, has the plain name.

`taw hooks test <event> --pane <pane>` tells the event about a real pane instead, one you
name the way any `taw` verb takes one (`%31`, or the `w3:p31` that `taw pane list` shows).
The hook is given that pane's own ids, its session, window and directory, still with
`TAW_TEST` set to `1`, and the output says it used a real pane. Every event is told the
pane, its window and its session, the ones a real event has none of included. So a hook
that acts on `$TAW_PANE` acts on it: pick a pane you do not mind a hook typing into, or
start the hook with the line above. What happened (an agent finishing, a toast's words, a
worktree's branch) is still the sample's, and for the worktree events the pane's own
session and directory stand for the worktree, so a hook that reads the checkout has a real
one to look at. Nothing is made for such a test, and nothing is taken away.

An event about something going away, a close, a removal or a process ending
(`workspace.closed`, `tab.closed`, `pane.closed`, `pane.exited`, `worktree.removed`, and the
two hooks that can refuse, `pre_close` and `pre_worktree_remove`), is refused with `--pane`
unless you add `--force`, since a hook for one of them may close, remove or respawn what it
is told of, and here that is really there. The refusal is on stderr, `taw hooks test:
worktree.removed with --pane runs your hook on the real session and directory of %31; add
--force if that is what you want`, the exit status is 2 and nothing runs. Without `--pane`
no event needs `--force`, and the other events do not either.

`taw hooks test notification --machine studio` sends the sample as a toast about `studio`, one of the
names under `[machines]`, so a hook that reads `TAW_MACHINE` can be tried before a real one comes. Any other
name is refused with the list of yours, and the option takes no other event and no `--pane`. The sample keeps
its invented local pane and session values on purpose, so that testing a hook that acts on a pane cannot reach
a real pane; a real toast about another machine has them empty, so a hook that tests `[ -n "$TAW_PANE" ]`
behaves one way under the test and another for real.

`taw config check` warns about a name under `[hooks]` that taw does not fire, so a typo is
not a hook that quietly never runs.
