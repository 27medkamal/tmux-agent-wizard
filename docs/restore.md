---
title: Save and restore
description: Save and restore tmux sessions, layouts and folders, and 18 coding agents in their own conversations once each one's hook is installed. Next to tmux-resurrect.
---

# Save and restore

Save and restore, after a reboot or a tmux server crash: 18 agents (Claude Code, Codex and OpenCode among them) reopen in their own conversations once the agent's hook is installed (one command).

taw saves on its own, a few seconds after each change (a new window, a split, a pane or session
closing, an agent changing state), and keeps a dated copy of the layout at most every 15 minutes.
`taw session restore` brings back your sessions, windows, layouts and folders, and the agents in
them.

| | taw | tmux-resurrect with tmux-continuum |
|---|---|---|
| Saves | ✅ on its own, after each change, plus a copy at most every 15 minutes | ✅ every 15 minutes (continuum), or on `prefix + Ctrl-s` |
| Sessions, windows, layouts, folders | ✅ | ✅ |
| Agents back in their conversations | ✅ 18 agents, once the agent's hook is installed | ❌ at most the agent's program again, not its conversation |
| Restores when tmux starts | ❌ you run `taw session restore`, in tmux or in a terminal before tmux starts | ✅ with `@continuum-restore 'on'` |
| Other programs, such as vim, less or htop | ❌ their panes come back as shells | ✅ from a list you can extend (vim sessions too, with a vim plugin) |
| What was on each pane's screen | ⚠️ opt-in, `[experimental] pane_history` | ⚠️ opt-in, `@resurrect-capture-pane-contents` |

So you do not need tmux-resurrect unless you want it to start programs such as vim again. If you
keep it, one line runs taw's restore after resurrect's own:

```tmux
set -g @resurrect-hook-post-restore-all 'taw session restore'
```

```sh
taw session save [--name default]      # snapshot every session/window/pane to <state>/sessions/<name>.json
taw session restore [--name <name>] [--dry-run]
taw session list
taw session delete <name>
```

