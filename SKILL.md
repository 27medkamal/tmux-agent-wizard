---
name: tmux-agent-wizard
description: "Control tmux-agent-wizard (`taw`), which gives tmux awareness of the coding agents running in its panes. Use only when the user explicitly mentions taw/tmux-agent-wizard or asks to use it to inspect or control panes, tabs, workspaces, commands, or another agent. Do not use merely because a task could benefit from a background terminal, delegation, or parallel work. Requires running inside tmux."
---

# tmux-agent-wizard (`taw`)

`taw` organizes tmux into workspaces, tabs, and panes, recognizes coding agents running inside panes, and exposes the current server through the `taw` CLI. tmux itself is the database: state lives in `@agent_*` tmux options, so there is no daemon and no socket.

Before issuing any control command, verify that this agent is running inside a tmux pane:

```bash
test -n "${TMUX:-}"
```

If the check fails, say that you are not running inside tmux and stop. Do not inspect or control a tmux server from outside it.

When the check passes, call `taw` by name: the plugin puts it on the `PATH` of every pane and window opened from inside tmux after it loads (a pane that was already open when it loaded, or the first pane of a session started from a terminal, may not have it). When `command -v taw` finds nothing, use the full path of the plugin's link instead, `${TAW_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/tmux-agent-wizard}/bin/taw`, with the same arguments. The `taw` binary talks to the tmux server named by `$TMUX`. Use it to inspect neighboring work, create terminal layout, start agents and commands, read output, and wait for state changes.

## Learn the current CLI

The installed binary is the authority for command syntax. Start with:

```bash
taw --help
```

Then print the relevant command group by running the group without a subcommand:

```bash
taw agent
taw pane
taw layout
taw workspace
taw tab
taw worktree
taw notification
taw hooks
taw session
taw integration
taw plugin
taw terminal
taw popup
taw watch
```

Do not probe a mutating nested command by omitting arguments. Commands such as `taw workspace create` are valid with defaults and will execute.

Two verbs act on what the user is looking at rather than on a target you name, so run them only when the user asked for that:

- `taw session open <path>`: switch to the tmux session already rooted at `<path>`, or create one there and switch to it. It moves the user's client.
- `taw ui open`: open the agent popup on the user's client, sized to leave their status line visible. `taw ui` without `open` is different and you must not run it: it starts the interactive popup in *your own* pane and blocks until someone quits it.

Watchers are the processes that read each pane for `taw`, one per pane, and each keeps running the `taw` it started with until it is started again. `taw watch status` only reads: it lists every pane with whether a watcher is alive on it, and marks the watchers that run an older `taw` than the one installed (`binary` is `old`, `binary_note` says why, `old` counts those a restart would start and `restart` names the command). `taw watch attach-all --restart` starts those again. It stops and starts the user's watchers, and `taw watch attach-all` without the flag attaches one to every pane that has none, so run either only when the user asked, for example after they rebuilt `taw` and say a pane still behaves as before. `taw watch attach`, `detach` and `run` belong to the plugin's hooks: do not run them.

`taw open` is the same switch for a person typing at a shell: it takes a directory or zoxide terms, prints nothing when it works, and outside tmux attaches its own terminal. Use `taw session open` instead, which prints an envelope.

Almost every command returns one JSON envelope on stdout (the exceptions are noted below). Read identifiers and state from those responses instead of predicting them:

```json
{"id":"cli:pane:list","result":{"type":"pane_list","panes":[]}}
{"id":"cli:pane:get","error":{"code":"not_found","message":"pane 'w9:p9' not found"}}
```

