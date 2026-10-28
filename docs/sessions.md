---
title: Sessions and worktrees
description: "A project picker and git worktrees as tmux sessions: switch to a project or create its session, and reopen a closed worktree from the tree."
---

# Sessions and worktrees

`n` opens a project picker (zoxide and your project folders) and `w` makes a worktree with its own session.

## Opening a project

`n` in the popup opens the project picker: one input, a candidate list, and a last
line spelling out exactly what `Enter` will do. It opens empty, listing `zoxide` and
`project_dirs` right away rather than waiting for you to type anything.

Candidates are deduplicated and come in this order:

1. **filesystem completion** of whatever you have typed: directories only, `~`
    expanded, and it works anywhere, not just under a configured root; nothing typed is
    nothing to complete, so this is where the other two sources show through;

2. **zoxide's** ranked directories, when `[sessions] zoxide` is on (default) and the
    binary is on `PATH`: it runs `zoxide query -l`, and silently contributes nothing
    when `zoxide` is not installed;

3. the children of each directory in **`[sessions] project_dirs`**.

With nothing typed, 2 and 3 list in their own order instead of being fuzzy-ranked.
Typing plain text fuzzy-matches them as usual; a fragment is never read as a literal
path just because it contains a `/`, so `work/api` searches, it does not complete.
Typing an actual path (`~`, `/`, `./`, `../`, `.` or `..`) switches to filesystem
completion instead. As in a shell, completion leaves out folders whose name starts with a dot
(a repository's `.git`) until you type the dot, and a path you type in full that is a running
session's folder is offered first.

Each candidate is tagged `running`, `zoxide` or `project dir`. `Tab` completes to the
highlighted one, adding a `/` for a directory; `↑`/`↓` choose. The last line reads
`Enter: switch to running session "api"` when a live session already has that path,
`Enter: new-session -s "api" -c ~/work/api` when it would create one, and
`no such directory yet` when the path does not exist; `Enter` refuses rather than
creating a directory for you. The last line shows what taw would do without the pre-create
hook's say.

Creating runs, in order: the pre-create hook, `new-session -d -s <name> -c <path>`,
`zoxide add <path>` when zoxide is on, the post-create hook, then `switch-client`.
The pre-create hook runs first, on every open. A session that is already running is then
switched to as-is: no `new-session`, no `zoxide add`, no post-create hook.

The same switch-or-create is a CLI verb: `taw session open ~/work/api` prints a JSON
envelope, which is what an agent or a script wants. For a person at a shell there is
`taw open`.

## Open a session from your shell

`taw open` is tmux-session-wizard's `t <path or zoxide term>`: it takes you to the session
for a directory, making the session first when there is none, and prints nothing when it
works.

```sh
taw open ~/code/app     # a directory
taw open ../app         # or one relative to where you are
taw open app            # anything that is not a directory is a zoxide term
taw open                # the new-session picker, inside tmux
alias t='taw open'      # for anyone used to tmux-session-wizard's t
```

The terms, joined with spaces, are tried as a directory first, with `~`, `.` and `..` read
as `cd` reads them. Otherwise zoxide picks one, given each term as a keyword the way
`z foo bar` is, and never the directory you are in. It asks zoxide whatever
`[sessions] zoxide` says: that setting only stops the picker listing zoxide's directories
and the `zoxide add` after an open. The session is made the way the picker makes it: the
same name, the create hooks, `zoxide add`, and a session already running there is switched
to as it is. Inside tmux the client you typed it in switches to it, even when its session
is on two screens. Outside tmux this terminal attaches to it, and a tmux that is not
running is started first.

With no terms the popup opens on its new-session picker, which needs tmux, and Esc closes
the popup at once, as it does in tmux-session-wizard's `t`. Outside tmux,
`taw open` says `taw open: name a directory or a zoxide term (the picker needs tmux)` and
exits 2. A term that nothing matches is said in one line, as in
`taw open: no directory matches api`, and exits 1. `--json` is for scripts: it prints the
envelope `taw session open` prints, and attaches nothing. Outside tmux, `taw open` attaches
the terminal it was typed in, so with no terminal (piped, or run from a script) it says
`taw open: this is not a terminal, use --json to open without attaching` and exits 2,
before it has made anything.

`taw open` runs the create hooks exactly as `taw session open` does (see "Create hooks").
A pre-create hook that refuses stops it: nothing is created and nothing is switched to,
`taw open` says the hook's last line of stderr and exits 1, as in
`taw open: pre-create hook: not today`. With `--json` the error code is `hook_refused`,
as it is for `taw session open`.

## Projects in the filter

`[ui] projects_in_filter = true` (off by default) makes `/` an answer to "take me to a
project" as well, the way tmux-session-wizard's list is. While the filter is open (`/` in
the modal preset, always in the search preset) the tree panel has a section just under the
sessions and agents, after one blank line, with the panel's empty space below it: a dim
`open a directory` line, then the directories the project picker would list for the same
text, each with the picker's tag (`running`, `zoxide` or `project dir`) at the end of its
row. A tree too tall to leave the section room keeps it at the panel's foot instead, so it
is there however many sessions there are, and the sessions and agents scroll in the space
above it. The section never takes more than half of the panel, and on a short one it scrolls
to the cursor. When no session or agent matches, it starts at the top of the panel. Right
after `/` it is the picker's list for nothing typed, and as you type it is the picker's
matches for what you typed, the first eight either way: the same sources, the same order,
the same fzf terms as `n`. A path too long for the panel loses its middle, so its row keeps
its tag and the last part of the path. A directory that already has a session is listed too,
tagged `running`, and `Enter` on it switches to that session instead of making a second one.