`delete` needs a saved session's name exactly as `list` shows it, letter case included,
so `Default` does not delete `default`, even on a case-insensitive disk (macOS's default).
A name that starts with `-` goes after `--`: `taw session delete -- -x`.
A save taken while an agent is suspended with Ctrl+Z keeps it with its session, so a restore
resumes that conversation.

Restore recreates whatever the current server is missing: a saved session that is not open is
made whole, and in one that is open each window with an agent that runs nowhere in the server is
made again at the end of it, with nothing else of the open session changed (below). With no
tmux server running it starts one. With no `--name` it reads the layout saved as `default`, and
when that has nothing to make (a tmux server that started after the last one died has autosaved
its own layout over it) it goes back to the newest layout taken before this server started: see
"After a tmux server dies" below. `--name <name>` is exactly that file, and `--name default` exactly
`default.json`. It then, when `[session] resume_agents_on_restore` is `true`
(default) and a saved pane has a native session id, which the agent's own hook reports
once it is installed (see "Integrations"), relaunches the agent into its own conversation:

| Kind | Resume command |
|---|---|
| `claude` | `claude --resume <id>` |
| `codex` | `codex resume <id>` |
| `copilot` | `copilot --resume=<id>` |
| `cursor` | `cursor-agent --resume <id>` |
| `devin` | `devin --resume <id>` |
| `droid` | `droid --resume <id>` |
| `grok` | `grok --resume <id>` |
| `hermes` | `hermes --resume <id>` |
| `qodercli` | `qodercli --resume <id>` |
| `qwen` | `qwen --resume <id>` |
| `omp` | `omp --resume=<id>` |
| `kilo` | `kilo --session <id>` |
| `kimi` | `kimi --session <id>` |
| `opencode` | `opencode --session <id>` |
| `pi` | `pi --session <id>` |
| `mastracode` | `mastracode --thread <id>` |
| `agy` | `agy --conversation <id>` |
| `letta` | `letta --conversation <id>`, or `letta --conversation default --agent <agent-id>` for Letta's own default conversation (encoded `default:<agent-id>`) |

Every other kind restores as a plain shell, not resumed.

**A Claude pane comes back under the profile it ran under.** Claude reports the path of its
transcript, `<profile>/projects/<project>/<id>.jsonl`, and the folder above `projects/` is the
profile (`CLAUDE_CONFIG_DIR`) the pane's Claude ran under. For any profile but the default,
`$HOME/.claude`, the restore types it before the command:
`CLAUDE_CONFIG_DIR=/home/you/.claude-alt claude --resume <id>`, the profile written in single quotes
when it holds a space or any character a shell reads (the way every word of the line is). A pane
that ran on the default profile gets the bare `claude --resume <id>`, as before, and so does one with
no saved path, a relative one, or one of another shape. The project folder plays no part: it can
differ from the pane's folder. Without this a pane that ran under another profile came back logged
out. Only Claude is given this: Codex's hook reports no home, so a Codex pane is typed what it
always was, and a sandboxed pane comes back through its sandbox profile. `taw session restore
--dry-run` lists the exact line it would type in each pane that would be resumed (`window 0 'zsh',
pane 1: would type: CLAUDE_CONFIG_DIR=/home/you/.claude-alt claude --resume <id>`, and the same in
the result's `would_type`), with the reason for a pane it would not type.

**A Claude pane comes back with the flags it was started with.** You may start Claude as `claude
--effort max`, and a restore that typed only `claude --resume <id>` brought it back with the default
effort. When Claude's `SessionStart` hook reports its session, taw reads the exact arguments of the
Claude process in front of the pane and keeps the few flags that set how the session runs:
`--effort`, `--model`, `--permission-mode` and `--agent`, each with its value, and
`--dangerously-skip-permissions`. Both `--flag value` and `--flag=value` are read, and written back
as `--flag value`, in the order you gave them. The restore types them after the command, with the
profile before it: `CLAUDE_CONFIG_DIR=/home/you/.claude-alt claude --effort max --model opus --resume
<id>`. Nothing else is kept: never a prompt, never print mode, never a flag that chooses a session
(`--resume`, `--continue`, `--session-id`), never any other flag, because arguments can carry
credentials, so everything outside the list is dropped unread and nothing is logged. A flag with no
value is dropped, reading stops at `--`, and when the arguments could not be read exactly (a system
that only gives the line `ps` prints) nothing is kept, so a line is never guessed at. After a restore
the running Claude has `--resume <id>` in its own arguments, which is never kept, so a second crash
brings back the same line. A pane saved without these flags, or one whose Claude was not seen, gets
the line without them. Only Claude is given this, and a sandboxed pane comes back through its sandbox
profile. A Claude pane outside a sandbox comes back at the effort the session's last turn ran at,
including one set inside the session with `/effort`, which is on no command line: taw reads it from
the session's own file (the last 2 MiB of the transcript its Claude reported, the two effort fields
of the turn records, nothing else kept, nothing logged). With no file or no field, the line is as
before. The dry run shows the flags in its line.
The flags are in the snapshot as `agent_claude_flags` (older snapshots have none) and on the pane as
`@agent_claude_flags`.

An agent can also report, itself, the command that resumes its session: `taw pane report-agent
... -- <command...>`, from its own code or its own hook (see "Report an agent of your own"). A
restore types that command in place of the table's for that agent, at the shell of the pane it
restored in the directory the pane was saved in, and quoted so that the shell runs each word as it
was reported: a word made of letters, digits and `_ - . / : @ % +` as it is, any other in single
quotes (a word with `=` in it too). The pane keeps the command for as long as the agent and the source
that reported it hold the pane, and a save keeps it with the pane, as `agent_resume`; the restore
puts it back on the pane it types it into. A snapshot written before there was one loads as having
none. `[session] resume_agents_on_restore = false` types neither command.

A restore resumes each session once, in the first pane that saved it. A session saved in two panes
(the same conversation open in two panes, say) is typed into the first, and the plan says why not
into the second: `skip resume in pane %14: pane %12 resumes the same
session`. A command an agent reported is the same session only in the same directory, so
`my-agent --continue` saved in two projects is two sessions, and both are resumed.

`pi` and `omp` usually report the path of their session file and no id. A pane that
reported only a path is resumed from it, in the same place (`pi --session <path>`,
`omp --resume=<path>`), and the restored pane holds that path, so that what the restore
saves still says which conversation it is.

Successive resumes are spaced `[session] startup_per_agent_delay_ms` apart (default
100ms, 0 disables it), so a session with several resumable agents does not launch
them all in the same instant.

Once every command is typed, restore records the pid of the process each one started
(`@agent_pid`), as `agent start` does: the newest child of the pane's shell that is in
front of the pane's terminal, looked for over two seconds for all of them together (three
times at the least however long a look takes on a busy machine, and past the two seconds
until two looks agree or until the sixth), and taken from the last two looks, which have to
agree. A shell that runs a helper before every command (the preexec hooks of atuin,
starship and bash-preexec) starts the helper first, and the helper has ended by then.
That pid is what says the agent is in front of a pane that only its hint names, so
`taw agent prompt` does not type into what you start after the agent has exited.

