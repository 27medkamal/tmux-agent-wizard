---
title: Remote machines
description: "Every machine's coding agents in one list over your own ssh, with Enter into that machine's tmux: taw app, taw machine add and taw machine check."
---

# Remote machines

taw lists every machine's agents in one list, over your ssh: this machine's sessions first, then the sessions of each machine you name under `[machines]`, each under a header of its own. A machine's list reaches the screen within a second of a change there, not at once, because it is read over your ssh. Enter on one of its rows puts the terminal on that machine's own tmux, at that pane, and the key back returns you to the list. taw keeps no key, no password and no host key: ssh does all of that, as your ssh, with your `~/.ssh/config`.

When the only machine listed is this machine's own tmux server (a config shared between machines may name it), that is one machine, and the list draws no machine header, as with no `[machines]` at all. As soon as one other machine is listed, every header is drawn: this machine's, the own server's entry (with no rows under it) and each other machine's.

## Two ways in

If you live in one terminal window, make the terminal run `taw app` instead of a shell. In WezTerm:

```lua
config.default_prog = { "/bin/zsh", "-l", "-c", "source ~/.zshrc; exec taw app" }
```

The window is then the app: the list of every machine, and Enter goes in. In a terminal that starts tmux itself, `taw app` finds the tmux it is in and opens the popup there, so the same line is safe in both.

If your terminal starts tmux, keep the popup as it is (`prefix + A`). While the app runs on this machine, the popup lists the machines' rows too, read only, and Enter on a pane or a session of a connected machine is the doorway: it takes you into that machine through the app, at that row. A closed worktree on a machine is acted on from the app, and the popup says so. With no app running, the popup lists this machine's sessions, and each machine's header says `not connected: run taw app outside tmux`: the popup lists another machine only through a running app, and `taw app` typed inside tmux opens the popup instead. Outside tmux `taw` is on your `PATH` only through the one link on the [install page](install.md).

While the cursor is on a row of a connected machine, the popup's hint line says what Enter does there: `Enter go to studio (Q there brings you back)`, and `?` says it in the line under Enter's: `on another machine's row it hands this terminal to the app; Q there brings you back`. The key named is the one that ends the app there at once: `Q`, or `Ctrl-C` in the search preset or when `[keys]` gives `Q` to another action. On this machine's own rows the line stays `Enter go`, and the app, outside tmux, says `Enter go in` on every row.

## Setting a machine up

1. Install taw on each machine, as you did here (`prefix + I` there, or the release binary): the app finds what `prefix + I` installed with no `--taw`. The version there must speak this one's protocol (protocol 1): a machine that is older or newer says so in its header. Keep taw at the same version on every machine: a machine whose taw is older can drop a toast forwarded through it without a word.
2. Add each machine with `taw machine add <name> <ssh-host>`: a name you choose and what you would type after `ssh`:

    ```sh
    taw machine add studio studio.local
    taw machine add pi pi
    ```

    The command writes the machine into `config.toml`, under `[machines]` (a new table at the end of the file when there is none, and a new file when there is no config yet), and keeps every comment and line of the file as it was. It refuses a name that is already listed, and changes nothing when it refuses. `taw machine remove pi` takes that machine out again: its line, or its `[machines.pi]` table, and nothing else. A running app picks a change up within a couple of seconds, with no restart: a machine added shows its sessions, one removed loses its rows and its ssh ends, and the machines that stay keep their connections. A change the app cannot read (a broken file, a line taw drops) is said once, and the app goes on with the machines it has until the file is fixed. The file can be edited by hand as well, one line each:

    ```toml
    [machines]
    studio = "studio.local"
    pi = "pi"
    ```

    With no `--taw`, the app looks for taw on that machine in this order: `taw` on the `PATH` that ssh gives a command there; the link the plugin keeps in its state folder while `@agent-wizard-path` is on, the default (`bin/taw` under `$XDG_STATE_HOME/tmux-agent-wizard` when `XDG_STATE_HOME` is an absolute path, then under `~/.local/state/tmux-agent-wizard`); the binary `prefix + I` puts in the plugin's folder (`~/.tmux/plugins/tmux-agent-wizard/bin/taw`, then `tmux/plugins/tmux-agent-wizard/bin/taw` under `$XDG_CONFIG_HOME` when `XDG_CONFIG_HOME` is an absolute path, then under `~/.config`); then `~/.local/bin/taw`, `~/.cargo/bin/taw`, `/opt/homebrew/bin/taw` and `/usr/local/bin/taw`. Only when taw is in none of those, add `--taw ~/.local/bin/taw` (`~/` is that machine's home): `taw machine add pi pi --taw ~/.local/bin/taw` writes the long form, `[machines.pi]` with `ssh = "pi"` and `taw = "~/.local/bin/taw"`. A name is lowercase letters, digits, `-` and `_`, starts with a letter and has at most 32 characters. With no `[machines]` taw behaves as it always did.