The sessions and agents your query matches come first, and the cursor goes where it always
went, on the best of them. When none of them matches, the cursor goes to the first directory,
so typing a name and pressing `Enter` opens that project. The arrows, `ctrl-n` / `ctrl-p`
and `ctrl-j` / `ctrl-k` move into the section and out of it like any rows (a plain letter is
text while the filter is open). `Enter`, or a double-click, on a directory does what `Enter`
does in the picker: it switches to the session for that directory, or creates it (running
the create hooks, and telling zoxide) and switches to it, closing the popup, and a failure
is a toast. `Tab`, the number and letter keys, `x`, `r` and the right-click menu leave a
directory alone, and the header is a line, not a row: nothing selects it, and a click or a
right-click on it does nothing. The help says so while the switch is on: `filter (fzf
syntax), by state too; directories to open show below`.

## Naming

`[sessions] name` decides what a directory is called as a session:

| Mode | `~/code/github/api` becomes |
|---|---|
| `directory` (default) | `api` |
| `full-path` | `~/code/github/api` |
| `short-path` | `~/co/gi/api` |

Only tmux's own rule is applied on top: `.` and `:` become `_`, because tmux rewrites
them anyway. Case, spaces, `/` and a leading `~` are all legal session names, and taw
leaves them alone. Whether a project is already running is decided by comparing
`session_path`, never by comparing names, so a session you renamed by hand is still
found. The one exception is a pre-create hook that chooses a name; see "Create hooks".

In `directory` mode two projects can want the same name. The second one takes the
shortest suffix of its path that no same-named session shares: opening `~/oss/api`
while `~/work/api` is running as `api` proposes `oss/api`. The picker shows the
proposed name before you commit to it.

A name you type yourself (at a rename prompt, or as a path) is refused only where
tmux itself would refuse or rewrite it: empty, containing `.` or `:`, or duplicating a
live session's name.

## Create hooks

`[sessions] pre_create_hook` and `post_create_hook` behave the way tmux-session-wizard's
two hooks do. Each value is a command, run through `sh` the way a command typed at a
prompt is run:

