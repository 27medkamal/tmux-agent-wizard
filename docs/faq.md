---
title: Questions
description: Short answers to what people ask about tmux-agent-wizard, from why not herdr to what it costs, what goes online and how to remove it.
---

# Questions

## Why not herdr?

herdr is its own terminal workspace, and a good one outside tmux. taw is for the people who stay
in tmux: it brings herdr's detection engine and rules into the tmux you already run, with your
config, keys and plugins kept. [Coming from herdr](herdr.md) says what carries over, and the
[parity ledger](PARITY.md) goes through herdr feature by feature.

## Claude Code has its own agent view. Why this?

`claude agents` opens Claude Code's agent view of its own background sessions, which run under
Claude Code's own supervisor, outside tmux. taw lists every agent running in a tmux pane, 22
kinds with Claude Code among them, in one tree with the sessions, panes and worktrees around them
and, through `taw app`, your other machines, and tells you when one needs you.
[Agents](agents.md)

## How does it know an agent is blocked?

A small watcher per pane reads what the pane shows and matches it against herdr's detection rules
for 22 agents, plus seven rules of taw's own (six for Claude Code, one for Codex). Hooks are
optional: `taw integration install <agent>` adds one that reports the agent's session id, and for
some agents its own state. `taw agent explain <pane>` names the rule that produced a pane's state.
[Agents](agents.md)

## What happens when an agent changes its screen?

The detection rules come from herdr.dev: taw asks for newer ones at most once every 6 hours, and
`taw manifest update` fetches them there and then. When a state looks wrong,
`taw agent explain <pane>` shows what matched. [Troubleshooting](troubleshooting.md)

## I only run one agent. Is it for me?

Yes. When it blocks or finishes while you are in another pane, a toast says so and `prefix + O`
takes you to it. The popup is a session picker too: `n` picks a project folder and switches to its
session, or makes it. [Notifications](notifications.md)

## Can tmux's bell and activity alerts not do this?

They say something happened in a window. They cannot tell blocked from done, do not know which
agent it was, and do not take you there; taw does all three.
[How it compares](index.md#how-it-compares)

## Does it work with the mouse?

Yes, in the popup: a click selects a row, a double-click goes there, the wheel scrolls the tree or
the preview under the pointer, and a right-click opens a menu with go, rename, close and new
worktree. [Keys](keys.md)

## What does it cost?

One small watcher per pane, about 9 MB each, and no central daemon. Eleven panes with ten agents
among them used 2.7% of one core of an M1 Pro, and
[`scripts/demo/measure.sh`](https://github.com/27medkamal/tmux-agent-wizard/blob/master/scripts/demo/measure.sh)
measures your own.

## Does it change my tmux.conf, keys or status line?

No. taw writes nothing into your `tmux.conf` or your status line. When it loads it adds two keys,
`prefix + A` and `prefix + O`, and its tmux hooks beside yours; either key can be changed
(`@agent-wizard`, `@agent-wizard-notification-key`). The count on the status line shows only once you add `#{@agent_wizard_status}` to it.
[Configuration](config.md)

## Does it phone home?

On its own it asks herdr.dev for newer detection rules, at most once every 6 hours, and
`[update] manifest_check = false` turns that off. Everything else that goes online is something you
start. [What goes online](install.md#what-goes-online)

## Is the download safe, and how do I remove it?

The first load downloads the `taw` binary from the project's GitHub releases and checks it against
the release's SHA-256 checksum before it installs it; a download that does not match is never
installed (with `cargo` on the machine, taw is built from source then). You can also build it from
source from the start. [Remove](install.md#remove) has every step to take it out
again.

## Zellij, WezTerm, Windows?

taw is a tmux plugin: it needs tmux 3.2 or newer, on macOS or Linux. It sees agents in tmux panes,
not in Zellij's or WezTerm's own panes; tmux running inside WezTerm or any other terminal is fine.
[Install](install.md)
