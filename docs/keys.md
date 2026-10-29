---
title: Keys
description: "Every key in the tmux-agent-wizard popup: Tab to the agent that needs you, Enter to go there, filters, rows, the preview, the mouse and the two key presets."
---

# Keys

The popup is one tree: every tmux session, the panes under it (agents and plain
panes alike), then any closed worktrees, always in tmux's own order and never
re-sorted while it is open. The cursor opens on the pane you came from and stays
where you put it, and the sessions are numbered in that same tmux order. The
tree's border counts what needs you by kind, each in its own glyph and colour
(`⏸1 ✓2 need you`), while any do.

Its bottom line is the hint line. It names the keys that are actually bound, so
rebinding an action changes the hint too; its `Enter` item is the constant
`Enter go` in both presets, and names no target:

```
1-9 session · a-e rows · Tab next up · Enter go · j/k move · / filter · ? help
```

The `a-e rows` entry is there whenever the letters are (see `a-e` below), and
reads the same in every session, however many rows it has; a letter your `[keys]` gives
to an action leaves the others listed (`a b d e`), and the help's line for the letters
lists the same ones.

The same line carries the filter you are typing while `/` is active, and a
transient message after an action (`renamed`, a failed `kill-pane`). While the
filter is open it names only the keys that work there (`/query▏ · 4a session 4 row a ·
Esc clear · Tab next up · ↑/↓ move · Enter go`), a letter or a digit being text for the
query: typing a row's number and letter, `4a`, is how you reach it from there. Notifications never land on that line; they go to the real tmux status
line, which shows taw's bar while the popup is open.

## Modal preset (default)

One mnemonic key per action, no mode to enter first.

| Key | Action | What it does |
|---|---|---|
| `1-9` | cursor to that session | its header, whose letters then light up, as any session's do while the cursor is in it. A number is yellow and bold while its key works, on every header whichever session the cursor is in, and on the rows of a flat list, which it puts the cursor on. It stays yellow while `/` is open, where you type it instead: typing a number puts the cursor on its session, and a number then a letter on that row, so type 4a to reach row a of session 4, in the search too. Dim while anything is open over the tree (the help, a prompt, a question, the picker, a menu), which takes every key itself |
| `a-e` | jump to one of the rows of the session you are in | shown beside the first five rows of every session, under the header's `*` (a session's rows start two columns in, so the first column holds the session numbers alone): yellow beside those of the session the cursor is in, however it got there (a number, `j`/`k`, `J`/`K`, `Tab`, a click), which are the ones a letter jumps to, and dim beside every other session's, so you know a row's letter before you press its session's number; `f` onwards are keys, so a sixth row has no letter. None in a flat list, at the sessions depth, or with `[ui] labels = false`. While `/` is open every row keeps its letter, dim, and a letter is text for the query: type the session's number, which lights its letters, then the letter (`4a`, or `4A`), and the cursor is on that row. A filter only hides rows, so no row's number or letter changes while you type. A letter the session has no row for says so (`no row d in this session`). A letter your `[keys]` gives to an action stays that action's, and its row goes without |
| `Tab` / `Shift-Tab` | next up: who to deal with next | walks what needs you (blocked, or finished and not yet looked at) while an agent in view does, else every agent, idle and working alike. One row a press, in the order drawn, wrapping; with one agent in the walk it stays on it. At the sessions depth a header stands for its session's agents. See "Next up" |
| `Enter` | go | a pane runs `switch-client`, `select-window` and `select-pane`; a session header runs `switch-client` and nothing else, so tmux lands you where you left it; your own row just closes the popup, with no tmux command; a closed worktree creates its session |
| `Enter` on a machine's row | go in | in the app, the terminal is lent to that machine's own tmux through ssh, at that pane (a session header: at the session); in the popup it is the doorway into the app. See "Remote machines" |
| the popup key (`prefix + A`) inside a machine's tmux | back to the list | that machine's own key, as its plugin binds it: the app's list returns with the cursor on the pane you were in |
| `Q` / `Ctrl-C` in the app | quit the app | `Esc` and `q` go back instead |
| `j` `k` `↑` `↓` | move one row | |
| `J` `K` | next / previous session | jumps header to header |
| `/` | filter | fzf-style query over the whole tree (see below); with `[ui] projects_in_filter = true` it also lists the directories `n` would offer, just under the sessions (see "Projects in the filter") |
| `h` `l` | show less / more: sessions, agents, every pane | steps `[ui] depth`; the cursor stays on your row, or its session while it is folded |
| `f` | flat: one list of what is in view (agents, or every pane), most urgent first; again for the tree | drops the headers and orders by `[ui] agent_panel_sort` (most urgent first is its default, `attention`); at the sessions depth the screen does not change until `l` |
| `r` | rename | a pane is `rename-window`, a header is `rename-session`; an agent row asks first: `w` for the window, `a` for the agent's own label |
| `x` | close | a pane is `kill-pane`, a header is `kill-session`, a closed worktree is `git worktree remove` with its branch kept. Confirms first, naming the tmux command and saying when the window goes with its last pane, or the session with the last pane of its last window (and the sessions grouped with it, when it is in a tmux session group, since they share its windows). A header whose session has worktree sessions open asks `Close them too?`, and so does the last pane of such a session's last window, since closing it closes the session: `y` closes them all, `Enter` or `n` just that session or pane, `Esc` nothing. When the close takes the session you are in, the question ends with where you go (`You go to api.`): tmux's last session, or the first one still open when that one is closing too, and you are moved there before the close, so you stay in tmux; with no session left it says `It is your only session: tmux ends.` (`No session is left: tmux ends.` for a close of several). A worktree git will not remove (changes in it, submodules, a folder left by a removal that stopped halfway) asks `remove anyway? uncommitted changes are lost`, and only `y` goes ahead. See "Worktrees" |
| `n` | new session | the project picker (see "Sessions and worktrees"). On a row of another machine the app asks for a folder or a zoxide term and makes the session on that machine (see "Remote machines"); the popup says that row is read only |
| `w` | new worktree | branch prompt, then its own session |
| `o` | options | the effective configuration, read-only, with its file path |
| `ctrl-v` | paste image | the image on this machine's clipboard into the agent on the cursor row, on whichever machine it runs (see "Pasting an image") |
| `?` | help | one screen: move · go · act on the row · mouse · legend |
| `q` / `Esc` | quit | `Esc` first leaves a filter or an overlay |

