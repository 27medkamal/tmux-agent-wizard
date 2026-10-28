#!/usr/bin/env bats
#
# Tests for scripts/install.sh. install.sh is copied into an isolated
# throwaway directory per test (so no test ever creates bin/ or target/ in
# the real plugin checkout) and run there against a fake `cargo` on PATH —
# that keeps the suite fast and independent of whether the taw crate
# currently builds; a real `cargo build --release` is not this suite's job.

setup() {
  PLUGIN_DIR="$(cd "$(dirname "$BATS_TEST_FILENAME")/../.." && pwd)"
  WORK="$BATS_TEST_TMPDIR/plugin"
  mkdir -p "$WORK/scripts"
  cp "$PLUGIN_DIR/scripts/install.sh" "$WORK/scripts/install.sh"
  chmod +x "$WORK/scripts/install.sh"
  # The checkout's version, which --download fetches the release of.
  printf '[package]\nname = "tmux-agent-wizard"\nversion = "9.9.9"\n' >"$WORK/Cargo.toml"
  # No case reaches the network, not even one whose fakes go wrong: --download looks in
  # an empty file:// directory unless the case points it at releases of its own
  # (use_local_releases).
  export TAW_RELEASE_BASE="file://$BATS_TEST_TMPDIR/no-releases"
  export TAW_RELEASE_LATEST="file://$BATS_TEST_TMPDIR/no-releases"
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

# make_release <dir> <version>: the layout --download fetches from, holding
# taw-test-target.tar.gz (a fake taw answering "taw <version>") and its
# .sha256 in the `shasum -a 256` format the release workflow writes.
make_release() {
  local dir="$1" version="$2" pkg="$BATS_TEST_TMPDIR/pkg-$2"
  mkdir -p "$dir" "$pkg"
  printf '#!/usr/bin/env bash\necho "taw %s"\n' "$version" >"$pkg/taw"
  chmod +x "$pkg/taw"
  tar -czf "$dir/taw-test-target.tar.gz" -C "$pkg" taw
  printf '%s  taw-test-target.tar.gz\n' "$(sha256 "$dir/taw-test-target.tar.gz")" \
    >"$dir/taw-test-target.tar.gz.sha256"
}

# Points --download at file:// release directories under the test's tmpdir,
# for a machine whose target is "test-target". Nothing is fetched from the
# network by this suite.
use_local_releases() {
  export TAW_TARGET=test-target
  export TAW_RELEASE_BASE="file://$BATS_TEST_TMPDIR/releases"
  export TAW_RELEASE_LATEST="file://$BATS_TEST_TMPDIR/latest"
}

# Fake cargo: `cargo build --release` (any args) creates target/release/taw
# and logs its argv to $CARGO_FAKE_LOG.
install_fake_cargo() {
  local dir="$BATS_TEST_TMPDIR/fakebin"
  mkdir -p "$dir"
  CARGO_FAKE_LOG="$BATS_TEST_TMPDIR/cargo.log"
  : >"$CARGO_FAKE_LOG"
  {
    printf '#!/usr/bin/env bash\n'
    printf 'printf "%%s\\n" "$*" >>%q\n' "$CARGO_FAKE_LOG"
    printf 'if [ "$1" = "build" ]; then\n'
    printf '  mkdir -p target/release\n'
    printf '  printf "#!/usr/bin/env bash\\necho taw 0.0.0-fake-build\\n" >target/release/taw\n'
    printf '  chmod +x target/release/taw\n'
    printf 'fi\n'
    printf 'exit 0\n'
  } >"$dir/cargo"
  chmod +x "$dir/cargo"
  FAKE_CARGO_DIR="$dir"
}

@test "builds with cargo and symlinks bin/taw -> target/release/taw" {
  install_fake_cargo
  cd "$WORK"
  PATH="$FAKE_CARGO_DIR:$PATH"
  export PATH
  run "$WORK/scripts/install.sh"
  [ "$status" -eq 0 ]
  grep -q "build --release" "$CARGO_FAKE_LOG"
  [ -L "bin/taw" ]
  [ -x "bin/taw" ]
  run bin/taw
  [[ "$output" == *"taw 0.0.0-fake-build"* ]] || false
}

@test "--download installs the release for this checkout's version as bin/taw" {
  use_local_releases
  make_release "$BATS_TEST_TMPDIR/releases/v9.9.9" 9.9.9
  cd "$WORK"
  run "$WORK/scripts/install.sh" --download
  [ "$status" -eq 0 ]
  [[ "$output" == *"downloading taw 9.9.9 for test-target"* ]] || false
  # A real file, not the source build's symlink, and it runs.
  [ -f "bin/taw" ]
  [ ! -L "bin/taw" ]
  run bin/taw --version
  [ "$output" = "taw 9.9.9" ]
  # installed version, checkout version, target
  [ "$(cat bin/.taw-release)" = "9.9.9 9.9.9 test-target" ]
}

@test "--download refuses an asset whose checksum does not match" {
  use_local_releases
  make_release "$BATS_TEST_TMPDIR/releases/v9.9.9" 9.9.9
  printf '%s  taw-test-target.tar.gz\n' "$(printf '0%.0s' {1..64})" \
    >"$BATS_TEST_TMPDIR/releases/v9.9.9/taw-test-target.tar.gz.sha256"
  cd "$WORK"
  # No cargo on PATH, so there is no source build to fall back on.
  PATH="/usr/bin:/bin"
  export PATH
  run "$WORK/scripts/install.sh" --download
  [ "$status" -ne 0 ]
  [[ "$output" == *"checksum of taw-test-target.tar.gz does not match"* ]] || false
  [ ! -e "bin/taw" ]
  [ ! -e "bin/.taw-release" ]
}

@test "--download keeps an https download on https, redirects included" {
  # A fake curl logs its arguments and fails, so nothing reaches the network.
  local dir="$BATS_TEST_TMPDIR/fakecurl" log="$BATS_TEST_TMPDIR/curl.log"
  mkdir -p "$dir"
  printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$*" >>%q\nexit 1\n' "$log" >"$dir/curl"
  chmod +x "$dir/curl"
  export TAW_TARGET=test-target
  export TAW_RELEASE_BASE="https://releases.invalid/download"
  export TAW_RELEASE_LATEST="https://releases.invalid/latest"
  cd "$WORK"
  # No cargo on PATH, so the failed download ends the run.
  PATH="$dir:/usr/bin:/bin"
  export PATH
  run "$WORK/scripts/install.sh" --download
  [ "$status" -ne 0 ]
  grep -q 'https://releases.invalid/download/v9.9.9/' "$log"
  # Every call, the latest release's too, has the flags.
  [ -z "$(grep -v -e '--proto =https --proto-redir =https' "$log")" ]
}

@test "--download falls back to the latest release when this version has none" {
  use_local_releases
  make_release "$BATS_TEST_TMPDIR/latest" 9.9.8
  cd "$WORK"
  run "$WORK/scripts/install.sh" --download
  [ "$status" -eq 0 ]
  [[ "$output" == *"nothing installed from v9.9.9; trying the latest release"* ]] || false
  run bin/taw --version
  [ "$output" = "taw 9.9.8" ]
  # 9.9.8 for a 9.9.9 checkout: the marker gives 9.9.8 as what it was fetched for, so the
  # plugin script tries again at its next load, until 9.9.9's release is out.
  [ "$(cat bin/.taw-release)" = "9.9.8 9.9.8 test-target" ]
}

@test "--download leaves a downloaded taw alone when the release it finds is that same version" {
  use_local_releases
  make_release "$BATS_TEST_TMPDIR/latest" 9.9.8
  cd "$WORK"
  mkdir -p bin
  printf '#!/usr/bin/env bash\necho "taw 9.9.8"\n' >bin/taw
  chmod +x bin/taw
  echo "9.9.8 9.9.8 test-target" >bin/.taw-release
  before=$(ls -i bin/taw | awk '{print $1}')
  run "$WORK/scripts/install.sh" --download
  [ "$status" -eq 0 ]
  [[ "$output" == *"taw 9.9.8 is already installed as bin/taw"* ]] || false
  [ "$(ls -i bin/taw | awk '{print $1}')" = "$before" ]
  [ "$(cat bin/.taw-release)" = "9.9.8 9.9.8 test-target" ]
}

@test "--download builds from source when nothing can be downloaded" {
  use_local_releases
  install_fake_cargo
  cd "$WORK"
  PATH="$FAKE_CARGO_DIR:$PATH"
  export PATH
  run "$WORK/scripts/install.sh" --download
  [ "$status" -eq 0 ]
  [[ "$output" == *"building from source instead"* ]] || false
  grep -q "build --release" "$CARGO_FAKE_LOG"
  [ -L "bin/taw" ]
  [ ! -e "bin/.taw-release" ]
}

@test "--download with TAW_NO_BUILD keeps the binary it has when the download fails" {
  use_local_releases
  install_fake_cargo
  cd "$WORK"
  mkdir -p bin
  printf '#!/usr/bin/env bash\necho "taw 1.0.0"\n' >bin/taw
  chmod +x bin/taw
  PATH="$FAKE_CARGO_DIR:$PATH"
  export PATH
  run env TAW_NO_BUILD=1 "$WORK/scripts/install.sh" --download
  [ "$status" -ne 0 ]
  # No source build was started, and the old binary is untouched.
  [ ! -s "$CARGO_FAKE_LOG" ]
  run bin/taw --version
  [ "$output" = "taw 1.0.0" ]
}

@test "--download fails clearly on a machine with no prebuilt taw and no cargo" {
  fake="$BATS_TEST_TMPDIR/fakeuname"
  mkdir -p "$fake"
  printf '#!/bin/sh\ncase "$1" in -m) echo mips ;; *) echo Plan9 ;; esac\n' >"$fake/uname"
  chmod +x "$fake/uname"
  cd "$WORK"
  PATH="$fake:/usr/bin:/bin"
  export PATH
  run "$WORK/scripts/install.sh" --download
  [ "$status" -ne 0 ]
  [[ "$output" == *"there is no prebuilt taw for Plan9 mips"* ]] || false
  [[ "$output" == *"cargo is not installed"* ]] || false
  [ ! -e "bin/taw" ]
}

@test "a source build replaces a downloaded taw and drops the release marker" {
  use_local_releases
  make_release "$BATS_TEST_TMPDIR/releases/v9.9.9" 9.9.9
  install_fake_cargo
  cd "$WORK"
  run "$WORK/scripts/install.sh" --download
  [ "$status" -eq 0 ]
  [ -e "bin/.taw-release" ]
  PATH="$FAKE_CARGO_DIR:$PATH"
  export PATH
  run "$WORK/scripts/install.sh"
  [ "$status" -eq 0 ]
  [ -L "bin/taw" ]
  # Without the marker the plugin never swaps a binary you built for a download.
  [ ! -e "bin/.taw-release" ]
}

@test "an unknown flag prints usage and exits 2 without building" {
  install_fake_cargo
  cd "$WORK"
  PATH="$FAKE_CARGO_DIR:$PATH"
  export PATH
  run "$WORK/scripts/install.sh" --bogus
  [ "$status" -eq 2 ]
  [[ "$output" == *"Usage:"* ]] || false
  [ ! -s "$CARGO_FAKE_LOG" ]
}

@test "--help and -h print usage and exit 0 without building" {
  install_fake_cargo
  cd "$WORK"
  PATH="$FAKE_CARGO_DIR:$PATH"
  export PATH
  for flag in --help -h; do
    run "$WORK/scripts/install.sh" "$flag"
    [ "$status" -eq 0 ]
    [[ "$output" == *"Usage:"* ]] || false
  done
  # --help must neither build nor symlink anything.
  [ ! -e "bin/taw" ]
  [ ! -s "$CARGO_FAKE_LOG" ]
}

@test "re-running install.sh replaces the symlink instead of failing" {
  install_fake_cargo
  cd "$WORK"
  PATH="$FAKE_CARGO_DIR:$PATH"
  export PATH
  run "$WORK/scripts/install.sh"
  [ "$status" -eq 0 ]
  run "$WORK/scripts/install.sh"
  [ "$status" -eq 0 ]
  [ -L "bin/taw" ]
}

@test "exits non-zero with a clear message when cargo is missing" {
  cd "$WORK"
  PATH="/usr/bin:/bin"
  export PATH
  run "$WORK/scripts/install.sh"
  [ "$status" -ne 0 ]
  [[ "$output" == *"cargo"* ]] || false
  [ ! -e "bin/taw" ]
}