A resume command is typed only once the pane's shell is at its prompt. A shell that is
still starting hands what is typed to whatever reads the terminal first, and a question
its startup asks (mise asks "Trust them? Yes No All" when a folder's config is new to it)
would be answered by the `a` in `claude`. So restore looks at each new pane first: the
shell has to be the only thing in front of the terminal, it has to have drawn something,
and its screen has to have stayed as it is for 300ms. A pane already at its prompt is
typed into at once; one that comes to it is typed into as it does, while the others are
still waited for. A shell that has not come to its prompt within 15 seconds of its pane
being made is left alone: nothing is typed, the pane is not marked as holding an agent,
and the plan says so in plain words (`pane %12 did not reach its prompt; resume it with:
claude --resume <id>`), with the pane's id in `not_resumed` in the result. A pane closed
before its turn is reported the same way, and so is one that takes no input, in copy mode or with
its input off (`pane %12 is in copy mode; resume it with: claude --resume <id>`), at once.

The command goes to the pane as text, and its Enter after it, each only while the shell the look
found at its prompt is still in front of the pane and the pane takes input, which tmux is asked in
the call that sends. As text, a command of one word that tmux reads as the name of a key (`Up`,
`Home`, `F1`) is typed and not pressed. And a program started in the pane after the look (the
spacing between resumes gives it time) is sent nothing: the pane is left alone, as above, and the
plan says so (`pane %12 had another program in front of its shell when its command was to be typed,
so nothing was typed; resume it with: claude --resume <id>`). One that comes to the front between
two pieces of a long command, or before its Enter, is sent nothing more, and the plan says that what
was typed may be left on the shell's line (`pane %12 had another program in front of its shell when
its command was to be run, and the command typed into it may be left on its line; resume it with:
claude --resume <id>`).

What is never typed into, because taw cannot tell it from a shell that is not ready, or
cannot tell at all: a pane whose process is not itself a shell (an ssh pane, a `docker
exec` pane, a wrapper that starts the shell without replacing itself); a shell that draws
nothing, such as one with an empty prompt (`PS1=''`) in a pane that is still blank; and a
question the shell asks itself, with a `read` of its own, which looks like a prompt (zsh's
question about insecure directories from `compinit` and the oh-my-zsh update prompt are
two). Only a program the shell runs in front of it is told from a prompt. The look runs
`ps -o pgid=,tpgid=,comm=` and `pgrep -g <group>` (the members of the terminal's foreground group; on macOS with `-a`, which makes its `pgrep` list a process's own ancestors, which it leaves out by default), which macOS and Linux (procps) have; a `ps`
with no `tpgid` column cannot tell, and then nothing is typed. In each case the plan says
what to run by hand.

A pane whose saved directory is gone (removed or renamed, or on a disk that is not mounted)
is still made, since tmux opens it in your home folder and taw cannot stop that, but nothing is
typed into it, so that an agent's conversation is not resumed in a directory that is not its
own. The plan says which pane and which directory, and for an agent that was to be resumed, the
command to run once the directory is back, with the pane's id in `not_resumed`: `pane %12: saved
directory /home/you/api is gone, so tmux opened it in your home folder and nothing was typed; once
the directory is back, resume it with: cd /home/you/api && claude --resume <id>`. A pane that
tmux opens somewhere else, though its saved directory is there, is left alone like a gone one, and
the plan says where tmux opened it: a directory you have no permission to go into opens in your
home folder (`pane %12: tmux opened it in your home folder and not in its saved directory
/home/you/api, so nothing was typed; resume it with: cd /home/you/api && claude --resume <id>`).
The restore looks at each pane's folder as it makes the pane, and asks tmux which folder it read,
and goes by both. The pane carries herdr's words for it, `Saved directory is unavailable. Restore the
directory and restart this session.`, which `taw pane get` and `pane list` show as
`restore_error` (`null` on every other pane), and with `[experimental] pane_history` it is given no
saved screen either. Every save after that keeps the pane as it was saved, its directory, its
agent and session and the command its agent reported, and not as tmux opened it, as herdr keeps
such a pane: an autosave does not forget the conversation, and a restore after tmux's next start
resumes it there once the directory is back. Only the pane's place in its window, and whether it
is the active pane, are saved as they are now. A pane whose agent ran in a container keeps
where it ran beside its session. If `taw sandbox` started it, that is its profile, and the
restore resumes it through the profile once the directory is back. If you typed the container,
that is its runtime, and the restore leaves it a plain shell, its conversation not resumed.
All of this lasts until you close the pane, or until it
has an agent of its own: one taw names in front of it, one `taw agent start` starts in it, or
any agent's report of its state or session. Then the error goes, and the pane is saved as it is,
with that agent, in the folder it is in, so that its conversation is not written over by the old
one. Its shell going to another folder, or a program that is no agent, ends nothing. A pane split
from it is saved as it is.

A folder whose name holds a `#`, which tmux reads as the start of a format, opens as itself:
`C#Sharp` opened in `C<the session's name>harp`, or in your home folder, and the restore typed
its agent's resume there. So does a folder whose name ends in `;`, where tmux ends a command (the
restore stopped with `unknown command`). The same goes wherever taw has tmux open a folder:
`worktree create` and `open`, `session open`, `pane split --cwd`, `pane move --new-workspace`,
`layout apply`, `workspace create --cwd`, `tab create --cwd`, and a plugin's panes and popups.
tmux reads the name of a session or window it makes or renames as a format too: left as it
is, a session saved as `C#Sharp` would come back as `Charp`, a window `W#Sx` as `W<the
session's name>x`, and the next restore would not find the session and make it again. Every
name taw gives tmux is the name it gets, a `#` or a `;` at its end included: a restore's sessions and windows, `session
open`, `worktree create` and `open` (a repository in a folder whose name holds a `#`),
`workspace create --label` and `rename`, `tab create --label` and `rename`, `pane move` with
`--new-workspace` or `--new-tab` and its labels, and the popup's renames.