Errors carry a stable `error.code`: `not_found`, `invalid_argument`, `unsupported`, `tmux_error`, `timeout`, `config_dir_missing`, `trust_required`, `plugin_build_failed`, `unsupported_plugin_version`, `checkout_unreadable`, `not_a_git_repo`, `session_exists`, `workspace_group_close_required`, `worktree_dirty`, `agent_explain_file_read_failed`, `hook_refused`, `shell_not_ready`, `agent_not_started`, `agent_prompt_stalled`, `agent_not_running`, `agent_not_found`, `agent_not_ready`, `invalid_resume_argv`, `resume_not_accepted`, `machine_unreachable`, `internal`. Runtime errors exit 1; command-line syntax errors exit 2 with usage text on stderr. Add `--pretty` to indent the envelope while reading it by hand.

Two commands are exceptions and print their payload raw, because they exist to be redirected into a file: `taw skill` (this markdown) and `taw config default` (a commented `config.toml`). A third, `taw open`, is for people at a shell and prints nothing when it works. A fourth, `taw sandbox` typed with an agent or nothing (not `taw sandbox new` or `taw sandbox list`, which print the envelope), is too: it runs an agent in a container in the pane it is typed in and returns when the agent exits, so never run it to see its help in your own pane. To start an agent in a sandbox, use `taw agent start ... --sandbox`, below.

## Understand layout, panes, and agents

Choose the primitive that matches the job:

- Workspace, tab, and pane topology organize terminal locations. A workspace is a tmux **session**, a tab a tmux **window**, a pane a tmux **pane**.
- Pane commands control raw terminals, shells, tests, servers, input, and output.
- Agent commands control the recognized coding agent currently occupying a pane.

A pane exists whether or not it contains an agent. `agent start` requires an existing available shell pane and never creates, splits, or moves layout. Use pane commands for ordinary processes. Use agent commands when `taw` must validate agent identity or interpret `idle`, `working`, `blocked`, `done`, and `unknown` lifecycle states.

Agent commands accept either a unique live agent name or the pane ID currently hosting that agent. Names must match `[a-z][a-z0-9_-]{0,31}` and be unique among live agents: `agent rename` and `agent start` refuse one that another pane's live agent has, with `invalid_argument` naming that pane, and `agent start` does so before it types anything. The popup's rename (`r` on an agent row, then `a`) refuses one by the same rule, with its own message. A pane whose own shell is at its prompt, or that was kept after its program ended, holds no live agent, whatever it records. A name follows the current pane occupant and is cleared when that agent exits, is released, or is replaced. `agent start` assigns the initial name; an agent detected in a pane by other means (started by the user, not by `taw`) has no name until you set one with `agent rename <target> <name>` (or `agent rename <target> --clear` to remove it).

Lifecycle state lives in the pane's `@agent_state` tmux option (`idle`, `working`, `blocked`, `done`, or `unknown`) and every `agent`/`pane` record reports it as `agent_status`. `idle` means the agent is ready for input and its pane has been focused since it finished (tracked by `@agent_seen`). `done` is the same underlying idle state after *unseen* background work finishes. An agent becoming ready is no finished work: the first `idle` after it starts, is restored, or moves to another session (`/clear`, `/new`) is `idle`, never `done`, unless the agent worked first, and a new session turns a `done` back into `idle`. Focusing the pane or its window marks it seen; CLI reads do not. `blocked` means `taw` recognized an approval or question UI. `unknown` means an agent is present but `taw` cannot classify it confidently; it does not prove completion.

Use `taw agent explain <target>` to see exactly which detection rule produced the current state.

## Use IDs and caller context

Public IDs are herdr-shaped, stable handles:

- workspace: `w1`
- tab: `w1:t1`
- pane: `w1:p1`

Raw tmux ids (`$1`, `@1`, `%1`), tmux targets (`session:window.pane`), and live agent names are accepted everywhere too. The `terminal_id` field of a pane record is its raw tmux pane id (`%N`), the handle to pass to plain `tmux` commands.

Closed tab and pane IDs are not reused. A pane moved into another workspace receives a new workspace-qualified pane ID. After `pane move`, continue with `.result.move_result.pane.pane_id` or the live agent name; the old value is reported as `.result.move_result.previous_pane_id`.

