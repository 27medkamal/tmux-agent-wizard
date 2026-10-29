---
title: Configuration
description: "Configure tmux-agent-wizard: tmux options, config.toml, 12 themes that can follow your terminal's light or dark mode, and two key presets."
---

# Configuration

Two places, and the split is not arbitrary.

| A tmux option, in `tmux.conf` | `config.toml` |
|---|---|
| which prefix keys open the popup, go next up and clear the pane | what the popup's *own* keys do (`[keys]`) |
| the popup's size caps, title and border | what the popup shows: depth, layout, preview, git flags, ordering, glyphs, row templates (`[ui]`) |
| the path to the `taw` binary | notifications and sounds (`[ui.toast]`, `[ui.sound]`) |
| whether watchers attach automatically | sessions, naming, the project picker and its hooks (`[sessions]`) |
| the status-line token, which you place yourself | detection, worktrees, plugins, theme, session restore, sandbox profiles |

A tmux option carries a **bind-time fact**: something the plugin script has to know
before `taw` ever runs, when the only thing that exists is tmux. Everything else is
structured, and structure is what a tmux option cannot hold: options are flat strings
with no validation, no tables or lists, no aliases, and no diagnostics when you
re-source your config, so a typo is silently the empty string. `config.toml` gets
`taw config check`, tables for row templates and per-agent overrides, lists for key
bindings, and herdr's own spellings accepted as aliases, which is the other half of
the reason: a herdr user drops their `config.toml` in and it means what it meant, with
`[keys]` the one table worth editing first (see "Rebinding"). The `@agent-wizard*`
surface stays the thin bind layer it is, and everything you tune afterwards lives in
one validated file.

## tmux options

| Option | Default | Meaning |
|---|---|---|
| `@agent-wizard` | `A` | prefix key(s) that open the popup (space-separated list allowed) |
| `@agent-wizard-notification-key` | `O` | prefix key: next up, who to deal with next |
| `@agent-wizard-clear-key` | *(empty)* | prefix key(s), a space-separated list as `@agent-wizard` takes, that clear the pane they are pressed in: its history, and its screen as well when bash, zsh or fish is at its prompt (see "Clearing a pane"). Empty, the default, binds nothing |
| `@agent-wizard-width` | `100` | **cap** on popup width, % of the client |
| `@agent-wizard-height` | `100` | **cap** on popup height, % of the client |
| `@agent-wizard-popup-title` | `taw`, padded with one space each side | `display-popup -T` title, a framed popup only: a width or height cap below `100`, or tmux older than 3.3 |
| `@agent-wizard-popup-border` | *(empty)* | `display-popup -b` border style, a framed popup only, which on tmux ≥ 3.3 means a width or height cap below `100`; skipped below 3.3, which has no `-b` |
| `@agent-wizard-bin` | *(empty)* | explicit path to `taw`, overriding auto-resolution |
| `@agent-wizard-watch` | `on` | `off` disables attaching watchers automatically |
| `@agent-wizard-auto-install` | `on` | `off` stops the plugin downloading `taw` when it finds none (and refreshing a downloaded one after an update) |
| `@agent-wizard-path` | `on` | `off` stops the plugin putting `taw` on the `PATH` of every pane created after it loads (a folder in taw's state folder, with only a `taw` link in it, goes in front of tmux's global `PATH`), and a reload takes the folder out again |
| `@agent-wizard-herdr-env` | `off` | `on` gives tmux's global environment herdr's `HERDR_ENV=1`, `HERDR_BIN_PATH` and `HERDR_PANE_ID`, so the agents that report to herdr themselves report to taw (see "Agents that report to herdr themselves") |

Change `@agent-wizard`, `@agent-wizard-notification-key` or `@agent-wizard-clear-key` (or empty
`@agent-wizard-clear-key`; the other two go back to `A` and `O` when empty) and source the
plugin again (`prefix + I` with TPM, or `source-file` on your `tmux.conf`), and a key the
option no longer names is unbound. A key is the plugin's own to unbind only if
its binding carries the message the plugin shows when it fails (`taw: could not open the
popup`, `taw: next up failed` or `taw: could not clear the pane`), so a binding of your own on
the same key is left alone.

