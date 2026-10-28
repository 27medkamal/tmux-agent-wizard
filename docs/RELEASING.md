---
description: How a tmux-agent-wizard version reaches users, and how to try one before it goes out.
---

# Releasing

How a version reaches users, and how to try one before it goes out.

## What a user gets

TPM clones the plugin at its default branch. On the first load with no `taw`
binary, `tmux-agent-wizard.tmux` starts `scripts/auto-install.sh` in the background,
which runs `scripts/install.sh --download`:

1. It works out this machine's target: `aarch64-apple-darwin`, `x86_64-apple-darwin`,
   `x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl` (the Linux builds are
   static, so they run on any distribution).
2. It fetches `taw-<target>.tar.gz` and its `.sha256` from the GitHub release named
   after the checkout's version (`v` plus `version` in `Cargo.toml`), or from the
   latest release when that version has none.
3. It checks the SHA-256, checks the binary runs, and only then puts it at `bin/taw`,
   with `bin/.taw-release` recording what was installed, the checkout version it was
   fetched for, and the target. A taw older than the checkout (the latest release,
   while the checkout's own version has none yet) gives its own version as the one it
   was fetched for, so the next load tries again.
4. The plugin loads itself again, and the status line says `taw is ready`.

When there is no asset for the machine, or the download fails, it builds from
source if `cargo` is installed. `set -g @agent-wizard-auto-install off` turns all of
this off; the plugin then only says the binary is missing.

After TPM updates the plugin (`prefix + U`) to a checkout with a new version, the next
load fetches the matching binary in the background while the old one keeps working.
Once the new binary is in place, the watchers already running are started again from
it (`taw watch attach-all --restart`), unless watching is off. When the release it
finds is the taw already there, nothing changes and nothing is said, and the load
after that tries again.
Only a downloaded binary is ever refreshed: a source build (`scripts/install.sh` with
no flag, or `mise run build`) links `bin/taw` to `target/release/taw` and leaves no
`bin/.taw-release`, and the plugin never downloads over a link. That holds when the
build behind the link has gone (after a `cargo clean`, say: the plugin then says to
build again) and beside a marker an earlier download left.

## Cutting a release

A merged version bump releases by itself:

1. Set `version` in `Cargo.toml` to the new version, run `cargo check` so that
   `Cargo.lock` follows, and commit the two together: `scripts/package-release.sh`
   builds with `--locked`, which refuses a lock file that no longer matches.
2. Run the release gate (`mise run check`) and merge to master.
3. Push master. `.github/workflows/release.yml` starts on a push that changes
   `Cargo.toml`, reads the version, and stops if the tag `v<version>` is already on
   GitHub. Otherwise it builds the four targets with the Rust version `mise.toml`
   pins, and then creates the release, with the four archives and their checksums and
   generated notes, and its tag at the commit it built. A Cargo.toml change that keeps
   the version releases nothing. The workflow's own tag starts no second run.
4. `scripts/install.sh --download`, which the plugin runs, fetches from that release
   (the one named after the checkout's version, else the latest).

The tag appears only after all four targets have built, so a failed build leaves no
tag: fix it, merge, and push again, or press Run workflow on master (Actions, release).

To make a release by hand, tag the commit and push only the tag:

```sh
git tag -a v0.1.0 -m v0.1.0
git push origin v0.1.0
```

The workflow then checks that the tag matches `Cargo.toml`, builds, and publishes the
release for that tag (nothing is built when its release exists already). The Run
workflow button does the same as a push of the bump, and only from master.

Between the push and the end of the workflow (a few minutes), master names a version
that has no release yet. A fresh TPM install or update in that window installs the
latest release instead and tries again at the next load, as described above, so
nothing breaks. Bump the version only when you want a release: every merged bump
makes one.

## What CI checks

`.github/workflows/ci.yml` runs on every push to a branch and every pull request:
`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and
`cargo build --release`, on Linux and macOS, with the Rust version `mise.toml` pins.
It does not run the tests: the suite drives real tmux servers and shells, and runs in
the release gate on the maintainer's machine.

## Trying a release locally

Do this in a throwaway clone, never in the checkout your tmux loads the plugin from:
`scripts/install.sh --download` puts the downloaded `taw` at `bin/taw` of the checkout
it runs in, so in your plugin's own directory it would take the place of your build.
In the clone, build and package this machine's target, lay it out the way the release
does, and point the installer at it:

```sh
git clone . /tmp/taw-try && cd /tmp/taw-try     # a throwaway clone
target=aarch64-apple-darwin                      # or this machine's target
scripts/package-release.sh "$target" /tmp/taw-release/v0.1.0
TAW_RELEASE_BASE=file:///tmp/taw-release scripts/install.sh --download
```

`TAW_RELEASE_BASE` and `TAW_RELEASE_LATEST` replace the two GitHub URLs, and
`TAW_TARGET` overrides the target detection. `tests/bats/install.bats` uses the same
three to test the download against `file://` releases.

## Checking the crate

`cargo package --list` shows what `cargo publish` would upload; publishing to
crates.io makes `cargo install tmux-agent-wizard` work.