A pane whose shell ends while its session is still being made does not stop the restore.
The plan says which window came back with fewer panes than were saved, or not at all, and
a session that closed under the restore is given up on, with the windows it made and the
ones it did not, while the sessions after it are still made. Every line of the plan that
names a window names it by the number it had when the layout was saved and by its name
(`window 1 'zsh'`), because names repeat, and the restore numbers the windows it makes
afresh. The agents of the windows
that were made are typed all the same, and so are those of the sessions made before a
later one failed: the restore types what it had planned, and then gives the error back,
with a line after its message for each pane it left alone and the command to run in it
(`pane %12 did not reach its prompt; resume it with: claude --resume <id>`), so that no
command is lost with the plan that would have held it.

Autosave is held off while a restore runs. The restore sets the server option
`@agent_wizard_restoring` to its process id and the time it began, and clears it when it
ends. A restore that is killed (Ctrl-C, a closed terminal) cannot clear it, so autosave
goes on once that process is gone, and ignores a mark ten minutes old, in case the process
id has been given to another process since. The restore saves once when it is done, if an
autosave was asked for while it ran (the hooks ask on every new window and split), with
the agents it resumed in the saved layout. It does not when it left a pane alone: the
restore itself does not save over the snapshot it was restoring from, which still holds
that pane's agent, but the next autosave records the layout as it is by then, with no
agent in that pane (unless its saved directory was gone, as above), so the plan's `resume it
with` line is the record to keep. Without the
hooks, nothing asks, and a restore never rewrites the snapshot autosave keeps. Holding
autosave off keeps the hooks from writing a half-made layout over the snapshot the restore
is reading.

**A session that is open.** A saved session that is open is not skipped whole. Each of its saved
windows that holds at least one agent pane whose conversation runs in no pane of the server is made
again as a new window after the open session's last window (never into a number a window you closed
left free between yours), with its saved name, layout, panes and their directories, and its agents
are resumed as in any restore (profile, flags, waits for the prompt, autosave held off). A
conversation runs in a pane when that pane holds what the restore would resume it by: the same
agent kind and session id, or, for an agent that reports its own resume command and has no id, the
same reported command in the same directory, or, for a sandboxed agent that has not reported an id
yet, the same session in its kept resume. So a second restore adds nothing the first one already
brought back. A saved agent pane with no session id counts only where a restore would resume
something in a session it makes (a command the agent reported for itself, or a path to resume
from), and nothing counts with `[session] resume_agents_on_restore` off. Nothing already open
changes: no window or pane is closed, moved, renamed, typed into or selected, the plain shell
windows of an open session are not added, and the window your client is on stays. The plan says
`add saved window 1 'claude' to open session 'api' as window 3` for each window (`would add
...` on `--dry-run`), naming the number the window gets as well as the one it had when saved, and
the result lists them in `added`, as `saved window 1 'claude' as window 3 of 'api'`; an open
session appears in `skipped` only when nothing is added to it. `--name` follows the same rule, and
so does a session a restore makes whole: a pane whose conversation already runs in another open pane
is made but not resumed (`skip resume in pane %14: pane %12 already runs the same session`). So
after a crash, a new server in which you opened one session at once and ran an agent in it gets the
agent windows its old layout had in that session, without closing anything: run `taw session
restore` in the new tmux server, or in a plain terminal before you start tmux.

**With no tmux server.** A restore with no server running (the crash left none, and you have not
started tmux) treats every saved session as missing. It makes the first with `new-session -d`,
which starts the server with your own tmux config, as any start does, and holds autosave off in
that same tmux call; the rest follow as usual. The plan's first line says `started a tmux server
(none was running), with your own tmux config`, `started_server` is `true` and `attach` says how
to attach, `tmux attach -t '=<the first session>'`. With no server, a plain restore reads
`default.json` (there is no server start to look back from). `--dry-run` with no server starts
nothing, and its plan begins `would start a tmux server (none is running), with your own tmux
config`.

**After a tmux server dies.** A new server has no layout, and its first autosave replaces
`default.json` with its own one session. A plain `taw session restore` therefore reads
`default.json` while that has something to make (a session that is not open, or a window of an
open one that a restore would add, above), as it always did, and when it has
none, the newest layout of `default` that was taken before this server started (its own
`taken_unix`, strictly before tmux's `#{start_time}` for the server): `default.json` itself if
it is from before, otherwise the newest copy in `session-snapshots/` that is. That is one layout
and no more: when it has nothing to make (its sessions are all open, and none has a window to add)
the restore says there is nothing to restore, as a restore with every session open does, and it never goes further back, so sessions closed long
before are not brought back. The layout it looks back to does not change while this server
runs, so a plain restore can also bring back a session from the previous server's last layout
that you closed since: that is what the command is for. It also adds that old layout's agent
windows to the sessions that are open, in any server, one that has run for days included, when
their conversations run nowhere now (you closed those agents since); that is what the command is
for too, and `--dry-run` shows what it would add first. `--name default` restores this
server's own saved layout only, with no look-back. Nothing is restored when a server starts; it
is a command you run.
The restore says which layout it used, with both times, as the first line of its plan (`restored
the layout saved at 16:23, before this tmux server started at 16:41`, with the date added when
it is not the same day), and `--dry-run` says `would restore ...` and makes nothing. A server
whose start taw cannot read restores `default.json` alone.