Width and height are caps rather than sizes because `taw ui open` computes the
geometry itself: it reads the client's size and `status`/`status-position` and places
a full-width popup against the status line, as many rows short of the client as the
line has (one, or up to five with `status 2` … `status 5`). While the popup is open
that line shows taw's bar (`taw · 7 sessions · 9 agents`, with `3 machines` first when you list two machines or more, then the clock at the
right, on its first row, with any other rows blank; what needs you is on the
list's border, so the bar does not repeat it, and neither does it show the status
rail) and gets its own text back
when the popup closes; a `display-message` notification lands in it as
always. A full-size popup on tmux ≥ 3.3 draws no frame or title of its own
(`display-popup -B`), so `@agent-wizard-popup-title` and `-border` have nothing to
style; lower a cap and the popup shrinks, centred, framed and titled as before, with
both options in effect. With `status off` there is no row to protect and the popup
takes the whole client, with no bar.

## `config.toml`

`taw` reads its config from `$TAW_CONFIG_DIR/config.toml`, falling back to
`$XDG_CONFIG_HOME/tmux-agent-wizard/config.toml`, falling back to
`~/.config/tmux-agent-wizard/config.toml`. Run `taw config path` to see what's
actually in effect. There's no first-run wizard: an absent file just means every
default applies.

```sh
taw config default   # print the full default file, with comments, to stdout
taw config check      # validate your file and print diagnostics
```

`taw config check` exits 1 when it reports an error (a file that does not parse, a word a key
does not allow, a number out of range) and 0 when the file is clean or has warnings alone, so a
script, a pre-commit hook or a dotfiles CI can run it. It prints its answer either way: one JSON
object with the diagnostics, errors first. A key that takes one of a few words and is given
anything else is named with what it was given and the allowed words: `[ui] depth: 3 is not one of
sessions, agents, all`. Another key given the wrong thing is reported beside it, so one check shows both.

A UTF-8 byte-order mark at the start of the file is accepted, and one a line-oriented
edit has moved into the middle is dropped rather than failing the next load; a mark inside
a multi-line string is data and stays.

It mirrors herdr's `config.toml` where the semantics carry over to a tmux popup.
Unknown keys are warnings, not errors, so a config written for herdr mostly just
works. Main sections (see `src/config_default.toml` for the full annotated
version):

