# Status line placements

`tmux-agent-wizard` never edits your `status-left`/`status-right`/
`pane-border-format` itself, and never modifies the user's status
line, keybindings, hooks, or config destructively. Instead it publishes tokens you
place yourself. Three ways to use them, from least to most involved.

## 1. `status-left` / `status-right` token

The opt-in "compact status rail" (herdr's collapsed status summary): one glyph and
count per non-zero agent state across the whole server, e.g.
`⏸1 ⏵3 ✓2` (blocked 1, working 3, done 2; idle and unknown are omitted here
because they're zero). Colour and glyph style (`dots` vs `symbols`) follow
`[ui] status_indicators` in `config.toml`.

```tmux
set -ag status-right ' #{@agent_wizard_status}'
```

While the popup is open, taw swaps your status line for its own bar (the counts of
sessions and agents, the machines when you list two or more, and the clock). The bar does not show this token, so the token
is for your own line the rest of the time.

Use `-ag` (append) rather than `-g` so you don't clobber whatever else is already
in `status-right`. Put it in `status-left` instead if that's where you'd rather see
it. It's a plain tmux global option, so `taw` doesn't care which side you put it on.

## 2. Per-pane token in `pane-border-format`

`[ui] show_agent_labels_on_pane_borders` in `config.toml` is accepted for
herdr-config compatibility but has no effect: `taw` never touches
`pane-border-format` (see `config_default.toml`'s header comment). Add the
per-pane tokens yourself instead: `#{@agent_kind}` and `#{@agent_state}` are plain
tmux pane options, readable in any pane-scoped format string.

```tmux
set -g pane-border-status top
set -g pane-border-format ' #{pane_index} #{?@agent_kind,#{@agent_kind}:#{@agent_state} ,}#{pane_title} '
```

`#{?@agent_kind,...}` is tmux's own conditional format: the bracketed
`<kind>:<state>` prefix only renders in panes that actually have an agent (an
unset `@agent_kind` expands to the empty string, which tmux's `#{?...}` treats as
false). A pane with no agent just shows its title, exactly as it would without
this plugin installed.

## 3. A dedicated second status row

tmux 3.2+ (this project's own floor) supports a multi-row status line via the
`status` option and the `status-format[]` array. Give the agent rail its own row
instead of squeezing it into `status-right`:

```tmux
set -g status 2
set -g status-format[1] '#[align=centre]#{@agent_wizard_status}'
```

Only `status-format[1]` needs setting; leave `status-format[0]` alone and tmux
keeps rendering your normal status line (window list, `status-left`,
`status-right`) on row 0 exactly as it did with `status 1`. Row 1 here just shows
the agent rail, centred; change `align=centre` to `left`/`right`, or combine it
with other `#{...}` formats, same as any other `status-format` entry.