3. Make `ssh <destination>` work in a terminal without asking anything: the key in ssh-agent (`ssh-add`), and the host key accepted once by running `ssh <destination>` and answering yes. taw never answers a prompt; a machine that would ask is not connected, and its header says what to do.

4. Run `taw machine check`: it tries each machine once and says, in one line each, what it found. `--json` gives the envelope `cli:machine:check`. `taw machine list` shows what the app last heard, with no ssh.

Nothing is copied between machines: the app lists and shows, and taw on each machine does the work there.

## The app's screen

```
 taw app  ·  3 machines  ·  6 sessions  ·  7 agents  ·  ⏸2 need you      ⏸2 ⏵4 ✓1  │  14:02
 this machine  ⏸1 ⏵1
   (its sessions and panes, as the popup draws them)
 studio  ⏸1 ⏵2 ✓1
   (studio's sessions and panes)
 pi  ⏵1
   (pi's sessions and panes)
```

The top line says how many machines the list holds (this machine counted), then the sessions and agents, then who needs you. Each header carries the machine's state counts while it is connected and lists sessions, and its own words when it is not (the table below). A row of a machine that is not connected stays in the list, dim, after every connected row: its state colours are gone, it counts for nothing in the numbers, and its preview says `no preview` and why. A machine that drops while you look says so once in the top line. A machine that is away shows its last words and when it was last seen.

The numbers of a session count across machines, and a machine that appears or drops never moves a number of this machine's. A remote row's id is `studio:%16`, the machine and that machine's own tmux id, so it can never be one of this machine's.

## Going in, and the key back

Enter on a pane goes in at that pane, on a session header at the session. The terminal is lent to that machine's own tmux through ssh, so inside it its keys are its own: its prefix, its popup. `prefix + A` there (that machine's popup key, as its plugin binds it) is the key back: the list returns with the cursor on the pane you were in. `Esc` on the list then goes back into the machine at the pane you came from. No key of a machine's is forwarded into the app: the machine's own tmux has the terminal, and its keys are its own.

An attach that fails does not lose its words. The app does not draw the list over what the far side printed (tmux's `missing or unsuitable terminal: ...`, ssh's `Connection refused`): it prints one line under them, `taw: <what went wrong>. Press Enter to go back to the list.`, and waits. Enter takes the terminal back and draws the list, with the same toast as before. The wait ends by itself when the terminal closes or the app is told to end, and an attach that ended well holds nothing.

Every attach the app makes shows, for 4 s on the status line of that machine's tmux, how to get back: `C-b + A: back to the taw list`, with that machine's own prefix and its own popup key in place of `C-b` and `A`. tmux draws it there, so it needs no answer from the machine, and a key you changed on that machine is the one it names. `prefix + A` there needs taw's plugin loaded in that machine's tmux (`prefix + I` there, or the plugin line in its tmux.conf and a tmux started after it): where it is not, as with only the release binary there, the words say `C-b + d: back to the taw list`, since detaching from that machine's tmux always brings the list back.

A connected machine whose tmux has no session yet is not an empty gap. Under its header it has one dim row, `no tmux session here: Enter starts one`, in the app and in the popup. It takes a number from the same 1 to 9 as the session rows, at its place in the list, while one is left, so each session of a machine after it numbers one further on; that number key puts the cursor on it, as a session's number does. The number stays with the machine while it connects, drops or comes back, so no other row's number moves. `Tab` passes it by, and the cursor stops on it with `j` and `k`. Enter on it starts a session on that machine and goes in: the app runs `taw machine attach --new` there, which starts tmux with a session in that machine's home folder and attaches to it; the popup goes through the doorway as `taw app --go studio:new`. The machine's taw needs a terminal for that, so `taw machine attach --new` run without one says so and ends with status 2. When the last session of a machine ends while the list is open, a toast says `studio has no tmux session now: Enter on its row starts one`.