| Section | Covers |
|---|---|
| `[keys]` | `preset` plus per-action overrides for the popup's own keys; see "Keys" |
| `[theme]` | one of 12 built-in themes (`catppuccin` default), plus `[theme.custom]` token overrides |
| `[ui]` | `depth`, `layout`, `preview`, `show_git`, `projects_in_filter`, `sidebar_width`, `agent_panel_sort`, `status_indicators`, `confirm_close` |
| `[ui.sidebar.agents]` / `[ui.sidebar.sessions]` | herdr's row templates and per-agent overrides, with per-token style and `rules`; opt-in, and they replace the popup's own column layout when you set them (`[ui.sidebar.spaces]` is the same table under herdr's name) |
| `[ui.toast]` | notification delivery, `delay_seconds` and `done_delay_seconds`; see "Notifications" |
| `[ui.sound]` | sound on/off, sound files, per-agent mutes |
| `[update]` | `manifest_check`, the six-hourly check for newer detection rules |
| `[sessions]` | naming mode, the project picker's sources, create hooks; see "Sessions and worktrees" |
| `[hooks]` | a command of yours on any taw event and on every notification, and two that can refuse a close or a worktree removal, see "Hooks" |
| `[session]` | `resume_agents_on_restore`, `startup_per_agent_delay_ms` (see "Sessions and worktrees") |
| `[detection]` | `supplements`; see "Beyond herdr" |
| `[sandbox]` | `profile`, the profile `taw sandbox` uses when you name none, and `[sandbox.agents]` per agent; see "Agents in a sandbox" |
| `[worktrees]` | the worktree checkout directory |
| `[machines]` | one `name = "ssh destination"` per line, or `[machines.name]` with `ssh` and `taw`; `taw machine add` and `taw machine remove` edit it; see "Remote machines" |
| `[plugins]` | `compat_herdr_version`, checked against a plugin's `min_herdr_version` |
| `[experimental]` | `pane_history`; see below |

The keys taw adds to herdr's, or reads with another default or meaning, with their defaults:

| Key | Default | Meaning |
|---|---|---|
| `[keys] preset` | `"modal"` | `modal` or `search`; per-action overrides on top, herdr's action names accepted |
| `[keys] select_session` | `["1", …, "9"]` (modal preset; `[]` in search) | the nth key moves the cursor to the session labelled n |
| `[ui] depth` | `"agents"` | how far the tree is expanded when it opens: `sessions`, `agents`, `all`; shells are one `l` away |
| `[ui] labels` | `true` | number the sessions (`1-9`) and letter their rows (`a-e`); `false` hides both, their keys do nothing, and the filter does not match them |
| `[ui] titles` | `true` | show each agent's own pane title on its row, when it has set one |
| `[theme.custom] attention` | peach `#fab387` (dark), `#fe640b` (light) | the colour for a blocked agent: its row, and its count on the list's border |
| `[ui] layout` | `"tree"` | `tree` or `flat` when it opens; `f` toggles |
| `[ui] preview` | `true` | the preview pane, beside the tree while the popup is 100 columns or wider; narrower, the tree has the whole width and no preview is drawn. `false` keeps it off at any width |
| `[ui] show_git` | `true` | branch and git flags on session headers |
| `[ui] projects_in_filter` | `false` | also list the directories `n` would offer (zoxide, `project_dirs`, path completion) just under the sessions while `/` is open; `Enter` on one opens or creates its session. See "Projects in the filter" |
| `[ui] agent_panel_sort` | `"attention"` | the flat list's order. `priority`, herdr's name for its own attention order, is accepted as an alias, though herdr's order differs (see "Beyond herdr"); `sessions` (aliases `spaces`, `workspaces`) is tmux order. The tree itself is always tmux order |
| `[ui] status_indicators` | `"symbols"` | `symbols` or `dots`; herdr's default is `dots` |
| `[ui] confirm_close` | `true` | covers `x` and worktree removal alike; off, `x` on a session closes just that session, and a removal git refused is still asked about |
| `[ui.sidebar.sessions]` | *(unset)* | accepted as an alias of `[ui.sidebar.spaces]` |
| `[ui.toast] done_delay_seconds` | `5` | how long a *done* toast waits before it fires; `0` restores herdr's timing |
| `[sessions] name` | `"directory"` | `directory`, `full-path`, or `short-path` |
| `[sessions] zoxide` | `true` | offer zoxide's directories in the picker, and run `zoxide add` on each directory opened, when `zoxide` is on `PATH`. `taw open <term>` asks zoxide for its match whatever this is set to |
| `[sessions] project_dirs` | `[]` | directories whose children are projects |
| `[sessions] pre_create_hook`, `post_create_hook` | *(unset)* | a command, run as `<hook> <session> <dir>`; see "Create hooks" |
| `[sessions] hook_timeout_seconds` | `10` | how long a create hook may run before taw stops it, and how long `pre_close` and `pre_worktree_remove` may take to answer; `0` means no limit for all of them; see "Create hooks" and "Hooks" |
| `[detection] supplements` | `true` | taw's own additive detection rules; see "Beyond herdr" |
| `[hooks] "<event>"` | *(unset)* | a command, or a list of them, run when the event fires; `taw hooks list` names every event; see "Hooks" |
| `[hooks] pre_close`, `pre_worktree_remove` | *(unset)* | commands taw waits for before a close or a worktree removal; a non-zero exit refuses |
| `[hooks] timeout_seconds` | `30` | how long an event hook may run before taw stops it; `0` means no limit |

Row templates use herdr's format. These are the defaults: while your rows equal them, and no `rows_by_agent`
entry is set, taw draws its own columns (below); set different rows to draw from the template instead:

```toml
[ui.sidebar.agents]
row_gap = 0
rows = [["state_icon", "workspace", "tab"], ["agent"]]

[ui.sidebar.spaces]                  # [ui.sidebar.sessions] is the same table
row_gap = 0
rows = [["state_icon", "workspace"], ["branch", "git_status"]]
```

A row drawn from a template keeps what the popup puts in front of every row while `[ui] labels` is on: a session's
number and its `*` or `-` flag, and an agent's letter and tree connector (in the flat list, the row's number), so the
keys stay on screen. When an agent's template draws more than one line, the tree line runs down through the further
lines of every row but the last. The template draws what follows. With `[ui] labels = false` the rows are the
template's alone.

