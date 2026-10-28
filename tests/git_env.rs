//! No git that taw or a test runs reaches a repository that git's variables of whoever runs it point at.
//!
//! git runs the command of a `rebase -x` with `GIT_DIR` (and more) set, and a test run from
//! one that ran git in a repository of its own (`git -C <dir> init`, a plugin install's `fetch --depth 1`)
//! would leave the repository being rebased with `core.bare = true` and a shallow file. Every git
//! of taw's and of the tests' is made by `taw::git::command()`, which takes the variables that
//! point git elsewhere out (`taw::git::REDIRECTING_VARIABLES`); a script that runs git takes them
//! out itself first. The test files that make repositories prove it on a decoy
//! (tests/common/git_decoy.rs, and src/git.rs for the library's own tests). These tests hold every
//! git there is to that.

mod common;

use std::path::{Path, PathBuf};

use common::repo_root;

/// Every file under `dir` (recursively) whose name ends in one of `extensions`, or every file for none.
fn files_under(dir: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                folders.push(path);
            } else if extensions.is_empty()
                || path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| extensions.contains(&ext))
            {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// The path of `file` in the repository, for a message.
fn named(file: &Path) -> String {
    file.strip_prefix(repo_root())
        .unwrap_or(file)
        .display()
        .to_string()
}

/// Every Rust source of the product's, its tests' and its examples'.
fn rust_sources() -> Vec<PathBuf> {
    ["src", "tests", "examples"]
        .iter()
        .flat_map(|dir| files_under(&repo_root().join(dir), &["rs"]))
        .collect()
}

/// The one `Command::new` of git there is, in `taw::git::command()`: any other would run a git that
/// keeps the variables that point it at another repository.
#[test]
fn no_git_is_run_but_through_the_one_command_that_takes_gits_variables_out() {
    let spawn = concat!("Command::new(", "\"git\")");
    let mut found = Vec::new();
    for file in rust_sources() {
        let text = std::fs::read_to_string(&file).unwrap();
        for (n, line) in text.lines().enumerate() {
            if line.contains(spawn) {
                found.push(format!("{}:{}", named(&file), n + 1));
            }
        }
    }
    let own = repo_root().join("src").join("git.rs");
    let own = named(&own);
    assert_eq!(
        found.len(),
        1,
        "a git run other than through taw::git::command(): {found:?}"
    );
    assert!(
        found[0].starts_with(&format!("{own}:")),
        "the one git run is not taw::git::command()'s: {found:?}"
    );
    let text = std::fs::read_to_string(repo_root().join("src").join("git.rs")).unwrap();
    let start = text.find("pub fn command() -> Command {").unwrap();
    let body = &text[start..start + text[start..].find("\n}\n").unwrap()];
    assert!(
        body.contains(spawn) && body.contains("for name in REDIRECTING_VARIABLES"),
        "taw::git::command() does not take the variables out:\n{body}"
    );
}

/// A test file that runs git (through `taw::git::command()`, the only way there is) makes repositories,
/// and runs its tests that do again on a decoy (`common::git_decoy::rerun`), so that one that keeps a
/// variable of git's is seen. The helper itself is the decoy's, and so is the guard of the gate's.
#[test]
fn every_test_file_that_runs_git_runs_the_tests_that_do_again_on_a_decoy() {
    let mut without = Vec::new();
    for file in files_under(&repo_root().join("tests"), &["rs"]) {
        if file.starts_with(repo_root().join("tests").join("common"))
            || file == repo_root().join("tests").join("git_decoy_gate.rs")
        {
            // The decoy's own, and the test of the gate's decoy (scripts/git-decoy.sh), whose git runs
            // with the variables on purpose and only in a decoy of its own.
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        if text.contains("taw::git::command()") && !text.contains("git_decoy::rerun(") {
            without.push(named(&file));
        }
    }
    assert!(
        without.is_empty(),
        "these run git and have no test that runs them again on a decoy: {without:?}"
    );
}

/// Whether `line` runs git: the word at the start of a command (the line's, or after `;`, `&&`, `||`,
/// `|`, `(`, `$(`, `!`, `then`, `do`), on a line that is not a comment.
fn runs_git(line: &str) -> bool {
    let line = line.trim_start();
    if line.starts_with('#') {
        return false;
    }
    let mut rest = line;
    loop {
        if rest.starts_with("git ") {
            return true;
        }
        let next = [
            "; ", "&& ", "|| ", "| ", "( ", "(", "$(", "! ", "then ", "do ",
        ]
        .iter()
        .filter_map(|mark| rest.find(mark).map(|at| at + mark.len()))
        .min();
        match next {
            Some(at) => rest = rest[at..].trim_start(),
            None => return false,
        }
    }
}

/// The line of a script that takes every one of git's variables out: an `unset` naming them all.
fn unsets_gits_variables(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("unset ")
        && taw::git::REDIRECTING_VARIABLES
            .iter()
            .all(|name| line.split_whitespace().any(|word| word == *name))
}

/// The line of a script that leaves git no config of the person running it: an `export` of
/// `GIT_CONFIG_GLOBAL=/dev/null` and `GIT_CONFIG_NOSYSTEM=1`, so no `commit.gpgsign` of theirs signs a commit.
fn exports_no_git_config(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("export ")
        && line
            .split_whitespace()
            .any(|word| word == "GIT_CONFIG_GLOBAL=/dev/null")
        && line
            .split_whitespace()
            .any(|word| word == "GIT_CONFIG_NOSYSTEM=1")
}

/// The scripts whose git runs elsewhere: the two that build a Linux machine run every command in it
/// (`run()`, `orb -m <machine> bash -lc`), where no variable of the host's is.
const RUN_ELSEWHERE: [&str; 2] = [
    "tests/e2e/vm-download-install.sh",
    "tests/e2e/vm-fresh-install.sh",
];

/// A script that runs git takes git's variables out before it does, itself or through the harness it
/// sources first (tests/e2e/harness.sh, which does): the acceptance run's `git init` and `git commit`,
/// the demo's projects.
#[test]
fn every_script_that_runs_git_takes_gits_variables_out_before_it_does() {
    let harness = std::fs::read_to_string(repo_root().join("tests/e2e/harness.sh")).unwrap();
    assert!(
        harness.lines().any(unsets_gits_variables),
        "tests/e2e/harness.sh does not take git's variables out"
    );
    assert!(
        harness.lines().any(exports_no_git_config),
        "tests/e2e/harness.sh leaves git the global config of whoever runs it (export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1)"
    );
    let mut scripts = files_under(&repo_root().join("scripts"), &[]);
    scripts.extend(files_under(&repo_root().join("tests"), &[]));
    let mut ran = 0;
    let mut wrong = Vec::new();
    for file in scripts {
        let name = named(&file);
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let is_script = name.ends_with(".sh")
            || name.ends_with(".bats")
            || text.starts_with("#!/bin/sh")
            || text.starts_with("#!/usr/bin/env bash")
            || text.starts_with("#!/bin/bash");
        if !is_script || RUN_ELSEWHERE.contains(&name.as_str()) {
            continue;
        }
        let Some(first_git) = text.lines().position(runs_git) else {
            continue;
        };
        ran += 1;
        let out_first = text.lines().take(first_git).any(|line| {
            unsets_gits_variables(line)
                || (line.trim_start().starts_with("source ") && line.contains("harness.sh"))
        });
        if !out_first {
            wrong.push(format!("{name}:{}", first_git + 1));
        }
    }
    assert!(ran >= 2, "no script that runs git was found ({ran})");
    assert!(
        wrong.is_empty(),
        "these run git before they take its variables out: {wrong:?}"
    );
}

/// The list is git's own: `git rev-parse --local-env-vars` of the git on PATH names every variable that
/// belongs to one repository, and taw clears all of them but the two that git itself keeps when it runs
/// in another repository (they put configuration in, they do not point at a repository), and
/// `GIT_NAMESPACE`, which git does not list and taw clears too. A git that adds one fails this, and the
/// list in `taw::git::REDIRECTING_VARIABLES` (and the `unset` lines of the scripts, which the test below
/// holds to it) grows.
#[test]
fn the_variables_taw_clears_are_the_ones_git_says_belong_to_one_repository() {
    let out = taw::git::command()
        .args(["rev-parse", "--local-env-vars"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git rev-parse --local-env-vars failed"
    );
    let mut want: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .filter(|name| name != "GIT_CONFIG_PARAMETERS" && name != "GIT_CONFIG_COUNT")
        .collect();
    want.push("GIT_NAMESPACE".to_string());
    want.sort();
    let mut have: Vec<String> = taw::git::REDIRECTING_VARIABLES
        .iter()
        .map(|name| name.to_string())
        .collect();
    have.sort();
    assert_eq!(
        have, want,
        "taw::git::REDIRECTING_VARIABLES is not git's list (git rev-parse --local-env-vars, less \
         GIT_CONFIG_PARAMETERS and GIT_CONFIG_COUNT, plus GIT_NAMESPACE)"
    );
}

/// A test server is started with none of git's variables in its start environment, whatever the test
/// process has (a `rebase -x` gives its tests `GIT_DIR`): every pane, hook and watcher the server then
/// starts has none. This runs the test below with each of them pointing into a decoy, in a process of
/// its own (the environment is the process's), so that the process has them, which that test checks it
/// does.
#[test]
fn a_test_server_starts_without_the_variables_that_point_git_elsewhere() {
    let decoy = common::git_decoy::Decoy::new();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "a_test_server_started_from_a_process_with_git_variables_has_none",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .envs(decoy.vars())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{said}");
    assert!(
        said.contains("1 passed"),
        "the test that looks at the server did not run:\n{said}"
    );
}

/// Run by the test above, with the variables set; ignored so that nothing else runs it without them.
#[test]
#[ignore = "run by a_test_server_starts_without_the_variables_that_point_git_elsewhere"]
fn a_test_server_started_from_a_process_with_git_variables_has_none() {
    for name in taw::git::REDIRECTING_VARIABLES {
        assert!(
            std::env::var_os(name).is_some(),
            "this process was to have {name}, to show the server does not"
        );
    }
    let server = common::Server::start();
    for name in taw::git::REDIRECTING_VARIABLES {
        let said = server.tmux(&["show-environment", "-g", name]);
        assert!(
            said.is_err(),
            "the test server started with {name}: {said:?}"
        );
    }
    // A fixture of its own that starts its server through the home keeps them out as well.
    let home = common::shell_env::ShellHome::new();
    let mut command = std::process::Command::new("true");
    home.apply(&mut command);
    for name in taw::git::REDIRECTING_VARIABLES {
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == name && value.is_none()),
            "the home's start environment does not take {name} out"
        );
    }
    // And a test that wants one gives it (ServerOptions::env wins).
    let wanted = common::Server::start_with(common::ServerOptions {
        env: vec![("GIT_NAMESPACE".to_string(), "a-test-wants-it".to_string())],
        ..common::ServerOptions::default()
    });
    assert_eq!(
        wanted.tmux_ok(&["show-environment", "-g", "GIT_NAMESPACE"]),
        "GIT_NAMESPACE=a-test-wants-it"
    );
}

/// The two rules above, on lines written for them.
#[test]
fn the_rules_find_git_where_it_runs_and_an_unset_only_of_every_variable() {
    for line in [
        "git -C \"$dir\" init -q",
        "  git commit -q -m x",
        "( cd \"$repo\" && git init -q && git commit -q --allow-empty -m init )",
        "x=$(git -C \"$wt\" rev-parse HEAD)",
        "if true; then git status; fi",
        "for d in a b; do git -C \"$d\" status; done",
        "true || git init",
    ] {
        assert!(runs_git(line), "{line}");
    }
    for line in [
        "# git init here would be wrong",
        "echo 'see git log'",
        "e2e_wait_screen 'git worktree remove pp30\\?' 2000",
        "digit 3",
    ] {
        assert!(!runs_git(line), "{line}");
    }
    let all = format!("unset {}", taw::git::REDIRECTING_VARIABLES.join(" "));
    assert!(unsets_gits_variables(&all));
    assert!(!unsets_gits_variables("unset GIT_DIR GIT_WORK_TREE"));
    assert!(!unsets_gits_variables(&format!("# {all}")));
    assert!(exports_no_git_config(
        "export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1"
    ));
    assert!(!exports_no_git_config("export GIT_CONFIG_GLOBAL=/dev/null"));
    assert!(!exports_no_git_config(
        "GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1"
    ));
}

/// Whether `line` is a site that makes a commit: a git argument `"commit"` on a line of Rust that is not
/// a comment, or the words `git`, then `commit` after it, in a shell line or in the string of a test that
/// runs one, or a shell line that goes on with `commit -` (the verb on the line after `git -c ... \`). It cannot see a commit made by another verb (`merge`, `rebase`, `cherry-pick`, `revert`,
/// `tag -a`; no test makes one now), nor a verb built at run time. A field named `commit` (`"commit":`, as in
/// what `taw plugin update` prints) and a search for the word (`contains("commit")`) make no commit.
fn makes_a_commit(line: &str) -> bool {
    let line = line.trim_start();
    if line.starts_with("//") || line.starts_with('#') {
        return false;
    }
    let line = line
        .replace("\"commit\":", "")
        .replace("contains(\"commit\")", "");
    line.contains("\"commit\"")
        || (line.contains("git ") && line.contains(" commit "))
        || line.starts_with("commit -")
}

/// No test signs a commit with the key of whoever runs it: their global git config may say
/// `commit.gpgsign=true` with an ssh key, and every commit a test makes would then be signed by it. A
/// file of Rust or shell that has commit sites (see `makes_a_commit`: one per line; scripts/ too) names `commit.gpgsign=false`
/// at least as often as it has them; a shell script of the acceptance run is covered once by the harness
/// (the next test). It counts lines, so a flag on a line of its own that no commit uses still counts; the
/// check of what a commit really did is the run with a signing config that logs (the H-sign proof). This
/// file holds the rule's own sample lines and is left out.
#[test]
fn no_file_makes_more_commits_than_it_names_commit_gpgsign_false() {
    let mut short = Vec::new();
    let mut checked = 0;
    for dir in ["src", "tests", "examples", "scripts"] {
        for file in files_under(&repo_root().join(dir), &["rs", "sh"]) {
            let name = named(&file);
            if name == "tests/git_env.rs" || RUN_ELSEWHERE.contains(&name.as_str()) {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            let sites = text.lines().filter(|line| makes_a_commit(line)).count();
            if sites == 0 {
                continue;
            }
            checked += 1;
            if name.ends_with(".sh") && text.contains("harness.sh") {
                continue;
            }
            let flags = text.matches("commit.gpgsign=false").count();
            if flags < sites {
                short.push(format!("{name}: {sites} commits, {flags} flags"));
            }
        }
    }
    assert!(
        checked >= 10,
        "too few files with commits were found ({checked})"
    );
    assert!(
        short.is_empty(),
        "these make a commit that is not told not to sign it (-c commit.gpgsign=false): {short:?}"
    );
}

/// The rule's own lines: what is a commit site and what is not.
#[test]
fn the_rule_finds_a_commit_where_a_test_makes_one() {
    for line in [
        "            \"commit\",",
        "    git(&[\"commit\", \"-q\", \"-m\", \"first\"]);",
        "\"git -c user.name=x commit -q --allow-empty -m planted\",",
        "( cd \"$repo\" && git init -q && git commit -q --allow-empty -m init )",
        "      commit -q --allow-empty -m decoy >/dev/null || return 1",
    ] {
        assert!(makes_a_commit(line), "{line}");
    }
    for line in [
        "/// A repository with a commit and a branch.",
        "# git commit here would sign",
        "run_git(dir, &[\"add\", \".\"]);",
        "git(&[\"log\", \"--format=%s\"]);",
        "json!({ \"id\": line.id, \"status\": \"unchanged\", \"commit\": commit })",
        "{ \"id\": \"c\", \"status\": \"unchanged\", \"commit\": \"c4\" },",
        "assert!(!trust_heading(&manifest, None).contains(\"commit\"));",
    ] {
        assert!(!makes_a_commit(line), "{line}");
    }
}
