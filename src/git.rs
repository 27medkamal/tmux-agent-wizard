//! Branch and working-tree status for a workspace path, via the `git` binary.
//!
//! The TUI's Spaces panel shows `branch` and a compact `git_status` per tmux session.
//! Git is not a database we own — every read is a `git status`
//! subprocess — so calls are bounded by a timeout and memoised by [`Cache`] for a short
//! TTL, and the TUI runs them off the render thread (see `tui::data::GitWorker`).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a single `git status` call may take before it is killed.
const TIMEOUT: Duration = Duration::from_secs(2);

/// The variables of git's environment that belong to one repository, so that git run with one of them
/// set reads or writes that repository (or file) instead of the one its directory names: its directory
/// (`GIT_DIR`, `GIT_COMMON_DIR`), work tree (`GIT_WORK_TREE`, `GIT_IMPLICIT_WORK_TREE`, `GIT_PREFIX`),
/// index, object store, shallow list, grafts and replacements (`GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`,
/// `GIT_ALTERNATE_OBJECT_DIRECTORIES`, `GIT_SHALLOW_FILE`, `GIT_GRAFT_FILE`, `GIT_NO_REPLACE_OBJECTS`,
/// `GIT_REPLACE_REF_BASE`), config file (`GIT_CONFIG`) and namespace of refs (`GIT_NAMESPACE`). This is
/// what `git rev-parse --local-env-vars` lists, less `GIT_CONFIG_PARAMETERS` and `GIT_CONFIG_COUNT`
/// (which put configuration in and point at nothing, and which git keeps itself when it runs in another
/// repository), plus `GIT_NAMESPACE`, which git does not list. git sets some of them for a command it
/// runs itself (the command of a `rebase -x` runs with `GIT_DIR`), and anybody's shell can. The decoys of
/// the tests, the test servers' start environment and the `unset` lines of the scripts take this one
/// list (tests/git_env.rs holds it to git's own).
pub const REDIRECTING_VARIABLES: [&str; 14] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CONFIG",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
];

/// `git`, without [`REDIRECTING_VARIABLES`]: every git taw runs names its repository by a directory
/// (`-C <dir>`, or the one it runs in), and none of them is to read or write another one. Run with
/// them, a plugin install's `git fetch --depth 1` wrote a shallow file into the repository a
/// `rebase -x` was rebasing. Every git subprocess of taw's, and of its tests, is made here.
pub fn command() -> Command {
    let mut command = Command::new("git");
    for name in REDIRECTING_VARIABLES {
        command.env_remove(name);
    }
    command
}

/// Branch and working-tree counts for one repository.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Info {
    /// The checked-out branch, or `None` on a detached HEAD.
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Tracked files with staged and/or unstaged changes (porcelain-v2 `1`/`2`/`u`).
    pub modified: u32,
    /// Untracked files (porcelain-v2 `?`).
    pub untracked: u32,
}

/// Runs `git -C <path> status --porcelain=v2 --branch` and parses it.
///
/// `Err` when `path` is not inside a work tree, when `git` is missing, or when the call
/// does not finish within two seconds (the child is killed).
pub fn info(path: &Path) -> anyhow::Result<Info> {
    let out = run_git(path, &["status", "--porcelain=v2", "--branch"], TIMEOUT)?;
    Ok(parse_porcelain_v2(&out))
}

/// The checked-out branch at `path`, or `None` for a detached HEAD, a path outside a work
/// tree, a missing `git`, or a call that does not finish in time.
///
/// Separate from [`info`] because callers that want only the name should not pay for a
/// working-tree walk: `git status` reads every tracked file's stat, which on a large
/// repository is orders of magnitude more work than resolving one ref. `workspace list`
/// asks this per session, so that difference is the difference between a listing and a
/// pause.
///
/// `rev-parse --abbrev-ref HEAD` answers the literal `HEAD` on a detached head, which is
/// not a branch name and is reported as `None` — the same answer [`info`] gives for
/// porcelain's `(detached)`.
pub fn branch(path: &Path) -> Option<String> {
    let out = run_git(path, &["rev-parse", "--abbrev-ref", "HEAD"], TIMEOUT).ok()?;
    let name = out.trim();
    (!name.is_empty() && name != "HEAD").then(|| name.to_string())
}