A terminal type the machine does not know is not a reason for the attach to fail. Ghostty, Kitty, WezTerm and Alacritty have terminfo entries that a fresh Linux machine often lacks, and the machine's tmux then refuses with `missing or unsuitable terminal`. The attach looks the terminal's `TERM` up in that machine's terminfo folders (`$TERMINFO`, `~/.terminfo`, `$TERMINFO_DIRS`, then the system folders) and runs the machine's tmux with `TERM=xterm-256color` when the entry is not there, or when `TERM` is unset or empty. A terminal the machine knows keeps its own `TERM`.

Through the popup's doorway (see "Two ways in") the terminal runs a second app, which follows the one that keeps the connections. Inside the machine `prefix + A` brings you back to that second app's list all the same: the key goes back to whichever app attached the client, the app you started, a doorway app, an app in another terminal, or an app you reopened a moment after the last one quit. Detaching from that machine's tmux also returns to the list.

The terminal that ran tmux is not lost when that second app ends. The doorway tells the app which session the popup was opened from (`taw app --back '$3'`, a flag you do not type yourself), and when the app ends, by `Q`, `Ctrl-C` or `Esc` with nowhere to go back to, the terminal becomes a tmux client again on that session, on the same tmux server. If that session is gone, it attaches to the session tmux picks, the most recent one. If the tmux server is gone, the app ends as an app does. A closed terminal or a signal that ends the app is not a reason to attach anything. In such an app the hint line and the help say `Q back to tmux`, where an app started by hand says `Q quit`.

Within one machine you move as tmux moves you. Between machines you go back to the list and go in again, or let next up take you: one machine's tmux has the terminal at a time. Two terminals in one session of a machine share its window, as any two tmux clients do: both show the same window, which tmux sizes by default to the terminal used last. `Esc` and `q` go back, `Q` and `Ctrl-C` quit the app, or go back to tmux in an app the doorway started (`Esc` first leaves a filter or an overlay). In such an app every word that would say quit says `back to tmux`: the hint line, the help's `Esc` line, its `Q` and the `q` of its options line. A plain `taw app` keeps saying quit.

The connection to a machine's tmux ends with the app, however the app ends: even when `taw app` and its supervisor are both killed with `kill -9`, the ssh that carried you in is hung up within two seconds and does not stay beside your shell. Killing only the app's child leaves you where you are, as before: the supervisor waits for the attach and starts the list again when you detach.

Next up (see "Next up") spans every machine in the app. `Tab` on the list walks the agents of every connected machine in the order drawn, this machine first. `prefix + O` on the list goes, as Enter on its row would, to the agent after the pane you last came out of, whatever a filter hides from `Tab`. In a tmux the app put you in, on this machine or another, `prefix + O` goes to the next agent of that walk: tmux moves you when it is in the same tmux, and the app takes you there at once, with no list in between, when it is on another machine.

`taw app --go last` opens on the row you were last in, `--go %7` on a pane of this machine and `--go <machine>:<id>` on one of a machine's ids, as in `--go studio:%16`, and `--go <machine>:new` on a machine that has no tmux session, as in `--go studio:new`, which starts one there; anything else is refused with the usage line in the table below. `taw app --go studio:%16` waits up to 10 s for studio's first list before it goes in, so a connected machine that sends none leaves the screen blank that long. The terminal's own line is `taw app` alone.

## Acting on a machine's rows

