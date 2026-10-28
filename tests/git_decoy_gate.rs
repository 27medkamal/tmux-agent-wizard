//! The gate runs its `cargo test` step and its bats step with every one of git's variables pointing into a
//! decoy repository, and fails when a step wrote to it.
//!
//! A git that a test, a child of a test or a script runs with one of the variables set (git runs the
//! command of a `rebase -x` with `GIT_DIR`) would write to the repository it names, leaving it
//! with `core.bare = true` and a shallow file. `taw::git::command()` leaves them out of
//! every git of taw's, and the test files that make repositories run their tests again on a decoy
//! (tests/common/git_decoy.rs), but only a run with the variables set for everything holds a git nobody
//! listed. scripts/git-decoy.sh makes the decoy and looks at it; scripts/check.sh runs the two steps
//! through it. These tests prove the script: the variables it sets are the list, a git that writes
//! under them is named, and a step that wrote nothing passes.

mod common;

use common::harness::run_bash;
use common::repo_root;

/// Runs `body` in a bash that has sourced scripts/git-decoy.sh and made a decoy in its directory.
fn with_a_decoy(body: &str) -> common::harness::BashRun {
    let tmp = tempfile::tempdir().unwrap();
    let run = run_bash(
        tmp.path(),
        &format!(
            "source '{}/scripts/git-decoy.sh'\n\
             git_decoy_make \"$PWD/decoy\" || {{ echo made=failed; exit 1; }}\n\
             {body}\n",
            repo_root().display()
        ),
        &[],
    );
    // The directory goes with `tmp`: keep it as long as the run's files are read.
    drop(tmp);
    run
}

#[test]
fn the_decoys_variables_are_the_list_and_each_points_into_the_decoy() {
    let names = taw::git::REDIRECTING_VARIABLES.join(" ");
    let run = with_a_decoy(&format!(
        "[ \"$GIT_DECOY_NAMES\" = '{names}' ] || echo \"names=$GIT_DECOY_NAMES\"\n\
         for name in $GIT_DECOY_NAMES; do\n\
         \x20 value=$(git_decoy_run printenv \"$name\") || {{ echo \"unset=$name\"; continue; }}\n\
         \x20 [ -n \"$value\" ] || echo \"empty=$name\"\n\
         \x20 case \"$name\" in\n\
         \x20   GIT_NAMESPACE | GIT_IMPLICIT_WORK_TREE | GIT_NO_REPLACE_OBJECTS | GIT_REPLACE_REF_BASE | GIT_PREFIX) ;;\n\
         \x20   *) case \"$value\" in \"$PWD/decoy/repo\"*) ;; *) echo \"outside=$name\" ;; esac ;;\n\
         \x20 esac\n\
         done\n\
         echo done"
    ));
    assert_eq!(run.stdout, "done\n", "{}", run.stderr);
}

/// Each plant is a git that kept the variables and wrote to the decoy, as a test's git would that did not
/// go through `taw::git::command()`; the check names what it wrote, and fails; the next look is clean.
#[test]
fn a_git_that_writes_under_the_variables_is_named_and_fails_the_check() {
    for (plant, named) in [
        ("git config core.bare true", "changed .git/config"),
        ("git branch planted", "added .git/refs/heads/planted"),
        (
            "git -c commit.gpgsign=false -c user.name=x -c user.email=x@example.invalid commit -q --allow-empty -m planted",
            "changed .git/refs/heads/decoy",
        ),
        (
            "git hash-object -w --stdin <<< planted",
            "changed objects (how many files)",
        ),
        ("git pack-refs --all", "added .git/packed-refs"),
        (
            "git update-ref refs/tags/planted HEAD",
            "added .git/refs/tags/planted",
        ),
        (": > \"$GIT_SHALLOW_FILE\"", "added .git/shallow"),
        (
            "cd \"$GIT_WORK_TREE\" && echo x > planted && git add planted",
            "changed .git/index",
        ),
    ] {
        let run = with_a_decoy(&format!(
            "git_decoy_run bash -c '{plant}' >/dev/null 2>&1; echo planted=$?\n\
             git_decoy_check 'the planted step' 2> check.err; echo check=$?\n\
             git_decoy_check 'the next step'; echo again=$?\n\
             cat check.err >&2"
        ));
        assert!(
            run.stdout.ends_with("check=1\nagain=0\n"),
            "{plant}: {}{}",
            run.stdout,
            run.stderr
        );
        assert!(
            run.stderr.contains("the planted step wrote to the decoy")
                && run.stderr.lines().any(|line| line == named),
            "{plant}: the check did not say {named:?}:\n{}",
            run.stderr
        );
    }
}

#[test]
fn a_step_that_wrote_nothing_passes_even_where_it_ran_git_on_another_repository() {
    let run = with_a_decoy(
        "other=$PWD/other; mkdir \"$other\"\n\
         git_decoy_run bash -c \"unset $GIT_DECOY_NAMES; git -C '$other' init -q && git -C '$other' status -s\" >/dev/null 2>&1\n\
         echo ran=$?\n\
         git_decoy_check 'a clean step'; echo check=$?",
    );
    assert_eq!(run.stdout, "ran=0\ncheck=0\n", "{}", run.stderr);
}
