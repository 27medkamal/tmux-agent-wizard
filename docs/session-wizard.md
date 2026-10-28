---
title: Coming from tmux-session-wizard
description: "Coming from tmux-session-wizard: each of its options mapped to tmux-agent-wizard, and how the two run side by side."
---

# Coming from tmux-session-wizard

[tmux-session-wizard](https://github.com/27medkamal/tmux-session-wizard) answers
"take me to a project". The popup answers that alongside "who needs me", so the two
overlap. Nothing is shared between them: **taw reads none of session-wizard's
`@session-wizard*` options**, and running both is fine: they are separate keys onto
separate popups.

| tmux-session-wizard | Here |
|---|---|
| `@session-wizard` (the key) | `prefix + A`, then `n` for the project picker, or `/` with `[ui] projects_in_filter` on to search sessions and projects at once; or `taw open` from a shell |
| `@session-wizard-mode` | `[sessions] name`: same three modes, same names (`directory`, `full-path`, `short-path`) |
| `@session-wizard-windows` | `[ui] depth`: `sessions` for headers only, `all` for every pane under them |
| `@session-wizard-preview` | `[ui] preview` |
| `@session-wizard-pre-create-session-hook` / `-post-create-session-hook` | `[sessions] pre_create_hook` / `post_create_hook`: the same command form, arguments, moments and answers; see "Create hooks" |
| `@session-wizard-log-file` | `taw config check` for config problems; the watcher's own log under `$TAW_STATE_DIR` for detection |
| `ctrl-x` to kill a session from the list | `x` on a session header |
| zoxide as the source of projects | `[sessions] zoxide`, plus filesystem completion and `[sessions] project_dirs` |
| `t <path or zoxide term>` from the shell | `taw open <path or zoxide term>`, and `alias t='taw open'` if you miss the name; see "Open a session from your shell". `taw session open <path>` is the same open for an agent or a script, and prints the envelope |
| `t` with nothing after it | `taw open` with nothing after it opens the popup on its new-session picker (inside tmux) |

**The hooks behave the same.** The value is a command that may carry its own flags, the
pre-create hook runs on every open before a running session is looked for and can print a
name and a directory, and the post-create hook runs only for a session that was just
made. A script written for session-wizard's hooks runs here as it is, unless it reads
standard input or prompts inside the popup (see "Create hooks"), and it is given taw's
session name, which is the difference below. The one addition is a limit: taw stops a hook
that runs longer than `[sessions] hook_timeout_seconds` (10 seconds unless you change it),
where the wizard waits as long as the hook takes.

**One deliberate difference: session names.** tmux-session-wizard lowercases the name
and replaces spaces, `.` and `:` with `-`. taw applies only tmux's own rule (`.` and
`:` become `_`) and leaves case, spaces and `/` alone. Lowercasing merges directories
that tmux would happily keep apart (`~/work/API` and `~/work/api` become one name),
and taw would rather show you two sessions than silently join them. When two paths
really do produce the same name, the collision suffix disambiguates them instead
(see "Naming").
