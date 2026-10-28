---
title: Notifications
description: "Know when a coding agent blocks or finishes: a toast in tmux, a desktop or terminal notification, a status line count, or a message on your phone."
---

# Notifications

When an agent in a pane you are not looking at blocks or finishes, taw tells you where you are
looking:

| Where | What you see | To turn it on |
|---|---|---|
| In tmux | a toast on the status line: `claude blocked · api:2 · needs input` | nothing, it is the default |
| On your desktop | a macOS or Linux notification | `[ui.toast] delivery = "system"` in place of the toast |
| In your terminal, over ssh too | your terminal's own notification (OSC 777 and OSC 9, for a terminal that shows them) | `[ui.toast] delivery = "terminal"` in place of the toast |
| On your status line, all the time | how many agents are blocked, working and done: `⏸1 ⏵3 ✓2` | one line in `tmux.conf`: `set -ag status-right ' #{@agent_wizard_status}'` |
| On your phone | a message through ntfy, Pushover or any command | a one-line `[hooks] notification` ([below](#notifications-on-your-phone)) |
| Out loud | a sound file of yours | `[ui.sound] path` |

The last part of a toast is a line from the agent's screen: what it asks for when it is blocked, the last line of
what it said when it is done. An empty input box says nothing, so a done agent with no reply on screen has no last
part. When the agent's state was read from its window title (Codex's title tells it), the last part is that title.

!!! tip "prefix + O takes you there"
    From anywhere in tmux, `prefix + O` goes to the agent that needs you, and the next press to
    the next one. With nothing waiting it goes to the next agent of any kind. It is the same rule
    as `Tab` in the popup (see [Next up](#next-up)).

Fires on two transitions: **request**: an agent just became `blocked`; **done**:
`working` or `blocked` to `idle`/`done`. Only for *background* panes (the pane's
window isn't the active window of any attached client). The first `idle` of an agent
that has just started (by you, `taw agent start` or a session restore), or that has
moved to another session, is not a **done** until the agent has worked: an agent that
asks its trust question at startup in a session you are not looking at is a request,
and once you answer it, `idle` with no toast (herdr v0.9.2's rule). A new session of
the agent (Claude's `/clear`, Pi's `/new`) turns a `done` back into `idle` and drops
a **done** toast still waiting out its delay. With tmux 3.6+ and
`set -g focus-events on`, a pane counts as background while your terminal itself is
in the background too: a finish while your terminal is in the background counts as
not looking, so it is `done`, with its toast. A **done** toast is decided when the
agent finishes: if you were looking at the pane then, there is none, even if you
leave before its delay is up. An agent that exits mid-turn while you are away has
finished too: its done toast arrives even though its pane has left the popup by then.
After the transition the watcher waits, then re-reads `@agent_state`, the agent and
its session, and the background gate, and only notifies if what triggered it is still
true.

| Key | Default | Waits before |
|---|---|---|
| `[ui.toast] delay_seconds` | `1` | a **request** toast |
| `[ui.toast] done_delay_seconds` | `5` | a **done** toast |

The two are separate because they are answering different questions. A blocked agent
is waiting on you right now, so a second's confirmation is enough. A finished turn is
not urgent, and agents routinely continue after one: Claude Code ends a turn with a
background shell still running and picks up again a moment later, and a `done` toast
fired into that gap is noise. Five seconds is long enough for the continuation to
show up and cancel the toast. Set `done_delay_seconds = 0` to get herdr's timing back.
Both accept fractions, up to 3600.

| `[ui.toast] delivery` | What happens |
|---|---|
| `off` | nothing (this mutes the pop-up only; sound, if enabled, still plays) |
| `herdr` (default) | `tmux display-message` to every attached client. **This replaces the status line while it's shown**: it's tmux's own mechanism, not a floating overlay, so it borrows the same space your status line uses for the `display-time` duration |
| `terminal` | writes OSC 777 and OSC 9 notification sequences straight to each attached client's tty. **Works over ssh**: it doesn't depend on tmux's own `display-message`, just on your terminal understanding the OSC sequences |
| `system` | the host OS's own notifier: `osascript` on macOS, so toasts land in Notification Centre and survive you not looking at the terminal; `notify-send` on Linux, where the notification names `tmux-agent-wizard` as its source. Nothing on any other platform. Set `delivery = "herdr"` to go back to the in-tmux toast |

The popup does not change any of this. It is sized to leave the tmux status line
in place (showing taw's bar while it is open), so a `herdr` toast draws into that
line exactly as it would with the popup closed. The popup's own hint line is only
ever used for the popup's own results: "renamed", a failed `kill-pane`, the filter
you are typing.

A `herdr` toast ends with the key's own hint, `prefix+O next up`. The key goes to who
to deal with next (see "Next up"), which is the agent the toast is about only when it
is the next one in line.

Sounds (`[ui.sound]`) follow the same background-only gating, independent of
`delivery` (even `off` still plays a sound if one is configured; herdr mutes the
pop-up, not the sound). `afplay` on macOS, `paplay` falling back to `aplay` on
Linux. No sound files ship with this project: set `path`/`done_path`/
`request_path` (relative to your `config.toml`'s own directory) to enable audio.
`droid` is muted by default; override per-agent under `[ui.sound.agents]`.

## Next up

`Tab` in the popup and `prefix + O` (`@agent-wizard-notification-key`) outside it are
one rule, "next up": who to deal with next. Both walk one step a press, wrapping:

1. the agents that need you, while any does: blocked, or finished and not yet looked at;
2. otherwise every agent, idle and working alike, and one that is unknown or has not
    reported a state.

`Tab` steps to the next row of the walk in the order the popup draws it (`Shift-Tab` to
the previous), and never leaves an agent that needs you for one that does not while one
is in view. With one agent in the walk it stays on it, so a blocked agent you leave
waiting on purpose keeps `Tab` there until it is answered. The tree's order does not
change as agents change state, so a sweep goes on from the cursor; `f` and then `Tab`
walks the same agents in the flat list's order, most urgent first. To walk the
agents whose row says `idle`, filter first: `/'idle` (the `'` asks fzf for the word as
written), then `Tab`; an agent whose title has the word `idle` in it is found too, and
so are the rows under a header that draws it (a branch called `idle`), so they are in the walk
whatever their state. At the sessions depth a folded header stands for the
agents of its session that an agent view shows, and under a filter too when the filter found it by
its words; one found only by the title it draws stands for that agent. `prefix + O` runs `taw notification open-target`, which goes to the next
agent of the walk after the pane you are in, in tmux order (session, window, pane), and
reports the group of the pane it went to: `needs_you`, `idle` or `rest`. With no agent anywhere it does nothing, and the
status line says `taw: no agents`. If it fails any other way, the status line says
`taw: next up failed (see taw notification open-target)`; running that command in a
shell shows the error.

The flat list is unchanged: it puts what needs you first and idle above working, and
`Tab` works the same way there as everywhere else. The latest notification
is not a step of its own any more, so a fresh request does not jump the queue; a
request is an agent that needs you, so it is in the first group anyway. herdr's own key
focuses the notification on screen and nothing else. The same key works inside the popup,
through the prefix chord: see "Keys".

## Notifications on your phone

taw does not send anything to a phone itself, and herdr does not either: its toasts go to
the app, the terminal or the OS of the machine you are on. What taw has is a hook that runs
whenever it shows a toast, and a command that sends a message is all a phone needs.
`[hooks] notification` runs at the moment a toast would, told the toast's words in
`TAW_MESSAGE` (for example `claude blocked · api:2 · needs input`) and whether it is a
`request` (an agent needs you) or a `done` in `TAW_KIND`. It runs for `taw notification
show` too.

It fires only when a toast would: after the same delay (`delay_seconds` for a request,
`done_delay_seconds` for a done), and not for a pane you are looking at or a finish you
watched. So you are not pinged for what you are already looking at, and you are pinged for
the request that has waited a second while you were away. It runs whatever
`[ui.toast] delivery` says: `off` mutes the pop-up and still sends the message, as it
leaves sounds alone.

Three ready examples, each a command under `[hooks]` or a script it names.

**ntfy** ([ntfy.sh](https://ntfy.sh), free, no account: pick a topic name nobody will
guess and subscribe to it in the app):

```toml
[hooks]
notification = 'curl -fsS --data-raw "$TAW_MESSAGE" "https://ntfy.sh/$NTFY_TOPIC"'
```

**Pushover**, with the token and the user in the environment, and neither in the config nor
on a command line (`curl -K -` reads them from standard input, so `ps` does not show them):

```sh
#!/bin/sh
# ~/bin/push-pushover
exec curl -fsS -K - --form-string "message=$TAW_MESSAGE" \
  https://api.pushover.net/1/messages.json <<EOF
form-string = "token=$PUSHOVER_TOKEN"
form-string = "user=$PUSHOVER_USER"
EOF
```

with `notification = "~/bin/push-pushover"`.

**Any command**, from a plain script that forwards the message to whatever sends yours.
`alert` here stands for any command that takes the text as its one argument:

```sh
#!/bin/sh
# ~/bin/push
exec alert "taw: $TAW_MESSAGE"
```

`TAW_KIND` tells a request from a done, so a script can be louder for one. With ntfy, high
priority for a request and the default for a finish:

```sh
#!/bin/sh
# ~/bin/push-ntfy
: "${NTFY_TOPIC:?set NTFY_TOPIC}"
priority=default
[ "$TAW_KIND" = request ] && priority=high
printf 'url = "https://ntfy.sh/%s"\n' "$NTFY_TOPIC" |
  curl -fsS -K - -H "Priority: $priority" --data-raw "$TAW_MESSAGE"
```

`taw hooks test notification` sends the sample request, `claude blocked ·
taw-hooks-test:2 · needs input`, through your command as it would a real one, which is how
to check the whole path.
`-f` makes curl fail on an HTTP error, so a wrong topic or token lands in `taw hooks log`.
`--data-raw` and `--form-string` are there so that a message that starts with `@` is sent
as text: `-d` and `-F` would try to read a file of that name.

Every account on a shared machine can read a command line with `ps`. The secret of ntfy is
its topic, and the one-liner above puts it in the URL, which is fine on a machine that is
yours. The Pushover script and the ntfy script with the priority keep their secret off the
command line: they give it to `curl -K -` on standard input.

A hook runs with tmux's global environment plus the `TAW_*` variables, whichever process
fired it, so `NTFY_TOPIC` and the like can live where tmux keeps its own. Put
`set-environment -g NTFY_TOPIC yourtopic` in `tmux.conf`, or export it in the shell that
starts the server. `set-environment -gh` works too: the value stays out of `tmux
show-environment` and out of every pane's shell, and it still reaches taw's hooks. The
watcher reads that environment when a hook first needs it and again on `taw config reload`,
so a value you change while tmux is running reaches your hooks after `taw config reload`.
A variable the session the event is about has of its own goes over the global one, as it
does in a pane of that session ("How a hook runs" has the rules).
`taw hooks test notification` runs your command with the same environment, so a test that
sends is a hook that will send. A script that reads its secret from a file of yours has
none of these questions.
