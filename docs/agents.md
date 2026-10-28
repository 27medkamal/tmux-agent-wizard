---
title: Agents
description: The 22 coding agents tmux-agent-wizard recognises, Claude Code, Codex, OpenCode, Gemini CLI and more, how their state is read, and the optional hooks.
---

# Agents

Claude Code, Codex, Gemini CLI, OpenCode, Copilot CLI, Cursor, Amp and 15 more, through herdr's engine and rules. Plus rules of taw's own, so a Claude Code that is still working does not read as done.

## Integrations

`taw integration install|uninstall|status <agent>` installs a small hook script
into an agent's own config directory, so that agent reports its native session
identity (and, where it has lifecycle hooks, its own state) straight to `taw`
instead of relying on manifest-based screen detection for that pane. **The target
directory must already exist**: install refuses to fabricate an agent's config
dir, on the theory that a missing one means the agent itself isn't installed.

The targets are herdr's own eighteen, with the same directories and the same
`$ENV_VAR` overrides, so a machine already set up for herdr needs no new paths. The
hook script is named `taw-agent-state` rather than herdr's, and it calls `taw`.

| Agent | Config directory (override) | Writes | Reports |
|---|---|---|---|
| `pi` | `~/.pi/agent` (`$PI_CODING_AGENT_DIR`) | `extensions/taw-agent-state.ts` | session identity plus lifecycle (`idle`/`working`/`blocked`) |
| `omp` | `~/.omp/agent` (`$PI_CONFIG_DIR`, `$PI_CODING_AGENT_DIR`) | `extensions/taw-omp-agent-state.ts` | session identity plus lifecycle (`idle`/`working`/`blocked`) |
| `claude` | `~/.claude` (`$CLAUDE_CONFIG_DIR`) | `hooks/taw-agent-state.sh` + a `hooks.SessionStart[*]` entry in `settings.json` | session id, transcript path |
| `codex` | `~/.codex` (`$CODEX_HOME`) | `taw-agent-state.sh` + `hooks.json` + `hooks = true` in the `[features]` table of `config.toml` (below) | session identity |
| `copilot` | `~/.copilot` (`$COPILOT_HOME`) | `hooks/taw-agent-state.sh` + `settings.json` | session identity |
| `devin` | `~/.config/devin` (`$XDG_CONFIG_HOME`) | `taw-agent-state.sh` + `config.json` | session identity |
| `droid` | `~/.factory` | `hooks/taw-agent-state.sh` + `hooks.json` / `settings.json` | session identity |
| `kimi` | `~/.kimi-code` (`$KIMI_CODE_HOME`) | `hooks/taw-agent-state.sh` + `config.toml` | session identity, and lifecycle over 12 hook events |
| `opencode` | `~/.config/opencode` (`$XDG_CONFIG_HOME`) | `plugins/taw-agent-state.js`, `taw-tui-session.js` and `taw-opencode/tui.js`, plus an entry in `tui.jsonc` (or `tui.json`) and in `cli.json` | session identity plus lifecycle (`idle`/`working`/`blocked`), reported for the pane each OpenCode TUI is in |
| `kilo` | `~/.config/kilo` (`$XDG_CONFIG_HOME`) | `plugin/taw-agent-state.js` | session identity plus lifecycle (`idle`/`working`/`blocked`) |
| `hermes` | `~/.hermes` (`$HERMES_HOME`) | `plugins/taw-agent-state/{plugin.yaml,__init__.py}` + `config.yaml` | session identity |
| `qodercli` | `~/.qoder` (`$QODER_CONFIG_DIR`) | `hooks/taw-agent-state.sh` + `settings.json` | session identity |
| `qwen` | `~/.qwen` (`$QWEN_HOME`) | `hooks/taw-agent-state.sh` + `settings.json` | session identity |
| `cursor` | `~/.cursor` (`$CURSOR_CONFIG_DIR`) | `taw-agent-state.sh` + `hooks.json` | session identity |
| `mastracode` | `~/.mastracode` | `hooks/taw-agent-state.sh` + `hooks.json` | session identity plus lifecycle (`idle`/`working`/`blocked`) |
| `antigravity-cli` | `~/.gemini/config` (`$ANTIGRAVITY_CLI_CONFIG_DIR`) | `hooks/taw-agent-state.sh` + `hooks.json` | session identity, on `PreInvocation` |
| `grok` | `~/.grok` (`$GROK_HOME`) | `hooks/taw-agent-state.sh` + `hooks/taw.json` | session identity |
| `letta` (experimental) | `~/.letta` | `hooks/taw-agent-session.sh` + a `hooks.SessionStart[*]` entry in `settings.json` | session identity |