Back where you were is tmux's own `prefix + L`.

With several clients on one session, `Enter` and every other key that takes you somewhere
moves the client the popup is shown on, and no other, and the session marked to go back to
is that client's own. That holds for the popup the key binding and `taw open` open, which
tell it its client (`--client`); a `taw ui` typed by hand in a pane has no `--client` and
picks its session's first-listed client.

A confirmation prompt takes `y` or `Enter` to confirm and `n` or `Esc` to cancel;
`[ui] confirm_close = false` skips it for both `x` and worktree
removal, and with it the question whether to close a session's worktree sessions too,
which then closes just the session, as `x` always did. After a kill or a worktree removal,
an extra `Enter` meant for the confirmation is ignored for a moment. Two questions have
an answer of their own, and show it as `y / N`: `Close them too?` (`Enter` is the `N`,
just this session) and `remove anyway?`, which only `y` answers. The removal question is
asked with `confirm_close` off too, since what it puts at risk cannot be put back, and a `y`
typed in the moment after the removal that raised it is not an answer.
A rename or branch prompt is a one-line readline input (the same chords
as the filter, `ctrl-k` included), prefilled with the current name, `Enter` to
confirm, `Esc` to cancel, and a rejected value's reason shown under the line.

An agent suspended with Ctrl+Z keeps its name, its session and its hook while its process is
alive behind the shell, and a watcher started meanwhile keeps it too: it reads `idle`, with no
notification, though lifecycle hooks see it go idle; its row says `suspended` where it says the
state, a remote machine's row as well; and after `fg` it is the same agent again.