```toml
[sessions]
pre_create_hook = "~/.config/tmux-agent-wizard/hooks/pre-create.sh"
post_create_hook = "~/bin/setup --layout dev"    # a hook may carry its own flags
hook_timeout_seconds = 10                         # the default; 0 means no limit
```

A path to a script is the plainest command. Flags, quoting and a variable in front
(`LABEL=work ~/bin/setup`) all work, and a leading `~` is your home directory. A path
with a space in it needs quotes of its own inside the value, `"'/opt/my hooks/pre.sh'"`.

taw adds two arguments after the command, the session name and the directory:

```sh
<hook> <session-name> <directory>
```

They reach the hook unchanged: a directory called `$(rm -rf ~)` arrives as a directory
with that name and runs nothing. Quote `$1` and `$2` inside your script, as you would
any file name. End the value with the command that should receive the two arguments: a
value that ends in `;`, `&` or `|` would run the session name as a command.

**The pre-create hook** runs every time you open a directory, whether a session is
already running there or one is about to be made, and before taw looks for one. It can
change what happens:

- Print two lines to change the session name (the first) and the directory the session
    runs in (the second; it has to exist, and a leading `~` works).

- Print nothing to keep both. A blank line keeps what it stands for, and a hook that
    prints only a name keeps the directory.

- Exit non-zero to stop the open. Nothing is created and nothing is switched to. The
    popup shows the last line the hook wrote to stderr, for example
    `pre-create hook: not on a Friday`, or `pre-create hook refused (exit status 3)` when
    it wrote nothing. `taw open` says that line and exits 1. `taw session open` fails with
    the error code `hook_refused`, and its message carries the whole of stderr.

Then taw looks for the session. If the hook left the name and the directory as they
were, it is the session running in that directory, whatever it is called, so a session
you renamed by hand is still found. If the hook chose a name or a directory, it is the
session with that name: taw switches to it when it exists and creates it there when it
does not, so a hook can send an open of a running project to a session of its own. The
name the hook is given is the one taw would give a new session in that directory (see
"Naming"), not the name of a session already running there.

**The post-create hook** runs only when taw has just made a session, never when it found
one, right after the session exists and before you are switched to it. It is given the
name and the directory the session was made with, after the pre-create hook has had its
say. What it prints is thrown away, and a failure is logged and undoes nothing: the
session is there and you still land in it.

A hook gets no standard input, and taw shows nothing it prints except a refusal's stderr.
A hook that opens the terminal itself, as fzf does, can prompt from `taw session open` but
not inside the popup.

If a hook starts something in the background, redirect its output (`cmd >/dev/null 2>&1 &`),
or taw waits for it. A post-create hook's output goes to `/dev/null` in any case, so taw
never waits for what one leaves running.

**How long a hook may take.** taw waits for the pre-create hook on every open, from the
project picker and from `taw session open`, and the popup does not respond meanwhile. A
hook that runs longer than `hook_timeout_seconds` (10 unless you change it; `0` means no
limit) is stopped, along with what it started. A pre-create hook that is stopped refuses
the open, and the popup and `taw session open` say
`pre-create hook took longer than 10 s`. A post-create hook that is stopped is logged and
changes nothing: the session is there and you still land in it. A hook that waits for you
to answer a prompt needs `0`.

taw reads none of tmux-session-wizard's options for these; see "Coming from
tmux-session-wizard".

## Worktrees

`taw worktree create|open|remove|list` pairs a `git worktree` with its own tmux
session:

```sh
taw worktree create --workspace <w> --branch <b> [--base <ref>] [--trust-repository]
```