The tokens of a row line up in columns down the list: each token position is a column across the rows drawn from the same template line (session rows with the sessions template, agent rows with the agents template, a template of several lines line by line). A column is as wide as the widest value in it, up to 24 columns (the agent's name up to 16), measured over every row the list holds, so scrolling moves nothing, and over the rows a filter keeps. Every token but the last one on a row is padded to its column; a longer value pushes only its own row's later tokens. No row ends in spaces, and the padding has no colour.

An agent row that needs you is drawn in its state's colour, bold while it is blocked, as the popup's own rows are: every token with no `fg` of its own that none of its `rules` restyled. A row of the window its session has open is lit as the popup's own rows of it are, on the same rows: the agent and its place (`tab`, `pane`) in the text colour, bold, the state word, the title and the session's name in the text colour, the state glyph in its state's colour. A token with its own `fg`, or one a rule restyled, keeps its own, and a row that needs you keeps its state's colour.

A token can be styled inline, `{ token = "workspace", fg = "#89b4fa", bold = true }`
(`fg` is `#rrggbb`, `#rgb` or a colour name), and can carry up to 16 `rules`, conditions
on its text that restyle it or hide it (herdr v0.9.1); the example's `$load` and `$machine`
are `$name` tokens, values a script or an agent sets on its pane with
`taw pane report-metadata <pane> --source <id> --token name=value`:

```toml
[ui.sidebar.agents]
rows = [
  ["state_icon", "workspace", "tab"],
  [{ token = "$load", fg = "#fff", rules = [{ gt = 80, fg = "#f55", bold = true }, { gt = 50, fg = "#fc0" }] }],
  [{ token = "$machine", rules = [{ equals = "Local", hide = true }] }],
]
```

The first rule a token's text meets wins. Its `fg`, `bold` and `dim` replace the token's
own, and what it leaves out stays as it was; with none met, the token is as written. A
rule has exactly one condition: `equals`, `contains` or `starts_with` for text (exact,
unless `ignore_case = true`, which folds ASCII letters only), or `gt` or `lt` for a number
(finite, compared strictly, and the whole text must be the number: `90` is over `gt = 80`,
`90%` is not). The text is the token's whole value, before it is cut to fit the row.
`hide = true` removes the token: in the middle of a row it leaves its column blank, so the tokens after it stay in line, and at the end of a row it leaves nothing. A row left with no token is
not drawn. `state_icon` and `git_status` take no rules. A rule that is wrong, with two
conditions or none, an unknown one (`regex`), a threshold that is not finite, or a
seventeenth rule, is a config error, and `taw config check` says which. A key a token
does not know, `hide` written outside a rule or `rule` for `rules`, is ignored when the
popup loads the config, and `taw config check` warns about it by name.

**Leave them alone and the popup does not use them.** Its default drawing is
column-aligned so the rows line up down the tree. An agent row is the connector,
the state glyph, *what* (the agent's label, or a plain pane's command), *where*
(the tmux target), the state word and how long it has been in it, and, with
`[ui] titles` on, the agent's own pane title. A *what* or a *where* too long for its
column is cut with an `…`, so the columns after it stay in line. A session header carries no state
glyph: it is the label, the `*`/`-` flag, the name in bold, then branch and git
flags when `[ui] show_git` is on; at the sessions-only depth it also shows one
count per state present and, with `titles` on, the title of its first agent that
needs you.

Set `rows` yourself and that row kind switches to herdr's template rendering,
space-joined, exactly as herdr draws it, so a herdr config that customised its
sidebar keeps working. The switch is per row kind and is decided by comparing your
value against the default above: `[ui.sidebar.agents] rows` (or *any*
`[ui.sidebar.agents.rows_by_agent]` entry) moves agent rows onto templates,
`[ui.sidebar.spaces] rows` moves session headers. Under a template the `tab` token
renders tmux's own target spelling (`index:name`, plus `.pane` when the window holds
more than one). Plain-pane and closed-worktree rows have fixed formats and ignore both
tables.