`/` opens a one-line input with readline editing. `ctrl-j`/`ctrl-k` and
`ctrl-n`/`ctrl-p` (fzf's own) belong to the query line itself and move the cursor
down and up through the rows, so `ctrl-k` does not kill to the end of the line here
as it does in a prompt.
Matching is fzf syntax (space is AND, `'exact`, `^prefix`, `suffix$`, `!negate`)
over each row's name, its session, its target, an agent's state word (`blocked`,
`done`, `idle`, `working`, `unknown`, `suspended`, as the row draws it), an agent's title while
`[ui] titles` is on and, for a header, the
session's branch and git flags while `[ui] show_git` draws them, so `/blocked` lists the
blocked agents under their sessions' headers. A title is found by the words in it as you
type them, whole even where the row cuts it off, but not by letters scattered through it,
so a short query does not light every long title: `/flaky` finds `Fix flaky auth t…`
(`^` anchors at the title's start and `$` at its end, and `!flaky` hides the row). A
closed worktree's row is found by `closed`, as it draws `worktree, closed`. It matches
what the tree draws: a
session's folder is not drawn, so it is not matched either (each row under a header
starts with its session's name, so `api claude` finds that row). A row is also found by
the words of the header above it, its session's branch and git flags while `[ui] show_git`
draws them (or what a `[ui.sidebar.spaces] rows` template draws on the header), found by
the words as you type them, but not by letters scattered through them
(`rel` finds `release`), so the words of a query can be split between a session and its
rows: `release claude` finds the claude row of the session on branch `release` in the tree
(a query of the header's words alone leaves the cursor on the header). The flat list draws
no header, so it finds a row only by what the row itself draws, its session's name included.
A search finds only what is on the screen: a name or title drawn cut short still counts in
full. At the sessions depth a header is found by the words it draws and no others: its
session's name, its branch and git flags, and the title of the agent that needs you; the agents it
folds are not on it, so `/blocked` finds no header there (go one depth down to search by state).
`Tab` stops at a header the search found by its words as it does with no search, and at one found
by the title it draws as at that agent alone; Enter switches to the session as ever. A row a configured `rows` template draws is found by the template's words and by none
of the default row's it leaves out, as you type them and not by letters scattered through them. The time beside the state
word, the state glyphs and the `*` and `-` marks are not matched: the time would answer
every number you type and changes while you look, and each glyph's word is matched
instead; nor is a directory's tag (`running`, `zoxide`, `project dir`): a short query
would match every directory of that kind, so its path is what finds it. A header shows
when it or any child matches; matched characters are highlighted, and when they are in
the end of a long name, window name or title that the row cuts off, the `…` that marks
the cut is highlighted instead; in the flat list a row's session name, written at its
end, is highlighted where it matched. The cursor follows the best-scoring row on every
keystroke. The numbers and letters the rows wear are part of what is matched, as drawn,
as well as each row's own words: a header's text starts with its number (`4`), a row's
with its session's number and its letter (`4a`; a tenth session's rows with the letter
alone), and a matched number or letter is highlighted where it is drawn. Each term of
the query is matched with the row's label in front and without it, and keeps the better
match, so `^` anchors a row's own words (`^api` finds the api session and its rows) and
`^4` the labels, while `!` drops a row whose label or words hold its text (`!4` hides
session 4 and its rows). A typed address ignores case (`4A` finds what `4a` does, `!4A`
drops what `!4a` drops); any other term with a capital in it matches case-sensitively,
as fzf's smart case does. The numbers and letters stay on screen while you type and do
not move: a filter only hides rows. A query that is exactly a number, or a number and a
letter, in either case (`4`, `4a`, `4A`), puts the cursor on that row whatever else
matches, as pressing `4` then `a` does with the filter closed. `Enter` acts on the
cursor row from inside the filter, `Esc` clears and leaves the cursor on the row it was on. With `[ui] projects_in_filter = true`
the tree panel has a section of directories to open just under the sessions, and `Enter`
on one opens or creates its session (see "Projects in the filter").

The filter, the rename and branch prompts and the new session picker are text
fields, and they edit as herdr's do (the search preset's query line goes by fzf's rules
where the two differ, which the rows and the paragraph below say):

| To | Press |
|---|---|
| move one character | `Left` `Right`, `ctrl-b` `ctrl-f` |
| move to the start or end | `Home` `End`, `ctrl-a` `ctrl-e` |
| move one word | `alt-b` back to its start, `alt-f` on to its end |
| delete the character before or after the cursor | `Backspace` `ctrl-h`, `Delete` `ctrl-d` |
| cut to the start or end of the line | `ctrl-u`, `ctrl-k` (a prompt; in the filter `ctrl-k` moves up) |
| cut the word before the cursor | `ctrl-w`, `alt-backspace`, `ctrl-backspace` |
| cut the word after the cursor | `alt-d`, to its end: the space after it stays |
| put back the last cut | `ctrl-y` |

`alt-b`, `alt-f` and `alt-d` share one rule for where a word ends, so going back over a
word and cutting takes exactly it. In the filter, the prompts and the picker it is
herdr's: the space there is, then a run of letters, digits and `_`, or of punctuation,
so `tmux-agent-wizard` is `tmux`, `-`, `agent`, `-` and `wizard`. In the search preset's
query line it is fzf's: whatever is not a letter or a digit is skipped, then a run of
letters and digits, so it is `tmux`, `-agent` and `-wizard`, and `src/foo_bar.rs` is
`src`, `/foo`, `_bar` and `.rs`.

The last cut is kept per field and never reaches the clipboard. The cursor is drawn
where it is, so `Left`, `Home`, `ctrl-f` and the rest visibly move it, and what you type
goes in at it. In the `/` filter, a prompt and the picker, `ctrl-d` never closes
anything: with nothing after the cursor it does nothing. (The search preset's query line
is fzf's own, and quits on `ctrl-d` when it is empty: see "Search preset".) Your tmux
prefix keeps its meaning, and so does a second one, tmux's `prefix2`. In the filter either
starts the prefix chord, whichever key it is. A prompt and the picker hold the keyboard
whole, so there `ctrl-b`, `ctrl-f`, `ctrl-d`, `ctrl-y` and `alt-d` do nothing when one of
them is your `prefix` or your `prefix2`: with tmux's default `ctrl-b`, the key never moves
the cursor. An `alt` key needs a terminal that sends Alt or Option as Escape;
`ctrl-backspace` needs one that tells it from `ctrl-h`.

Mouse: click selects; double-click goes; the wheel scrolls the tree, or the
preview under the pointer; right-click: a menu with go, rename, close and new worktree, and on an agent's row paste image.
A click outside the menu closes it, and a right-click there opens the menu of the
row under the pointer instead. A click moves the lit letters with the cursor.

## Search preset

`[keys] preset = "search"` is a pure fzf search. The filter is always open, every
plain letter is text for it, and the only keys are fzf's own:

| Key | Action |
|---|---|
| *any character* | filters as you type (fzf syntax) |
| `↑` `↓`, `ctrl-j` `ctrl-k`, `ctrl-n` `ctrl-p` | move one row down / up |
| `Enter` | go to the row under the cursor |
| type a name, `Enter` | go to the best match |
| type `4a`, `Enter` | go to row a of session 4. Every session's number and every row's letter stay on screen while you type, the numbers yellow; typing a number puts the cursor on its session and lights its letters, and a number then a letter (`4a`, `4A`) on that row, whatever else matches. A `[keys] select_session` entry that binds the numbers makes them keys here, and the hint line and the help name them |
| `Esc` | quit |
| `Backspace`, `ctrl-u`, `ctrl-w`, `ctrl-a` / `ctrl-e`, `ctrl-b` / `ctrl-f`, `Left` `Right`, `ctrl-d`, `alt-d`, `alt-backspace`, `ctrl-y` | edit the query, as in `/`; these are fzf's own editing keys. `ctrl-d` deletes the character under the cursor, and on an empty query it quits, as in fzf (`Esc` quits whatever the query holds). `ctrl-backspace` deletes one character here, as in fzf, and `alt-b`, `alt-f` and `alt-d` go by fzf's words (letters and digits), where `/` goes by herdr's |

Its hint line is the whole of it: `type filter (fzf syntax) · 4a session 4 row a ·
↑/↓ ctrl-n/p move · Enter go · right-click menu · Esc quit`. Nothing else is bound. Paste image is an entry of an agent row's right-click menu here, not a key (a `[keys]` entry for `paste_image` binds one, and `?` then lists it). A `[keys]` entry for
`move_down` or `move_up` puts the keys it binds in that line and in `?`, in place of
the ones it replaces. With
`[ui] projects_in_filter = true` its filter is a search for projects too: the directories
show under the sessions from the first frame (see "Projects in the filter").
Right-click a row for its menu (go, rename, close, new worktree); `prefix + O` still goes next up.
Options and help are reachable in this preset only through `[keys]`, as is every
action not listed here (a new session, depth, the flat list, the next-up walk): bind
one, say `options = "ctrl+o"` or `help = "ctrl+g"`, and that key does it. (tmux hands
a popup `ctrl-/` as `ctrl-7`, so a binding for that key is `ctrl+7`.) A
`new_session = "ctrl+n"` clashes with ctrl-n's own move and is reported as bound
to both; bind new session to another key.

## Rebinding

Every tmux user remaps keys, so the popup's own keys are not fixed. `[keys]` uses
herdr's key grammar and accepts herdr's action names, so a herdr user's bindings mean
here what they meant there, and the two presets cover both mental models: mnemonic
letters, or type-to-search.

```toml
[keys]
preset = "modal"      # or "search"
rename = "R"
move_down = ["j", "down"]
close = "ctrl+x"
```

An override *replaces* that action's preset keys rather than adding to them. Key
specs are `+`-joined modifiers (`ctrl`/`control`, `alt`/`meta`/`option`, `shift`)
then a single character or a named key (`tab`, `enter`, `esc`, `space`, `backspace`,
`up`/`down`/`left`/`right`, `home`, `end`, `pageup`, `pagedown`, and words for the
awkward punctuation: `slash`, `backslash`, `minus`, `comma`, `period`, `quote`,
`semicolon`, `colon`, `percent`, `ampersand`, `backtick`, `plus`, `question`). A
character's case implies shift, so `"R"` and `"shift+r"` are the same key.
`prefix+…` is refused: the prefix chord is tmux's, not this table's.

Action names: `next_attention`, `previous_attention`, `go`, `move_down`, `move_up`,
`next_session`, `previous_session`, `filter`, `shallower`, `deeper`, `toggle_flat`,
`rename`, `close`, `new_session`, `new_worktree`, `options`, `paste_image`, `help`, `quit`,
`select_session`.
`select_session` takes an ordered list rather than a single key or list of
equivalent keys: the nth key in it labels the nth session (that label is what's
drawn) and moves the cursor there when pressed.

herdr's own names are accepted as aliases:

| herdr name | taw action |
|---|---|
| `goto`, `workspace_picker` | `filter` |
| `new_workspace` | `new_session` |
| `rename_workspace` | `rename` |
| `close_workspace` | `close` |
| `next_workspace` / `previous_workspace` | `next_session` / `previous_session` |
| `settings` | `options` |
| `help`, `new_worktree` | same name in both |

`open_notification_target` is deliberately *not* a `[keys]` entry: that key is read
from tmux (`@agent-wizard-notification-key`), so the same key works inside and
outside the popup. Neither is herdr's `clear_pane`: it clears the pane you press the key
in, which the popup is not, so it is a tmux key too (`@agent-wizard-clear-key`), and a
`clear_pane` under `[keys]` is reported as an unknown action name.

**If you are pasting a herdr `[keys]` table in, edit the values.** herdr binds its
whole UI behind its own prefix, so almost every line reads `goto = "prefix+g"`, and
`prefix+…` is refused here, because that chord is tmux's. Drop the `prefix+` and the
binding works (`goto = "g"`). herdr also has actions with no equivalent inside a tmux
popup (`detach`, `reload_config`, `prefix`, `open_worktree`, `remove_worktree`), and
each of those is reported as an unknown action name rather than silently accepted.

An unknown action name, a key spec that does not parse, and two actions contesting one
key are each a diagnostic. The popup shows the first one as a message on its hint line
when it opens, `(+N more)` when there are several, and logs all of them.
`taw config check` reports a key spec that does not parse, in the popup's own words,
and the other mistake, a `[keys]` value that is neither a key nor a list of keys. An
override none of whose keys parse keeps the action's preset keys, so a typo never leaves
an action unbound. In a list, the entries that parse and do not clash with another action's key are bound;
the rest are reported; for `select_session`, each entry that does not parse keeps that
session's preset key, unless another entry of the list has taken it.

## The prefix chord

tmux hands the raw prefix bytes to an open popup instead of running your binding
(`C-b O` arrives as two bytes; the binding does not fire). So the popup reads
`prefix`, `prefix2` and `@agent-wizard-notification-key` itself and watches for the
chord: within two seconds of either prefix, the notification key means "next up";
anything else is ignored. With `set -g prefix2 C-a`, `C-a O` goes next up in the
popup as `C-b O` does, from the filter too, where `C-a` would otherwise move the
cursor to the start of the line. tmux's `None`, which is what an unset `prefix2`
reads, is no key. A `prefix` or `prefix2` this cannot parse is left out, and when
neither can be read the chord is off and `prefix + O` does nothing inside the popup
(outside it, tmux's own binding still works).

## Dots and glyphs

`[ui] status_indicators` picks the alphabet: `symbols` (default) gives each state
its own glyph, `dots` gives every state the same `●` in a different colour.

| `symbols` | `dots` | State | Means |
|---|---|---|---|
| `⏸` | `●` in the attention colour (peach by default; yellow in the status token) | blocked | waiting on you |
| `✓` | `●` green | done | finished, you have not looked yet |
| `⏵` | `●` blue | working | busy |
| `○` | `●` grey | idle | looked at, or quiet |
| `?` | `?` magenta | unknown | an agent is there, its state is unreadable |
| `▢` | `▢` | none | a pane with no agent; the column beside it names the program |
| `◌` | `◌` | none | a worktree whose session is closed |

An agent in a container has 📦 after its name, two columns wide: in a container that taw
started, or saw start (see "Agents in a sandbox"). It keeps its own colours on any row, and the
rest of the row keeps the colour that says whether it needs you.

A yellow number or letter is a key that works now. Every session's number,
alone in the first column, is yellow and bold while its key works, and while you
type in the filter or the search preset, where typing it takes the cursor to its
session (dim while anything is open over the tree (the help, a prompt, a question,
the picker, a menu), which takes every key itself), and so are the letters of the
session the cursor is in (every other session's letters are dim, and so are these
under anything open over the tree). While you type the letters are text, so every
session's are dim but those of the session whose number alone you have typed, which
the next letter reaches. The session you came
from is the one with the `*`, and its tree line (the
`├`/`└` beside its rows) is drawn in the accent colour, except on the rows
of the window it has open. In every session
the rows of the window it has open are lit (that window is where Enter on the
header lands, and in your own session where you are): the kind (`claude`,
`lazygit`), the place (`2:zsh`) and its tree line in white, bold, and the rest
of the row in white (the state, elapsed time and title, a plain pane's `▢`, and
in the flat list the session name). Every other row keeps those dim, and a row
that needs you keeps its own colour end to end.

Attention order, worst first: **blocked and not looked at**, blocked and looked at,
**done**, idle, working, unknown: `[ui] agent_panel_sort = "attention"`'s ordering
for the flat list, ties broken by the newest state change then tmux's own order.
The first three tiers are hot: counted on the border, each in its state's colour
(blocked in the attention colour, done in green). `Tab` and `prefix + O` do not walk this order: they walk one group of it (see
"Next up"), in tmux order, top to bottom, wrapping. `done` means it finished while
you were not looking at it (glancing at it earlier does not count), and looking at
the pane turns it `idle`, which outranks `working`: an agent waiting at its prompt is
something to come back to, a running one is not.

Session headers carry no state glyph any more: just the label, the `*`/`-` flag,
the session name in bold, and its branch and git flags when `[ui] show_git` is on.
At the sessions-only depth a header also shows one count per state present among
its agents, and, with `[ui] titles` on, the title of its first agent that needs
you. The session's directory is not in the row; it titles the preview pane
(`preview · ~/code/github/api`), and the filter does not match it, as it matches
only what the tree draws.

The preview wraps lines wider than the panel, except for a pane running a
full-screen program (nvim, lazygit, less, htop, or anything else that sets tmux's
`#{alternate_on}`), which is cut on the right as its layout is the screen itself. The
wheel over the preview scrolls it; moving the cursor to another row returns it to the tail.

The list comes first. The preview sits beside the tree only when both fit, from 100
columns up; in a narrower popup, a very small terminal window say, the tree takes the
whole width and the preview is not drawn, so there is always a list to navigate. Made
wide enough again, the popup shows the preview at once.

## Agent views

herdr's `agent.view.set` (a query the agent rows are projected through) is
`taw agent view set --source <id> [--label <text>] [--filter <json>] [--sort <json>]`,
removed by `taw agent view clear --source <id>`. While one is installed the tree's
border reads ` sessions · <label> `, agent rows that do not match are gone (taking a
session header they leave with nothing under it), and a view carrying `sort` stands in
for `[ui] agent_panel_sort` in the flat list without rewriting your config. Plain panes
and parked worktrees are not agents, so a view never hides one. A query that is
well-formed but matches nothing leaves no agent rows at all, the label still on the
border, rather than being silently ignored; the CLI is what decides whether a query is
usable. A view narrows the popup only: `taw agent
list`, the status token and notifications always see every agent. See `taw agent view
set --help` for the query's shape and `docs/PARITY.md` for how it maps onto herdr's.

## Outside the popup

Two keys are bound on your tmux prefix, both configurable: `prefix + A` opens the
popup, `prefix + O` goes next up. A third, which clears the pane you are in, is bound
only once you name a key for it (`@agent-wizard-clear-key`, see "Clearing a pane" under
"CLI"). See "Customisation".