`uninstall` reverts each edited config file byte-for-byte, provided nothing else
touched it since install; `status` prints installed/not, the file path, and the
integration's version marker beside the one this build writes. `taw integration status`
with no agent reports every target at once, and `--outdated-only` lists the ones that
installing again would change: a script older than the one this build writes (each
integration has its own version), or an OpenCode 2 `cli.json` that exists without taw's
entry, and its `note` says which and what to run. After updating taw, run `taw integration
install <agent>` again for each it lists; for OpenCode that also moves an old single plugin
over to the pair below. An OpenCode 2 that has never run has no `cli.json` yet, and nothing a
reinstall does would change that: `status` says so in its `note`, and `--outdated-only` leaves
it out.

Codex asks once to review a new or changed hook (Hooks need review): trust taw's hook there, or
it never runs and codex sessions are not kept for a restore; taw never writes Codex's trust for you.

Codex only reads hooks with `hooks = true` under `[features]`, and it keeps a `[features]`
table of its own in `config.toml`, which it refuses to start on a second time. So an install adds
the line `hooks = true  # taw integration` right under your `[features]` header and leaves your
keys as they are; when the table already sets `hooks = true` it adds nothing, and when there is
no `features` at all it adds the two marked lines `[features]  # taw integration` and
`hooks = true  # taw integration` at the end. taw writes only marked lines, never a block with
start and end markers, because Codex saves its own switches into the `[features]` table it finds
and would save them inside such a block. An uninstall takes out the marked `hooks` line, then a
marked `[features]` header when nothing but blank lines is left under it (a key or a comment of
yours keeps it; a header without the mark is yours and stays). It stops, changing nothing and
saying where, when the table sets `hooks = false` (taw does not switch back what you switched
off), when `features` is written as dotted keys, an inline table, a quoted header, a
`[features.<name>]` sub-table or an array of tables (the message names which and its line, and
says where to add `hooks = true` by hand), or when the file does not parse: it parses what it
would write first, and never writes a file Codex could not load. An earlier taw appended a
delimited block (`# >>> taw integration >>>` to `# <<< taw integration <<<`) without looking for
your table, which left two `[features]` tables and a Codex that would not start. taw's own block
comes out on an install: it turns into your lines (its two marker lines and its `hooks = true`
go and the other lines in it stay; when you have no other `features`, that is all, and the
tables Codex saved inside the block, such as `[projects."<dir>"]`, `[mcp_servers.<name>]` or
`[notice]`, stay exactly where they are; when you have your own `[features]` header, the block's
keys, the lines between its header and its first table, move under yours and its tables stay
where they are too; comments and blank lines in a block are not keys of yours and stay where they
are). It comes out also when the install then stops for one of the reasons above,
and the message says both what it removed and why it stops. Run `taw integration install codex`
again to repair a file an older taw broke, and `taw integration status` calls such a file out of
date; its note says that Codex cannot load the config only when the file does not parse, and
otherwise that an older taw's block is in it and a reinstall tidies it. A repair keeps the file's
final newline, and taking a block out never joins the line before it to the line after it:
when text follows the end marker, one line break stays between them.

A reinstall does not repair these cases:

- A key set both in the block and in your own `[features]` table. The install stops and names
    both lines so you can choose one. `status` does not call the file out of date, since a
    reinstall alone changes nothing.

- Dotted keys, an inline table, or a value that is not a table for `features`, beside a block
    that holds keys of yours. Codex cannot load a file that defines `features` twice: the message
    names the block's lines and says to move its keys into your own `features`, in the form you
    wrote, and delete the block. A block that holds only comments and blank lines is repaired:
    the comments stay, and the install then stops with the note for that form.

- Two or more older blocks, when you have no `[features]` of your own and one of them holds a
    table Codex saved. The install stops and asks you to add `hooks = true` by hand.

- A block with a table in it and key lines of yours right after its end marker, beside your own
    `features`. taw cannot tell which table those lines are meant for, so the install stops, and
    the file stays unloadable for Codex until you move the block's keys and delete it. An
    uninstall stops too when the block holds keys of yours; otherwise it takes out the two marker
    lines and taw's own `hooks = true` only, so the block's `[features]` header stays and the file
    still does not load. A table header or a comment after the end marker is not such a line, and
    the block converts as usual.