tmux sets `$TMUX_PANE` in every pane, which is the caller's own pane:

```bash
printf '%s\n' "$TMUX_PANE"
taw workspace id          # the workspace holding this pane
taw pane current          # this pane's full record
```

Discover live state with:

```bash
taw workspace list
taw tab list --workspace "$(taw workspace id | jq -r .result.workspace_id)"
taw pane current
taw pane list
taw agent list
```

Creation responses expose the IDs to use next. `workspace create` returns `.result.workspace`, `.result.tab`, and `.result.root_pane`. `tab create` returns `.result.tab` and `.result.root_pane`. `pane split` returns the new pane as `.result.pane`.

## Act on another machine

`taw --machine NAME <verb>` runs a verb on a machine the user listed under `[machines]` in `config.toml`, over ssh, and prints that machine's own answer: `taw --machine studio agent list`. `--machine` goes before the verb. The ids are that machine's: a pane `%16` or `w1:p2` in its answer is not the pane with that id here, so never pass one machine's id to another's verb.

`taw --machine NAME` refuses, on stderr with status 2 and no envelope, the verbs that belong to this machine: `ui`, `app`, `machine`, `agent attach` and `open` without `--json`. `taw app` is a screen for a person: never run it from an agent's pane.

Run `taw machine add` and `taw machine remove` only when the user asks: they add a machine to the user's `[machines]` in `config.toml`, or take one out.

A forwarded verb waits as long as the far command runs, as a local one does: there is no time limit and `Ctrl-C` ends it, so `agent wait` and `agent prompt --wait` can run long. When no answer came the code is `machine_unreachable`: ssh could not log in or run, no taw is there, or the far taw ended by a signal (ssh reports 255 for that). Nothing is retried: look again before you try once more. `taw machine check` says what each machine found.

`taw image paste --pane <id> [--machine <name>]` pastes the image on this machine's clipboard into a pane, here or on a machine (it runs `taw image receive --bytes <n>` there through ssh, which pastes only when all `n` bytes arrive): the image is written to a file in taw's state folder and its path is pasted into the pane as a bracketed paste (a backslash before each space the path holds), which Claude Code and Codex attach as an image. The clipboard tool has 10 seconds to answer; each paste removes the `image-<digits>.png` files it wrote there that are over 7 days old and leaves other files in `images/` alone. The list's key (`ctrl-v`, or paste image in the row's menu) runs this verb in a process of its own and asks an agent that is waiting on a question to be answered first. `taw image paste` and `taw image receive` paste into any pane they are given, a shell's too; only the list limits the paste to an agent's row.

## Start and coordinate an agent

Default to a sibling pane in the current tab and the current working directory. Do not create a workspace, tab, worktree, or different cwd unless the user explicitly requests that topology or location.

Honor a direction requested by the user. Otherwise inspect the caller pane's tab:

```bash
taw pane layout --pane "$TMUX_PANE"
```

Split a wide pane to the right and a narrow or tall pane down. Avoid repeated same-direction splits that create unusably narrow columns or short rows. Keep the user's focus in the calling pane and explicitly preserve the caller's working directory:

```bash
taw pane split "$TMUX_PANE" --direction right --cwd "$PWD" --no-focus
```

Replace `right` with `down` when appropriate. Read the new pane ID from `.result.pane.pane_id`.

An available shell pane must be at its interactive prompt, with the shell itself in the foreground and no foreground command, editor, or agent running. Start a supported agent in that pane with a useful unique name:

```bash
taw agent start reviewer --kind codex --pane <returned-pane-id>
```

Use the kind requested by the user. Run `taw manifest list` to see the agent kinds this build can detect. Pass native agent arguments only after `--`:

```bash
taw agent start reviewer --kind codex --pane <returned-pane-id> -- <agent-args...>
```

When the user asks for an agent in a sandbox, add `--sandbox` (and `--profile <name>` if they named one) to `agent start`, and allow for a first start that takes minutes: its wait defaults to 300 seconds.