creates (or checks out, if it already exists) `<b>` under `[worktrees] directory`
(default `~/.tmux-agent-wizard/worktrees`), then opens a new tmux session named
`<repo-basename>--<branch>` rooted there (a `/` in the branch becomes `-`, and tmux's own
rule turns `.` and `:` into `_`). `remove [--force]` runs
`git worktree remove` and kills that session; it never deletes the branch.
`--trust-repository` adds `-c safe.directory=<repo root>` to every git command of that
one request, for a checkout git would otherwise refuse as owned by someone else.
Every git taw runs (here, for a session's branch, and for a plugin's checkout) acts on
the repository of the folder it names: `GIT_DIR`, `GIT_WORK_TREE` and the other
variables that point git at another repository are left out of its environment, so a
taw run from a git hook or a `git rebase -x` command does not reach the repository
being rebased. taw's sandbox profile `default` finds its checkout the same way, and shares
that repository's hooks and config read-only.

`--workspace <w>` must name an **existing workspace**: a running tmux session, given as its public id (`wN`), its session name, or any tmux target, whose working directory is inside a git repository; taw cuts the new worktree from that repo. It is not a free-form label and does not create a session. Omit it to use the workspace of the pane you run the command from. A `<w>` that matches no live session is reported as `not_found`; a workspace whose directory is not in a git repo is `not_a_git_repo`.

`taw workspace close <w>` on a workspace that still has worktree workspaces open is
refused with `workspace_group_close_required`, naming them, and nothing is closed;
`taw workspace close <w> --group` closes it and them together, one `workspace.closed`
event each. A worktree workspace closes on its own without the flag, even when another
worktree was cut from it (that one stays open and still closes with the primary), and so
does any workspace with no worktree workspace open. Run from a pane inside the group, the
close takes the workspace holding that pane last, so it still finishes the whole group.

In the popup, `w` creates a worktree from the row's session and switches to it. A
worktree whose session is not running shows as a `◌` row under its parent session:
`Enter` starts its session again, `x` runs `git worktree remove` (branch kept, with a
confirmation).

When git refuses that removal, the popup asks instead of only showing git's reason. A
checkout with modified or untracked files, or with submodules, is refused without
`--force`: the popup says so and asks `remove anyway? uncommitted changes are lost`, with
`y / N` under it. Only `y` runs it again with `--force`; `Enter`, `n` and `Esc` keep the
worktree. A removal that failed partway (a file in the checkout would not go) can be tried
again: git has let go of the worktree by then, so the next `x` finds a folder git no
longer lists, asks the same question about what is left and names the folder, and `y`
clears it, but only when its own `.git` file says it was this repository's checkout.
`[hooks] pre_worktree_remove` is asked before every attempt, the forced one too, and can
still refuse it. `taw worktree remove` has the same `--force`, answers `worktree_dirty` under `--json` when git
refused for that reason, and finishes a removal whose worktree is already gone.

`taw worktree open` on a worktree whose session is closed and whose folder is gone refuses
with `not_found` and names the folder (`worktree_not_found: the checkout of worktree feat/x is
gone: <folder>`), rather than make the session in your home folder, which is what tmux does
with a folder that is gone. `taw worktree list` and the popup leave such a worktree out.

`x` on a session that still has worktree sessions open (the ones `taw workspace close
--group` would close) asks whether to close those too, and names up to three of them:
`y` closes the session and all of them, `Enter` or `n` only the session, `Esc` nothing.
`x` on the last pane of the session's last window asks the same, since closing that pane
closes the session; `Enter` or `n` then closes just the pane, and the session with it.
`[hooks] pre_close` is asked about each session that would close before any of them does,
and one refusal keeps them all open. When a close from the popup takes the session you are
in (one session, a group, or the last pane of its last window) and another session stays
open, the popup first moves you to tmux's last session, or to the first session still open
when that one is closing too, and the question says so (`You go to api.`); tmux's
`detach-on-destroy`, which would take the client and the popup down with the session,
cannot then drop you out of tmux. When nothing would be left, the question says
`It is your only session: tmux ends.` (`No session is left: tmux ends.` for a group). A
group goes in one tmux command with your own session last, so the close is never cut short.
This is for this machine's sessions; a machine's rows are closed as before.
Closing sessions removes no worktree: each one comes back as a `◌` row.