**When tmux itself crashes (macOS).** macOS writes a crash report for a crashed program into
`~/Library/Logs/DiagnosticReports`, and reports there can be removed later, so taw keeps its
own copy and says where. When that folder does not exist, macOS saves no report and does not
make the folder; `mkdir -p ~/Library/Logs/DiagnosticReports` brings reports back. When the
plugin loads on a new tmux server, taw looks at the server that was
last on the same socket. If that server is gone and left a report (a `tmux-*.ips` file with its
pid, which taw watches for up to two minutes), taw copies it into `<state>/tmux-crashes/` (a
folder only you can open, with each copy readable only by you) under its own name, never over a
file that is already there, and tells you once, as a notification
about no pane: `tmux crashed at 09:12:33`, with where the copy is kept and that `taw session
restore` brings back what the server had. The notification goes through `[ui.toast] delivery`
and the `[hooks] notification` commands like any other, and with delivery `herdr` or `terminal`
it waits, for up to ten minutes, until a client is attached (with none, there is no
notification and the copy stays). A server that was killed (`kill-server`) or that ended with its
last session leaves no report, so nothing is kept and nothing is said, and loading the plugin
again on the same server (a `source-file` of tmux.conf) does nothing more. taw never reads that
folder for anything else. Elsewhere than macOS it only writes down which server is running, in
`<state>/servers/`. The plugin runs `taw server started` for all of this, in the background; it
is not a command to type.

Autosave runs on its own, debounced 5s, whenever the watcher records a transition
or a tmux hook fires (`after-new-window`, `after-split-window`, `after-kill-pane`,
`session-closed`); you don't normally need to call `session save` by hand.

An autosave never replaces a saved layout with an empty one, and the two hooks that fire
when something *closes* (`after-kill-pane`, `session-closed`) wait five seconds first and
do nothing at all if tmux has gone by then. Quitting a server that closes its panes one by
one therefore keeps the layout it had, not the moment when only some were left.

