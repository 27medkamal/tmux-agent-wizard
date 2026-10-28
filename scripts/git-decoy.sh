#!/usr/bin/env bash
# A decoy repository for git's variables to point at, for a whole run of the tests.
#
# git runs the command of a `rebase -x` with GIT_DIR (and more) set, and a test that makes a repository of
# its own (`git -C <dir> init`, a plugin install's `git fetch --depth 1`) then writes to the repository
# being rebased instead, leaving it with `core.bare = true` and a shallow file.
# Every git of taw's and of the tests' leaves those variables out (src/git.rs), and the test files that
# make repositories run their tests again with each of them on a decoy (tests/common/git_decoy.rs), but a
# list of test names cannot hold a test that nobody listed, a child process, or a script. So the gate runs
# its `cargo test` step and its bats step with every variable of that list pointing into a decoy
# repository made fresh for the run, and when each step has ended compares the decoy with what it was:
# a git that kept a variable, and wrote, shows as a file that changed.
#
# Sourced, not run:
#
#   source scripts/git-decoy.sh
#   git_decoy_make <dir>              # the decoy in <dir>/repo, a snapshot of it, the variables' values
#   git_decoy_run <command> [args...] # runs the command with every one of the variables on the decoy
#   git_decoy_check <what> || status=1 # after the step: says each file that changed, and fails, if any
#
# The names are what `taw::git::REDIRECTING_VARIABLES` holds (tests/git_env.rs holds the two to each
# other, and holds this file to its list). The run itself needs no git: nothing that cargo, rustc or a
# test binary starts reads these variables, but a git that does read them.

# Every variable of the list.
GIT_DECOY_NAMES="GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_NAMESPACE GIT_CONFIG GIT_IMPLICIT_WORK_TREE GIT_GRAFT_FILE GIT_NO_REPLACE_OBJECTS GIT_REPLACE_REF_BASE GIT_PREFIX GIT_SHALLOW_FILE"

# The decoy in use, what it was when it was last looked at, and each variable's value (NAME=value words).
GIT_DECOY_DIR=""
GIT_DECOY_SNAPSHOT=""
GIT_DECOY_ENV=()

# _git_decoy_git <args...>: git, on the decoy, with none of the variables set.
_git_decoy_git() {
  local args=() name
  for name in $GIT_DECOY_NAMES; do args+=(-u "$name"); done
  env "${args[@]}" git -C "$GIT_DECOY_DIR" "$@"
}

# git_decoy_state: what is in the decoy, a line for each file outside its objects (its path in it and the
# checksum of its bytes) and one for how many files its objects hold; sorted, so two looks compare.
git_decoy_state() {
  local file
  (
    cd "$GIT_DECOY_DIR" || exit 1
    find . -path ./.git/objects -prune -o -type f -print | sort | while IFS= read -r file; do
      printf '%s\t%s\n' "${file#./}" "$(cksum < "$file" | tr ' ' '-')"
    done
    printf 'objects (how many files)\t%s\n' "$(find .git/objects -type f | wc -l | tr -d ' ')"
  )
}

# git_decoy_make <dir>: a repository in <dir>/repo with a branch, a commit and a config, looked at, and
# the value of each variable: GIT_DIR and the rest point into it, so that a git that keeps one reads and
# writes the decoy and nothing else. The caller's own variables are taken out first.
git_decoy_make() {
  local dir="${1:?a directory for the decoy}" name git work value
  work="$dir/repo"
  git="$work/.git"
  GIT_DECOY_DIR="$work"
  rm -rf "$work"
  mkdir -p "$work" || return 1
  for name in $GIT_DECOY_NAMES; do unset "$name"; done
  _git_decoy_git init -q -b decoy >/dev/null &&
    _git_decoy_git -c commit.gpgsign=false -c user.name=decoy -c user.email=decoy@example.invalid \
      commit -q --allow-empty -m decoy >/dev/null || return 1
  GIT_DECOY_ENV=()
  for name in $GIT_DECOY_NAMES; do
    case "$name" in
      GIT_DIR | GIT_COMMON_DIR) value="$git" ;;
      GIT_WORK_TREE) value="$work" ;;
      GIT_INDEX_FILE) value="$git/index" ;;
      GIT_OBJECT_DIRECTORY | GIT_ALTERNATE_OBJECT_DIRECTORIES) value="$git/objects" ;;
      GIT_CONFIG) value="$git/config" ;;
      GIT_GRAFT_FILE) value="$git/info/grafts" ;;
      GIT_SHALLOW_FILE) value="$git/shallow" ;;
      GIT_NAMESPACE) value="taw-decoy" ;;
      GIT_IMPLICIT_WORK_TREE) value="0" ;;
      GIT_NO_REPLACE_OBJECTS) value="1" ;;
      GIT_REPLACE_REF_BASE) value="refs/taw-decoy-replace/" ;;
      GIT_PREFIX) value="taw-decoy/" ;;
      *) echo "git_decoy_make: the decoy has no place for $name" >&2; return 1 ;;
    esac
    GIT_DECOY_ENV+=("$name=$value")
  done
  GIT_DECOY_SNAPSHOT="$(git_decoy_state)"
}

# git_decoy_run <command> [args...]: the command, with every one of the variables on the decoy.
git_decoy_run() {
  env "${GIT_DECOY_ENV[@]}" "$@"
}

# git_decoy_changes: a line for each file added, removed or changed since the snapshot, sorted.
git_decoy_changes() {
  awk -F'\t' '
    NR == FNR { was[$1] = $2; next }
    { seen[$1] = 1
      if (!($1 in was)) print "added " $1
      else if (was[$1] != $2) print "changed " $1 }
    END { for (path in was) if (!(path in seen)) print "removed " path }
  ' <(printf '%s\n' "$GIT_DECOY_SNAPSHOT") <(git_decoy_state) | sort
}

# git_decoy_check <what>: 0 when the decoy is as it was; else each change, on standard error, and 1. The
# decoy is looked at again after, so that the next step's check names that step's changes only.
git_decoy_check() {
  local what="${1:?what ran}" changes
  changes="$(git_decoy_changes)"
  GIT_DECOY_SNAPSHOT="$(git_decoy_state)"
  [ -z "$changes" ] && return 0
  echo "$what wrote to the decoy repository that git's variables pointed at, so a git it ran kept one of them (each change, by its path in the decoy):" >&2
  printf '%s\n' "$changes" >&2
  return 1
}
