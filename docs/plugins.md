---
title: Plugins
description: Install herdr's CLI-driven plugins in tmux-agent-wizard with the same manifest and trust preview, and keep them current. Plugin support is in beta.
---

# Plugins

Herdr-compatible: same `herdr-plugin.toml` manifest format, same trust-preview
install gate, same environment contract for the parts tmux can support.

!!! note "Plugin support is in beta"
    Ten real herdr plugins were installed and run before this release. The
    [compatibility matrix](COMPAT.md#compatibility-matrix) says which work, which cannot work in
    tmux, and which need a tool of their own. Other plugins have not been run under taw yet, so
    expect gaps.

```sh
taw plugin install owner/repo[/subdir][@ref] --yes   # or omit --yes for an interactive prompt
taw plugin update [id|source...] [--yes]             # fetch the installed ones again
```

Before running anything, install prints every `[[build]]` and `[[startup]]`
command the manifest declares and requires `--yes` or an interactive `y`: the
same trust preview herdr shows. A control character in the plugin's id, version or commands is written out
there (`\x1b`, `\r`), so a command cannot redraw its line to look like another. Plugin processes get `HERDR_BIN_PATH` (call back
into `taw` portably), `HERDR_PLUGIN_CONFIG_DIR`/`HERDR_PLUGIN_STATE_DIR`,
`HERDR_PLUGIN_CONTEXT_JSON`, and matching `HERDR_WORKSPACE_ID`/`HERDR_TAB_ID`/
`HERDR_PANE_ID`, plus a `TAW_` twin of every one of those. An action invoked with
`--pane` gets that pane's tab and workspace too (with `--tab`, its workspace). Invoked
without `--pane`, it gets a pane as herdr fills a missing context from the pane in front:
the active pane of `--tab` or `--workspace` when you give one, else the pane the command
runs in, else (outside tmux) the active pane of the session tmux takes as current. And
`HERDR_PLUGIN_CONTEXT_JSON` carries herdr's own fields (`workspace_id`, `tab_id`,
`focused_pane_id`, `focused_pane_cwd`, `workspace_cwd`, `selected_text` and the rest of
herdr's `PluginInvocationContext`, `null` where taw has nothing) with taw's nested
`workspace`, `tab`, `pane` and `caller_pane_id` beside them. `taw plugin pane open` takes
herdr's flags (`--plugin`, `--entrypoint`, `--placement`, `--direction`, `--target-pane`,
`--cwd`, `--width`, `--height`, `--env KEY=VALUE`, `--focus`) as well as `PLUGIN.PANE`, and
answers in herdr's shape (`plugin_pane` holds `plugin_id`, `entrypoint` and the pane's record), with
`--cwd` the folder the pane starts in and `--target-pane` the pane it opens beside.
A pane's program named by a path inside the plugin (`command = ["bin/tool"]`) is found in
the plugin's folder, as herdr finds it, wherever the pane starts; a bare name (`sh`, `node`)
is looked up on `PATH`. Actions, hooks and build steps run in the plugin's folder.
An event hook also gets `HERDR_PLUGIN_EVENT_JSON`, herdr's event envelope (`event`, and `data` with the fields
herdr's event has, `null` where taw has nothing). A popup or overlay pane needs a client to show it on: with none attached, the call fails with
"no attached client to open the popup on" (use `--placement split` there), and no error holds an `--env` value.
`taw plugin action invoke` takes herdr's `ACTION --plugin ID` as well as `ID.ACTION`, and `taw plugin action list`
and `taw plugin log list` take `--plugin`. `disable` and `uninstall` end the processes that plugin's `[[startup]]`
commands started in this boot: the command's process group while the command runs, and once it has exited
(a command that puts a daemon in the background), its group while a process in it started within 10 seconds of
the command. A group that cannot be told for the plugin's that way is left alone, with a line in taw's log.

`taw plugin update` fetches a plugin installed from GitHub again at the ref it was
installed with (a branch moves, a tag stays, a full commit pins), with no argument for
every installed plugin. Name a plugin by its id or by the source you installed it from
(`owner/repo[/subdir]`). A plugin whose ref still points at the commit you have is left
as it is, found by asking GitHub for the commit, with nothing downloaded; a plugin
pinned to a full commit is left as it is without asking at all. Each commit of
a plugin lives in a folder of its own, named for it, so a build that writes the path it
was built in (a Python virtual environment, for one) still finds it after an update. A
new commit is fetched into its folder beside the old one, shown in the same trust
preview with the same `--yes` rule, and built there; only then does the plugin switch to
it, and the old folder is removed (if that fails, the result says so, and the update
still stands). A build only ever runs in a folder that run made: `taw plugin install`
over a plugin you have at the commit you already have, and an update to a commit whose
folder another plugin lives in, build in a new folder of their own
(`<repo>@<12-char sha>-2`, then `-3` and so on) and switch to it the same way. Declining
or a failed fetch or build removes the new folder and leaves the copy you have exactly as
it was, build output included; a folder left by Ctrl-C at the prompt is removed at the
next install or update of that repository, and your records and the copy you have are
untouched. Plugins that an older taw installed from folders of one repository share its
copy, so they update together, and a decline by any of them changes none. A plugin you
linked from a local folder is left alone. An update keeps a plugin enabled or disabled
and starts nothing: the new copy's startup commands run the next time tmux starts, as
after `disable` and `enable`. A process the plugin started from its old folder keeps
running from it after an update: restart it, or run `taw plugin startup` once the old one
has stopped. `@<sha>` (and `--ref <sha>`) take a full 40-character commit and pin the
plugin to it; a ref that looks like a short commit (7 to 39 hex characters) and that
GitHub does not name as a branch or tag is refused before anything is fetched, with "give
the full 40-character commit, or a branch or tag". GitHub serves a commit of any fork of a
repository under that repository's address, so a pin should be a commit you have looked
at: the trust preview names the commit it fetched, so check that it is the one you meant.
The result
lists each plugin as updated, unchanged, linked or failed, with the reason; one failing
does not stop the others, and the command then exits 1.

**`HERDR_SOCKET_PATH` is never set.** There's no daemon and no socket here, so a
plugin that talks raw JSON-RPC over it directly (rather than through
`HERDR_BIN_PATH`), such as parts of `herdr-plus`, is unsupported. `herdr-browser` is
unsupported for another reason: it draws in a pane with graphics, which tmux does not
show. See `docs/COMPAT.md` for the full environment contract and a
compatibility read on specific known plugins.