/// Parses `git status --porcelain=v2 --branch` output.
///
/// Header lines carry the branch (`# branch.head`, `(detached)` → `None`) and the
/// ahead/behind pair (`# branch.ab +N -M`, absent when there is no upstream). Entry
/// lines are counted by their leading token: `1` (ordinary change), `2` (rename/copy)
/// and `u` (unmerged) all count as *modified*; `?` counts as *untracked*; `!` (ignored)
/// is not counted. Unknown lines are ignored rather than treated as errors, so a newer
/// git that adds a header cannot break the panel.
pub fn parse_porcelain_v2(s: &str) -> Info {
    let mut info = Info::default();
    for line in s.lines() {
        match line.split_once(' ') {
            Some(("#", rest)) => parse_header(rest, &mut info),
            _ => match line.split(' ').next().unwrap_or_default() {
                "1" | "2" | "u" => info.modified += 1,
                "?" => info.untracked += 1,
                _ => {}
            },
        }
    }
    info
}

/// One `# <key> <value…>` porcelain-v2 header. Only `branch.head` and `branch.ab` carry
/// anything the Spaces panel shows; every other header (`branch.oid`, `branch.upstream`,
/// anything a future git adds) is skipped.
fn parse_header(rest: &str, info: &mut Info) {
    let Some((key, value)) = rest.split_once(' ') else {
        return;
    };
    match key {
        "branch.head" => {
            // git prints the literal `(detached)` rather than a name on a detached HEAD.
            info.branch = (value != "(detached)").then(|| value.to_string());
        }
        "branch.ab" => {
            for field in value.split_whitespace() {
                match field.split_at(1) {
                    ("+", n) => info.ahead = n.parse().unwrap_or(0),
                    ("-", n) => info.behind = n.parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// A short-lived memo of [`info`] per path, so the 500 ms TUI tick does not spawn a
/// `git` process per workspace per frame.
pub struct Cache {
    ttl: Duration,
    entries: HashMap<PathBuf, (Instant, Option<Info>)>,
}

impl Cache {
    pub fn new(ttl: Duration) -> Cache {
        Cache {
            ttl,
            entries: HashMap::new(),
        }
    }

    /// The cached [`Info`] for `path`, refreshing it when the entry is older than the
    /// TTL. `None` when `path` is not a git repository (that answer is cached too, so a
    /// non-repo workspace does not spawn `git` on every tick).
    pub fn get(&mut self, path: &Path) -> Option<Info> {
        if let Some((at, cached)) = self.entries.get(path)
            && at.elapsed() < self.ttl
        {
            return cached.clone();
        }
        let fresh = info(path).ok();
        self.entries
            .insert(path.to_path_buf(), (Instant::now(), fresh.clone()));
        fresh
    }
}

/// Spawns `git -C <path> <args>` and returns its stdout, killing the child if it outlives
/// `timeout`.
///
/// stdout is drained by a reader thread while the main thread polls for exit: a large
/// working tree writes more than a pipe buffer holds, so polling `try_wait` without
/// draining would deadlock instead of timing out. stderr is discarded — the exit status
/// is all the caller needs to distinguish "not a repository" from a real answer.
fn run_git(path: &Path, args: &[&str], timeout: Duration) -> anyhow::Result<String> {
    let mut child = command()
        .arg("-C")
        .arg(path)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to run git in '{}': {e}", path.display()))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("git stdout was not captured"))?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => anyhow::bail!("waiting for git in '{}': {e}", path.display()),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            anyhow::bail!(
                "`git {}` in '{}' did not finish within {:?}",
                args.join(" "),
                path.display(),
                timeout
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    let out = reader
        .join()
        .map_err(|_| anyhow::anyhow!("git stdout reader panicked"))?;
    anyhow::ensure!(
        status.success(),
        "`git {}` in '{}' exited {status} (most often: not a git work tree)",
        args.join(" "),
        path.display()
    );
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// How often [`run_git`] checks whether the child has exited. Short enough that a
/// fast `git status` is not delayed noticeably, long enough not to spin a core.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
# branch.oid 6d3f2a1c0b9e8d7c6b5a4938271605f4e3d2c1b0
# branch.head feature/popup
# branch.upstream origin/feature/popup
# branch.ab +3 -2
1 .M N... 100644 100644 100644 6d3f2a1 6d3f2a1 src/tui/mod.rs
1 M. N... 100644 100644 100644 aaaaaaa bbbbbbb src/git.rs
2 R. N... 100644 100644 100644 ccccccc ddddddd R100 src/new.rs\tsrc/old.rs
u UU N... 100644 100644 100644 100644 eeeeeee fffffff 1111111 src/conflict.rs
? target/debug/taw
? notes.md
! ignored.log
";

    #[test]
    fn parses_branch_ahead_behind_and_counts_from_porcelain_v2() {
        let info = parse_porcelain_v2(FIXTURE);
        assert_eq!(info.branch.as_deref(), Some("feature/popup"));
        assert_eq!(info.ahead, 3);
        assert_eq!(info.behind, 2);
        // two `1` lines + one `2` rename + one `u` unmerged; `!` ignored is not counted.
        assert_eq!(info.modified, 4);
        assert_eq!(info.untracked, 2);
    }

    #[test]
    fn parses_a_clean_repo_with_no_upstream() {
        let out = "# branch.oid 6d3f2a1\n# branch.head main\n";
        assert_eq!(
            parse_porcelain_v2(out),
            Info {
                branch: Some("main".to_string()),
                ahead: 0,
                behind: 0,
                modified: 0,
                untracked: 0,
            }
        );
    }

    #[test]
    fn reports_a_detached_head_as_no_branch() {
        let out = "# branch.oid 6d3f2a1\n# branch.head (detached)\n? a.txt\n";
        let info = parse_porcelain_v2(out);
        assert_eq!(info.branch, None);
        assert_eq!(info.untracked, 1);
    }

    #[test]
    fn ignores_unknown_header_and_entry_lines() {
        let out = "# branch.head main\n# branch.future whatever\nx something\n";
        assert_eq!(parse_porcelain_v2(out).branch.as_deref(), Some("main"));
        assert_eq!(parse_porcelain_v2(out).modified, 0);
    }

    #[test]
    fn info_reads_a_real_repository_and_errors_outside_one() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let status = command()
                .arg("-C")
                .arg(&repo)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "trunk"]);
        std::fs::write(repo.join("untracked.txt"), "hi").unwrap();
        let info = info(&repo).unwrap();
        assert_eq!(info.branch.as_deref(), Some("trunk"));
        assert_eq!(info.untracked, 1);
        assert_eq!(info.modified, 0);

        // A directory that is not a work tree is an error, not an empty Info.
        let outside = dir.path().join("not-a-repo");
        std::fs::create_dir(&outside).unwrap();
        assert!(info_outside_repo_is_err(&outside));
    }

    fn info_outside_repo_is_err(path: &Path) -> bool {
        // `git status` walks up from `path`; the temp dir is outside any repo on a
        // normal machine, but guard anyway so the assertion states what it means.
        let inside = command()
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--is-inside-work-tree"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if inside {
            return true; // nothing to assert: the temp dir really is inside a repo
        }
        info(path).is_err()
    }

    #[test]
    fn branch_names_the_checked_out_branch_and_answers_none_where_there_is_not_one() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let status = command()
                .arg("-C")
                .arg(&repo)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "trunk"]);
        std::fs::write(repo.join("a.txt"), "hi").unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=Test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "first",
        ]);

        assert_eq!(branch(&repo).as_deref(), Some("trunk"));
        // `info` reads the same fact the expensive way; the two must not disagree.
        assert_eq!(branch(&repo), info(&repo).unwrap().branch);

        // A detached HEAD is `HEAD` to `rev-parse --abbrev-ref`, and that is not a branch.
        git(&["checkout", "-q", "--detach", "HEAD"]);
        assert_eq!(branch(&repo), None);
        assert_eq!(branch(&repo), info(&repo).unwrap().branch);
    }

    #[test]
    fn cache_reuses_an_entry_within_the_ttl_and_caches_non_repositories() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::new(Duration::from_secs(60));
        // Whether or not the temp dir happens to sit inside a repository, the answer is
        // stable, one entry is stored, and the second call must return the same thing.
        let first = cache.get(dir.path());
        let second = cache.get(dir.path());
        assert_eq!(first, second);
        assert_eq!(cache.entries.len(), 1);
    }

    /// A repository for those variables to point at, with a branch, a commit and a config: what
    /// every test that makes a repository of its own must leave as it was, whatever the variables
    /// of whoever runs it say.
    struct Decoy(tempfile::TempDir);

    impl Decoy {
        fn new() -> Decoy {
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
                let status = command()
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

        /// Each of [`REDIRECTING_VARIABLES`], pointing into the decoy.
        fn vars(&self) -> Vec<(&'static str, String)> {
            let work = self.0.path().display().to_string();
            let git = self.0.path().join(".git").display().to_string();
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
        fn contents(&self) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
            let mut found = std::collections::BTreeMap::new();
            let mut folders = vec![self.0.path().to_path_buf()];
            while let Some(folder) = folders.pop() {
                for entry in std::fs::read_dir(&folder).unwrap() {
                    let path = entry.unwrap().path();
                    let name = path.strip_prefix(self.0.path()).unwrap().to_path_buf();
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

    /// What differs between two [`Decoy::contents`]: a line for each path added, removed or changed.
    fn changes(
        before: &std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>,
        after: &std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>,
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

    /// git runs the command of a `rebase -x` with GIT_DIR (and more) set, and a test run from one that
    /// runs git in a repository of its own (`git -C <dir> init`, a plugin install's `fetch --depth 1`)
    /// wrote to the repository being rebased instead, leaving it with `core.bare = true`
    /// and a shallow file. So the tests in this binary that make a repository (the plugin installs,
    /// the worktree verbs, the git reads) are run again, by this test, with every one of the variables
    /// pointing into a decoy, and they must pass and leave the decoy byte for byte as it was.
    #[test]
    fn the_tests_that_make_a_repository_leave_the_one_gits_variables_point_at_as_it_was() {
        let decoy = Decoy::new();
        let before = decoy.contents();
        let filters = [
            "plugin::tests::",
            "plugin::install::",
            "git::tests::",
            "session::worktree::tests::",
        ];
        let out = tempfile::tempdir().unwrap();
        let (printed, complained) = (out.path().join("out"), out.path().join("err"));
        // Into files, not pipes: a process a test left running would hold a pipe open.
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(filters)
            .args(["--skip", "leave_the_one_gits_variables_point_at"])
            .envs(decoy.vars())
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&printed).unwrap())
            .stderr(std::fs::File::create(&complained).unwrap())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(300);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
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
            status.is_some_and(|status| status.success()),
            "the tests run again with git's variables on a decoy did not pass:\n{said}"
        );
        for filter in filters {
            assert!(
                said.lines().any(
                    |line| line.starts_with(&format!("test {filter}")) && line.ends_with(" ok")
                ),
                "no test of {filter} ran:\n{said}"
            );
        }
    }
}
