<h1 align="center">tmux-agent-wizard</h1>

<p align="center"><b>Every agent. One key. Your tmux.</b></p>

<p align="center">
  herdr's detection engine for 22 coding agents, as a plugin for the tmux you already run.
  From the maker of <a href="https://github.com/27medkamal/tmux-session-wizard">tmux-session-wizard</a>.
</p>

<p align="center">
  <a href="https://github.com/27medkamal/tmux-agent-wizard/releases"><img alt="Release" src="https://img.shields.io/github/v/release/27medkamal/tmux-agent-wizard"></a>
  <a href="LICENSE"><img alt="MIT" src="https://img.shields.io/badge/licence-MIT-blue"></a>
  <img alt="tmux 3.2 or newer" src="https://img.shields.io/badge/tmux-3.2%2B-1bb91f">
  <img alt="macOS and Linux" src="https://img.shields.io/badge/macOS%20%7C%20Linux-333">
</p>

<p align="center"><b><a href="https://27medkamal.com/tmux-agent-wizard/try/">Try it in your browser, no install</a></b></p>

<p align="center">
  <img src="docs/assets/demo.gif" width="100%" alt="The taw popup: every session and agent across machines, Tab to the one that needs you, Enter to go there.">
</p>

<p align="center">
  <a href="#install">Install</a> &nbsp;|&nbsp;
  <a href="https://27medkamal.com/tmux-agent-wizard/">Docs</a> &nbsp;|&nbsp;
  <a href="#how-it-compares">How it compares</a> &nbsp;|&nbsp;
  <a href="https://27medkamal.com/tmux-agent-wizard/herdr/">Coming from herdr</a>
</p>

You run coding agents in tmux: Claude Code in one session, Codex in another, OpenCode in a
worktree. One of them has been waiting on a permission prompt for twenty minutes; another
finished an hour ago. You find out by flicking through windows.

**taw** reads every pane and shows you. `prefix + A` opens one tree of every session and agent,
with each one's state: blocked, working, done or idle. `Tab` goes to the one that needs you,
`Enter` takes you there. Not in the popup? A toast says who needs you, and `prefix + O` jumps
there. After a reboot, the agents come back in the conversation they were in, once each agent's
hook is installed.

One plugin line in the tmux you already run. Nothing to restart, nothing to migrate.

## One plugin for agents, sessions and restore