On a connected machine's row, `x`, `r` and `w` work as on a row of this machine's, and the verb runs on that machine: its own `[hooks]` run there, with its own environment, not this machine's. The confirmation names the machine, as in `kill-pane studio · garden:1 (claude)?`. A hook there that refuses is a toast and nothing closes: `pre-close hook on studio: uncommitted changes in ~/code/garden`. A worktree with changes (the machine answers `worktree_dirty`) asks `git will not remove <branch>: it has modified or untracked files. remove anyway? uncommitted changes are lost`, and `y` removes it with `--force`. `w` asks for the branch as here, then says `opening worktree on studio` and goes in; if the cursor moved while the machine was making it, the toast `worktree made on studio: Enter on its row goes in` says where it is. A removed worktree says `removed worktree w4 on studio`. Closing a session that has worktree sessions open asks the question that names them, as on this machine, with the machine named: `kill-session studio · garden? It has 2 worktree sessions open (garden--rain, garden--pdf). Close them too?`; `y` closes them all there, and the toast then says how many went: `closed studio · garden and its 2 worktree sessions`. When the list has not caught up with the machine yet, the question names no count.

After 2 s the hint line says `waiting for studio`; with no answer in 20 s a toast says `no answer from studio in 20 s; it may still have run`; a second action on that machine while one runs says `studio is still answering the last action`; when ssh could not reach it, `studio did not answer; look again before trying once more`; any other error is `studio: <its message>`; a row that went meanwhile is `studio: that row is gone`. An answer that comes after the list moved on asks nothing and says `studio answered after the list changed; look again before trying once more`. A row of a machine that is not connected answers every key, Enter included, with `pi is not connected (last seen 3 min ago)`. Each action is also one command with `taw --machine studio pane close %16` (see below).

## Pasting an image

`ctrl-v` on an agent's row in the default preset (listed in `?`), or `paste image` in the right-click menu of an agent's row, which is the search preset's way to it, in the popup or the app, or `taw image paste --pane %16 [--machine studio]`, reads the image on this machine's clipboard (`osascript` on macOS, `wl-paste` or `xclip` on Linux, each given 10 seconds to answer; a Linux machine with neither `WAYLAND_DISPLAY` nor `DISPLAY` set says there is no clipboard to read there) and delivers it to that agent, on another machine too: the bytes go over that machine's ssh to `taw image receive --bytes <n>` there, which writes and pastes only when exactly `n` bytes arrive, so a send that is cut leaves no part of an image behind. The image is written into taw's state folder there (`images/`, readable by you alone; each paste removes the files taw wrote there, `image-<digits>.png` and `image-<digits>-<digits>.png`, once they are 7 days old, and leaves any other file of yours alone; an `images` that is a symbolic link is refused) and its path is pasted into the pane as a bracketed paste, with a backslash before each space the path holds, as a terminal writes a dragged file. Claude Code and Codex attach an image whose path arrives that way; another agent gets the path typed into its prompt, to open. Each paste runs as a process of its own, `taw image paste`, in a session of its own, so the popup and the app keep drawing and taking keys while it does, and leaving the screen is at once: the paste finishes on its own within its bounds (10 seconds for the clipboard tool, 30 for the send to a machine). While the screen is there, the toast says where it went when the paste ends, and a second `ctrl-v` meanwhile says one is under way. A paste that fails after you have left the popup is shown on the tmux client the popup was on; the app outside tmux has no such client, so a failure after you leave it is not shown. A row that is not an agent says so, a machine that is away says it is away, an agent that is waiting on a question (blocked, here or on a machine) says to answer it, then paste the image (a working or an idle agent takes it), and a machine whose taw has no `image receive` says its taw is too old. `taw image paste` and `taw image receive` paste into any pane they are given, a shell's too; only the list limits the paste to an agent's row.

## Notifications and the lease

With no other machine listed, nothing names this machine: its toasts carry no machine name, and the new session picker is titled `new session` alone; once another machine is listed its title says it is for this machine's own session.