- A file that does not parse for another reason. It is left as it is, with the reason.

An uninstall never stops on text meant for an install: where it
cannot convert the block it takes out at least the two marker lines and taw's own `hooks = true`,
or says in its own words what clashes. An uninstall straight from an older block takes the
block's `[features]` header with it when nothing but blank lines and comments is left under it
before the next table or the end of the file. Kimi's `config.toml` and Hermes's `config.yaml` are read the same way
before an install: one that taw's entries would break (a `hooks` that is not a list of tables, a
`plugins` key written in a shape taw's line edit does not know) is left as it is, with the
reason, except that taw's own older lines (Kimi's block, or the lines Hermes's edit inserted
where they made the file unreadable) are taken out first, and `status` calls the file out of date
while they are in it. For Hermes, `status` and the repair cover the three shapes taw can
recognise by the lines it wrote: `plugins` or `plugins.enabled` set to something that is not a
list, with taw's lines under it, and a quoted `"plugins"` key with taw's block appended.

An agent's config folder can be spelled more than one way: `~/.claude-home` and
`~/.claude-alt` may be the same directory through links, and the entry an install writes
carries the path as `$CLAUDE_CONFIG_DIR` was spelled then. `uninstall` finds taw's own hook
by the hook script's file name and by the path with every link in its folder resolved, so any
spelling of the same folder matches, and a reinstall replaces an entry written under another
spelling instead of adding a second.

Claude runs a hook's command through a shell, so for Claude a path that a shell would expand
from your home folder is read against it and judged the same way: `~/` outside quotes, or
`$HOME/` or `${HOME}/` outside quotes or inside double quotes (the slash after the variable may
be quoted any way, as in `"$HOME"/...`), as a config kept in a dotfiles repository spells it.
Words are split at a space, a tab or a newline, and at a `;`, `&`, `|`, `(`, `)`, `<` or `>`
outside quotes, and a `#` that starts a word (after a blank or after one of those) starts a
comment, which is not judged. That is recognised as taw's, even on a machine where the hooks folder
is not there yet (the path is then compared as text, as an absolute one is). An install over
such an entry keeps its path word as it is spelled when the word is the path alone, and
writes the rest of the entry in the current form, so a matcher or a timeout from an older
install is brought up to date, in the place the entry stood. When the word holds more than the
path (`~/...:echo`), taw writes its own entry, with the path as `"$HOME/..."`, instead. It also
drops an absolute copy of the same entry that an older install added beside it. An uninstall takes it out. The command is read as a shell reads it: a path in
single quotes (`'$HOME/...'`), a `~` in double quotes (`"~/..."`), an escaped `\~/...` or a
`~/...` that is only part of a longer word (`echo 'see ~/...'`) is not expanded by a shell,
and counts as a path taw cannot tell (below). The other agents' hook runners are not known to
use a shell, so for them a path that starts `~/`, `$HOME/` or `${HOME}/` also counts as one
taw cannot tell, and an install adds its own entry beside it.

Where the agent expands the home folder itself, an install writes paths home-relative when
they lie under it, so a config or a plugin that is copied to another machine, or kept in a
dotfiles repository, does not carry your home folder. The shell hook scripts name taw as
`"${HOME-}/..."` (with no `HOME` set, the script finds no taw and ends quietly), the
JavaScript and TypeScript plugins (OpenCode, Kilo, Pi, OMP) as `${process.env.HOME}/...`,
Hermes's plugin through `os.path.expanduser("~")`, and Claude's hook entry in `settings.json`
as `bash "$HOME/.claude/hooks/taw-agent-state.sh" session`. Every other agent's hook entry
keeps the absolute path, as does anything outside your home folder, and so does a path with a
`$`, a backtick, a backslash, a quote or a control character after the home folder: that is
written in full, as it always was. A script written with the absolute
path stays current: this changes how a path is spelled and nothing else, so no integration's
version moves and `status` does not ask you to install again. A hook that names
`taw-agent-state.sh` (or `taw-agent-session.sh`) by a path that cannot be told to be the
script taw installed (a folder that has gone, `./hook.sh`, `~other/...`, another variable, a
bare name, a `~/...` or `$HOME/...` that a shell would not expand or that belongs to an agent
not known to run a shell, or `~/...` with no home folder known) might be taw's or might not, and `uninstall`
refuses it with the error code `invalid_argument`, naming the file and the path, and changes
nothing: not the config, not the hook script, not the saved copy of the config. Remove that
hook by hand if it is taw's, and uninstall again. The saved copy goes only when the config was
restored from it or taw's entries were removed from the config: a config that holds nothing of
taw's and is not what the copy says it was is left as it is, with the copy.