A successful `agent start` returns only after the typed command has really started the expected agent in the same pane: the pane's own shell is no longer in front of the terminal, and either the process in front is named for the agent, or the agent's own integration has reported it, or the pane's watcher has read a state for it off the screen (any state but `unknown`, which a new agent shows for its first 3 seconds). The watcher reads only what is drawn below the line the command was typed on while that line is on the screen, so the frame an agent that died left above it is not taken for the new agent, and a new agent reads `unknown` until it draws. An agent that runs under `node` or a wrapper script, which no process name gives away, is told by its own integration's report or by the watcher, which the tmux plugin attaches to every pane. What the pane recorded of its last agent is taken off before the command is typed, so an agent that died and is started again in the same pane is read as a new one. `--timeout` (default 30 seconds, at most 300) is one budget for the whole verb: the wait for the shell counts against it, and the agent has what is left. It reports `timeout` if the agent never appears. If the pane's shell is back at its prompt before the agent is there (the command is not installed, or the agent exited at once) it reports `agent_not_started` at once and puts back the pane's agent hint: read the pane with `taw pane read <pane-id>` to see what the shell said.

`agent start` types the agent's command only once the pane's shell is at its prompt. A pane made a moment ago may have a shell that is still starting, and anything typed then goes to whatever reads the terminal first, such as a question the shell's own startup asks. A shell gets 15 seconds of the budget, or all of it when that is less, and if it is not at its prompt by then `agent start` reports `shell_not_ready`, having typed nothing. Read the pane with `taw pane read <pane-id>`: if a program is asking something, leave the answer to the user, and do not type the command with `agent prompt` or `pane run` instead. A pane in copy mode (or another tmux mode) or with its input turned off is refused at once with `shell_not_ready` and a message that says which ("pane w1:p3 is in copy mode; leave it and try again"); `agent prompt` and `agent send-keys` refuse it the same way, also when it goes into the mode between their check and the send. The user may be reading their scrollback: tell them, and do not take the pane out of its mode yourself. When the message adds that the command (or the prompt) may be left on its line, the pane went into the mode after it was typed: tell the user that too, since the line may need clearing before the next try. `agent start` types the command, and presses Enter, only while the shell it found at its prompt is still in front of the pane: when a program came in front in between (one the user started), it reports `shell_not_ready` saying the pane "had another program in front of its shell", and adds that the command may be left on its line when that program came in front after the command was typed. Read the pane, and leave the program to the user. A pane whose process is not itself a shell (ssh, `docker exec`, a wrapper that does not exec), a shell that draws nothing (an empty `PS1` in a blank pane) and a question the shell asks itself with a `read` of its own (zsh's `compinit` question, the oh-my-zsh update prompt) are never typed into, and are not told from a shell that is not ready. The check needs `ps -o pgid=,tpgid=,comm=` and one `pgrep -g` for the members of the terminal's foreground group (taw adds `-a` on macOS, which keeps `pgrep` from leaving out taw's own ancestors, which would list the group short when taw itself is inside it), which macOS and Linux have; where `ps` has no `tpgid` column nothing is typed.

Submit work through the agent surface:

```bash
taw agent prompt reviewer "Review the current diff and report only actionable findings." --wait --timeout 120000
```

