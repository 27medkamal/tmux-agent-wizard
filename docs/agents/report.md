---
title: Report an agent of your own
description: How a coding agent you build tells tmux-agent-wizard its state and session from its own code, with herdr's arguments.
---

# Report an agent of your own

If you build a coding agent, it can tell taw what it is doing from its own code, with no
integration of taw's and no change to taw. This is taw's version of herdr's guide, "Add Herdr
support to your agent". The verbs take herdr's arguments, so an integration written for herdr needs
only two things changed: the pane is `$TMUX_PANE` where herdr's is `$HERDR_PANE_ID`, and the binary is
`taw` where herdr's is `$HERDR_BIN_PATH`. Once your agent reports, people who run it in tmux get:

- its name and `idle`, `working` or `blocked` in the popup and in `taw agent list`
- notifications when it finishes a turn or needs a decision
- `taw agent wait` and the other automation that waits on an agent's state
- the same session back in the same pane after `taw session restore`

tmux gives every process in a pane `$TMUX_PANE`, the pane it runs in. Report only when it is
set and `taw` is on the `PATH`; anywhere else, do nothing. Your agent makes three kinds of report:

1. **State.** `idle`, `working` or `blocked`, whenever it changes.
2. **Resume command.** The command that reopens its current session.
3. **Release.** When the user quits it.

## Report state

```sh
taw pane report-agent "$TMUX_PANE" \
  --source my-agent \
  --agent my-agent \
  --state working \
  --seq 1
```

- Report `working` when a turn starts, `idle` when your agent is ready for input, and `blocked`
    when it needs the user to decide something.

- `--agent` is the name people see. Use your own agent's name, not the name of an agent taw already
    detects.

- `--source` names your integration. Keep it the same from run to run, and your own: the
    integrations taw installs use `integration:<agent>`. It may hold at most 128 bytes; a longer one
    fails with `invalid_argument`.

- `--seq` is optional but recommended. It must grow with every report from your source, across
    sessions and restarts of your agent too; a timestamp works well. The shell and Python hooks taw
    installs send the nanoseconds since the epoch; its JavaScript and TypeScript plugins (OpenCode's
    two, Kilo's, pi's and omp's) send the microseconds since the epoch at the moment the plugin
    loaded, plus one for each report it has sent since. taw ignores a report whose number is not
    higher than the last one your source sent for the pane, and, once your source has sent a number,
    a report without one, so a late report cannot overwrite a newer state. Such a report is not taken,
    and answers `ok` with `"in_order": false`. The numbers go with your agent when it leaves the
    pane. A pane keeps the numbers of the last 32 sources to send one, in at most 2 KiB, the oldest
    going first.

## Report the resume command

Put the command that resumes the current session after `--`, with the options that session needs
(the model, a permission mode), so the resumed session behaves the same:

```sh
taw pane report-agent "$TMUX_PANE" \
  --source my-agent \
  --agent my-agent \
  --state idle \
  --seq 2 \
  --agent-session-id "$SESSION_ID" \
  -- my-agent --resume "$SESSION_ID" --model my-model
```

You can send it with any state report, or with `taw pane report-agent-session` when only the
session changes. Report it again whenever your agent moves to another session.

`taw session restore` recreates the pane in the directory it was saved in and, once the pane's
shell is at its prompt, types the command there, in place of taw's own resume command for your
agent. The command must follow these rules:

- The first word is a plain command name on the user's `PATH` (letters, digits, `_`, `-` and `.`,
    not starting with `-`), such as `my-agent`, not a path.

- No word holds an apostrophe or a control character.
- At most 64 words and 8 KiB in all.
- Your source holds the pane first: send a state report with `report-agent` before the command, or
    with it. A `report-agent-session` from a source that does not hold the pane answers
    `resume_not_accepted`, and the rest of that report is applied. While no hook holds the pane, an
    agent taw names from the program in front of it (the agents taw detects) may set its own.

taw refuses a command that breaks the first three rules with `invalid_resume_argv`, and applies
nothing of that report; so it does a command too long for one tmux command once it is quoted for
tmux, as 8 KiB of quotes or backslashes is. A `--` with nothing after it is no
command. The restore types each word as it is when it is made of letters, digits and
`_ - . / : @ % +`, and in single quotes otherwise (a word with `=` in it too, which zsh can read as
a path). People can turn resuming off with `[session] resume_agents_on_restore = false`.

