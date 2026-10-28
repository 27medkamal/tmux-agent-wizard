#!/usr/bin/env bash
# Installs the taw binary as bin/taw, where tmux-agent-wizard.tmux's binary
# resolution finds it. Run by hand, or by the plugin script itself (through
# scripts/auto-install.sh) when a first load finds no binary.
#
# With no flag it builds from source (cargo build --release) and symlinks
# bin/taw -> target/release/taw: the developer path. With --download it fetches
# the prebuilt release asset for this machine instead, checks its SHA-256, and
# falls back to building from source when there is no asset or the download
# fails. See docs/RELEASING.md for how the assets are made.
set -euo pipefail

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$CURRENT_DIR"

REPO_URL="https://github.com/27medkamal/tmux-agent-wizard"
# Where the assets live: <base>/v<version>/taw-<target>.tar.gz for one version,
# <latest>/taw-<target>.tar.gz for the newest release. Overridable for tests and
# mirrors (file:// works too).
RELEASE_BASE="${TAW_RELEASE_BASE:-$REPO_URL/releases/download}"
RELEASE_LATEST="${TAW_RELEASE_LATEST:-$REPO_URL/releases/latest/download}"
# Records what a download installed: "<installed version> <checkout version> <target>".
# The plugin script reads it to refresh the binary after the plugin is updated.
MARKER="bin/.taw-release"

usage() {
  cat <<'USAGE'
Usage: scripts/install.sh [--download]

Installs taw as bin/taw, where tmux-agent-wizard.tmux finds it.

  (no flag)    Build from source (cargo build --release) and symlink bin/taw
               to target/release/taw.
  --download   Download the prebuilt taw for this machine (macOS or Linux,
               Intel or ARM) from the release matching this checkout's version,
               or the latest release, and check its SHA-256. Builds from source
               instead if that fails and cargo is installed.
  -h, --help   Show this help and exit.
USAGE
}

say() { echo "tmux-agent-wizard: $*"; }

# The Rust target triple this machine's asset is named after. TAW_TARGET
# overrides the detection.
detect_target() {
  if [ -n "${TAW_TARGET:-}" ]; then
    echo "$TAW_TARGET"
    return 0
  fi
  case "$(uname -s)/$(uname -m)" in
  Darwin/arm64 | Darwin/aarch64) echo aarch64-apple-darwin ;;
  Darwin/x86_64) echo x86_64-apple-darwin ;;
  Linux/x86_64 | Linux/amd64) echo x86_64-unknown-linux-musl ;;
  Linux/aarch64 | Linux/arm64) echo aarch64-unknown-linux-musl ;;
  *) return 1 ;;
  esac
}

# This checkout's version: the package version in Cargo.toml.
checkout_version() {
  { sed -n 's/^version = "\([^"]*\)"$/\1/p' Cargo.toml 2>/dev/null || true; } | head -n 1
}

fetch() {
  local url="$1" dest="$2" https_only=""
  # Bounded, so a stalled network fails the download (and falls back) rather than
  # leaving the plugin waiting on it. An https download stays on https, redirects
  # included (file:// is for mirrors and the tests).
  if command -v curl >/dev/null 2>&1; then
    case "$url" in https://*) https_only="--proto =https --proto-redir =https" ;; esac
    # shellcheck disable=SC2086 # two flags and their values, split on purpose
    curl -fsSL $https_only --retry 2 --connect-timeout 15 --max-time 180 -o "$dest" "$url" 2>/dev/null
  elif command -v wget >/dev/null 2>&1; then
    # GNU wget has --https-only; busybox's wget has no such flag.
    case "$url" in
    https://*) case "$(wget --help 2>&1)" in *--https-only*) https_only=--https-only ;; esac ;;
    esac
    # shellcheck disable=SC2086 # empty or one flag
    wget -q $https_only --timeout=30 --tries=2 -O "$dest" "$url"
  else
    say "neither curl nor wget is installed, so nothing can be downloaded"
    return 1
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    return 1
  fi
}