If the snapshot a save is about to replace can no longer be read (a newer taw wrote it,
or it is damaged), the save first copies it to `<state>/sessions/session-backups/`, keeping
the three newest copies per snapshot name. If that copy cannot be made the save is skipped
and the original is left exactly as it was. A snapshot a newer taw wrote counts as unread
even when this taw could parse it, and `session restore` refuses it too (error code
`unsupported`, with a message saying to update taw) instead of rebuilding a layout without
whatever the newer format added.

Every save also keeps copies of the layout it replaces, as herdr does (and one more case, a
new tmux server, that herdr never meets and "Beyond herdr" explains): before a save replaces
`<state>/sessions/<name>.json`, and again right after it writes it, the file is copied into
`<state>/sessions/session-snapshots/` as `session-<name>-<time>-<pid>-<n>.json`. At most one
copy is made every 15 minutes (going by the newest copy's file time), so the first save of a
name is copied at once and a quick burst of saves adds nothing; a layout that did not change
since the newest copy is not copied twice, and a file that does not parse, was written by a
newer taw or holds no session is not copied. The 48 newest copies of each name are kept, the
older ones pruned, and other names' copies and any other file in that folder are never
touched. The one exception to the 15 minutes: a save that replaces a layout taken before the
current tmux server started copies it first whatever the age of the newest copy, so the last
layout of a server that died is kept by the first save of the next one (a layout the newest copy
already has is still not copied twice; outside a running tmux server, or when it does not answer,
the 15 minutes apply). A copy that cannot be made is logged and the save goes ahead. `taw
session delete` copies the file the same way before it removes it, and `taw session list` and
`delete` never see the folder.

**Older copies.** A plain `taw session restore` looks back one layout and no further (see "After a tmux server dies" above).
A `default.json` that taw cannot read is an error and not a reason to look back: the restore says
what it could not read and uses no copy, and a copy can be restored by hand, as below.
Nothing else recovers a copy by itself; to go back to any older one, copy a file from
`session-snapshots/` to `sessions/<a name>.json` and run `taw session restore --name <a name>`:

1. List the copies of the name by their time:
    `ls -lt <state>/sessions/session-snapshots/session-<name>-* | head`.
    The newest is first, and the file time says when it was made.

2. Copy the one you want to a new name that no saved session has:
    `cp <state>/sessions/session-snapshots/<that file> <state>/sessions/<new name>.json`. A new
    name, so an autosave cannot replace it before you restore it.

3. Check it with `taw session restore --name <new name> --dry-run`, then run it without
    `--dry-run`. A restore makes the sessions the server does not have, by their names, and in one
    that is open adds only the windows with an agent that runs nowhere in the server, at the end of
    it (the dry run lists each as `would add`, and each open session with nothing to add as `skip`):
    nothing already open is changed, so there is nothing to close first, and the plain shell windows
    of an open session stay as they are. To remake a whole session, shell windows included, close
    it first (from another session) with `tmux kill-session -t =<session name>`. The new name is the
    name of the saved file, not of the sessions in it.

The saved screens of `[experimental] pane_history` are not replayed onto a recovered copy: they
are kept with the layout they were taken with (a fingerprint of it is stored beside them), and a
restore replays them only onto a snapshot with the same fingerprint, so a copy under a new name
has none, and screens saved without a fingerprint are not replayed, once (the next save writes
screens that are). The fingerprint holds the snapshot's format too, so screen history saved
under another format is not replayed, once (the next save writes it again). The copies stay after `taw session delete`; remove them with `rm`.

A pane that `session restore` resumes carries the session id it was resumed from straight
away, and so does one started with `taw agent start --kind codex -- resume <id>`. A save
before the agent's own hook reports, or a server restart before it ever does, therefore
still resumes the same conversation instead of dropping back to a plain shell.

The session id also stays on the pane while its agent runs, when the agent is one taw cannot
name from the program in front of it: an agent whose hook has reported its session is kept, with
that id, until the pane's own shell is back, and npm installs of Letta and Codex run as `node`,
so a saved session has to hold their conversations all the same. An agent taw does name goes when
its program does, and its session id with it.

To fold this into [tmux-resurrect](https://github.com/tmux-plugins/tmux-resurrect)/
continuum's own restore flow:

```tmux
set -g @resurrect-hook-post-restore-all 'taw session restore'
```
