#!/bin/sh
# Cuts herdr's default config out of herdr's source, byte for byte.
#
# herdr keeps its default config as one raw string, DEFAULT_CONFIG in src/main.rs: the text
# between `r##"` and the first `"##` after it. `herdr --default-config` prints that string,
# so reference/default-config.toml is a cut of it and is never edited by hand. A refresh of
# the vendored files runs this again:
#
#   third_party/herdr/extract-default-config.sh <herdr checkout>/src/main.rs
#
# The file is written to reference/default-config.toml beside this script, or to the path
# given second. Nothing is written when the string cannot be found or is never closed, and
# the file that was there is left as it was.

set -eu

usage() {
  echo "usage: extract-default-config.sh <herdr src/main.rs> [output file]" >&2
  exit 2
}

[ "$#" -ge 1 ] && [ "$#" -le 2 ] || usage
source_file=$1
here=$(cd "$(dirname "$0")" && pwd)
output=${2:-$here/reference/default-config.toml}

[ -f "$source_file" ] || {
  echo "extract-default-config: $source_file is not a file" >&2
  exit 1
}

staged=$(mktemp "$output.XXXXXX")
trap 'rm -f "$staged"' EXIT HUP INT TERM

# State 0 looks for the constant by its declaration: `const DEFAULT_CONFIG` and the colon of
# its type, at the start of a line (after blanks, and a `pub`, `pub(crate)` or the like). A
# constant whose name starts the same way is not it, and neither is a comment or a string
# that holds the same words. 1 looks for the opening `r##"`, 2 copies lines to the closing
# `"##`. Text before the opening on its line and after the closing on its line is not part
# of the string; a newline before the closing is. A line inside a block comment or another
# raw string that itself begins with the declaration would still be taken.
awk '
  state == 0 && $0 ~ /^[[:blank:]]*(pub([[:blank:]]*\([^)]*\))?[[:blank:]]+)?const[[:blank:]]+DEFAULT_CONFIG[[:blank:]]*:/ { state = 1 }
  state == 1 {
    at = index($0, "r##\"")
    if (at == 0) next
    $0 = substr($0, at + 4)
    state = 2
  }
  state == 2 {
    end = index($0, "\"##")
    if (end > 0) {
      printf "%s", substr($0, 1, end - 1)
      state = 3
      exit
    }
    print
  }
  END { exit (state == 3) ? 0 : 1 }
' "$source_file" >"$staged" || {
  echo "extract-default-config: no complete DEFAULT_CONFIG raw string in $source_file" >&2
  exit 1
}

chmod 644 "$staged"
mv "$staged" "$output"
trap - EXIT HUP INT TERM