Install and uninstall write every file they touch by staging a copy beside it and
renaming that into place, so a write that fails partway (a full disk, a read-only
directory) leaves the original exactly as it was rather than empty or half written. A
config that is read-only for you is refused and left as it is, even when its directory is
writable (a rename would otherwise replace it whatever its own permissions say). Every
config an install edits is checked before anything is written, so a `settings.json` that
is not valid JSON, has a second hard link or is read-only stops the install with nothing
half installed: no hook script, no backup copy, no edit to a second file. A
symlinked config (a dotfiles link) is written through and stays a symlink, keeping its
permissions, and if install created the file behind the link, uninstall removes that file
and leaves the link; a config with more than one hard link is refused, since replacing one
of its names would leave the others holding stale content. Hermes' `config.yaml` may list its
enabled plugins inline (`enabled: [a, "b"]  # note`): install and uninstall handle that
shape, keeping the other entries' original quoting and the trailing comment (the list may
come back as a block list).

OpenCode is two plugins, as in herdr, because the process that knows which pane an
OpenCode is in is its TUI, and OpenCode 2 runs one server for every pane. `taw-tui-session.js`
runs in each OpenCode TUI and reports the session that TUI shows, and its child agents, for
that TUI's own pane: `working` while a turn or a child agent runs, `blocked` on a pending
permission or question or a failed run, `idle` once a turn has finished or been interrupted.
`plugins/taw-agent-state.js` is left with `opencode run` and Mini, which have no TUI plugin.
The TUI plugin is registered by one entry in your own OpenCode config, `tui.jsonc` (or
`tui.json`, if that is where your plugins already are) for OpenCode 1 and `cli.json` for
OpenCode 2. taw adds it in place: your comments, layout and other settings stay as they were, a
config it cannot edit stops the install before anything is written, and `uninstall` gives each
file back as it was, or takes out only taw's entry from one you have changed since. OpenCode 2
moves the settings of an old `tui.json` (or of a `kv.json` in its state directory) into a
`cli.json` the first time it starts, and only while there is none; while either exists the
install leaves `cli.json` alone and says so (so does `taw integration status`), and after
OpenCode 2 has started once, running `taw integration install opencode` again registers it.
**Restart OpenCode after you install or update the integration**, so that it loads the plugins.

