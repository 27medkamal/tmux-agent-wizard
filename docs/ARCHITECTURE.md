---
description: "How tmux-agent-wizard works inside: tmux as its database, the watcher per pane, how ids map, and the tmux gotchas it works around."
---

# Architecture

This is the implementation-facing description of how taw works. See also
`docs/PARITY.md` (what matches herdr and what can't) and `docs/COMPAT.md` (the
plugin environment contract).

## Overview

One Rust binary, `taw`, with subcommands. No socket, no daemon. tmux hooks call
`taw` as a short-lived process; the only long-running processes are tmux-owned
`pipe-pane` watchers, one per pane, that tmux itself starts and stops. Every hook and
startup job the plugin gives tmux ends in `|| true`, so it always exits 0 and tmux never
prints a "returned" message into a pane.

```text
tmux hooks (after-new-window, after-split-window, after-new-session)
        └─► taw watch attach <pane>  ─►  tmux pipe-pane -o -t <pane> 'taw watch run <pane>'
                                              │  stdin = pane output bytes (activity signal + OSC parsing only)
                                              │  300 ms tick, read after output, then capture-pane + pane_title + pane_current_command
                                              │  then detect::evaluate (herdr manifest engine port)
                                              ▼
                              pane options  @agent_kind @agent_state @agent_state_since …  (tmux is the database)
                                              │
     ┌───────────────────────┬────────────────┴───────────────┬─────────────────────────┐
  taw ui (ratatui popup)   #{@agent_wizard_status}      notifications on transition   taw CLI (herdr verbs,
  agents · spaces · preview  (opt-in status token)       display-message / OSC / os    JSON) · plugins · hooks
```

Every consumer on the bottom row (the popup, the status token, notification
delivery, the CLI, plugin actions/events) reads the same pane and global tmux
options. None of them talk to each other directly, and none of them cache
anything: the popup's 500&nbsp;ms refresh, a CLI `pane list`, and a plugin action's
context JSON all re-read tmux fresh. See "tmux is the database" below.

## Module map

`crate tmux-agent-wizard`, binary and lib name `taw`, everything under `src/`:

| Path | Responsibility |
|---|---|
| `main.rs`, `cli/mod.rs`, `cli/<group>.rs` | clap command tree, JSON envelope, dispatch, one file per verb group (`agent`, `pane`, `workspace`, `tab`, `worktree`, `notification`, `hooks`, `server`, `session`, `integration`, `plugin`, `manifest`, `watch`, `seen`, `ui`, `status`, `open`) |
| `cli/launch.rs` | what `agent start` waits on after it types the command: whether the agent started (the process in front of the pane is named for it, its own integration reported it, or the watcher has read a state for it off a screen that shows something the command put there) or the pane's shell is back at its prompt with nothing running, which fails the verb at once (`agent_not_started`). `agent start` takes the record of the pane's last agent off the pane before it types, so what the pane records while the verb waits is the new agent's |
| `tmux/mod.rs` | `Tmux` client: runs the `tmux` binary (honours `$TMUX` and `-L`/`-S` overrides), typed queries, one-call pane listing, option get/set, command helpers |
| `tmux/ids.rs` | public/tmux id mapping and target resolution (see "ID mapping" below) |
| `tmux/version.rs` | tmux version detection and feature gates (floor 3.2; newer features behind version checks, degrading otherwise) |
| `store.rs` | typed read/write of the `@agent_*` pane options; `AgentRecord`, `AgentState` |
| `detect/mod.rs`, `detect/engine.rs`, `detect/regions.rs`, `detect/identity.rs`, `detect/loader.rs`, `detect/update.rs`, `detect/manifests/*.toml` (22 files) | herdr manifest engine port |
| `detect/job.rs`, `watch/job.rs` | which agent the processes in front of a pane are, by their names and arguments (`node /path/to/bin/codex` is codex), as herdr's `identify_agent_in_job` reads them: `detect/job.rs` is the pure port, `watch/job.rs` reads the pane's foreground job off the process table (`ps`, and `pgrep -g` for the members of the foreground group (here and for `pane_shell_is_idle`), with `-a` on macOS, which makes `pgrep` list its own ancestors, and the argument reader the container recognition has). The verbs that type ask it whether the agent a pane records is in front of it |
| `watch/mod.rs`, `watch/stream.rs`, `watch/state.rs`, `watch/nudge.rs`, `watch/binary.rs` | the `pipe-pane` watcher: byte stream (`stream.rs`), pure transition decision (`state.rs`), the tmux-facing loop (`mod.rs`), the nudge a report sends the watcher (`nudge.rs`), and which taw a watcher runs and whether it is still the installed one (`binary.rs`) |
| `detect/container.rs`, `watch/container.rs`, `watch/container/argv.rs` | an agent started inside a container from a pane (README "Agents in containers"): `detect/container.rs` reads a container runtime's arguments and says which word is the program the container runs, the one place identity looks past the name of the foreground command; `watch/container.rs` is where the arguments come from and how often they are asked for (once for each process that comes to the front of the pane, and a look a second at who is in front, which the system answers with no process started, `ps` only when it cannot, or always when the environment seam for the tests `TAW_FOREGROUND_BY_PS` is set, so a test's refusing `ps` can make that look fail), and where the exact arguments cannot be had it reads the line `ps` prints instead; `watch/container/argv.rs` reads a process's exact arguments from the system (`KERN_PROCARGS2` on macOS, `/proc/<pid>/cmdline` on Linux), ported from herdr's macOS platform code, and never reads, keeps or logs the environment that follows them; `watch/container/tpgid.rs` reads the foreground group of a process's terminal from the system (`proc_pidinfo` on macOS, field 8 of `/proc/<pid>/stat` on Linux) |
| `notify/mod.rs`, `notify/tmux_message.rs`, `notify/terminal.rs`, `notify/system.rs`, `notify/sound.rs` | notification delivery, one file per `[ui.toast] delivery` value plus sound |
| `crash.rs`, `cli/server.rs` | what the plugin's startup one-shot `taw server started` does (the plugin fires it with `watch attach-all`, `status refresh`, `plugin startup` and `manifest update --if-stale`, each in the background with its output discarded): it writes `<state>/servers/<socket path, made into a file name>.json` (the server's pid and start time, written whole), and when the last server on that socket is gone, on macOS, watches `~/Library/Logs/DiagnosticReports` for up to two minutes for a `tmux-*.ips` whose pid is that server's, copies it to `<state>/tmux-crashes/` (a hard link of a temporary copy, so a name that is taken is never overwritten and only the run that made the copy tells the user), and announces it once through `notify::announce`. The folder and the waits have environment seams for the tests (`TAW_CRASH_REPORTS_DIR`, `TAW_CRASH_WATCH_SECS`, `TAW_CRASH_POLL_MS`, `TAW_CRASH_CLIENT_WAIT_SECS`) |
| `rail.rs` | computes `@agent_wizard_status`, the opt-in status-line token |
| `config.rs`, `config_default.toml` | `config.toml` model, defaults, `taw config check` |
| `tui/*` | the ratatui popup (`taw ui`): one tree of sessions, their panes and any closed worktrees, in tmux order, opening on the agent that most needs you; `tui/agent_view.rs` is the `agent.view.set` query its agent rows are projected through |
| `plugin/*` | herdr-compatible plugin system: manifest, registry, install/link, env, actions, events, panes (see `docs/COMPAT.md`) |
| `hooks/*` | the config's `[hooks]` (see README "Hooks"): `events.rs` is the names and when each fires, `context.rs` the `TAW_*` environment and the JSON a hook is told, `run.rs` the supervisor each background hook runs under (`taw hooks exec`, with `[hooks] timeout_seconds`), `log.rs` the hooks log, `sample.rs` what `taw hooks test` fires, `server_env.rs` the environment every hook is given: tmux's global environment (read once per watcher), the environment of the session the event is about over it (read for each event, since a client attaching changes it), and `TMUX`, and `mod.rs` starts the hooks of an event (`fire`, called from `plugin::events::emit`), of a notification (`notify::announce`), and asks the two that can refuse (`pre_close`, `pre_worktree_remove`) |
| `integration/*` | agent hook installers, ported from herdr, the same 18 targets, Letta being experimental (see README "Integrations") |
| `session/*` | snapshot save/restore with agent resume (`restore.rs`, `snapshot.rs`), the wait for a pane's shell to be at its prompt before anything is typed into it (`prompt.rs`, shared with `agent start`, and `pane clear`, which asks the same question once and does not wait on a program), the autosave debounce (`mod.rs`), and worktrees (`worktree.rs`) |
| `git.rs` | branch/status via the `git` subprocess, cached |
| `paths.rs` | XDG-ish filesystem locations (`$TAW_CONFIG_DIR`, `$TAW_STATE_DIR`, and their defaults) |

