---
title: Install
description: "Install tmux-agent-wizard with one TPM line on tmux 3.2 or newer, macOS or Linux: the checked download, a build from source, what goes online, how to remove it."
---

# Install

Needs tmux 3.2 or newer, on macOS or Linux (Intel or ARM).

**1.** Add the plugin to `~/.tmux.conf`:

```tmux
set -g @plugin '27medkamal/tmux-agent-wizard'
```

**2.** Press `prefix + I` (TPM installs it and loads it). The first load downloads the `taw` binary
for your machine from the project's
[GitHub releases](https://github.com/27medkamal/tmux-agent-wizard/releases). The download is
checked against the release's SHA-256 checksum first and is never installed when the two
differ, or when there is no `sha256sum` or `shasum` to check it with (with `cargo` on the
machine, taw is built from source then). Building it from source from the start is under
[Details](#details).

**3.** Press `prefix + A`. Done: every pane is watched from now on.

No [TPM](https://github.com/tmux-plugins/tpm)? Clone the plugin and load it from `~/.tmux.conf` instead
of steps 1 and 2:

```sh
git clone https://github.com/27medkamal/tmux-agent-wizard ~/.tmux/plugins/tmux-agent-wizard
echo "run-shell ~/.tmux/plugins/tmux-agent-wizard/tmux-agent-wizard.tmux" >> ~/.tmux.conf
tmux source-file ~/.tmux.conf
```

**4. Optional: `taw` outside tmux too.** `taw` is on `PATH` in every pane and window you open from
inside tmux after the plugin loads. A shell outside tmux, the first pane of a session you start
from a terminal, and a pane that was already open keep their terminal's `PATH`. Give them `taw`
with one link, made once, that follows every update (under `$XDG_STATE_HOME` or `$TAW_STATE_DIR`,
that folder instead):

```sh
mkdir -p ~/.local/bin && ln -s ~/.local/state/tmux-agent-wizard/bin/taw ~/.local/bin/taw
```

`~/.local/bin` has to be on that shell's `PATH`, or use another folder that is (macOS's zsh does
not have it there by default), so add it if it is missing:

```sh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc    # zsh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc   # bash (~/.bash_profile on macOS)
```

`set -g @agent-wizard-path off` turns the `PATH` entry in tmux off, and a reload then takes it out
of tmux's `PATH`.

If the install fails, [Troubleshooting](troubleshooting.md) has what each message means.

## Details

A TPM plugin and one binary. Your `tmux.conf`, keys, plugins, status line and ssh sessions stay
as they are. The first load downloads the `taw` binary for your machine (macOS or Linux, Intel
or ARM), and tmux says `tmux-agent-wizard: taw is ready, press prefix + A` when it has.

Needs tmux 3.2 or newer. To build it yourself instead, run `./scripts/install.sh`
(a Rust toolchain; `mise.toml` pins the version): the plugin never replaces a build of
your own, and `set -g @agent-wizard-auto-install off` stops the download. taw watches every pane on
its own (one small watcher process per pane); `set -g @agent-wizard-watch off` turns that
off.

After a rebuild or an update, the watchers that are already running keep the `taw` they
started with. `taw watch status` marks them, and `taw watch attach-all --restart`, run
with the new one, starts each of them again (see Troubleshooting). When the plugin
updates a `taw` it downloaded, it does that itself.

## What goes online

At most once every 6 hours, taw asks herdr.dev for newer
detection rules, with `tmux-agent-wizard/<version>` as its `User-Agent`;
`[update] manifest_check = false` turns that off. Everything else that goes online is
something you start:

- The plugin's installer downloads the `taw` binary from GitHub on the first load and
    after an update (until that version's release is out, each time tmux loads the
    plugin). On the first load, when the download fails and cargo is installed, it builds
    `taw` from source, which fetches its crates from crates.io.

- `taw manifest update` fetches the detection rules from herdr.dev there and then.
- `taw plugin install` and `taw plugin update` fetch the plugin you name (or each
    one you installed) from GitHub and run its build steps, which may fetch more.

- A sandbox: the first `taw sandbox` builds its image, and again whenever an update
    changes its recipe, and your container runtime downloads what the image is built from
    (mise's image from ghcr.io and a few Debian packages). An agent's first start in the
    sandbox has mise install the agent (from npm for most of them) or runs its maker's
    installer, and then installs node's latest long-term release when the agent needs it
    and the project names none, and the tools in the project's `mise.toml`. `docker=read-only` also has
    your container runtime pull a small Docker proxy image from Docker Hub.

taw itself does nothing else online.

## Remove

1. While `taw` is still there, take out what it put into other programs.
    `taw integration status` lists every agent with whether taw's hook is installed in it, and
    `taw integration uninstall <agent>` takes it out of each one that has it, putting back each
    config file it edited byte for byte when nothing else changed the file since (see
    [Integrations](agents.md#integrations)). If you installed herdr plugins, `taw plugin list`
    names them and `taw plugin uninstall <plugin>` removes each one.
2. Delete the plugin line from `~/.tmux.conf` and press `prefix + alt + u`: TPM removes the
    plugins that are no longer listed, taw's folder and its `taw` binary with it. Without TPM,
    delete the `run-shell` line and the clone, `~/.tmux/plugins/tmux-agent-wizard`.
3. Delete taw's state folder, `~/.local/state/tmux-agent-wizard` (`$XDG_STATE_HOME/tmux-agent-wizard`
    or `$TAW_STATE_DIR` when set): saved layouts, logs, downloaded detection rules and herdr
    plugins. Delete its config folder too if you made one, `~/.config/tmux-agent-wizard`
    (`$XDG_CONFIG_HOME/tmux-agent-wizard` or `$TAW_CONFIG_DIR`), and the `~/.local/bin/taw` link
    and its `PATH` line if you added them.
4. If you ran `taw sandbox`, remove its images and volumes with your container runtime: their
    names begin with `taw-sandbox-`.

The keys, tmux hooks and per-pane watchers taw added to the tmux server that is running stay
until that server ends (each watcher also ends when its pane closes); a new tmux server starts
without them.
