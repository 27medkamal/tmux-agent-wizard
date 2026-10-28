---
title: Coming from herdr
description: "Moving from herdr to tmux: your config.toml, key names, agent integrations and CLI-driven plugins (in beta) carry over to tmux-agent-wizard."
---

# Coming from herdr

Copy your `config.toml` to `~/.config/tmux-agent-wizard/config.toml`; it means what it
meant, and `taw config check` points at anything that does not apply inside tmux.
herdr's key action names are accepted. Drop the `prefix+` from each binding, because
inside tmux the prefix belongs to tmux: `goto = "prefix+g"` becomes `goto = "g"`.
Agent integrations install into the same directories (`taw integration install
claude`), and CLI-driven herdr plugins install with the same trust preview
(`taw plugin install owner/repo`); plugin support is in beta, and the
[compatibility matrix](COMPAT.md#compatibility-matrix) says which plugins were run and how they did. An agent that reports to herdr from its own code reports
to taw with `$TMUX_PANE` and `taw` in place of `$HERDR_PANE_ID` and `$HERDR_BIN_PATH` (see
[Report an agent of your own](agents/report.md)), and the agents that do that with nothing
installed (Crush, Command Code and the others herdr lists) can report to taw as they are with
`@agent-wizard-herdr-env` on (see [Agents that report to herdr themselves](agents/report.md#agents-that-report-to-herdr-themselves)).
A machine that herdr saves with `herdr machine add` is saved with `taw machine add` (see [Remote machines](machines.md)), and herdr's `[remote]` and `[server]` tables stay accepted and ignored.
