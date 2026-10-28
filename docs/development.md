---
title: Development
description: "How to work on tmux-agent-wizard itself: building it, and its docs."
---

# Development

```sh
mise install     # rust, tmux, shellcheck, bats: the pinned versions from mise.toml
mise run build    # cargo build --release, then links bin/taw to target/release/taw
mise run check     # cargo fmt --check, clippy -D warnings, cargo test, shellcheck, bats
```

Every test or script that starts a tmux server uses a throwaway one, never the
default: `tmux -L <unique-socket-name> -f /dev/null new-session -d … /bin/sh`,
followed by `set-option -g default-shell /bin/sh` before any other pane is
created. `-f /dev/null` stops a fresh `-L` socket from loading the host's own
`tmux.conf` (`base-index`, hooks, a TPM autoload line, …); `/bin/sh`, as both
the pane's start command and `default-shell`, keeps a login shell's rc-file
banners or interactive prompts from swallowing the keystrokes a test sends.
Give every server its own socket name (e.g. a prefix plus the test process's
pid and a per-fixture counter) so parallel `cargo test` runs never collide.
See `tests/tmux_client.rs` and `tests/cli_json.rs` for this in practice, and run
`scripts/cli-sweep.sh` for an envelope-conformance sweep of every CLI verb
against a server set up the same way (`mise run check` doesn't run it; invoke
it directly).

No test runs an agent or a container runtime installed on the machine: a test that
types an agent's command finds a fake of its own first (`ServerOptions::path_prefix`,
`common::named_agent`), or gives its panes a narrow `PATH` of their own. A test that
restores a pane saved with an agent puts a fake of that agent first too, one that notes
each of its runs, and checks that nothing ran that should not. `mise run
check` and the acceptance run put a net of refusing stubs first on `PATH` to hold them
to that (`scripts/agent-net.sh`, one stub for every name in
`scripts/agent-net-names.txt`): a test that runs `claude`, `codex`, `docker` and the
rest from the runner's `PATH` reaches a stub, which notes the call and refuses it, and
the gate fails, naming each call. `tests/agent_net.rs` shows the net is there.

The same net holds the five names taw logs in to other machines by (`scripts/ssh-net-names.txt`): `ssh`, `scp`, `sftp`, `ssh-add` and `ssh-agent`. A test that wants a machine runs taw with `TAW_SSH_BIN` naming a stand-in, never one of these, so no test logs in to a real machine.

Nor does a test show a notification or play a sound on the machine that runs it: a
test that looks at what taw shows or plays records it (`$TAW_NOTIFY_CMD`,
`$TAW_SOUND_CMD`), and a test server (`tests/common`) starts with refusing recorders
for both where its test sets none. `mise run check` and the acceptance run put a
second net of refusing stubs first on `PATH` (`scripts/desktop-net.sh`, one stub for
every name in `scripts/desktop-net-names.txt`: `osascript`, `notify-send`, `afplay`
and the other notifiers and players) and point the two variables at refusing
recorders of the net's, so a test that reaches any of them with no recorder of its
own is noted and refused, and the gate fails, naming each call. `tests/desktop_net.rs`
shows the net is there. A targeted `cargo test` outside the gate has the same default:
`.cargo/config.toml` sets `TAW_NOTIFY_CMD` and `TAW_SOUND_CMD` to
`scripts/refuse-desktop.sh` (a script that refuses), unless the variables are already
set, so the gate's recorders and a test's own win. The same two defaults apply to
`cargo run`: a notification or a sound that taw shows from a `cargo run` is refused
until you unset the variables (`env -u TAW_NOTIFY_CMD -u TAW_SOUND_CMD cargo run ...`)
or set your own. The binary from `mise run build` is not run by cargo and is not
affected. `tests/gate_checks.rs` holds this config, and that the gate and the
acceptance run still make each net and read each log.

Nor does a test's git reach a repository that git's variables point at (git runs the
command of a `rebase -x` with `GIT_DIR` set): every git of taw's and of the tests' is
made by `taw::git::command()`, which leaves them out, and a script that runs git
unsets them first. Each test file that makes repositories runs those tests again with
every such variable pointing into a decoy repository, which must stay byte for byte
as it was (`tests/common/git_decoy.rs`), and `tests/git_env.rs` holds every git there
is to that, and holds the list of variables to the one `git rev-parse --local-env-vars`
gives. `mise run check` also runs its `cargo test` step and its `bats` step with every
one of them pointing into a decoy repository made for the run (`scripts/git-decoy.sh`),
looks at it when each step has ended, and fails naming each file that changed, which
holds a git that nobody listed; `tests/git_decoy_gate.rs` shows it does.

`docs/try/index.html` is the "try it in your browser" page, published at
<https://27medkamal.com/tmux-agent-wizard/try/> by GitHub Pages (the Pages source is
`docs/`, and `docs/index.html` sends a visitor on to `try/`). It is the popup running over made-up projects,
built from the popup's design outside this repository. Change it in the same commit
as the popup, so the page people click from the README never describes a popup this
project no longer has (`tests/try_page.rs` holds what the page must and must not say).
`docs/assets/demo.gif` is recorded the same way whenever the
popup changes: see `scripts/demo/README.md`.

## Docs

- `docs/PARITY.md`: what matches herdr and how, the structural gaps tmux cannot
    close (each with a workaround), and what is deliberately pinned.

- `docs/COMPAT.md`: the plugin environment contract, all 19 plugin event names and
    what fires each (and how `[hooks]` hears them), and a compatibility read on `herdr-reviewr`,
    `herdr-file-viewer`, `herdr-navigator`, `herdr-plus`, and `herdr-browser`.

- `docs/ARCHITECTURE.md`: the architecture diagram, module map, every
    `@agent_*`/`@agent_wizard_*` tmux option, the watcher's transition rules, id
    mapping, and a list of non-obvious tmux behaviour taw works around.

- `docs/RELEASING.md`: how the plugin gets its binary on a first load and after an
    update, how to cut a release, and how to try one locally.

- `docs/try/index.html`: the "try it in your browser" page, published at
    <https://27medkamal.com/tmux-agent-wizard/try/>.

- `examples/tmux.conf`, `examples/status-line.md`.