`agent prompt` sends the text and then Enter. It does not type into a pane whose agent has gone, nor does `agent send-keys`: the text would land at the shell's prompt, and a shell may run it as a command. Both answer herdr's codes before anything is typed: `agent_not_ready` for a pane that hosts no agent or whose agent has gone from it, and `agent_not_found` for a target that resolves to no pane. A pane hosts its agent only while a process in front of its shell is positively that agent, by its name or its arguments (`node /path/to/bin/codex` is codex): a shell at its prompt, a shell you started inside the pane (`sh`, `ssh`, `nix develop`, `sudo -s`), a pager, an editor or `cat` is no agent and is typed nothing, whatever the pane records, so an agent you started by hand that has exited is refused even once you have started something else in its place. An agent only its kind hint names (`agent start` of a script, or one a session restore resumed) hosts it while the process that was recorded for it is in the foreground of the pane's terminal: a suspended agent (C-z) and a dead one do not, a number the system has since given to a program of another pane does not, and a shell or a program you started in front of it is not it. The send is guarded in the tmux call that makes it, on the command in front of the pane still being the one that was checked, so an agent that exits in between is typed nothing either, though a command of the same name taking its place is not told apart. (`agent_not_running` is for waits.) For normal agent work, `--wait` is enough: it waits for the first settled `idle`, `done`, or `blocked` state. Do not repeat those defaults with `--until`.

With `--wait`, a prompt sent to an agent that is not working must produce observed `working` or `blocked` activity. After the text is sent, `taw` looks for it for up to 5 seconds, and an `idle` or `done` the agent was already in, or any other change, does not satisfy this gate. It returns `agent_prompt_stalled` if no activity is observed, or `timeout` if `--timeout` expires first. A stalled prompt may still have been delivered: read the agent with `taw agent read <name>` before sending it again. `--timeout` is one budget for the sending, the look for activity and the wait after it; without `--timeout` the settled-state wait is indefinite once activity is observed. This wait tracks lifecycle state, not an individual turn: if the agent is already working, the end of the turn it is in satisfies it. The text is sent to an agent that is `blocked`, since answering its dialog is often what the caller wants, and the `blocked` it was already in is not activity, nor is a report that says again the state the agent was already in. An agent that leaves its pane while `taw` looks for activity or waits for the end of the turn, or that another agent takes the pane from, ends the wait with `agent_not_running`.

