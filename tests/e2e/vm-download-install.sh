#!/usr/bin/env bash
# The download install path on a clean Linux machine (OrbStack Ubuntu): builds the
# static release asset for the machine inside it, lays it out the way a GitHub
# release does, then does what TPM does (clones the plugin and runs its .tmux file)
# with a tmux whose PATH holds neither taw nor cargo. Passes when the plugin fetches
# the asset, checks it, installs bin/taw and loads itself, as a new user sees it.
# Nothing is fetched from GitHub: the plugin comes from a local clone and the
# release from a file:// directory. Creates and (by default) deletes "taw-e2e-dl".
#
#   tests/e2e/vm-download-install.sh            # create, test, delete
#   tests/e2e/vm-download-install.sh --keep     # leave the machine running
# The commands in single quotes run inside the machine, so their $ is meant for it.
# shellcheck disable=SC2016
set -eu
MACHINE="${TAW_E2E_VM:-taw-e2e-dl}"
KEEP="${1:-}"
command -v orb >/dev/null || { echo "orb (OrbStack) not found on PATH"; exit 1; }
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
run() { orb -m "$MACHINE" bash -lc "$*"; }

echo "== creating $MACHINE (ubuntu) =="
orb list --format '{{.Name}}' 2>/dev/null | grep -qx "$MACHINE" || orb create ubuntu "$MACHINE"

echo "== tools: tmux, git, a C toolchain for musl, Rust =="
run 'sudo apt-get update -qq && sudo apt-get install -y -qq tmux git curl file build-essential musl-tools rsync >/dev/null'
run 'command -v rustup >/dev/null || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null'
run 'tmux -V'

echo "== the release asset for this machine, built inside it =="
# Never copy .git: in a worktree it is a file naming the host's repository, and the
# machine can reach host paths, so git commands in the copy would write to the host.
run "rsync -a --delete --exclude .git --exclude target --exclude bin --exclude dist '/mnt/mac$REPO/' ~/src/"
run 'cd ~/src && . ~/.cargo/env && rustup toolchain install 1.95.0 --profile minimal >/dev/null 2>&1; rustup default 1.95.0 >/dev/null; target=$(uname -m)-unknown-linux-musl; rustup target add "$target" >/dev/null && version=$(sed -n "s/^version = \"\([^\"]*\)\"$/\1/p" Cargo.toml | head -n 1) && scripts/package-release.sh "$target" ~/release/v$version | tail -2'
run 'file ~/release/v*/taw-*.tar.gz; tar -xzOf ~/release/v*/taw-*.tar.gz taw | file -'

echo "== what TPM does: clone the plugin, run its .tmux file =="
run 'rm -rf ~/plugin.git ~/.tmux/plugins && cd ~/src && git init -q && git add -A && git -c user.name=e2e -c user.email=e2e@example.invalid commit -q -m e2e && git clone -q --bare ~/src ~/plugin.git && mkdir -p ~/.tmux/plugins && git clone -q ~/plugin.git ~/.tmux/plugins/tmux-agent-wizard'
# No history file and a ZDOTDIR on the line that starts the server, as on every start of a server in the
# tests (tests/shell_isolation.rs holds them to it), though the shells here are the machine's own.
run 'tmux -L e2e kill-server 2>/dev/null; env -i HOME="$HOME" HISTFILE=/dev/null ZDOTDIR="$HOME" TERM=xterm-256color PATH=/usr/bin:/bin TAW_RELEASE_BASE="file://$HOME/release" TAW_RELEASE_LATEST="file://$HOME/release/none" tmux -L e2e -f /dev/null new-session -d -x 120 -y 30'
run 'tmux -L e2e run-shell "bash ~/.tmux/plugins/tmux-agent-wizard/tmux-agent-wizard.tmux"'

echo "== result =="
run 'for i in $(seq 1 100); do tmux -L e2e show-messages | grep -q "taw is ready" && break; sleep 0.2; done
  p=~/.tmux/plugins/tmux-agent-wizard
  status=0
  tmux -L e2e show-messages | grep -q "downloading taw for this machine" && echo "PASS said it was downloading" || { echo "FAIL no downloading message"; status=1; }
  tmux -L e2e show-messages | grep -q "taw is ready, press prefix + A" && echo "PASS said it was ready" || { echo "FAIL no ready message"; status=1; }
  [ -f "$p/bin/taw" ] && [ ! -L "$p/bin/taw" ] && echo "PASS bin/taw is the downloaded file" || { echo "FAIL no downloaded bin/taw"; status=1; }
  "$p/bin/taw" --version && echo "PASS it runs" || { echo "FAIL it does not run"; status=1; }
  file "$p/bin/taw" | grep -q "statically linked" && echo "PASS static binary" || { echo "FAIL not static"; status=1; }
  echo "marker: $(cat "$p/bin/.taw-release")"
  tmux -L e2e list-keys -T prefix A | grep -q "$p/bin/taw" && echo "PASS prefix + A opens it" || { echo "FAIL prefix + A not bound"; status=1; }
  sed "s/^/  install.log: /" "$p/bin/install.log"
  tmux -L e2e kill-server
  exit $status' && result=0 || result=1

if [ "$KEEP" = "--keep" ]; then
  echo "== leaving $MACHINE running (--keep) =="
else
  echo "== deleting $MACHINE =="
  orb delete --force "$MACHINE"
fi
exit "$result"