# Downloads taw-<target>.tar.gz and its .sha256 from $1, checks the digest,
# checks the binary runs, and only then replaces bin/taw, unless bin/taw is a
# download of that same version already. $2 is the target, $3 this checkout's
# version (for the marker). Any failure leaves bin/taw as it was.
#
# The marker gives the checkout version the download is for only when it is that
# version's taw. An older one (the latest release, while this version has none yet)
# gives its own version instead, so the plugin tries again at its next load.
install_from() {
  local url="$1" target="$2" version="$3"
  local asset="taw-$target.tar.gz" tmp want got installed current fetched_for rc=1
  tmp=$(mktemp -d)
  if fetch "$url/$asset" "$tmp/$asset" && fetch "$url/$asset.sha256" "$tmp/$asset.sha256"; then
    want=$(awk '{print $1; exit}' "$tmp/$asset.sha256")
    got=$(sha256_of "$tmp/$asset" || true)
    if [ -z "$got" ]; then
      say "no sha256sum or shasum to check the download with"
    elif [ "$want" != "$got" ]; then
      say "the checksum of $asset does not match; not installing it"
    elif ! tar -xzf "$tmp/$asset" -C "$tmp" taw 2>/dev/null; then
      say "$asset holds no taw binary"
    elif ! installed=$("$tmp/taw" --version 2>/dev/null); then
      say "the downloaded taw does not run on this machine"
    else
      installed="${installed##* }"
      mkdir -p bin
      current=""
      if [ -f bin/taw ] && [ ! -L bin/taw ]; then
        current=$(bin/taw --version 2>/dev/null || true)
      fi
      if [ "${current##* }" = "$installed" ]; then
        say "taw $installed is already installed as bin/taw"
      else
        # Renamed into place in two steps so bin/taw is never missing: the first
        # move may cross filesystems, the second is an atomic rename in bin/.
        mv "$tmp/taw" bin/.taw.new
        chmod +x bin/.taw.new
        mv -f bin/.taw.new bin/taw
        say "installed taw $installed as bin/taw"
      fi
      fetched_for="$version"
      [ "$installed" = "$version" ] || fetched_for="$installed"
      printf '%s %s %s\n' "$installed" "$fetched_for" "$target" >"$MARKER"
      rc=0
    fi
  fi
  rm -rf "$tmp"
  return "$rc"
}

download() {
  local target version
  if ! target=$(detect_target); then
    say "there is no prebuilt taw for $(uname -s) $(uname -m)"
    return 1
  fi
  version=$(checkout_version)
  say "downloading taw $version for $target"
  if install_from "$RELEASE_BASE/v$version" "$target" "$version"; then
    return 0
  fi
  say "nothing installed from v$version; trying the latest release"
  install_from "$RELEASE_LATEST" "$target" "$version"
}

build() {
  if ! command -v cargo >/dev/null 2>&1; then
    say "cargo not found. Install Rust (https://rustup.rs) to build taw" >&2
    return 1
  fi
  say "building taw (cargo build --release)"
  cargo build --release
  mkdir -p bin
  ln -sfn ../target/release/taw bin/taw
  rm -f "$MARKER"
  say "installed bin/taw -> target/release/taw"
}

case "${1:-}" in
-h | --help)
  usage
  ;;
--download)
  if download; then
    exit 0
  fi
  # TAW_NO_BUILD=1: an update in the background keeps the binary it has rather
  # than starting a long source build nobody asked for.
  if [ -z "${TAW_NO_BUILD:-}" ] && command -v cargo >/dev/null 2>&1; then
    say "the download did not work; building from source instead"
    build
  else
    say "could not download taw, and cargo is not installed to build it (https://rustup.rs)" >&2
    exit 1
  fi
  ;;
"")
  build
  ;;
*)
  usage >&2
  exit 2
  ;;
esac