- **Every agent's state, in the tmux you already run.** taw reads 22 agents with
  [herdr](https://herdr.dev)'s own detection engine and rules.
- **Pick a project, land in its session.** `n` in the popup picks a project folder and switches
  to its session, or makes it: tmux-session-wizard's session picking, folded in.
- **Back after a reboot, tmux-resurrect's job included.** Save and restore for sessions, layouts
  and folders, and agents come back in their conversations.

Your `tmux.conf`, keys, plugins and status line stay as they are. No central daemon (one small
watcher per pane, about 9 MB each), no account.

## Install

Needs tmux 3.2 or newer, on macOS or Linux, and [TPM](https://github.com/tmux-plugins/tpm).

1. Add the plugin to `~/.tmux.conf`:

    ```tmux
    set -g @plugin '27medkamal/tmux-agent-wizard'
    ```

2. Press `prefix + I`. TPM installs the plugin, and its first load downloads the `taw` binary for
   your machine from the project's
   [GitHub releases](https://github.com/27medkamal/tmux-agent-wizard/releases), checked against
   the release's SHA-256 checksum. tmux says `tmux-agent-wizard: taw is ready, press prefix + A`
   when it is done.
3. Press `prefix + A`. The agents you already have running are in the list.
4. Optional, for restore: once for each agent you use, let it tell taw its session, so a restore
   brings it back into the same conversation:

    ```sh
    taw integration install claude    # and codex, opencode, cursor, ...
    ```

5. Optional, for a shell outside tmux (inside tmux, `taw` is on your `PATH` already):

    ```sh
    mkdir -p ~/.local/bin && ln -s ~/.local/state/tmux-agent-wizard/bin/taw ~/.local/bin/taw
    ```

Without TPM, from source, or with the download off: see
[Install](https://27medkamal.com/tmux-agent-wizard/install/).

## Three keys

| Key | What happens |
|---|---|
| `prefix + A` | One tree: every session, its panes, and the worktrees whose session you closed. Each agent shows its state, its task and for how long. The border counts what needs you. |
| `Tab` | Moves the cursor to the next agent that needs you (blocked, or finished and not looked at yet), else to the next agent. |
| `Enter` | Goes there. |

`1`-`9` jump to a session, `/` filters the whole tree with fzf syntax, and `?` lists every key on
one screen. If you know tmux, you already know taw.
[Keys](https://27medkamal.com/tmux-agent-wizard/keys/)

## Told when an agent needs you, wherever you are

When an agent in a pane you are not looking at blocks or finishes:

| Where | What you see |
|---|---|
| In tmux | a toast on the status line (the default) |
| On your desktop | a macOS or Linux notification (`[ui.toast] delivery = "system"`) |
| In your terminal, over ssh too | your terminal's own notification (`[ui.toast] delivery = "terminal"`) |
| On your status line, all the time | `⏸1 ⏵3 ✓2`: blocked, working, done, once you add `#{@agent_wizard_status}` to it |
| On your phone | a message through ntfy, Pushover or any command, from a one-line hook |

> **`prefix + O`** takes you to the agent that needs you, from anywhere in tmux. Press it again
> for the next one.

[Notifications](https://27medkamal.com/tmux-agent-wizard/notifications/)

<p align="center">
  <img src="docs/assets/toast.png" width="880" alt="A Codex asking approval fills the screen; on the tmux status line under it, outlined in yellow, a toast says claude in infra is done, with the last line it said (With that, staging stays up during apply.), and prefix+O goes to it, beside taw's summary: one blocked, four working, one done.">
</p>

## On your phone, with nothing new to install

It is all tmux, so your agents are wherever your tmux is: ssh in from any terminal app on your
phone, `tmux attach`, and `prefix + A` lists every agent; `Tab` and `Enter` take you to the one
that needs you. With the one-line hook above, your phone also tells you when one does.

## Back after a reboot, in the same conversation

taw saves on its own, a few seconds after each change, and keeps a copy of the layout at most
every 15 minutes. After a reboot or a tmux crash, `taw session restore` brings back your
sessions, windows, layouts and folders, and 18 agents (Claude Code, Codex and OpenCode among
them) reopen in their own conversations once each agent's hook is installed (one command per
agent, such as `taw integration install claude`).

Next to tmux-resurrect and tmux-continuum: taw restores when you run the command, not by itself
when tmux starts, and it does not start other programs such as vim again, which resurrect can.
So you do not need resurrect unless you want that, and if you keep it, one line runs taw's
restore after resurrect's own.
[Save and restore](https://27medkamal.com/tmux-agent-wizard/restore/)

## Also in the box

- **22 agents.** Claude Code, Codex, Gemini CLI, OpenCode, Copilot CLI, Cursor, Amp and 15 more,
  also when started through npm, a wrapper script or a container.
  [Agents](https://27medkamal.com/tmux-agent-wizard/agents/)
- **Worktrees as sessions.** `w` makes a git worktree with its own tmux session; a closed one
  stays in the tree and `Enter` reopens it.
  [Sessions and worktrees](https://27medkamal.com/tmux-agent-wizard/sessions/)
- **Your other machines.** `taw app` lists every machine's agents in one list, over your ssh.
  [Remote machines](https://27medkamal.com/tmux-agent-wizard/machines/)
- **A sandbox on request.** `taw sandbox` runs an agent in a container, through a profile you
  can read (in beta).
  [Sandbox](https://27medkamal.com/tmux-agent-wizard/sandbox/)
- **Hooks.** A command of yours on any event: an agent going blocked, a session opening, a
  worktree closing. [Hooks](https://27medkamal.com/tmux-agent-wizard/hooks/)
- **Agents can drive agents.** Every verb is a JSON CLI (`taw agent prompt --wait`), and
  `taw skill` prints a skill any agent can read. [CLI](https://27medkamal.com/tmux-agent-wizard/cli/)
- **herdr-compatible.** Your herdr `config.toml` means what it meant, and herdr's CLI-driven
  plugins install here (plugin support is in beta).
  [Coming from herdr](https://27medkamal.com/tmux-agent-wizard/herdr/)

## How it compares

✅ has it, ⚠️ in part, ❌ does not. Where taw is ahead first, then what they all do.

| | taw | herdr | workmux | tmux and its plugins |
|---|---|---|---|---|
| Runs in the tmux you already run | ✅ a TPM plugin, no restart; running agents show up at once | ❌ its own terminal workspace and config | ⚠️ a CLI beside it; an agent shows up only once `workmux setup` has put hooks in its settings | ✅ |
| Knows each agent's state | ✅ 22 agents, read from the screen with nothing installed: herdr's rules plus seven of taw's own | ✅ 22 agents | ⚠️ 9 agents, only through hooks it installs; 3 of them never show waiting | ❌ |
| One key to the agent that needs you | ✅ `prefix + O` from any pane, bound for you, or `Tab` in the popup | ✅ its sidebar and notification key | ⚠️ only once you bind a key to `workmux last-done`, or `1` to `9` in its dashboard | ❌ |
| Project picker (your project folders, open or not) | ✅ `n` in the popup, or `taw open` | ❌ a new workspace starts in the folder `new_cwd` names | ⚠️ `p` in its dashboard switches between projects' worktrees | ✅ with tmux-session-wizard (zoxide) |
| Notifications on your phone | ✅ a one-line hook | ⚠️ through a plugin | ❌ | ❌ |
| A command of yours on any event | ✅ one line under `[hooks]` | ⚠️ through a plugin or its socket | ⚠️ commands after a worktree is made, before a merge and before a removal | ⚠️ tmux's own hooks, for tmux's events |
| An agent you start in Docker or Podman | ✅ recognised, with nothing set | ⚠️ only with `HERDR_AGENT` set | ⚠️ the ones its own sandbox starts; its docs do not cover one you start by hand | ❌ |
| Agents on other machines, in the same list | ✅ over your ssh, one line in `config.toml` per machine | ✅ `herdr machine add` | ❌ | ❌ |
| Agents back in their conversations after a reboot | ✅ 18 agents, once each one's hook is installed | ✅ the same 18 | ⚠️ `workmux resurrect`: one repo's worktree windows, each agent with its resume arguments | ❌ tmux-resurrect: layouts, folders, some programs |
| Tells you the moment an agent needs you | ✅ a toast in tmux, your terminal or your desktop | ✅ toasts and a sound | ⚠️ an icon in the window's name | ⚠️ a bell or an activity flag |
| Supports herdr's plugins | ✅ in beta: herdr's CLI-driven plugins, beside your TPM ones | ✅ plugins and a marketplace | ❌ | ❌ |
| An agent in a container | ✅ `taw sandbox`, through a profile you can read | ❌ no sandbox of its own | ✅ Docker, Podman, Apple Container or a Lima VM | ❌ by hand |
| Git worktrees, each in its own session | ✅ `w` in the popup, or `taw worktree create` | ✅ | ✅ `workmux add`, and `workmux merge` to finish | ❌ by hand |
| Agents always accessible | ✅ in a popup on a key, plus a count on the status line | ✅ in a sidebar | ✅ a dashboard in a popup, a sidebar pane in every window | ❌ |
| Updates live as agents change | ✅ the popup, the status line and the toasts, as it happens | ✅ the sidebar, and events on its socket | ✅ window names and its sidebar | ❌ |

herdr as of v0.9.4 and workmux as of v0.1.277, October 2026.

taw is independent of herdr and uses herdr's Apache-2.0 detection engine and rules, with
credit. herdr is its own terminal workspace, and a good one outside tmux; taw is for the people who
stay in tmux. The [parity ledger](docs/PARITY.md) goes through herdr feature by feature, and
[Beyond herdr](https://27medkamal.com/tmux-agent-wizard/beyond-herdr/) lists what taw does differently.

workmux is a CLI for one task per git worktree: it makes the branch, the worktree, the window and the agent, and
merges them back when you are done. Its agents show up once its hooks are in each agent's settings, nine kinds so
far. taw asks for none of that: one TPM line, and every agent already running in your tmux, 22 kinds, shows up at
once, with your other machines in the same list and your sessions back after a reboot. It makes worktrees too,
one session each, and leaves merging to the git tools you already use.

## Footprint

One small watcher per pane, about 9 MB each; eleven panes with ten agents among them used 2.7%
of one core of an M1 Pro, and [`scripts/demo/measure.sh`](scripts/demo/measure.sh) measures your
own. State lives in tmux's own options, kept current by those watchers. taw writes nothing into
your `tmux.conf` or your status line: it adds its two keys and its tmux hooks when it loads,
beside yours. The only thing it asks the network for on its own is newer detection rules from
herdr.dev, at most once every 6 hours, and one setting turns that off. An end-to-end suite drives
real tmux servers.

## Docs

- [Install](https://27medkamal.com/tmux-agent-wizard/install/) and [Keys](https://27medkamal.com/tmux-agent-wizard/keys/)
- [Configuration](https://27medkamal.com/tmux-agent-wizard/config/): tmux options, `config.toml`, 12 themes that can follow your terminal's light or dark mode, two key presets
- [Notifications](https://27medkamal.com/tmux-agent-wizard/notifications/), [Sessions and worktrees](https://27medkamal.com/tmux-agent-wizard/sessions/), [Save and restore](https://27medkamal.com/tmux-agent-wizard/restore/)
- [Agents](https://27medkamal.com/tmux-agent-wizard/agents/): integrations, reporting an agent of your own, containers, [Sandbox](https://27medkamal.com/tmux-agent-wizard/sandbox/)
- [Remote machines](https://27medkamal.com/tmux-agent-wizard/machines/), [Hooks](https://27medkamal.com/tmux-agent-wizard/hooks/), [Plugins](https://27medkamal.com/tmux-agent-wizard/plugins/), [CLI](https://27medkamal.com/tmux-agent-wizard/cli/)
- [Questions](https://27medkamal.com/tmux-agent-wizard/faq/): why not herdr, Claude Code's own agent view, what it costs, how to remove it
- [Coming from herdr](https://27medkamal.com/tmux-agent-wizard/herdr/), [Coming from tmux-session-wizard](https://27medkamal.com/tmux-agent-wizard/session-wizard/), [Troubleshooting](https://27medkamal.com/tmux-agent-wizard/troubleshooting/)
- In the repo: [COMPAT.md](docs/COMPAT.md), [ARCHITECTURE.md](docs/ARCHITECTURE.md), [RELEASING.md](docs/RELEASING.md), [CONTRIBUTING](https://27medkamal.com/tmux-agent-wizard/development/)

## Licence

MIT. The parts taken from [herdr](https://github.com/herdrdev/herdr) keep herdr's Apache 2.0
licence; [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md) lists them.