`[experimental] pane_history = true` saves each pane's recent screen alongside the
session snapshot and replays it into the restored pane, so a pane comes back with its
last output visible instead of an empty shell. It is **off by default, and worth
leaving off unless you want it**: a saved screen holds whatever was on it, including
secrets, tokens, prompts and command output, in a plain file under `$TAW_STATE_DIR`.
Treat that directory the way you treat your shell history. herdr's default and
reasoning are the same (`third_party/herdr/reference/session-state.mdx`). A restore replays the
screens only onto the layout they were taken with; "Save and restore" says what that means for a
recovered copy.

A few herdr keys are accepted (so herdr configs load) but have
no effect, because tmux (or your terminal, or your OS) owns the thing they'd
configure: `onboarding`, `theme.custom.light.*`/`dark.*`, `[terminal]` (including `terminal.kitty_graphics`),
`[remote]`/`[server]` (the tmux server is the server, and `[machines]` is how you list other machines, see "Remote machines"; `taw config check` names their keys as unknown),
`update.channel`/`version_check` (TPM updates the plugin, and a downloaded `taw`
follows it), `ui.show_agent_labels_on_pane_borders` and `ui.pane_borders` (see "Status
token" below), and `ui.toast.herdr.position` (`tmux display-message` has no configurable
screen position). `docs/PARITY.md` "Structural gaps" has the closest workaround for
each.

Whatever the theme, the preview draws a pane in the pane's own colours: what the pane leaves
at your terminal's default colours stays at them, so a translucent terminal shows through the
preview as it does through the pane. The `terminal` theme leaves the rest of the popup's
background to your terminal too. On tmux 3.3 and later taw opens its popup at your terminal's own
colours (`display-popup -s fg=terminal,bg=terminal`), so this holds on tmux 3.8 as well, whose
`popup-style` paints popups dark grey; your `popup-border-style` still draws the popup's frame.

`[theme] auto_switch = true` follows the terminal's light or dark mode, as in herdr: the
popup and `taw app` inside tmux draw `dark_name` while their tmux client's terminal is dark
and `light_name` while it is light, and change within a tick when the terminal's mode
changes. It works on tmux 3.6 and later (tmux's `#{client_theme}`, seen on 3.6b and 3.8),
with a terminal that reports its light or dark mode to tmux; where the mode is unknown, or
the tmux is older, the theme stays `name`, and `taw config check` warns about an older tmux.
A `dark_name` or `light_name` you leave out is `name`'s sibling for that mode
(`catppuccin` and `catppuccin-latte`), and `name` itself when it has none, so
`name = "tokyo-night"` stays tokyo-night. `taw app` outside tmux keeps `name`. With
`auto_switch` on, `taw app` inside tmux asks tmux one more question each tick; the popup asks
nothing extra.

## Status token (opt-in)

Outside the popup, `taw` never edits your status line itself. It publishes an
`@agent_wizard_status` global tmux option, a compact per-state count, tmux-format
styled, e.g. `⏸1 ⏵3 ✓2`, that you add yourself:

```tmux
set -ag status-right ' #{@agent_wizard_status}'
```

For a per-pane label instead (e.g. in `pane-border-format`), the plain tmux
options `#{@agent_kind}` and `#{@agent_state}` are readable in any pane-scoped
format string; `taw` sets them on every pane that has a detected agent. See
`examples/status-line.md` for three ready-to-paste placements (status line token,
pane-border token, and a dedicated second status row). The bar the popup puts on
your status line shows the same rail on its right.
