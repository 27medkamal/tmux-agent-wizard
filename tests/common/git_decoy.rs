//! No git that a test runs reaches a repository that git's variables of whoever runs the tests point at.
//!
//! git runs the command of a `rebase -x` with `GIT_DIR` (and more) set, and a test run from one that ran
//! git in a repository of its own (`git -C <dir> init`, a plugin install's `fetch --depth 1`) wrote to the
//! repository being rebased instead, leaving it with `core.bare = true`
//! and a shallow file. Every git of taw's and of the tests' is made by `taw::git::command()`, which takes
//! those variables out ([`taw::git::REDIRECTING_VARIABLES`]), and a test file that makes repositories
//! proves it with [`rerun`]: its tests that do, run again with every one of the variables pointing into a
//! [`Decoy`], must pass and leave the decoy byte for byte as it was. `tests/git_env.rs` holds every file
//! that runs git to that.

// Each test file that includes this module uses a subset of it; unused-item warnings would
// otherwise fail `clippy --all-targets -- -D warnings` for the other files' sake.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use taw::git::REDIRECTING_VARIABLES;

use super::wait_for;

/// A repository for git's variables to point at, with a branch, a commit and a config: what every
/// test that makes a repository of its own must leave as it was.
pub struct Decoy(tempfile::TempDir);

impl Decoy {
    pub fn new() -> Decoy {
        let decoy = Decoy(tempfile::tempdir().unwrap());
        for args in [
            &["init", "-q", "-b", "decoy"][..],
            &[
                "-c",
                "user.name=decoy",
                "-c",
                "user.email=decoy@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "decoy",
            ][..],
        ] {
            let status = taw::git::command()
                .arg("-C")
                .arg(decoy.0.path())
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "making the decoy: git {args:?}");
        }
        decoy
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }

    /// Each of [`REDIRECTING_VARIABLES`], pointing into the decoy.
    pub fn vars(&self) -> Vec<(&'static str, String)> {
        let work = self.path().display().to_string();
        let git = self.path().join(".git").display().to_string();
        REDIRECTING_VARIABLES
            .iter()
            .map(|name| {
                let value = match *name {
                    "GIT_DIR" | "GIT_COMMON_DIR" => git.clone(),
                    "GIT_WORK_TREE" => work.clone(),
                    "GIT_INDEX_FILE" => format!("{git}/index"),
                    "GIT_OBJECT_DIRECTORY" | "GIT_ALTERNATE_OBJECT_DIRECTORIES" => {
                        format!("{git}/objects")
                    }
                    "GIT_NAMESPACE" => "taw-decoy".to_string(),
                    "GIT_CONFIG" => format!("{git}/config"),
                    "GIT_GRAFT_FILE" => format!("{git}/info/grafts"),
                    "GIT_SHALLOW_FILE" => format!("{git}/shallow"),
                    "GIT_IMPLICIT_WORK_TREE" => "0".to_string(),
                    "GIT_NO_REPLACE_OBJECTS" => "1".to_string(),
                    "GIT_REPLACE_REF_BASE" => "refs/taw-decoy-replace/".to_string(),
                    "GIT_PREFIX" => "taw-decoy/".to_string(),
                    other => panic!("the decoy has no place for {other}"),
                };
                (*name, value)
            })
            .collect()
    }

    /// Every file and folder in it, by its path in it, each file with its bytes.
    pub fn contents(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut found = BTreeMap::new();
        let mut folders = vec![self.path().to_path_buf()];
        while let Some(folder) = folders.pop() {
            for entry in std::fs::read_dir(&folder).unwrap() {
                let path = entry.unwrap().path();
                let name = path.strip_prefix(self.path()).unwrap().to_path_buf();
                if path.is_dir() {
                    folders.push(path);
                    found.insert(name, None);
                } else {
                    found.insert(name, Some(std::fs::read(&path).unwrap()));
                }
            }
        }
        found
    }
}

impl Default for Decoy {
    fn default() -> Self {
        Decoy::new()
    }
}

/// What differs between two [`Decoy::contents`]: a line for each path added, removed or changed.
pub fn changes(
    before: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    after: &BTreeMap<PathBuf, Option<Vec<u8>>>,
) -> Vec<String> {
    let mut said = Vec::new();
    for (path, was) in before {
        match after.get(path) {
            None => said.push(format!("removed {}", path.display())),
            Some(now) if now != was => said.push(format!("changed {}", path.display())),
            Some(_) => {}
        }
    }
    for path in after.keys().filter(|path| !before.contains_key(*path)) {
        said.push(format!("added {}", path.display()));
    }
    said
}

/// The names in a `--list` of a test binary (`name: test`, one a line) that hold `part`.
fn tests_named<'a>(listing: &'a str, part: &str) -> Vec<&'a str> {
    listing
        .lines()
        .filter_map(|line| line.strip_suffix(": test"))
        .filter(|name| name.contains(part))
        .collect()
}

/// Runs the tests of this test binary whose names hold one of `filters` (all of them, for none) again, in
/// a process of their own, with every one of git's variables pointing into a [`Decoy`], and `guard` (the
/// test that calls this) skipped: they must pass, at least one for each filter, and leave the decoy byte
/// for byte as it was.
pub fn rerun(filters: &[&str], guard: &str) {
    // A guard that names no test of this binary skips nothing, and the rerun would run the guard itself, which
    // starts another rerun, for ever: the guard must name the one test that calls this.
    let listing = Command::new(std::env::current_exe().unwrap())
        .args(["--list", "--format", "terse"])
        .stdin(Stdio::null())
        .output()
        .expect("the test binary lists its tests");
    let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
    let guards = tests_named(&listing, guard);
    assert_eq!(
        guards.len(),
        1,
        "the guard {guard:?} must be part of the name of exactly one test of this binary (the one that calls rerun), \
         and it is part of {guards:?}"
    );
    let decoy = Decoy::new();
    let before = decoy.contents();
    let out = tempfile::tempdir().unwrap();
    let (printed, complained) = (out.path().join("out"), out.path().join("err"));
    // Into files, not pipes: a process a test left running would hold a pipe open.
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(filters)
        .args(["--skip", guard])
        .envs(decoy.vars())
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&printed).unwrap())
        .stderr(std::fs::File::create(&complained).unwrap())
        .spawn()
        .unwrap();
    let ended = wait_for(
        || matches!(child.try_wait(), Ok(Some(_))),
        Duration::from_secs(600),
    );
    if !ended {
        let _ = child.kill();
    }
    let status = child.wait().unwrap();
    let said = format!(
        "{}{}",
        std::fs::read_to_string(&printed).unwrap_or_default(),
        std::fs::read_to_string(&complained).unwrap_or_default()
    );
    let changed = changes(&before, &decoy.contents());
    assert!(
        changed.is_empty(),
        "the tests wrote to the repository git's variables pointed at: {changed:?}\n{said}"
    );
    assert!(
        ended && status.success(),
        "the tests run again with git's variables on a decoy did not pass:\n{said}"
    );
    for filter in filters
        .iter()
        .copied()
        .chain(filters.is_empty().then_some(""))
    {
        assert!(
            said.lines().any(|line| line.starts_with("test ")
                && line.contains(filter)
                && line.ends_with(" ok")),
            "no test with {filter:?} in its name ran:\n{said}"
        );
    }
}
