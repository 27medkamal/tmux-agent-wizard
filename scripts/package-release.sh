#!/usr/bin/env bash
# Builds taw for one Rust target and packages it the way `scripts/install.sh
# --download` fetches it: <outdir>/taw-<target>.tar.gz, holding the taw binary
# and the licences, plus <outdir>/taw-<target>.tar.gz.sha256. The release
# workflow (.github/workflows/release.yml) runs this once per target; run it by
# hand to try a release locally (docs/RELEASING.md).
#
#   scripts/package-release.sh <target> [outdir]    # outdir defaults to dist
set -euo pipefail

if [ $# -lt 1 ] || [ "$1" = -h ] || [ "$1" = --help ]; then
  echo "Usage: scripts/package-release.sh <target> [outdir]"
  exit 2
fi
target="$1"
outdir="${2:-dist}"

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$CURRENT_DIR"

cargo build --release --locked --target "$target"

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp "target/$target/release/taw" "$stage/taw"
# The binary carries code derived from herdr (Apache-2.0) and a patched copy of
# nucleo-matcher (MPL-2.0), so their licence texts travel with it, beside this
# project's own MIT licence and the per-file list.
cp LICENSE THIRD_PARTY_LICENSES.md "$stage/"
cp third_party/herdr/LICENSE "$stage/LICENSE-herdr"
cp third_party/nucleo-matcher/LICENSE "$stage/LICENSE-nucleo-matcher"

mkdir -p "$outdir"
asset="taw-$target.tar.gz"
tar -czf "$outdir/$asset" -C "$stage" taw LICENSE LICENSE-herdr LICENSE-nucleo-matcher THIRD_PARTY_LICENSES.md

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$outdir" && sha256sum "$asset" >"$asset.sha256")
else
  (cd "$outdir" && shasum -a 256 "$asset" >"$asset.sha256")
fi

echo "$outdir/$asset"
echo "$outdir/$asset.sha256"