taw keeps the command while your source and your agent hold the pane, and a save keeps it with the
pane. It goes when your source releases the pane, when another agent or another source takes the
pane, and when your agent's process has ended: the pane's own shell back at its prompt, or the end
of the process a restore started for it. A restore that types your command puts it back on the pane
with your agent's word that it is there (`@agent_reported_kind`), as your own report would, so the
pane keeps your agent and its command until one of those happens, even when taw has no detection
for your agent and it reports nothing more.

## Release on exit

```sh
taw pane release-agent "$TMUX_PANE" \
  --source my-agent \
  --agent my-agent \
  --seq 3
```

Releasing hands the pane back to taw's own reading of the screen at once, and takes the command your
source reported off it. Release only when the user actually quits; if your agent moves from one
session to another in the same process, report the new session instead.

If your agent exits without releasing, taw notices when the pane's own shell is back at its prompt
and clears your agent and its resume command, at the pane watcher's next look, a second or two
after. It is a safety net, not a replacement for releasing.

## Keep it out of the way

- Don't let taw slow your agent down: report in the background or with a short timeout, and ignore
    failures.

- Send only the latest state. If several changes queue up while a report is running, drop the older
    ones.

## Agents that report to herdr themselves

Command Code, Crush, Kiro CLI, Muse Code and Prime Agent follow herdr's guide on their own: they
report their state to herdr with nothing installed, but only where `HERDR_ENV` is `1` and
`HERDR_PANE_ID` and `HERDR_BIN_PATH` are set, by running `"$HERDR_BIN_PATH" pane report-agent
"$HERDR_PANE_ID" ...` (and `report-agent-session` and `release-agent`). No tmux pane has those
variables, so taw reads Kiro and Muse off the screen and sees Crush, Command Code and Prime Agent
as plain panes. One setting changes that:

```tmux
set -g @agent-wizard-herdr-env on
```

With it on, the plugin sets three values in tmux's global environment when it loads, after it has
cleared the terminal's own: `HERDR_ENV=1`, `HERDR_BIN_PATH`, the `taw` binary the plugin runs (an
absolute path), and `HERDR_PANE_ID=%calling`. A pane created from then on has them, and those
agents report to taw from any pane, with the commands they would run for herdr. Panes that exist
already keep the environment they started with, so open a new one (or restart the agent in a new
pane) after you turn it on.

`%calling` is a word taw's pane verbs know: the pane the command runs in, read from `$TMUX_PANE`.
tmux gives every pane the one global value, so it cannot hold each pane's own id. taw reads the
word for every verb that takes a pane (`report-agent`, `report-agent-session`, `release-agent`,
`get` and the rest), whether the setting is on or off, so a pane started while it was on keeps
working after you turn it off. tmux reads a leading `%` as a pane id, so no session or window name
can have meant it.

It is off by default because `HERDR_ENV=1` tells every program that knows herdr, herdr's own
agent skill included, that it runs inside herdr, and it would act on that in every pane you open.
Set it to `off` (or remove it) and source the plugin again, and the three are removed from tmux's
global environment, but only while `HERDR_PANE_ID` is still `%calling`: a value you or a herdr
around tmux put there is left as it is. If one of the three is set already when the setting is
on, the plugin replaces it, and an off reload then removes all three rather than putting back what
was there.

The setting reaches panes only. A plugin's action, startup or event hook, a plugin's pane or popup
and a `[hooks]` command never inherit `HERDR_PANE_ID` from tmux's global environment: a plugin
process has the pane, tab and workspace ids of its own invocation and none where it has no pane,
and a hook names its pane in `TAW_PANE`.

Two limits. taw has no socket, so `HERDR_SOCKET_PATH` is never set. The setting helps an agent
that checks only those three variables; one that also requires `HERDR_SOCKET_PATH` (herdr's guide
lists it among the variables) reports nothing, and so does one that reports through the socket
instead of the command line. And an agent running in a container cannot reach the `taw` on your
machine from inside it, so the setting does nothing for a sandboxed one.

## Check your integration

Start your agent in a tmux pane, then look at what taw sees:

```sh
taw pane get "$TMUX_PANE"
taw agent list
```

To try a restore without touching your saved layout, save under a name of its own, close the session
your agent runs in, and restore it:

1. `taw session save --name my-agent-test`
2. `tmux kill-session -t <the session>`
3. `taw session restore --name my-agent-test`

The pane should run your resume command, and your agent should report again.