## tmux is the database

The rule: all agent state lives in tmux pane options and global options.
Nothing is computed when the popup opens or a CLI command runs; everything reads
what the watcher (or a hook, or a plugin) already wrote.

### Per pane (`set-option -pt <pane>`)

| Option | Values |
|---|---|
| `@agent_kind` | manifest id: `claude`, `codex`, `opencode`, … (empty = no agent). Written by the watcher, by `taw pane report-agent` and `report-agent-session`, and by `taw agent start` and a restore for the agent they start |
| `@agent_state` | `idle` \| `working` \| `blocked` \| `done` \| `unknown` (empty = no agent). Written by the watcher, by `taw pane report-agent` for a hook-reported agent, and by `taw seen` (run from tmux's focus hooks, not by the watcher), which turns `done` back into `idle` |
| `@agent_state_since` | unix seconds of the last state change, written with `@agent_state` |
| `@agent_rule` | id of the matched manifest rule (empty if none), written by the watcher; a hook's `taw pane report-agent` takes it off |
| `@agent_source` | `manifest` \| `hook:<source-id>`; hook-reported states are authoritative until released, or until the pane's shell is back. The watcher writes `manifest`, `taw pane report-agent` writes `hook:<source-id>`, and `taw pane release-agent` takes it off |
| `@agent_session_id` | agent-native session id reported by an integration hook (`taw pane report-agent-session`, or `report-agent --agent-session-id`), put back by a restore, or taken from `taw agent start --kind codex -- resume <id>`; a restore or a start through a sandbox profile keeps it in `@agent_sandbox_resume` instead |
| `@agent_session_path` | agent-native transcript path reported by an integration hook (`taw pane report-agent-session`, or `report-agent --agent-session-path`), or put back by a restore; a restore through a sandbox profile keeps it in `@agent_sandbox_resume` instead |
| `@agent_name` | user-assigned name (`taw agent rename`, the popup's rename, `taw agent start`'s name, put back by a restore), `[a-z][a-z0-9_-]{0,31}`, unique among live agents; it belongs to the agent it was given to, and goes when another kind takes the pane |
| `@agent_display` | display label override (`pane report-metadata --display-agent`) |
| `@agent_title` | title override (`pane report-metadata --title`, and `taw pane rename`, which sets or clears the same option) |
| `@agent_seen` | written by the watcher, and by `taw seen` (run from tmux's focus hooks), which sets it to `1`: `1` once the pane has been looked at since its agent last completed, or was in front of the user as it completed; `0` from the start of each turn and on `done`, which a completion nobody is looking at becomes |
| `@agent_kind_hint` | manual kind hint (herdr's `HERDR_AGENT` env at spawn), set by `taw agent start` and by a restore for the agent they start, or by hand; the watcher's clear of a record keeps it |
| `@agent_watch_pid` | pid of the attached watcher, which it writes as it starts and clears as it stops (for `watch status`, and the pid `taw pane report-agent` nudges) |
| `@agent_watch_build` | which taw that watcher runs, `<pid> <device>:<inode>:<size>:<seconds>:<nanoseconds> <path>`: its own pid, the file it was started from as it was then, and where that file is. The watcher writes it before its pid. `watch status` compares it with the file at the path now, and a different file, or none, means the watcher runs a taw that has since been replaced; it counts only while its pid is the one in `@agent_watch_pid`, so a watcher that wrote none (a taw from before it) reads as old too. It goes with the pid when the watcher leaves |
| `@agent_turn` | a stamp (nanoseconds since the epoch) that `taw pane report-agent` writes, in the same tmux invocation as the rest of the report, each time it reports `working` or `blocked`; the watcher reads it to tell that a turn began and ended between two of its reads. It goes with the record |
| `@agent_acquiring` | `taken`, or `taken <stamp>`, while the pane's agent is being taken (herdr's `agent_process_acquisition_pending`, #4457): taw took it just now and has not seen it working since, so its first `idle` is the agent becoming ready and no completion. The stamp is the `@agent_turn` the pane had when the agent was taken (none for `taken`), so that a watcher's first read of the pane counts a reported `working` as work only when the stamp has moved since; a bare `1` does not say when, and keeps the agent being taken. `taw agent start` sets it as it takes the last agent's record off the pane (herdr's `begin_managed_agent`), and a session restore for the agent each resume starts; the watcher sets it for an agent it takes as a new one or whose session another replaced, and takes it off once the agent works or is first idle. A watcher started again finds it. It goes with the record |
| `@agent_reported_kind` | the kind an agent's own hook reported with `pane report-agent-session --agent <kind>`, or that a session restore puts back with the command an agent reported that resumes its session; it counts only while it equals `@agent_kind`, and it goes with the occupant |
| `@agent_container` | the container runtime (`docker`, `podman`, `nerdctl`, `container` or `sbx`) in front of the pane while its agent was named by that runtime's arguments; it goes at the first read that finds the runtime no longer in front, and with the rest of the record when the agent goes. Written by the watcher. It is the `runtime` of the pane record's `sandbox` field (`taw agent list`, `agent get`, the JSON hooks and plugins are given) |
| `@agent_sandbox` | the profile `taw sandbox` started the pane's agent through (`default`, or one of yours): `taw sandbox` sets it in its own pane before it becomes the profile, and a restore with the line it types. It is the `profile` of the pane record's `sandbox` field; it never draws 📦 by itself. It outlives the agent's record, so it is not one of the options that go with an occupant: while a first start builds an image and installs tools, the profile in front of the pane is a shell script, which the watcher reads as the agent's shell come back. The watcher takes it off, with `@agent_sandbox_pid`, and empties the pane's report folder, once the process that set it has ended with no container in front: the profile ended without starting one, at once or after a while, or the runtime it became has ended, and the container with it. A read that cannot see the container for a moment while that process runs is no end. A restore's mark, which has no process, is kept until a container that came leaves. The watcher takes the mark off only while the pane holds the mark, the process and the report stamp its read found, in one tmux call; `taw agent start` takes both off as it takes the last agent's record off, so that the agent it starts is never taken for a sandboxed one |
| `@agent_sandbox_pid` | the pid of the `taw sandbox` that set `@agent_sandbox`, which `exec` keeps through the profile and the runtime it becomes, so that its end is the profile's. A restore's mark has none until the line it typed runs. It goes with `@agent_sandbox`, and outlives the record as that does |
| `@agent_sandbox_resume` | what a restore resumed a sandboxed agent with (its kind, session id or path, and the command the agent reported, as JSON), which the restore sets with `@agent_sandbox` in one tmux call, and the one place it writes them: a sandboxed resume puts no `@agent_kind`, `@agent_session_id`, `@agent_session_path` or `@agent_resume_*` on the pane, which could land after the typed `taw sandbox` had marked the pane and its mark had ended, and leave a session with no profile beside it. `taw agent start --sandbox` keeps the session it resumes here too (codex's `resume <id>`), only while the pane holds a mark with a process. A save takes the kind and the session from here, the kind always with the session, while the pane holds the mark and its record holds no session of its own. It goes with `@agent_sandbox` at each of the mark's ends |
| `@agent_resume_source` | the `--source` of the report that carried the command that resumes the agent's session (`-- <command...>` on `pane report-agent` or `report-agent-session`), or put back by a restore |
| `@agent_resume_agent` | the `--agent` it was reported for |
| `@agent_resume_argv` | the command, as the line `session restore` types: each word bare when it is made of letters, digits and `_ - . / : @ % +`, in single quotes otherwise. The three go together, while the source and the agent hold the pane, and with what the agent reported of itself |
| `@agent_restore_error` | why a session restore left the pane as tmux made it, in herdr's words (its `restore_error`): `Saved directory is unavailable. Restore the directory and restart this session.` when tmux did not open the pane in the directory it was saved in, so the restore typed nothing into it: the directory was gone, or one that could not be opened (tmux opens the home folder for either), or tmux read it as another (`#{pane_start_path}`, which the restore asks for as it makes each pane, is not the saved directory). `taw pane get` and `pane list` show it as `restore_error`. It is the pane's and not its agent's, so nothing that clears an agent's record takes it off: it goes with the pane, or with `@agent_restore_saved` once an agent of the pane's own is recorded in it |
| `@agent_restore_saved` | the pane as the snapshot a restore read had saved it (the JSON of its entry in `<state>/sessions/<name>.json`), which the restore sets with `@agent_restore_error`. Every save keeps that pane as it was saved, its directory, its agent and session and the command its agent reported, and not as tmux and the pane have it now (the home folder tmux opened it in, a shell that has gone somewhere else since), as herdr keeps such a pane's saved directory and agent session, until the pane is closed or an agent of its own is recorded in it: the watcher naming one in front of the pane, `taw agent start` once the agent it started is there, or any agent's report of its state or session, which take both options off in the tmux call that records the agent, so that the pane is saved as it is from then on, with that agent. Only its place in its window, and whether it is the active pane, are taken as they are. A pane kept with a profile (`agent_sandbox`) keeps it beside its session, and one with a runtime keeps that, so a restore resumes its session only through the profile, and one with a runtime and no profile not at all, as of any container pane (below). Without the reported command when the whole is too long for one tmux command. It goes with the pane |
| `@agent_claude_flags` | the flags the pane's Claude was started with that set how its session runs, as a JSON list of words (`["--effort","max","--model","x"]`): `--effort`, `--model`, `--permission-mode` and `--agent` with their values and `--dangerously-skip-permissions`, read from the exact arguments of the Claude process in front of the pane by `taw pane report-agent-session` (from Claude's `SessionStart` hook, outside a sandbox), written in the same tmux call as the session, so a report that loses the race writes neither, and taken off by a report that found the process and no listed flag. Nothing else of the arguments is kept, and nothing is logged. It is part of the agent's record: it goes when the agent leaves the pane or another takes it. A save keeps it as `agent_claude_flags`, and a restore types the words after `claude` (with `--effort` overridden by the effort of the last turn record that gives one in the last 2 MiB of the session's transcript, its `perTurnEffort` or its `effort` when that is null, when it has one) and puts them back on the pane |
| `@agent_typed_line` | `<row> <history> <column> [right] <length> <command> <start>`: where `taw agent start` typed the agent's command (the row of the screen its last row was on, the pane's `#{history_size}` then, the column it was typed at, which is how wide the prompt before it was, the word `right` when that row showed a right prompt beyond the cursor before the command was typed, the command, whole, which is `<length>` characters long, and what the row starts with: the prompt as it was drawn left of the cursor before the command was typed, cut at the cursor's column as tmux draws it, a sequence it draws as one cell (an emoji with U+FE0F, a keycap, a ZWJ or skin-tone sequence) counted as that cell, and the command, or for a command that wrapped its part on its last row), written once the shell has echoed the command, before Enter (where the command was typed, when the shell has not echoed it within a second). The pane still shows what its last occupant left, so the watcher reads only the rows below this line while it is on the screen: it looks from the row the history's growth puts it on, upwards (a transient prompt rewrites the row and moves it up, and a history at its limit stops growing, so the search can start well below the line), for a row with the line's shape: what the prompt left of the cursor, or a transient prompt put in its place that is no wider than it or than 16 columns (the last line of a prompt of two lines can be as short as `❯ `, and a prompt that ended with a line break leaves nothing, while a transient prompt can show the time before its `❯ `), then the whole command, then a right prompt only with `right`. So a row is the line when it reads `<start>`; or, with `right`, starts with `<start>`, a prompt and the command, then a space and whatever follows (a right prompt, a clock in it that moves on); or holds the whole command after the row's start or a space, with no more before it than that room, and nothing after it but, with `right` and past a run of two or more spaces, a right prompt, whatever that holds (a transient prompt rewrote what came before it, kept the right prompt as zsh does unless `TRANSIENT_RPROMPT` is set, or took a command that had wrapped back to one row). A row of the agent's own that holds the command's text is not the line: a status row naming the model, flush left or not, a tip to run `claude --continue` or one that ends `run claude`, the footer that says `Update available! Run: mise upgrade claude`, or a box row or a status row with the word padded after it. With a right prompt on the prompt row, a row at the bottom of a full pane that holds the command padded by spaces (a box or status row) can still be taken for the line. A row that ends with the bare command is taken for it when no more than the prompt's width (at least 16 columns) comes before it, as a tip that ends `run claude` can be after a wide prompt. It reads the whole alternate screen. The watcher takes it off once two reads in a row do not find the line on the screen (the agent's drawing scrolled it off or cleared it; the first such read decides on no screen at all and has the next made at once) and when the agent only its hint names has exited, once that agent's grace is over (the pid `taw agent start` leaves when its budget runs out with the agent still coming up is what was in front of the pane then, which can be a helper the shell ran before the command, dead before the agent has a process of its own); it goes with the occupant like the rest, and a start that fails takes it off |
| `@agent_pid` | the pid of the process `taw agent start` recorded for what it started (the newest child of the pane's shell in front of the pane's terminal: recorded as the verb waits once the job in front names the agent, by its processes' names or arguments, and for an agent nothing names, once the wait is over, the one its last two looks both found, with more looks for up to 1.5 s when the wait's last two did not agree, three at the least however long a look takes and past that until two agree or until the sixth, none begun once `--timeout` has run out (the pid then recorded only if the last two agree), so that a helper the shell runs before the command, which names no agent, is not taken for it while the agent comes up), or a session restore for what each resume command started (the newest child of the pane's shell in front of the pane's terminal, looked for over two seconds once every command is typed, three times at the least however long a look takes and past the two seconds until two agree or until the sixth, and taken from the last two looks, which have to agree, so that the helper a preexec hook runs before the command is not taken for it), so that the exit of an agent only the kind hint names is told even when its last screen still matches a rule; `taw agent wait` reads it too, and the verbs that type count it only while its process is in the foreground of the pane's terminal (its process group is the group the pane's own process says that terminal gives its input to, `ps -o pid=,pgid=,tpgid=` for both in one call). It goes with the occupant when `agent start` takes the last agent off, and a report (`pane report-agent`, `report-agent-session`) for a pane whose pid is dead takes it off when the reporter descends from the job in front of the pane (the hook of a live agent does; the orphaned hook of the agent that died, which launchd or init took in, does not), since that reporter is not the dead pid's agent |
| `@agent_final_state` | `<status> <milliseconds>`: the status the pane's last agent left with, and when, to the millisecond, so that a status left a moment before a wait began in the same second is told from one left after. The watcher writes it as it takes a record off a pane (copied by tmux from `@agent_state` in the same invocation, only when there is one) and with the exit of an agent whose pane keeps its hint; `taw agent start`'s own clear of the last agent's record writes none, since what the pane held is the last agent's and was recorded when it left. A wait for an agent that has gone answers with it when it is a status the wait was for and is newer than the wait (herdr's final status of a released agent). It is the last agent's, so it is not one of the options that go with an occupant |
| `@agent_report_seq` | `<source>=<seq>` words: the last `--seq` each source sent for the pane's agent, the source with every byte but letters, digits, `_`, `.`, `:` and `-` written `%XX`, the last 32 sources in at most 2 KiB once written, the oldest going first; a report not newer than its source's last is ignored (herdr's `hook_report_is_newer`). It goes with the agent |
| `@agent_report_stamp` | a stamp (nanoseconds since the epoch) of the last report taken for the pane, which the report writes; the watcher's clear of the record of an agent that has left lands only while the pane holds the stamp its read saw. It goes with the agent |
| `@agent_token_<key>` | one per token, read apart from the columns with `show-options -p` (the popup reads them only when its rows or the Agents view use a token): `pane report-metadata --token <key>=<value>` writes `<key>`, `--state-label <status>=<text>` writes `state_label_<status>`, and `pane report-agent-session --session-start-source` writes `session_start_source`, which goes when another agent takes the pane. Every token goes with the record |

`store::OPTION_FORMATS` is the exact `#{...}` format list `Tmux::list_panes` appends
after tmux's own pane/window/session fields; the code and this table are the same
22 options, in the same order, except `@agent_watch_build`, `@agent_turn`, `@agent_acquiring`, `@agent_sandbox_pid`, `@agent_sandbox_resume`, `@agent_typed_line`, `@agent_pid`, `@agent_final_state`, `@agent_report_seq`, `@agent_report_stamp` and `@agent_token_<key>`, which are not columns of it.

### Global (`set-option -g`)

| Option | Meaning |
|---|---|
| `@agent_wizard_status` | the status rail text, tmux-format styled, e.g. `#[fg=yellow]⏸1 #[fg=blue]⏵3 #[fg=green]✓2` (non-zero states only). Written by the watcher after every transition, by `taw seen` when it turns `done` back into `idle`, and by `taw status refresh` |
| `@agent_wizard_version` | binary version that installed hooks (idempotency marker for the plugin script) |
| `@agent_wizard_installing` | set by the plugin script while `scripts/auto-install.sh` downloads `taw` in the background, or refreshes a downloaded one, and cleared when it ends, so that a second load meanwhile starts no second download |
| `@agent_wizard_restoring` | set by `taw session restore` while it runs, to `<pid> <unix time>`: its process id and when it began. `taw session autosave` saves nothing while it is set by a process that is still there, and the restore clears it when it ends. A restore that is killed cannot clear it, so a mark whose process has gone, or that is ten minutes old (a process id can be reused), is ignored |
| `@agent_wizard_view` | the active Agents view projection: herdr's `agent.view.set` params JSON, written by `taw agent view set` and removed by `agent view clear`. Unset means no projection. Read by the popup only (`src/tui/agent_view.rs`); `agent list`, notifications and detection never consult it |
| `@agent_wizard_reload` | an opaque value that `taw config reload` and `taw manifest reload` change; every watcher reads it with the rest of its read, and a change makes it read the config and the manifests again (`src/watch/reload.rs`) |
| `@agent_wizard_panes` | the public ids of the panes that were live when the roster was last taken, space-separated: the hooks that fire on a change run `taw` to take it again, and a pane missing from the new roster is a `pane.closed` (`src/cli/events.rs`, `detect_closed`). Unset until the first take, which reports nothing |
| `@agent_wizard_panes_generation` | how many times the roster has been written; a write lands only while it is the one the writer read before it listed the panes |
| `@agent_wizard_exited` | the raw ids (`%12`) of panes whose process ended, each followed by a space, which the plugin's `pane-exited` hook adds with its own tmux commands, at most 400 characters; a roster take leaves those panes out of what it reports, and takes their marks off |
| `@agent_wizard_saved_set_titles`, `@agent_wizard_saved_set_titles_string` | tmux's `set-titles` and `set-titles-string` as they were before `taw terminal title set` changed them, which `taw terminal title clear` puts back |

### Per session (`set-option -t <session>`)

| Option | Meaning |
|---|---|
| `@agent_wizard_bar` | the text of taw's bar on the status line while the popup is open, the one thing the popup writes as it runs; `taw ui open` puts the bar in row 0 of the session's `status-format` (as `#{T:@agent_wizard_bar}`) and takes it away when the popup closes (`src/cli/status_bar.rs`) |
| `@agent_wizard_status_saved` | the rows of `status-format` the bar replaced, and whether the session had its own or inherited the global one, from the moment the bar goes up until `taw ui open` puts them back; still there at the next open means the last launcher died with the popup open, and that open puts them back first |

### On a new session, window or pane

| Option | Meaning |
|---|---|
| `@agent_wizard_created` | the payload of the `workspace.created`, `tab.created` or `pane.created` event, as JSON, left on the session, window or pane it is about by the hook that builds it (only when a plugin or a `[hooks]` command listens for the event). `taw workspace create`, `taw tab create` and `taw pane split` answer with it and take it off, so that what the caller is told and what the event said are one read: a new window's name, which tmux's automatic-rename changes as its shell starts, read twice was two names. It stays on what a key or plain tmux made, where no verb reads it |

### Options the user sets (never written by `taw`)

| Option | Default | Meaning |
|---|---|---|
| `@agent-wizard` | `A` | prefix key(s) that open the popup (space-separated list allowed) |
| `@agent-wizard-height` | `100` | cap on the popup's height, % of the client (`taw ui open` sizes the popup itself) |
| `@agent-wizard-width` | `100` | cap on the popup's width, % of the client |
| `@agent-wizard-popup-title` | ` taw ` | `display-popup -T` title, shown only on a framed popup: a cap below `100`, or tmux older than 3.3 |
| `@agent-wizard-popup-border` | *(empty)* | `display-popup -b` border style of a framed popup; ignored below tmux 3.3 |
| `@agent-wizard-notification-key` | `O` | prefix key: next up, who to deal with next (see README "Next up") |
| `@agent-wizard-clear-key` | *(empty)* | prefix key(s) bound to `taw pane clear` on the pane they are pressed in; empty binds nothing (see README "Clearing a pane") |
| `@agent-wizard-bin` | *(empty)* | explicit path to `taw`; otherwise resolved by the plugin script |
| `@agent-wizard-watch` | `on` | `off` disables attaching watchers automatically |
| `@agent-wizard-auto-install` | `on` | `off` stops the plugin downloading `taw` when it finds none, and refreshing a downloaded one after an update |
| `@agent-wizard-herdr-env` | `off` | `on` sets `HERDR_ENV=1`, `HERDR_BIN_PATH` (the taw binary) and `HERDR_PANE_ID=%calling` in tmux's global environment, for the agents that report to herdr themselves (see README "Agents that report to herdr themselves") |

## Watcher lifecycle (`taw watch`)

One `pipe-pane` process per pane, started by a tmux hook and stopped by tmux itself
when the pane closes or the pipe is removed.

1. **Attach.** A tmux hook (`after-new-window`, `after-split-window`,
   `after-new-session`) runs `taw watch attach <pane>`, which runs
   `tmux pipe-pane -o -t <pane> "<taw> watch run <pane>"`. `-o` is a
   toggle-if-absent: calling attach on a pane that already has a pipe is a no-op,
   so hooks can fire repeatedly without stacking watchers. `attach-all` does the
   same for every pane on the server (run once at plugin startup, for panes that
   existed before `taw` was loaded).
2. **Read.** `taw watch run <pane>` is the process tmux started; it reads stdin
   until EOF. The bytes are an **activity signal, not a transcript**: the only
   thing parsed out of them is the OSC `9;4;<state>[;<pct>]` progress sequence
   (`watch/stream.rs`). Every chunk read from the pipe updates `last_output_at`,
   which only the debug log shows: what schedules a read of the pane is step 3.
3. **Cadence (herdr v0.9.1's pane detection task).** While an agent is identified
   the watcher ticks every 300&nbsp;ms, and every 100&nbsp;ms while an idle verdict
   is on hold; a pane without an agent is read at most every 500&nbsp;ms, and only
   after output. Output makes a pane *eligible* for a read at its next tick and does
   nothing else: it never sets or holds `working`. A tick with no output since the
   previous read reads nothing, except a hold recheck, the end of the startup grace,
   a pending exit, and an identity recheck every 5&nbsp;s. A report is the other thing
   that makes a pane eligible: an agent that reports its own state is often quiet while
   it thinks, and a pane with no agent in it is never read at all without output, so
   `taw pane report-agent` tells the watcher (the nudge, `watch/nudge.rs`). It reads
   `@agent_watch_pid` in the same tmux invocation that writes the report, and sends that
   pid one `SIGURG`; the watcher's handler writes a byte to a socket of its own, a thread
   turns that into a wake-up of the loop (`Wake::Nudge`), and the pane is read at its next
   tick as if it had printed. `SIGURG` because it is ignored by a process that does not
   catch it: the pid is what tmux remembers for the pane, and may have gone to another
   process, which a `SIGUSR1` would end. A watcher of an older `taw`, or a pid that is no
   watcher, takes the nudge for nothing, and the pane is read on the schedule it always
   was. A nudge is a wake-up and never a state: what the read makes of the pane is what
   any read makes of it. One read is one
   `tmux display -p -t <pane>` round trip for 28 tab-separated fields:
   `pane_current_command`, `pane_title`, `window_active`, `session_attached`,
   `session_name`, `window_index`, `history_size`, `alternate_on`, eighteen `@agent_*` pane
   options (`kind_hint`, `source`, `state`, `seen`, `kind`, `display`, `name`, `rule`,
   `reported_kind`, `session_id`, `session_path`, `turn`, `acquiring`, `typed_line`,
   `report_stamp`, `container`, `sandbox`, `sandbox_pid`), the global `@agent_wizard_reload`, and
   whether the terminals attached to the pane's session have focus (step 6), which stays the last
   field. The pane's screen (`capture-pane -p`) comes with it when an
   agent's rules may read it, in the same tmux invocation when the read expects it
   (`watch::DISPLAY_FORMAT`). A read owed only to the identity recheck reuses the last
   capture while the command and kind hint are unchanged, since tmux changes a screen
   only when something in it prints; none is taken in the startup grace, while a hook
   owns the state, or of the shell a hinted agent left once a read has found its start
   pid gone (a turn still `working` keeps its capture, since that screen decides how it
   ends). `rule` is read back because step 6 compares it with the rule matching now;
   without it every read looks like a rule change. `reported_kind` is read so that an
   agent that named itself is told from one a process names (step 6), and `session_id`
   so that an agent taking the pane drops only the session the read saw. `container`, `sandbox`
   and `sandbox_pid` are read for the profile mark's ends (`sandbox::mark`): a watcher's first
   read takes the runtime the pane is marked with as the one it found last, so that a watcher
   started again takes no container already there for one that came, and a marked pane whose
   process still runs with no container in front is read again a second after each read, whether
   or not it prints, so that a profile that ends without a word is seen ending.
4. **Identity, then evaluation.** `@agent_kind_hint` wins over the foreground
   command (`detect::identify`), and it deliberately outlives any one occupant:
   `store::clear_agent` keeps it, as herdr does. So the three outcomes are:
   - **No identity at all** (no hint, and the command names no agent): clear
     `@agent_kind`, `@agent_state`, `@agent_rule` and update the rail. This is
     what a process-identified agent exiting looks like.
   - **Identity from the hint alone**, the command is not an agent, and no
     manifest rule matches: write `unknown` with an empty rule. Without this a
     hinted pane kept whatever state its agent left behind for ever, because the
     surviving hint makes the clear above unreachable. `unknown` does not toast.
     Not applied while the previous state is `working`: a capture landing
     mid-redraw matches nothing either, and letting that overwrite a live turn
     destroyed the `working` to `done` edge; such a pane reaches `unknown` one
     evaluation later.
   - **Otherwise**: `detect::evaluate` runs the manifest engine against the
     snapshot.

   **A container runtime in front of the pane** (`docker`, `podman`, `nerdctl`,
   `container`, `sbx`) names no agent itself, so the watcher reads the process's
   arguments, once per process (`watch::container`). The leader of the
   terminal's foreground process group is the process tmux names; the system gives it
   (`ps` only when it cannot), and it is asked at most once a second; the arguments are read only when it is not the process
   already read. So a docker that follows another with no prompt between
   (`docker run a claude; docker run b codex`) is read again, and the command being
   `docker` throughout does not hide it.

   The arguments are read exactly, one string for each: `watch::container::argv` asks the
   system (macOS's `KERN_PROCARGS2`, ported from herdr's platform code, and Linux's
   `/proc/<pid>/cmdline`), and `detect::identify_argv` takes the runtime's words as the
   real argument list and only the script a shell is given with `-c` as shell text, so a
   value with a space, a `;` or a newline in it is one word. Only when the list cannot be
   had (a process of another user, another system) is the line `ps -ww -o args=` prints
   read, by `detect::identify_ps`: as words split on white space, with the `\012` macOS
   prints for a newline undone, and a value with a space shifts what follows it. The two
   readings are never run one after the other over the same process, since what the exact
   list says is not a command (`bash -lc 'cat /usr/bin/claude'`) would be one to the other.

   The agent the arguments name as the program the container runs (`detect::container`)
   counts as the process naming it, not as a hint: a screen no rule matches reads `idle`
   rather than `unknown`, and the shell coming back clears the record as it does for any
   agent. It is one a process names for step 6 as well, so an ivy pane follows the ordinary
   exit and miss rules whatever its hook reported, and the pane's shell is never asked
   about for it. The watcher writes the runtime to `@agent_container` when it changes, and
   takes it away at the first read that finds the runtime no longer in front (the agent's
   record stays through the misses as any agent's does, and `store::clear_agent` takes the
   option with the rest of it). Any other command, the shell included, forgets what was
   read. A read that could not be taken (the process had gone, nothing would say) is tried
   again, five times in a row at most and at least a second apart; then the process is
   taken as naming no agent, the log says so once (never the arguments), and it is tried
   again at every identity recheck. Until then, a runtime that was read before and is still
   in front is taken as running the agent the last look found in it: a `ps` that fails once
   under load is no container leaving the pane, which would take `@agent_container` away and
   put it back, and fire `sandbox.stopped` and `sandbox.started` with it. Nothing of the arguments is kept or logged, since they
   can carry credentials: only the agent's id. `taw agent explain` reads the option, so it
   agrees with the watcher.

   A hint-only pane whose agent `taw agent start` started is told gone by the pid
   recorded for it (`@agent_pid`), whatever its last screen shows, and its end is the
   end of any agent's: a turn that was under way (`working` or `blocked`) is written
   as the exit's completion, an `idle` with the rule `process_exited`, `done` while
   nobody is looking, and announced and notified as a finish; the read after it finds
   `unknown` with the hint kept (the notice for the `done` stands over that
   `unknown` while the recorded pid is dead). The status it left with is kept in
   `@agent_final_state`. A hint-only pane with no recorded pid cannot be told from
   outside. One consequence worth knowing: a hint-only pane whose shell never resets the
   pane title stays `idle` after its agent exits, when the agent's manifest has an
   `osc_title_idle` rule (Claude, Amp, Hermes, Grok and Codex do), because the agent's
   own OSC 2 title still matches it. For Claude, Amp, Hermes and Grok, whose rules want
   a title of the agent's own shape, it reaches `unknown` once the title is cleared or
   set to anything else, which an interactive shell's prompt hook does on the next
   prompt. Codex's rule went missing in herdr's manifest 2026.09.23.1 and is back in
   2026.10.01.1, and it takes any title that is not a spinner for idle, so a shell
   prompt hook that sets any text keeps a hint-only Codex `idle`, and only the start
   pid that `taw agent start` recorded (`@agent_pid`) ends it: once that process has
   gone the watcher drops the rule and the pane reads `unknown`. A pane with no
   recorded pid, as when its hint was set by hand, stays `idle` while the title
   matches. Where the title says nothing (a Codex that sets none, so the pane has the
   host name tmux gave it, which is read as no title, as below), taw's own
   `taw_composer_idle` rule (a supplement) is what reads a hint-only Codex at its
   composer as `idle`, not `unknown`.
5. **Hooks are authoritative.** If `@agent_source` already starts with `hook:`, the
   watcher does **not** overwrite state from the manifest: an integration hook (see
   below) owns that pane's state until `taw pane release-agent` gives it back, or until
   its shell is back, which ends the record as any exit does (herdr's
   `ReportedAgentShellReturned`; step 6 says which shell). `@agent_kind` is still
   updated either way. The state is the hook's, but the change is the watcher's to
   announce, as herdr sends a hook's report through its own state machine: a report is
   written to the pane by `taw pane report-agent` before any read, so the watcher
   remembers the state its last read left and the agent it recorded (`state::Taken`, for
   a pane the screen decides as for one a hook reports for) and a read that finds
   another state is the change, announced once by the same steps as a change read off
   the screen (`Decision::Reported`: the completion rule, the status event and the
   `agent.*` event, the toast, the `notification` hook, the rail and the autosave). The
   same state again is nothing, and so is a `done` that `taw seen` turned back into
   `idle`; an `idle` reported over a `done` nobody has seen leaves it `done`. A turn
   that began and ended between two reads (`working` and `idle` reported back to back
   leave the state the watcher had taken in) is found by the stamp `@agent_turn`, which
   each report of work moves, and is announced as its start and its end
   (`Decision::Reported`'s `via`), the toast with the end; not while a turn is under
   way, which reports work all the way through. Another agent reporting the state the
   pane is in is the status event alone (`kind_changed`), as for an agent a screen read
   finds in the pane's place, and the name the last agent was given goes. The only state
   the watcher writes over a report is that `done`, and it writes it with the hook still
   the source, and only while the pane still holds what the read saw (`if-shell -F` in
   the same tmux invocation), so a report that came since is never undone. A pane a
   watcher finds hook-owned at its first read is taken as it is, as a restarting watcher
   is given no startup grace: no change is announced for what an earlier watcher
   announced.
6. **Transition rules.** `skip_state_update` (a manifest rule flag) keeps the
   previous state and ends any hold. The hold, the 3&nbsp;s startup grace,
   exit-idle-first and the 5 identity misses are herdr's
   (`src/pane/agent_detection.rs`, `src/pane.rs` @v0.9.1). The misses are not counted
   for an agent that reports for itself and that no process in front of the pane has
   named at a read while its record stands (its hook reported its kind,
   `@agent_reported_kind`, or its state). herdr keeps a reported agent it cannot identify
   by its process until its shell is idle (`report_self_reported_agent_shell_return` in
   `src/pane.rs:368-394` and `available_pane_shell_from_job` in
   `src/platform/mod.rs:367`, master `0d5d6f1`). The files taw is ported from do not show
   which panes herdr marks that way, so which agents taw keeps so is taw's own reading of
   "cannot identify": every agent whose hook reports for it and that the identity table
   does not name from the pane's command, or from the arguments of the container runtime
   that is the command (step 4). Such a pane keeps its kind and its session id,
   whatever its command is called, until the pane's own shell is at its prompt with
   nothing else running in its group. That is the Letta an npm install runs under `node`,
   and an agent run by a wrapper script. An agent a process does name (a Claude or a
   Codex started from a prompt) follows the misses and the exits above, whatever its
   hook reported: a shell in front of it ends it, and so does another program after the
   sixth miss. An agent whose hook reports its state (OpenCode, kimi, pi, omp, kilo,
   mastracode) is the exception to the second half: no miss is counted for a pane a hook
   owns, so another program in front of it does not end it, and only a shell coming back
   does. A completion (`idle` after `working` or `blocked`) on a **background**
   pane (its window is not the
   active window of any attached session) becomes `done` instead of `idle`, decided
   at that moment: a glance at the pane earlier in the turn does not count (herdr's
   completion rule, `src/app/actions.rs`), and `unknown` to `idle` is not a
   completion. Nor is the first `idle` of an agent taw has just taken (herdr's
   acquisition guard, #4457, v0.9.2): one the watcher takes as a new agent, one the
   pane marks `@agent_acquiring` (`taw agent start` and a restore mark the agent they
   launch, and a watcher started again finds the mark), and one whose session another
   replaced, until a read finds it working or a report of work (`Decision`'s `ready`):
   a trust question asked at startup and answered later is the agent becoming ready.
   An exit is a completion all the same. A change of session id or path of the same
   agent, or another agent in the pane, is herdr's completion reset: the pane is marked
   seen whatever its state, a `done` goes back to `idle` (announced, with no
   notification), and a notification still waiting out its delay is dropped when it
   comes due, as it reads the state with the agent and the session the pane holds then
   (`state::Pending`). A session report nudges no watcher, so the reset lands at the
   next read, which the agent's own redraw brings. An agent that reports for itself
   goes through the guard and the reset too (`Decision::Reported`). A pane is
   background, too, while every terminal attached to its
   session has reported losing focus: you are in another app, herdr's unfocused
   outer terminal. With `focus-events on` tmux asks terminals for those reports and
   keeps the answer in `#{client_flags}`, which the read's last field takes from a loop
   over the session's clients on tmux 3.6+ (`notify::terminal_focus`); with
   `focus-events` off, or an older tmux, nobody's focus is known and the window alone
   decides, as herdr's does when its terminal never reports focus. `taw seen <pane>` (fired by focus
   hooks) later turns `done` into `idle`, and so does the next read that finds the pane in
   the foreground. Those focus hooks are `after-select-pane`,
   `after-select-window`, `client-session-changed` and `pane-focus-in`, plus
   `client-focus-in` on tmux 3.6+. Each runs `taw seen --if-looked-at <pane>`, which marks the pane only
   if a client shows it when the job runs (it is the active pane of the active window of the session some client is on):
   a client that is attached to one session and switched to another in one command line, as `taw app`
   does to go in with a last session, passes through the first, and nobody looked at that one. A note on `pane-focus-in` and tmux 3.6b: it still
   *fires* there (verified: a terminal regaining focus or a pane being selected runs
   the `seen` hook), it is merely no longer enumerated by bare `show-hooks -g` (a
   cosmetic 3.6 change); queried by name, or driven by an event, it is present and
   working. The plugin reads and strips its own hook entries by name for exactly this
   reason, and additionally installs tmux 3.6's `client-focus-in`, the canonical
   "the terminal regained focus" signal, which does enumerate normally, as a second,
   version-gated carrier of the same `seen` action. Any real transition writes
   `@agent_state`, `@agent_state_since`, `@agent_rule`, `@agent_source=manifest`,
   recomputes the rail, emits the `pane.agent_status_changed` plugin event, touches
   the session-snapshot autosave debounce, and runs notification logic.

   When the state and the occupant are both unchanged but the *matched rule* is
   not, the rule alone is rewritten (`Decision::RuleChanged`): no event, no
   notification, no autosave, and `@agent_state_since` still says when the state
   last moved. `@agent_rule` is the pane's evidence (what `agent get`, the popup
   and `watch status` show as the reason for a state), and writing it only on a
   transition left an agent that was classified before it had drawn anything
   reporting `idle` with no explanation for as long as it stayed idle.

   When another agent takes the pane, what the last one left goes with it (herdr v0.9.1): the pane title it left reads as no title until
   tmux reports another, its OSC `9;4` progress is forgotten, and its
   `@agent_name` is unset unless the newcomer is the kind the pane's
   `@agent_kind_hint` names.

   A pane no program has titled has the host name for its title in tmux (`#{host}`, in
   full), and taw reads that as no title wherever it shows or passes a pane's title
   (`tmux::osc_title`): herdr's panes start with none, and a rule that takes any title
   for a sign of life, as Codex's `osc_title_idle` does, would otherwise read every pane
   nobody titled as an agent at rest. The watcher takes it so in its own display format,
   and the pane listing does for everything else, so the popup's rows (which then say the
   agent's kind), `taw agent explain`, the JSON verbs' `terminal_title`, and the hook and
   plugin contexts all see no title. A title a program set is read as it is, even one that
   holds the host name. So is the name of a pane made before the host name changed: tmux
   keeps the name it had, which is not the default now, so the pane reads as titled.
7. **EOF.** The pane closed, or something removed the pipe: clear
   `@agent_watch_pid` and `@agent_watch_build`, and exit 0. They are cleared only while
   `@agent_watch_pid` is still this watcher's own, in the one tmux command that checks, because
   a restart closes the old pipe and opens the next at once, and the old watcher is still on
   its way out when the new one has written its pid. tmux never restarts a `pipe-pane`
   process itself; a pane that reopens an agent gets a fresh watcher only via the next
   matching hook. Before that, a watcher whose pane has gone (tmux knows no such pane, or no
   server answers) with a container it last announced in it fires `sandbox.stopped`, told what
   its last read found and the pane as it last listed it; a pipe that was only removed
   (`taw watch detach`, a restart) leaves the pane and its container there, and fires nothing.

A watcher keeps running the taw it started with, so after a rebuild or a reinstall (cargo
replaces `target/release/taw` with a new file) it is a stale one, and nothing in tmux says so.
The watcher therefore writes `@agent_watch_build` as it starts, before its pid (`watch/binary.rs`): the file the
watcher was started from, as one `stat` tells it (device, inode, size and time of the last
write), and where it is. `taw watch status` looks at that path again for each live watcher and
marks `binary` `old` when another file is there, none is, or the watcher wrote no record.
`taw watch attach-all --restart` (`watch::restart_old`) closes the pipe of each old watcher
whose pipe is open (`WatchStatus::restartable`, which `status` counts in `old` as well: one whose
pipe is closed was let go of), waits up to three seconds for it to clear its pid, and opens the
next one from the path the old one started from (the taw that ran the command when there is
nothing runnable there: a path that is not absolute, or a file that is not the user's own or
cannot be run, is not taken from the record). A listing of who is on the panes that fails counts
as nobody having left, so the wait goes on to its end, and a next watcher that cannot be started
(tmux refuses, or watching was turned off meanwhile) is counted in `failed` with a note in words,
since its pane has none now. It does nothing while
`@agent-wizard-watch` is `off`, which no watcher could be opened under, and `status` says so in
`restart` instead of naming the command.

A watcher that hits a transient tmux failure (a busy server, a hit
file-descriptor limit) backs off (1, 2, 4, 8, 16, then a 30&nbsp;s ceiling between
retries) rather than either spinning or giving up. It only retires for good on a
message meaning the pane or the server is actually gone (`can't find pane`,
`no server running`, `error connecting to`); anything else is assumed transient, on
purpose; see `watch::is_gone`/`watch::GONE_MESSAGES` and `watch::backoff_delay`.
Logging is per-pane, `<state>/logs/watch-<pane>.log`, rotated at 1&nbsp;MB, level
from `TAW_LOG` (`error` by default). A watcher reads `TAW_LOG` from its own process
environment, which it inherits from the tmux **server** that spawned it as a
`pipe-pane` child, and a process's environment is fixed at spawn. So
`tmux set-environment -g TAW_LOG …` on a server whose watchers are already running
does **not** reach them; it applies only to watchers spawned afterwards. To turn on
logging for panes that are already watched, set `TAW_LOG` in the server's own
environment before the watchers start (launch or restart the tmux server with it
set), rather than exporting it into a live server. (The CLI's own stderr logging in
`cli/mod.rs` re-reads `TAW_LOG` on every invocation and is unaffected by this.) The
`[hooks]` commands a watcher starts are the one thing that does see a live
`set-environment -g`: taw reads tmux's global environment itself, once when a hook first
needs it and again when the watcher reloads its config, and hands it to the hook
(`hooks/server_env.rs`), because a phone push keeps its credentials there. It puts the
environment of the session the event is about over that, read for each event, as tmux does for
a new pane: a client that attaches changes the session's `SSH_AUTH_SOCK`, and the watcher, a
`pipe-pane` child, has only what the server was launched with.

### Integrations feed the same options from the other direction

`taw integration install claude|codex|copilot|opencode` writes a small hook
script into the agent's own config directory (see README "Integrations"). At
runtime that script calls `taw pane report-agent-session`/`report-agent` with
`$TMUX_PANE`, which sets `@agent_source=hook:<id>` and the session
id/path/state directly, with no manifest evaluation involved. That is the "hooks are
authoritative" rule above: once a hook has spoken for a pane, the watcher backs
off and only refreshes `@agent_kind`, until `pane release-agent` hands the pane
back to manifest-based detection, or until its shell is back and the record ends. Which shell
depends on whether a process has named the agent. For one it has (a `kimi` or an `opencode`
started from a prompt), any shell in front of it is that, the pane's own or one started
inside the pane. For one it has not (a wrapper script, an agent under `node`, one the identity
table does not name), the pane's own shell has to be idle, which the watcher asks `ps` and
`pgrep` at most once a second, by the pane's pid, as herdr's `pane_shell_is_idle` does: the
pane's process leads the terminal's foreground process group, nothing else is in that group
(its members come from `pgrep -g <group>`, with `-a` on macOS, which makes `pgrep` list its own
ancestors, which it leaves out by default and so lists the group short for a `taw` that runs inside it),
and its name is a shell. A script that runs the agent shares the shell's group or leads one of
its own, and a shell started inside the pane leads one of its own, so none of them is the
shell coming back, though tmux names each of them as the pane's command. A shell that is not
in `SHELLS` (the list in `watch::state`, herdr's) is not known to be back, so the record stays,
and so does it when `ps` or `pgrep` cannot say.

OpenCode reports from the TUI of each pane. Its state comes from a plugin loaded into the
TUI (`taw-tui-session.js`, registered in `tui.jsonc`/`tui.json` for OpenCode 1 and `cli.json`
for OpenCode 2), because OpenCode 2's one server cannot tell which pane a session belongs to;
the server plugin reports only for `opencode run` and Mini, which have no TUI. Both call
`report-agent` and `report-agent-session` with `$TMUX_PANE`, so the pane is hook-owned as for
any state-reporting integration.

Most integrations report nothing but the session (`report-agent-session` alone). That
verb writes no state and no `@agent_source`, so the screen still decides the state; it
records `@agent_kind`, the session, and `@agent_reported_kind`, the agent's word that it
is there. The watcher holds an agent that no process names to that word: the pane is kept,
with its session id, until the pane's own shell is idle, and it is that session id `session
restore` resumes. For an agent a process does name the word changes nothing, and the ordinary
rules end it. When the watcher records another agent over one whose hook reported, the session
and the word go with the last one: they were about that agent. A report from another agent takes
them away too: `report-agent` and `report-agent-session` drop the session id, the session path,
what started the session (the `session_start_source` token) and the word of the agent the pane
records when they name a different one, and then write what they carry. A pane that records no
agent has none to take it from, so a session id a restore wrote there stays. `taw sandbox` takes
those off as it starts, with the command the agent reported that resumes its session
(`@agent_resume_source`, `@agent_resume_agent`, `@agent_resume_argv`), in the tmux call that marks
the pane with its profile: it is typed at
the pane's prompt, so a session the pane holds then is the last agent's, and a watcher that never
read the pane's shell between the two (`claude; taw sandbox`, or a line typed ahead) would take
the agent in the container for the same one, and a restore would resume that conversation outside
the sandbox. The watcher does the same where a container comes into a pane, which covers one typed
by hand too (ivy and the like, which leave no profile mark): it marks the pane with the runtime
(`@agent_container`) and takes those options off in one tmux call, while the pane holds the report
stamp its read saw, and asks for a save. Nothing in a container reports through tmux, so what the
pane holds as the container comes was reported from outside, by the plain agent before it.
`taw session save` keeps a pane's session with its profile (`@agent_sandbox`) and its runtime
beside it: `@agent_container`, or a container runtime in front of a pane with a session to resume
by its command, which the watcher has not read yet (`session::snapshot`'s `runtime_of`). A pane a
restore kept as it was saved (`@agent_restore_saved`) keeps all it was saved with. A restore
resumes a pane saved with a profile only through it (`taw sandbox` and the agent's resume
arguments), and one saved with a runtime and no profile, a container typed by hand, not at all
(`session::restore`'s `plan_of`), so no saved session is ever typed outside its sandbox.
Each verb writes everything it reports in one tmux call (the kind, the state and its
owner, the session), so a reader sees the pane before the report or after it, never the new kind
beside the last agent's session, or the kind without the word that keeps it.

## ID mapping

Public ids are herdr-shaped so plugins and scripts written for herdr keep working;
raw tmux ids are accepted everywhere too.

| tmux | public |
|---|---|
| session `$3` | `w3` |
| window `@7` (in session `$3`) | `w3:t7` |
| pane `%31` (in session `$3`) | `w3:p31` |

`tmux/ids.rs::parse` recognises exactly four shapes as *ids*: `$N`, `@N`, `%N`, and
`wN[:tN|:pN]`. Anything else (`session:window.pane` tmux targets, session/window
*names*, an `=exact` form) is not parsed here at all; it is handed to tmux itself
via `resolve_pane`/`resolve_window`/`resolve_session`, which ask tmux to do the
lookup. Agent commands additionally accept a live `@agent_name`.

**A tmux session literally named like a public id must be addressed by its raw
id.** Because `parse` tries the public-id shapes first, the bare string `w5`
always resolves as "session `$5`'s public id", never as "the session named
`w5`", even if such a session exists. To target a session that happens to be
*named* `w5`, address it as `$N` (its raw tmux id) instead. This is inherited
from the id-parsing rule above, not a bug: it is the same tradeoff herdr's own
public ids make, and it only bites a session name that collides with the public-id
grammar in the first place.

### The caller pane (`$TMUX_PANE`)

A plugin action or plugin pane is handed a `caller_pane_id`: *who* invoked it, as
distinct from what it was aimed at. herdr injects its own `$HERDR_PANE_ID` into the
panes it manages and trusts that value; taw instead reuses tmux's own `$TMUX_PANE`,
set by whichever server owns the pane the invoking process sits in. That is almost
always the server taw then addresses, but not guaranteed: under a
`TAW_TMUX_SOCKET_*` override, or with a second tmux server running, the inherited
`$TMUX_PANE` can name a pane on a *different* server. So taw confirms the pane is
live on the addressed server (a cheap `resolve_pane`) before exposing it, and drops
a foreign or stale id to `null` rather than handing a plugin (or `split_target`) a
pane id that belongs to another server (`cli/plugin.rs::validated_caller_pane`).

## tmux gotchas

Behaviour that is not obvious from the tmux manual, met (and, where noted,
worked around) in the watcher, session restore, and the test suite.
Anyone adding a new tmux command string or a new integration test should read
this list first.

- **`-f /dev/null` for every throwaway test server.** A fresh `tmux -L <name>`
  server still loads the *host's* `tmux.conf` unless started with `-f /dev/null`,
  including whatever `base-index`, hooks, or TPM autoload line the developer's
  own dotfiles set. Every test server in `tests/tmux_*.rs` and `tests/bats/*.bats`
  is started with `-f /dev/null` and a per-server unique socket name; skipping
  either leaks host state into a test that assumes a clean server.
- **`pipe-pane` and `display-message` strings are expanded twice.** Any text
  handed to either command goes through tmux's `#{...}`/`#[...]` format expansion
  *and* `strftime`, so a literal `#` must be doubled (`##`) or it starts a style
  directive (`#(...)` in particular *runs a shell command*), and a literal `%`
  must be doubled (`%%`) or `strftime` eats it (a bare `%12` pane id becomes the
  empty string; `A%B` becomes `ASeptember`). `watch::pipe_command_word` doubles
  both when it builds the `pipe-pane` command string;
  `notify::tmux_message::format_message`'s `escape_tmux_text` doubles both
  in the notification *body* it builds from `detail`/`--body` text too.
  **`format_message`'s `kind_or_name`/`location` parameters are still passed
  through unescaped**, on the doc comment's stated assumption that they are
  "short ids/labels the caller controls … not free-form text". Of the three
  tmux options `notify::live_toast_context` reads for `kind_or_name`
  (`@agent_display`, then `@agent_name`, then `@agent_kind`, first non-empty wins), only
  `@agent_name` is actually charset-restricted (`cli::agent::validate_name`,
  `[a-z][a-z0-9_-]{0,31}`, enforced by `agent rename`/`agent start`); the other
  two are plain, unvalidated `String` CLI flags: `@agent_display` via
  `pane report-metadata --display-agent <TEXT>` and `@agent_kind` via
  `pane report-agent --agent <LABEL>` (the verb every integration hook script
  calls to report identity). `@agent_kind` is also the fallback used whenever a
  pane has no custom name or display set, i.e. the common case. A pane-less
  `taw notification show <TITLE>` adds a fourth, CLI-direct route: it falls back
  to `notify::split_title`, which hands its own unrestricted positional through
  as `kind_or_name` whenever the title carries no `" blocked · "`/`" done · "`
  marker, which covers every title a caller supplies. All of this fires under
  `[ui.toast] delivery = "herdr"` (the default). Audit before adding a new
  `display-message`/`pipe-pane` call with caller-controlled text in it; the
  general shape of this gap is still open.
- **A tab character in a user-settable option breaks a naive tab-split parse.**
  `@agent_display`, `@agent_title`, a renamed session, and a pane title straight
  off an OSC 2 sequence are all free text a user or a hook can set to anything,
  including a literal tab, and the watcher's one-round-trip `display-message`
  format is tab-separated. tmux's own `#{s/\t/ /:<var>}` format modifier
  (global substitution, verified on tmux 3.6b, including on `@`-prefixed user
  options) de-tabs every free-text column before it is joined, which is what
  keeps the split positional. The modifier pattern must not itself contain `:`, since
  tmux ends the modifier at the first one, so `s/[[:blank:]]/ /` silently expands
  to nothing.
- **`display-message -p -t <missing pane>` succeeds, with empty fields: it does
  not error.** On tmux 3.6b this returns exit 0 and a string with every column
  expanded to nothing, *including the format's own literal tab separators* (a
  13-column format returns exactly 12 tab bytes, not an empty string; verified by
  hexdump). Treat "every column blank" as the missing-pane signal, not an empty
  string and not a non-zero exit: both of the latter tests pass for the wrong
  reason.
- **`#{session_path}` is frozen at session-creation time.** It never reflects a
  `cd` a user makes afterward in that pane. Session restore uses each window's
  first pane's own saved `cwd` (its live `#{pane_current_path}` at snapshot time),
  never `session.path`, to seed `new-session -c`/`new-window -c`; falling back to
  `session_path` would restore a pane at wherever the session merely *started*,
  not where its first pane actually was.
- **Every `.` and `:` in a session name becomes `_`.** Confirmed empirically on
  3.6b: `new-session -s a.b:c` creates a session literally named `a_b_c`, and `-s
  .foo` creates `_foo`, most likely so a session name can never be mistaken for a
  target (`session:window.pane`) or a relative path. tmux does it silently:
  `new-session` succeeds, under the other name, and says nothing. This matters in
  practice at both ends of a worktree's `<repo-basename>--<branch>` name: a
  dotfiles repo lives at a dot-prefixed path (`~/.dotfiles`), and a release branch
  is `release/1.2`, so `tmux::session_name` is the one rule that applies the fold,
  shared by `session::worktree` and the TUI's `session_name_for`. It is not
  sufficient on its own: tmux also refuses some names outright, so anything that
  creates a session reads the real name back with `new-session -P -F
  '#{session_name}'` and records *that*.
- **tmux expands formats in a start directory and in the name it makes or renames a
  session or window with, and not in a target.** Verified on 3.6b: `-c` of `new-session`,
  `new-window` and `split-window`, `-d` of `display-popup`, `-s` and `-n` of `new-session`,
  `-n` of `new-window`, `rename-session` and `rename-window` all expand (`new-session -s
  C#Sharp` makes `Charp`, since `#S`, the name of a session not yet made, is nothing; `new-window
  -n W#Sx` makes `W<the session's name>x`); `break-pane -n` and every `-t` do not (`-t =C#Sharp`
  is that session). So every folder goes through `tmux::start_directory` and every such name
  through `tmux::new_name`, which write `#` as `##`. And a word that ends in `;` ends the command
  wherever it is, a target's included: those two, `tmux::session_target` and `break-pane -n`
  write a trailing `;` as `\;` (`store::word`).
- **`exit-empty` kills the whole server when the last session closes.** tmux's
  default behaviour: dropping to zero sessions ends the server process, not just
  that session. Anything that intentionally passes through zero sessions on a
  test server (e.g. killing a session outright so a restore test can recreate it)
  must first `set-option -g exit-empty off`, or the server (and every other
  session on it) disappears with it.
- **tmux says a pane is gone in two ways.** `capture-pane`, `send-keys`,
  `split-window` and `select-window` fail with `can't find pane: %7` (or `window`,
  `session`); `show-options -p` and `set-option -p` fail with `no such pane: %7`.
  `session::prompt::is_gone` reads both, and a command that has to go on when a
  pane closed under it (restore, `agent start`) asks it rather than counting any
  failure as the pane being gone. A pane's shell can end at any moment, the first
  pane of a restored window's included, and tmux takes the pane with it, so restore
  names a window by its own `@id` once it is made and never by a pane that was
  merely the first in it (`restore_selects_the_saved_window_when_its_first_pane_ended_before_the_selection`
  ends that pane on purpose).
- **`#{window_activity}` is whole seconds, and any pane's output in the window
  moves it.** Output in the second `S` may have come up to a second after `S`, so a
  window has been quiet for at least `now - (S + 1)`. `session::prompt` uses that to
  know a pane's screen has not moved for 300 ms without looking twice, and looks
  twice (two `capture-pane` reads that far apart) only when the window drew
  recently. Verified on tmux 3.6b.