The Claude entry is registered for Claude's own five `SessionStart` sources only
(matcher `^(startup|resume|clear|compact|fork)$`, not `*`). Grok imports Claude's
hooks, and a wildcard would let it run taw's Claude hook too, which would relabel a Grok
pane `claude` with Grok's session id and later restore it as `claude --resume <grok's id>`.
To move an existing install over, run `taw integration install claude` again: taw's old
command is replaced, not added twice (a hook of yours, or another tool's, that sits in
the same group stays where it is, and so does it on uninstall), and `uninstall` still
gives back the file as it was before the first install.

An integration is worth installing for the agents it covers, but it is not required:
without one, a pane's state comes from manifest-based screen detection, which is how
all 22 detected agents work. What a hook adds is the agent's *own* session identity
(so `taw session restore` can resume that conversation) and, where the agent has
lifecycle hooks, state transitions that do not depend on reading the screen. The agents
that report to herdr from their own code (Crush, Command Code and the others herdr lists) have
no integration to install: one tmux option lets them report to taw, see [Agents that report to
herdr themselves](agents/report.md#agents-that-report-to-herdr-themselves).

An agent that taw can name from the program running in the pane (Claude, Codex and the
others its table knows, started from a prompt) goes when that program does, whatever its hook
reported. An agent that a shell or a runtime runs, such as an npm install of Codex
(`node /path/to/bin/codex`), a script (`sh /path/to/codex`), or Hermes started by its installer's
Python launcher (`python -I -c` with the launcher's own bootstrap, which a Hermes install anywhere
on the disk shares), is named the same way: when tmux
names the pane's command by a shell or a runtime (`node`, `bun`, `python3`, `sh`, `bash`, `zsh`,
`fish`), taw reads the arguments of the job in front of the pane when a program comes in front,
and keeps only the agent they name. A container runtime in that job names none: taw names the agent
in a container only while the runtime itself is in front, so a sandbox profile that keeps a shell
in front of its runtime gets no agent row, as the sandbox page says. While they name none it reads
them again every five seconds
during that program's first minute in front (a wrapper may start its agent a moment after it
starts), and then no more for that program, unless they have named an agent: a program that has
named one is read again every five seconds for as long as it is in front, so an agent that its
wrapper restarts (`while :; do kimi; done`) is named again; while that agent runs they are not
read again, and whether that program is still in front is asked every five seconds. It never reads them while the
pane's own shell is at its prompt, but it does when the pane's own process is a runtime (`exec
codex` typed at the prompt, or a window started with `codex`). The shell back in front of it ends its record and its session id, the pane's own
shell or one you started inside the pane, and so does another program in front of it, after
about half a minute. An agent whose integration reports its state (OpenCode, kimi, pi, omp,
kilo and mastracode) is not ended by another program in front of it, only by a shell coming back.

An agent it cannot name that way stays until the pane's own shell is back at its prompt: an
agent started through a wrapper whose arguments name no agent, and any agent
whose program no name in taw's table matches, once its hook has told taw what it is (`taw pane
report-agent-session`, or `taw pane report-agent` for the agents that report their state,
OpenCode and kimi among them). Its record and its session id stay whatever runs in front of it,
and end as herdr ends such an agent: the pane's own process is a shell that has the terminal to
itself, at its prompt with nothing else running in its group. Shells are recognised by name
(`sh`, `bash`, `dash`, `zsh`, `fish`, `ksh`, `mksh`, `csh`, `tcsh`, `elvish`, `xonsh`, `nu`,
`pwsh`, `powershell` and `cmd`, herdr's list), and taw asks `ps` and `pgrep` at most
once a second (`ps` for the pane's process, `pgrep` for the members of its terminal's foreground
group), keeping the record when it cannot say. So quitting such an agent inside `nix develop`,
or a `bash` you typed at the prompt, keeps its record until that shell exits, and an agent
started through a shell script is not released while it runs. A pane whose own shell is not on
the list keeps the record too, and a hook-owned pane can still be given up with `taw pane
release-agent`.

## The detection supplements

`src/detect/supplements/claude.toml` holds six rules and `codex.toml` one. They are
appended after herdr's own so that at equal priority upstream still wins, carry a
`taw_` prefix and never touch the vendored manifests. Claude's six:

| Rule | State | Matches |
|---|---|---|
| `taw_live_turn_working_dot` | working | the spinner line when Claude is showing `●` rather than one of the seven glyphs herdr's rule knows |
| `taw_live_turn_working_vim_footer` | working | herdr's footer rule, tolerant of vim mode's `-- INSERT --` prefix |
| `taw_background_shell_working` | working | `· N shells still running` on the completion line, or a footer counting shells, agents or tasks |
| `taw_background_agents_working_asterisk` | working | `✳ Waiting for N background agents to finish`: herdr's own wait rule, in the one spinner frame it misses |
| `taw_stop_hooks_working` | working | a turn running its Stop hooks after the reply: still working, not done |
| `taw_subagent_running` | working | a sub-agent row under the prompt box that shows its time, and its tokens once it has counted any, because Claude's main agent goes quiet while its sub-agents keep working |

Codex's one, `taw_composer_idle`, reads a Codex at its composer as idle. The Codex
manifest taw bundles (herdr's `2026.10.11.1`, from its master; v0.9.3's own reads a Codex
at rest `unknown`) reads a Codex idle from its terminal title (any title that is not a
spinner or "Action Required"), so this rule matters only where the title says nothing:
none at all (a pane no program has titled has the host name in tmux, which taw reads as
no title), an empty one, or the first reads after another agent handed the pane over,
when the title that agent left is dropped. A Codex that only its launch hint
names has nothing else to read there.

**The dev-server caveat.** `taw_background_shell_working` is doing exactly what it
says: while Claude is holding a background shell open, the pane is `working`. Start a
dev server that way and it stays `working` until you stop the server, so that pane
never goes `done` and never notifies. That is the correct reading of the screen, but
it is worth knowing before you conclude the agent is stuck.

**The sub-agent list.** `taw_subagent_running` reads the list Claude draws under its
prompt box, so a pane whose main agent is idle stays `working` until the last sub-agent
has stopped. Claude keeps a finished sub-agent's row on that list for about 30 seconds (a
completed one that has a name shows idle at once), and for as long as you have its
transcript open, so `done` can come late.

`[detection] supplements = false` turns all seven rules off and restores herdr's
verbatim behaviour.