Use `--state` (herdr's `--until` is accepted as an alias) only for a state-specific workflow, such as waiting for an already-running agent to request input:

```bash
taw agent wait reviewer --state blocked --timeout 120000
```

Without `--state`, standalone `agent wait` uses the same settled-state defaults as `agent prompt --wait`. Without `--timeout`, a wait is indefinite while the agent is there: one that leaves its pane (it exited and its record was taken off, the pane was closed, its process is gone for an agent `agent start` started, or the pane's own shell is back at its prompt with nothing in front that identifies as the agent, which is how a wait ends for an agent you started by hand when no watcher takes its record off), or that another agent takes the pane from (another kind, another process, or another session where no recorded process shows it is the same one: `/clear` and `/resume` start a new session in the same process, and the wait goes on), ends the wait with `agent_not_running`. The exception is an agent that left with a status the wait was for: the wait then returns that status, as herdr's does for an agent released with it. An agent that dies in the middle of a turn ends that turn as `done` (or `idle`, when you were looking at its pane), then reads `unknown`; a default `agent wait` returns that `done`.

A timeout does not prove the prompt was never delivered. Read the agent (`agent get`, `agent read`) before deciding what to send, and do not submit the same prompt again blindly.

Use logical keys for interactive agent UI controls:

```bash
taw agent send-keys reviewer esc
taw agent send-keys reviewer ctrl+c
```

`taw` validates all keys before writing any bytes, and refuses an agent that has gone with `agent_not_ready`, as `agent prompt` does. Read the result through the resolved agent:

```bash
taw agent get reviewer
taw agent read reviewer --source recent-unwrapped --lines 120
```

If a wait fails or returns `blocked`, inspect `agent get` and `agent read` before deciding what input to send. Inspect the blocked UI and ask the user before answering an approval prompt. Use the pane surface only when raw terminal control is intentional.

## Make a sandbox profile with the user

Do this only when the user asks for a sandbox profile. A profile is a script in
`~/.config/tmux-agent-wizard/sandbox/` that `taw sandbox` runs. Start from taw's default and
change as little as the answers need.

Ask these in one message, each with its default, and wait for the answers:

1. Which agents will run in it? (default: Claude)
2. Should they ask before running commands? (default: no; the sandbox is the fence)
3. What may they reach: everything but this machine (default), or only the internet? Any port
   on this machine they need, such as a local model proxy or a database?
4. Do they need Docker: no (default), look only, or everything, which is the same as giving
   them this machine?
5. Their settings: the user's own, shared (default; the sandboxed agent can change them), or a
   separate set?
6. Logins: already logged in on this machine, log in inside on the first start, or a key from
   an environment variable (which variable)?
7. Any tools the project's `mise.toml` does not install?
8. A name for the profile.

Then:

- Run `taw sandbox new <name>` and read the file's path from `.result.path`.
- Uncomment the recipe lines the answers need, and change nothing else. An agent with no
  recipe gets one entry in the `case`, modelled on the others. Never write a secret's value
  into the file: put the variable's name in the `pass` recipe line, and its value comes from
  the user's environment at start.
- Check it without starting anything: `TAW_SANDBOX_PRINT=1 <path> <agent>` prints the command
  the profile would run.
- Show the user the lines you changed and the printed command.
- Tell them how to start it: `taw sandbox <agent> --profile <name>`, typed in the pane where
  the agent should run (a split or a window for it is tmux's own keys first). Start it
  yourself only if they ask, in a pane you split:
  `taw agent start <agent-name> --kind <agent> --pane <id> --sandbox --profile <name>`.
- To make it the one `taw sandbox` uses when no profile is named, they set `profile = "<name>"`
  under `[sandbox]` in their `config.toml`. Tell them; do not edit their config unasked.

## Run an ordinary command in another pane

Create a sibling pane with the same geometry rule, preserve the caller's working directory, and keep user focus unchanged:

```bash
taw pane split "$TMUX_PANE" --direction right --cwd "$PWD" --no-focus
```

Read the new pane ID from `.result.pane.pane_id`, then run and inspect the command:

```bash
taw pane run <returned-pane-id> "just test"
taw pane wait-output <returned-pane-id> --match "test result" --timeout 120000
taw pane read <returned-pane-id> --source recent-unwrapped --lines 120
```

`pane run` sends command text and Enter in one call. `pane wait-output` searches the selected snapshot immediately, so output that already exists can match. Use `--match <text>` for a literal substring or `--regex <pattern>` (also spelled `--pattern`) for a Rust regular expression. Omitting `--timeout` allows an indefinite wait.

`pane clear <pane-id>` empties a pane's scrollback and, when bash, zsh or fish is at its prompt with nothing else in front of it, clears its screen too: the shell is sent Ctrl-L and redraws its prompt and the line being typed at the top. A pane with a program in front keeps its screen and is sent nothing, and a pane on the alternate screen is left alone. The result says what was done (`history_cleared`, `screen_cleared`, and a `note` when something was left alone). It throws away output that `pane read` could have returned, so read what you need first, and clear only a pane you created or were asked to clear.

Pass environment variables to the new process at split time; a pane's environment cannot be changed afterwards:

```bash
taw pane split "$TMUX_PANE" --direction down --env RUST_LOG=debug --no-focus
```

Use the read source that matches the task:

- `visible`: the currently rendered viewport.
- `recent`: the viewport plus scrollback (the default).
- `recent-unwrapped`: the same with soft-wrapped lines joined; prefer it for logs and transcripts.
- `detection`: the plain-text snapshot the detection engine itself reads.

Use `--format ansi` (or `--ansi`) when colors and terminal styling are evidence. Otherwise use text.

`--lines` asks for more rows from the pane's scrollback. If increasing it does not reveal more of a completed response, the pane is probably running the agent on the terminal's alternate screen. Rows that leave the alternate screen never enter tmux's scrollback, so a larger line count cannot recover them.

After that failed read, ask the agent to write its complete response as Markdown in a temporary directory and reply only with the file path, then read the file directly. Use this only as a fallback; do not request file output in the initial prompt.

## Surface something to the user

`taw notification show <title> [--body <text>]` delivers through the user's configured toast/sound channels ([ui.toast]/[ui.sound] in config.toml) instead of printing to your own transcript. Use it for something the user should notice even if they are not looking at this pane right now; it does not target or affect any particular pane.

`taw hooks test <event>` runs the user's `[hooks]` commands for that event against a sample and reports each one's exit status and stderr. It runs them for real, so a hook that sends a message sends it: run it only when the user asked to check their hooks. Every event of the sample has a pane, a window and a session id, and they name nothing on any tmux server, and `TAW_TEST` is `1`, so a hook that acts on the pane finds none. `TAW_SESSION` is `=taw-hooks-test:`, an exact session name, so a hook that acts on it finds no session either, unless the user has a session called exactly `taw-hooks-test`, which the `[ -n "$TAW_TEST" ] && exit 0` guard is for. `--pane <id>` swaps in that real pane's ids, so a hook that acts on the pane then acts on it: never pass one the user did not name. It refuses an event about something closing, being removed or ending unless `--force` is added, since the hook would run on a live session, pane and directory; `--force` is the user's to add, never yours.

`taw agent view set --source <id> [--label <text>] [--filter <json>] [--sort <json>]` narrows and reorders the agent list the popup shows, under a label naming who asked; `taw agent view clear --source <id>` removes it. `--source` is required and is what `clear --source` matches, so one caller cannot tear down another's view. A filter is `{"op":"all"|"any"|"not"|"eq"|"in"|"exists", ...}` over the fields `status`, `workspace_id`, `tab_id`, `pane_id`, `agent`, `seen`, `state_change_seq` or `{"token":"<name>"}`; it is fully validated before it is stored, so a rejected filter never becomes a view that silently shows nothing. Set a view only when the user asked for one: it changes what *they* see in the popup, not what you see.

## Safety and coordination rules

- This agent's authority is scoped to the single tmux server named by `$TMUX`. Never point `taw` at a different server, and never act on a session on this server that you did not create or were not asked to work in. Other sessions belong to other work, possibly another agent's.
- Use `--no-focus` for background work unless the user asked to switch context.
- Target `$TMUX_PANE`, an explicit pane ID, or a unique agent name. Omitting a target may act on the pane another client has focused, which can belong to the user or another agent.
- Parse IDs from JSON responses. Do not derive them from examples or from the popup's row order.
- Do not close, rename, clear, or send input to workspaces, tabs, or panes you did not create, unless the user explicitly asked. `workspace close --group` closes a workspace together with the worktree workspaces created from it; never add it merely to bypass `workspace_group_close_required`.
- A close or a worktree removal that fails with `hook_refused` was refused by the user's own `pre_close` or `pre_worktree_remove` hook. Read the message, tell the user, and do not retry it or work around it.
- A `worktree remove` that fails with `worktree_dirty` was refused because the checkout holds changes or submodules that a removal would delete, or because git no longer lists the worktree but part of its folder is still there (the far verb then deletes that folder itself). Tell the user and add `--force` only when they agree that what is there is lost.
- Use `--trust-repository` on `worktree create`, `open` or `remove` only after the user has verified the repository. It grants per-request Git trust; it is not a routine retry for a failed worktree command.
- Do not poll a pane or agent in a loop by repeatedly calling `get`/`read`. Block on `agent wait` or `pane wait-output` instead, and give them `--timeout` so a stuck agent cannot hang you indefinitely.
- Read `agent read`/`pane read` output before sending more input or reporting a result. Do not assume a command or prompt succeeded just because the call returned.
- Never run `tmux kill-server`, and never kill the pane you are running in. Use a throwaway server (`tmux -L <name>`) for experiments that need isolation.
- `taw` writes only `@agent_*` tmux options and files under its own config and state directories. Do not hand-edit those options; use the `pane report-*` verbs.
