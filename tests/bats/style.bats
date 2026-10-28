#!/usr/bin/env bats
# What the bats files themselves hold to.

@test "every [[ ]] check is written to fail its test when it is false, in every bash" {
  # bash before 4.1 (macOS's /bin/bash is 3.2) does not stop on a [[ ]] that fails in the middle of
  # a function under errexit, so in a bats test only the last such check would count. Written
  # `[[ ... ]] || false`, a false check fails the test in every bash.
  run grep -nE '^[[:space:]]*\[\[.*\]\][[:space:]]*$' "$BATS_TEST_DIRNAME"/*.bats
  [ "$status" -eq 1 ] || {
    echo "a [[ ]] check that a bash before 4.1 lets pass: $output" >&2
    false
  }
}