A toast about an agent on a machine goes to where you are. In the list it is the top line, `studio: <what its watcher said>`. Inside studio's tmux it is not drawn again: studio's own tmux shows it. Inside another machine's tmux the app has taw there draw it, through ssh, with the machine's name ahead of it (a toast about an agent on this machine is named by this machine's short host name there, since `this machine` would read as that machine). It follows this machine's `[ui.toast] delivery` as a local one does, whatever that machine's own setting is.

A hook that runs for it reads `TAW_MACHINE`, the name under `[machines]`; `TAW_PANE` and the session variables are empty, because a pane id of another machine means nothing here (`taw hooks test notification --machine studio` tries one). The sound is this machine's own `[ui.sound]`, played once.

Only one app speaks a machine's toast: the one that holds the lease, which each connected machine is told of every few seconds. A second app on this machine shows the toasts of this machine where its own person is, and none of a machine's. An app that ends gives its lease up, however it ends, so an app you open next takes it at once, and one already running takes it with its next lease line, a few seconds later. The sound and the push of a toast that arrives while a lease lapses (15 s) can be missed.

## The phone

The phone is `[hooks]` and ntfy, as in "Notifications on your phone", and it follows its own setting. A machine's toast reaches it once, from the app that holds the lease, never from each app that sees it.

## Running a command on a machine

`taw --machine studio agent list` runs `agent list` on studio, over ssh, and prints its answer as it was given. `--machine` goes before the command, `--pretty` in either order. The ids are that machine's: `w1`, `w1:p2` and `%16` name studio's panes, not this machine's.

- Five verbs belong to this machine and are refused on stderr, status 2, with no envelope: `ui` (the popup is that machine's own; use `taw app`), `app`, `machine`, `agent attach` (use `taw app --go studio:<target>`), and `open` without `--json`.
- A forwarded verb waits as long as the far command runs, as a local one does: there is no time limit, and `Ctrl-C` ends it, so `agent wait` and `agent prompt --wait` can run long.
- `machine_unreachable` is the error code when taw could not get an answer: ssh could not log in or run, no taw is there, or the far taw ended by a signal (ssh reports 255 for that, so the message may be ssh's own last line, or `ssh exited 255 without saying why`). Nothing is retried.
- The far command line is quoted for a POSIX shell and run as `sh -c '<line>' taw <args>`. A login shell that is not sh-like (fish, csh) reads that the same except for an argument that holds a backslash, which it changes: give the machine an sh-like login shell if a `taw --machine` text holds backslashes.

## What a header says

A header says one of these. The first group is what the machine's header and `taw machine check` show; the second is what a row or a toast says. The samples are `studio`, `pi`, an age of 3 minutes and a retry of 30 seconds, and a version number, written 0.1.0 here.

| Says | Means |
|---|---|
| `connected: taw 0.1.0` | the feed runs and the taw there is this protocol |
| `not answering` | `taw machine check` could not reach it in time |
| `not answering, last seen 3m ago, trying again in 30s` | it was heard before; taw tries again by itself |
| `not answering, trying again in 30s` | it has not been heard since the app started |
| `connecting, last seen 3m ago` | the first answer has not come yet; it was heard before |
| `connecting` | the same, never heard |
| `no taw there: install it as you did here` | ssh worked and there is no taw to run |
| `runs a taw that has no machines: update it there` | the taw there is too old to feed a list |
| `runs taw 0.1.0, which has no machines: update it there` | the same, with its version |
| `runs taw 9.0.0, which no longer speaks to this one (0.1.0): update taw here` | the taw there is newer than this protocol |
| `ssh could not log in without asking: add your key to ssh-agent (ssh-add)` | a prompt or a refused key; taw never answers a prompt |
| `its host key is not known here: run ssh studio once in a terminal and accept it` | the host key step of setting up |
| `sent something taw does not read` | after the machine's name: a line of its feed was not one of taw's |
| `no tmux session here: Enter starts one` | the dim row under a connected machine that has no tmux session: Enter starts one there |
| `studio has no tmux session now: Enter on its row starts one` | the toast when the last session of a connected machine ended |
| `C-b + A: back to the taw list` | on the status line of the machine's own tmux for 4 s after an attach, with that machine's prefix and popup key; `C-b + d` where taw's plugin is not loaded in that tmux |
| `Q back to tmux` | the hint line and the help of an app the doorway started: `Q` and `Ctrl-C` put the terminal back in the tmux session it came from |
| `not connected: run taw app outside tmux` | the popup's header of a machine while no app runs on this machine: the popup knows nothing of it but its name (and, from an earlier app run, whether it is this machine's own tmux server, which then draws as with an app), and the top line does not count it |
| `go to studio (Q there brings you back)` | what the popup's hint line says after `Enter` while the cursor is on a row of a connected machine |
| `on another machine's row it hands this terminal to the app; Q there brings you back` | the line of `?` under Enter's, in the popup, while machines are listed |
| `studio is this tmux server` | the name is this machine's own tmux; it is not listed twice, and when it is the only machine listed no header is drawn |
| ` (README, "Remote machines"); it then connects by itself` | the end of a `taw machine check` line that needs you |
| `3 machines` | the top line: this machine counted, all connected |
| `3 machines, pi offline` | the top line when pi is not connected |
| `pi is not connected (last seen 3 min ago)` | a key on a row of a machine that is not connected |
| `no preview: pi is not connected` | the preview of such a row |
| `(3m ago)` | the age of the preview's last capture |
| `pi dropped off (last seen just now)` | the toast when a connected machine goes |
| `studio's rows are acted on from the app: Enter goes there` | a key in the popup on a connected machine's row |
| `studio's rows are acted on from the app` | Enter in the popup on a machine's closed worktree |
| `waiting for studio` | the hint line, after 2 s, while an action on that machine has no answer |
| `studio did not answer; look again before trying once more` | ssh could not reach the machine for an action |
| `pre-close hook on studio: uncommitted changes in ~/code/garden` | a hook there refused: a toast, and nothing closes |
| `studio is still answering the last action` | a second action on that machine while one runs: nothing is sent |
| `no answer from studio in 20 s; it may still have run` | an action had no answer within 20 s: a toast |
| `studio: that row is gone` | the machine says the row is not there any more: a toast |
| `studio: pane '%9' not found` | any other error the machine's taw gave for an action, in its own words |
| `studio answered after the list changed; look again before trying once more` | an answer that came after the cursor moved or an overlay opened: it asks nothing |
| `studio: that row has no id taw can send` | a key on a row that is not an id of that machine |
| `studio · garden:1` | the target in a confirmation, as in `kill-pane studio · garden:1 (claude)?` |
| `kill-session studio · garden? It has 2 worktree sessions open (garden--rain, garden--pdf). Close them too?` | closing a session that has worktree sessions open: the question names them and their count, as on this machine; only `y` closes them |
| `kill-session studio · garden? It has worktree sessions open. Close them too?` | the same, when the list had not caught up with the machine (the machine answered that worktree sessions are open before the list showed them); only `y` closes them |
| `kill-pane studio · garden:1 (claude)? It is the session's last pane, so the session closes with it. It has 2 worktree sessions open (garden--rain, garden--pdf). Close them too?` | the last pane of a session that has worktree sessions open: the same question in the pane's words, since closing the pane closes the session; only `y` closes them |
| `closed studio · garden and its 2 worktree sessions` | the toast after `y` to that question: the count is what the machine closed |
| `removed worktree w4 on studio` | the toast after a worktree was removed there, naming its branch when the machine said it |
| `worktree made on studio: Enter on its row goes in` | a worktree made there after the cursor had moved: its session is not entered |

And what the app says when going in does not work:

| Says | Means |
|---|---|
| `studio:%16 is gone` | the row went before Enter did |
| `no machine called pi` | the name is not under `[machines]` |
| `--go takes last, a pane of this machine as in %7, a machine and one of its ids as in studio:%16, or a machine and new as in studio:new` | `--go` was given something else |
| `nothing to go in to` | the row is not a place |
| `could not tell which tmux server this is` | the popup could not find its own server |
| `studio did not answer (ssh ended with status 255)` | ssh could not reach it |
| `studio refused the attach: the pane or session may be gone, or the taw there is older and does not know machine attach` | the machine's taw answered with its usage error |
| `the attach to studio ended with status 7` | any other status |
| `studio did not answer (ssh was ended by signal 9)` | ssh was killed |
| `studio did not answer (ssh was stopped)` | ssh ended with neither |
| `studio: ssh could not be run (no such file)` | ssh itself is missing |
| `taw: the attach to studio ended with status 7. Press Enter to go back to the list.` | printed on the plain terminal under the machine's own words when an attach failed; Enter draws the list, with the first sentence as its toast |
| `opening worktree on studio` | going in on a closed worktree has begun |
| `studio did not open the worktree within 20 s; ssh was ended` | it took too long |
| `studio could not open the worktree: no such worktree` | the machine refused, with its reason |
| `studio answered the worktree open with something taw does not read` | its answer was not an envelope |
