#!/usr/bin/env bash
# Fresh-install acceptance on a throwaway OrbStack Linux machine. Proves the
# README install path works from nothing on a clean OS, then runs the unit
# suite and the mechanical acceptance run inside the VM. Requires OrbStack
# (`orb`) on the host. Creates and (by default) deletes machine "taw-e2e-vm".
#
#   tests/e2e/vm-fresh-install.sh            # create, test, delete
#   tests/e2e/vm-fresh-install.sh --keep     # leave the machine running
set -eu
MACHINE="${TAW_E2E_VM:-taw-e2e-vm}"
KEEP="${1:-}"
command -v orb >/dev/null || { echo "orb (OrbStack) not found on PATH"; exit 1; }
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

echo "== creating $MACHINE (ubuntu) =="
orb list --format '{{.Name}}' 2>/dev/null | grep -qx "$MACHINE" || orb create ubuntu "$MACHINE"
run() { orb -m "$MACHINE" bash -lc "$*"; }

echo "== installing toolchain =="
run 'sudo apt-get update -qq && sudo apt-get install -y -qq tmux git curl build-essential pkg-config libssl-dev shellcheck bats >/dev/null'
run 'tmux -V; git --version'

echo "== copying the repo in (excluding build output) =="
run "rsync -a --delete --exclude target --exclude bin '/mnt/mac$REPO/' ~/taw/"

echo "== building via mise (falls back to rustup) =="
run 'command -v mise >/dev/null || curl -fsSL https://mise.run | sh'
run 'cd ~/taw && ~/.local/bin/mise trust >/dev/null 2>&1; ~/.local/bin/mise install >/dev/null 2>&1 || true; ~/.local/bin/mise run build 2>&1 | tail -2 || cargo build --release 2>&1 | tail -2'

echo "== unit suite =="
run 'cd ~/taw && ~/.local/bin/mise run check 2>&1 | tail -15 || scripts/check.sh 2>&1 | tail -15'

echo "== mechanical acceptance inside the VM =="
run 'cd ~/taw && bash tests/e2e/acceptance.sh 2>&1 | tail -40'

if [ "$KEEP" = "--keep" ]; then
  echo "== leaving $MACHINE running (--keep) =="
else
  echo "== deleting $MACHINE =="; orb delete --force "$MACHINE"
fi
