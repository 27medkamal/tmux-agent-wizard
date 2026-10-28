//! Integration tests for the git-worktree workflow against a real (throwaway) tmux server
//! and a real (temporary) git repository. Never touches the default tmux server or any
//! repository outside a tempdir.

mod common;

use common::shell_env::ShellHome;
use taw::config::Config;
use taw::paths::Paths;
use taw::session::worktree;
use taw::tmux::Tmux;

/// A throwaway tmux server on its own `-L` socket, torn down on drop, with one session
/// ("repo") whose cwd is `repo_path`. See tests/tmux_client.rs for why `-f /dev/null` and a
/// pid+counter socket name are required.
struct Server(Tmux, #[allow(dead_code)] ShellHome);

impl Server {
    fn start(repo_path: &std::path::Path) -> Server {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("taw-test-worktree-{}-{n}", std::process::id());
        let t = Tmux::with_socket_name(&name);
        let home = ShellHome::new();
        home.start_server(
            &t,
            &[
                "-d",
                "-s",
                "repo",
                "-c",
                &taw::tmux::start_directory(repo_path.to_str().unwrap()),
                "-x",
                "80",
                "-y",
                "24",
                "/bin/sh",
            ],
        );
        // `/bin/sh` here *and* as `default-shell` below: `-f /dev/null` isolates tmux's
        // config but not the *shell's*. The host login shell may print banners, run prompt
        // tools, or open an interactive prompt of its own — in this repo, `mise`'s "trust
        // this config?" question in a fresh worktree — which makes any test that types into
        // a pane flaky for reasons that have nothing to do with `taw`. `/bin/sh` reads no rc
        // file. It has to be named twice because the server does not outlive a
        // `start-server` with no sessions, so there is no earlier moment at which to set the
        // option for this first pane. (Shared with `tests/common/mod.rs`, which does the
        // same for the `e2e_*` files.)
        t.run_ok(&["set-option", "-g", "default-shell", "/bin/sh"])
            .unwrap();
        Server(t, home)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.run_ok(&["kill-server"]);
    }
}

/// A `Paths` rooted entirely inside a tempdir — every field is independent of the real
/// `$TAW_CONFIG_DIR`/`$TAW_STATE_DIR`, so these tests never write outside it.
fn test_paths(dir: &std::path::Path) -> Paths {
    let plugins_dir = dir.join("plugins");
    Paths {
        config_dir: dir.join("config"),
        config_file: dir.join("config").join("config.toml"),
        state_dir: dir.join("state"),
        override_dir: dir.join("config").join("agent-detection"),
        remote_manifest_dir: dir.join("state").join("agent-detection").join("remote"),
        sessions_dir: dir.join("state").join("sessions"),
        plugin_state_dir: plugins_dir.join("state"),
        plugins_dir,
        logs_dir: dir.join("state").join("logs"),
        manifest_marker: dir.join("state").join("agent-detection").join("last-check"),
    }
}

fn run_git(dir: &std::path::Path, args: &[&str]) {
    let status = taw::git::command()
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

fn init_repo(dir: &std::path::Path) {
    run_git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    run_git(dir, &["add", "."]);
    run_git(
        dir,
        &[
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
        ],
    );
}

/// A fresh git repo at `<tmp>/repo` — a name under this test's own control, not
/// `tempfile::tempdir`'s (dot-prefixed, on this platform: `.tmpXXXXXXXX`) naming
/// convention. tmux's `new-session` silently replaces a session name's leading `.` with
/// `_` (see `tmux::session_name`, the one rule that accounts for this on the
/// library side); using a stable, dot-free basename here keeps this file's own
/// `expected_session_name`/`collision` computations (which intentionally mirror the
/// library's *un*-mangled repo-basename-plus-branch naming, not its dot handling) accurate
/// without duplicating that mangling logic in two places, and keeps the test independent
/// of tempfile's exact naming scheme.
fn git_repo(tmp: &std::path::Path) -> std::path::PathBuf {
    let dir = tmp.join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    init_repo(&dir);
    dir
}

fn branch_exists(repo: &std::path::Path, branch: &str) -> bool {
    taw::git::command()
        .arg("-C")
        .arg(repo)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .unwrap()
        .success()
}

#[test]
fn create_open_and_remove_a_worktree_round_trip() {
    let repo_tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(repo_tmp.path());
    let repo_basename = repo.file_name().unwrap().to_str().unwrap();

    let server = Server::start(&repo);
    let t = &server.0;

    let state_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(state_dir.path());
    let worktrees_dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.worktrees.directory = worktrees_dir.path().to_path_buf();
    let taw_bin = std::path::PathBuf::from("/bin/true");

    let record = worktree::create(
        t, &cfg, &paths, &taw_bin, "repo", "feat/x", None, false, false,
    )
    .expect("create should succeed for a real git repo");

    let expected_session_name = format!("{repo_basename}--feat-x");
    let expected_dir = worktrees_dir.path().join(&expected_session_name);
    assert_eq!(record.path, expected_dir);
    assert!(expected_dir.is_dir(), "worktree dir must exist");
    assert_eq!(record.branch, "feat/x");
    assert_eq!(record.session_name, expected_session_name);
    assert_eq!(record.base, None);

    assert!(
        branch_exists(&record.repo_root, "feat/x"),
        "branch must exist after create"
    );
    assert!(
        t.list_sessions()
            .unwrap()
            .iter()
            .any(|s| s.session_name == expected_session_name),
        "session must exist after create"
    );

    let listed = worktree::list(t, &paths).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].workspace_id, record.workspace_id);
    assert_eq!(listed[0].session_name, expected_session_name);

    // open() on the still-running session reports it was already open, and does not
    // duplicate/alter anything.
    let (opened, already_open) =
        worktree::open(t, &cfg, &paths, &taw_bin, "feat/x", false, false).unwrap();
    assert!(already_open);
    assert_eq!(opened.workspace_id, record.workspace_id);
    assert_eq!(
        t.list_sessions()
            .unwrap()
            .iter()
            .filter(|s| s.session_name == expected_session_name)
            .count(),
        1
    );

    // Simulate a tmux restart: kill the session, then open() must recreate it from disk.
    t.run_ok(&["kill-session", "-t", &expected_session_name])
        .unwrap();
    assert!(
        !t.list_sessions()
            .unwrap()
            .iter()
            .any(|s| s.session_name == expected_session_name)
    );

    let (reopened, was_already_open) = worktree::open(
        t,
        &cfg,
        &paths,
        &taw_bin,
        &record.workspace_id,
        false,
        false,
    )
    .unwrap();
    assert!(!was_already_open);
    assert_eq!(reopened.workspace_id, record.workspace_id);
    assert!(
        t.list_sessions()
            .unwrap()
            .iter()
            .any(|s| s.session_name == expected_session_name)
    );

    // remove(force = false): the worktree is clean (nothing written into it), so this must
    // succeed without --force. The branch must survive.
    let removed = worktree::remove(
        t,
        &cfg,
        &paths,
        &taw_bin,
        &record.workspace_id,
        false,
        false,
    )
    .expect("remove of a clean worktree should not need --force");
    assert_eq!(removed.workspace_id, record.workspace_id);
    assert!(!expected_dir.exists(), "worktree dir must be gone");
    assert!(
        !t.list_sessions()
            .unwrap()
            .iter()
            .any(|s| s.session_name == expected_session_name),
        "session must be gone"
    );
    assert!(
        branch_exists(&removed.repo_root, "feat/x"),
        "remove must never delete the branch"
    );
    assert!(worktree::list(t, &paths).unwrap().is_empty());
}

/// A `Tmux` that appends what it is run with to a log, one line a call, and then runs the
/// real tmux: what a call asked for, read back.
fn recording(real: &Tmux, dir: &std::path::Path) -> (Tmux, std::path::PathBuf) {
    let log = dir.join("tmux-calls.log");
    let wrapper = dir.join("tmux-recording");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real.bin.display()
        ),
    )
    .unwrap();
    crate::common::make_executable(&wrapper);
    (
        Tmux {
            bin: wrapper,
            socket: real.socket.clone(),
        },
        log,
    )
}

/// What each `switch-client` in the log was asked, after the verb.
fn switches(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once(" switch-client "))
        .map(|(_, asked)| asked.to_string())
        .collect()
}

/// `create` and `open`, asked to focus, take the client to the worktree's session by its exact
/// name (`=name`), as every other switch to a session by name does: a bare name is matched as
/// a prefix and as a pattern too, and some names are not names at all to tmux (`~`).
#[test]
fn a_worktree_is_focused_by_its_exact_session_name() {
    let repo_tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(repo_tmp.path());
    let repo_basename = repo.file_name().unwrap().to_str().unwrap();
    let server = Server::start(&repo);
    let log_dir = tempfile::tempdir().unwrap();
    let (t, log) = recording(&server.0, log_dir.path());
    let state_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(state_dir.path());
    let worktrees_dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.worktrees.directory = worktrees_dir.path().to_path_buf();
    let taw_bin = std::path::PathBuf::from("/bin/true");
    let session = format!("{repo_basename}--feat-x");

    worktree::create(
        &t, &cfg, &paths, &taw_bin, "repo", "feat/x", None, true, false,
    )
    .unwrap();
    assert_eq!(switches(&log), [format!("-t ={session}")], "create");

    // A restart of tmux: the session is gone, and `open` makes it again and goes to it.
    server.0.run_ok(&["kill-session", "-t", &session]).unwrap();
    worktree::open(&t, &cfg, &paths, &taw_bin, "feat/x", true, false).unwrap();
    assert_eq!(
        switches(&log),
        [format!("-t ={session}"), format!("-t ={session}")],
        "open"
    );
}

#[test]
fn create_errors_with_not_a_git_repo_when_the_workspace_is_not_a_repository() {
    let plain_dir = tempfile::tempdir().unwrap();
    let server = Server::start(plain_dir.path());
    let t = &server.0;

    let state_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(state_dir.path());
    let worktrees_dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.worktrees.directory = worktrees_dir.path().to_path_buf();
    let taw_bin = std::path::PathBuf::from("/bin/true");

    let err = worktree::create(
        t, &cfg, &paths, &taw_bin, "repo", "feat/x", None, false, false,
    )
    .unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<worktree::WorktreeError>(),
            Some(worktree::WorktreeError::NotAGitRepo(_))
        ),
        "expected WorktreeError::NotAGitRepo, got: {err:#}"
    );
    assert!(worktree::list(t, &paths).unwrap().is_empty());
}

#[test]
fn create_rejects_a_dash_leading_or_hostile_branch_before_git_dumps_its_usage() {
    let repo_tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(repo_tmp.path());
    let server = Server::start(&repo);
    let t = &server.0;

    let state_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(state_dir.path());
    let worktrees_dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.worktrees.directory = worktrees_dir.path().to_path_buf();
    let taw_bin = std::path::PathBuf::from("/bin/true");

    // Against a real repo, so a bad branch is the only possible cause of failure: git would
    // read `-evil`/`--base` as its own options and dump multi-line usage text; instead the
    // name is refused as InvalidBranch (which the CLI reports as `invalid_argument`) and
    // nothing is created.
    for branch in ["-evil", "--base", "has space", "a;rm -rf"] {
        let err = worktree::create(
            t, &cfg, &paths, &taw_bin, "repo", branch, None, false, false,
        )
        .unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<worktree::WorktreeError>(),
                Some(worktree::WorktreeError::InvalidBranch(_))
            ),
            "branch {branch:?} should be InvalidBranch, got: {err:#}"
        );
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("usage:") && !msg.to_lowercase().contains("git worktree add"),
            "git usage text leaked for {branch:?}: {msg}"
        );
    }

    // The guard is not over-broad: an ordinary branch still creates a worktree.
    worktree::create(
        t, &cfg, &paths, &taw_bin, "repo", "feat/ok", None, false, false,
    )
    .expect("an ordinary branch name is still accepted");
    assert_eq!(worktree::list(t, &paths).unwrap().len(), 1);
}

#[test]
fn create_errors_with_session_exists_and_never_touches_git_when_the_target_session_name_is_taken() {
    let repo_tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(repo_tmp.path());
    let repo_basename = repo.file_name().unwrap().to_str().unwrap();

    let server = Server::start(&repo);
    let t = &server.0;
    let collision = format!("{repo_basename}--feat-x");
    t.run_ok(&["new-session", "-d", "-s", &collision]).unwrap();

    let state_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(state_dir.path());
    let worktrees_dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.worktrees.directory = worktrees_dir.path().to_path_buf();
    let taw_bin = std::path::PathBuf::from("/bin/true");

    let err = worktree::create(
        t, &cfg, &paths, &taw_bin, "repo", "feat/x", None, false, false,
    )
    .unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<worktree::WorktreeError>(),
            Some(worktree::WorktreeError::SessionExists(_))
        ),
        "expected WorktreeError::SessionExists, got: {err:#}"
    );
    assert!(
        !branch_exists(&repo, "feat/x"),
        "must fail before creating the git worktree/branch"
    );
    assert!(!worktrees_dir.path().join(&collision).exists());
}

#[test]
fn create_records_the_session_name_tmux_actually_gives_a_dotted_branch() {
    // tmux folds `.` and `:` in a session name to `_`, silently: `new-session -s
    // repo--release-1.2` creates a session actually named `repo--release-1_2`. `create`
    // computed the un-folded name, checked *that* was free, created the git worktree, ran
    // `new-session` (which succeeded under the other name), then failed to find its own
    // session and bailed "session vanished" — leaving an orphaned git worktree and an
    // orphaned tmux session behind, neither of which `open` or `remove` could ever find.
    let repo_tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(repo_tmp.path());
    let repo_basename = repo.file_name().unwrap().to_str().unwrap();

    let server = Server::start(&repo);
    let t = &server.0;

    let state_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(state_dir.path());
    let worktrees_dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.worktrees.directory = worktrees_dir.path().to_path_buf();
    let taw_bin = std::path::PathBuf::from("/bin/true");

    let record = worktree::create(
        t,
        &cfg,
        &paths,
        &taw_bin,
        "repo",
        "release/1.2",
        None,
        false,
        false,
    )
    .expect("a dotted branch name must not fail create");

    // Two names on purpose: the *session* is folded, because tmux folds it; the
    // *directory* is `<repo>--<branch>` with only `/` flattened, because `.`
    // is an ordinary character in a path and that is where a user looks for it.
    let expected = format!("{repo_basename}--release-1_2");
    assert_eq!(
        record.session_name, expected,
        "the record must hold the name tmux really used"
    );
    assert_eq!(record.branch, "release/1.2", "the branch is left as typed");
    assert_eq!(
        record.path,
        worktrees_dir
            .path()
            .join(format!("{repo_basename}--release-1.2")),
        "the directory keeps the dot <repo>--<branch> has"
    );
    assert!(record.path.is_dir(), "worktree dir must exist");

    let live: Vec<String> = t
        .list_sessions()
        .unwrap()
        .into_iter()
        .map(|s| s.session_name)
        .collect();
    assert!(
        live.contains(&expected),
        "tmux should hold exactly the recorded session: {live:?}"
    );

    // The whole point of recording the real name: `open` and `remove` can find it again.
    let (opened, already_open) =
        worktree::open(t, &cfg, &paths, &taw_bin, "release/1.2", false, false).unwrap();
    assert!(already_open);
    assert_eq!(opened.session_name, expected);

    let removed = worktree::remove(
        t,
        &cfg,
        &paths,
        &taw_bin,
        &record.workspace_id,
        false,
        false,
    )
    .unwrap();
    assert_eq!(removed.session_name, expected);
    assert!(!record.path.exists(), "worktree dir must be gone");
    assert!(
        branch_exists(&removed.repo_root, "release/1.2"),
        "remove must never delete the branch"
    );
    assert!(worktree::list(t, &paths).unwrap().is_empty());
}

#[test]
fn create_rolls_back_the_git_worktree_but_never_the_branch_when_the_session_cannot_be_made() {
    // If `new-session` fails after `git worktree add` has run, the worktree it just made
    // is the one thing `create` owns and has already done — leaving it behind orphans a
    // directory and a registered worktree entry nothing in `taw` can reach afterwards. The
    // branch is *not* undone: `remove` never deletes a branch either, and a branch that
    // existed before this call is emphatically not ours to delete.
    let repo_tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(repo_tmp.path());

    let server = Server::start(&repo);

    // A tmux that answers every command normally except `new-session`, which it refuses.
    // The failure has to land *after* `git worktree add`, and nothing about a branch name
    // can arrange that — every name `create` rejects, it rejects before touching git. The
    // binary is swapped rather than an env var set because `cargo test` runs these as
    // threads of one process, where a global env var belongs to no single test.
    let shim_dir = tempfile::tempdir().unwrap();
    let shim = shim_dir.path().join("tmux-refusing-new-session");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  [ \"$a\" = new-session ] && {{ echo 'create error' >&2; exit 1; }}\ndone\nexec '{}' \"$@\"\n",
            server.0.bin.display()
        ),
    )
    .unwrap();
    crate::common::make_executable(&shim);
    let refusing = Tmux {
        bin: shim.clone(),
        socket: server.0.socket.clone(),
    };

    let state_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(state_dir.path());
    let worktrees_dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.worktrees.directory = worktrees_dir.path().to_path_buf();
    let taw_bin = std::path::PathBuf::from("/bin/true");

    // The branch exists first, so "the branch survived" is a real assertion about
    // something `create` did not make and must not remove.
    run_git(&repo, &["branch", "keep-me"]);

    let err = worktree::create(
        &refusing, &cfg, &paths, &taw_bin, "repo", "keep-me", None, false, false,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("creating the tmux session"),
        "the real failure must be what is reported: {err:#}"
    );

    assert!(
        worktree::list(&server.0, &paths).unwrap().is_empty(),
        "a failed create records nothing"
    );
    let leftovers: Vec<_> = std::fs::read_dir(worktrees_dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "the just-created git worktree must be rolled back, found {leftovers:?}"
    );
    let registered = taw::git::command()
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    let registered = String::from_utf8_lossy(&registered.stdout);
    assert_eq!(
        registered.matches("worktree ").count(),
        1,
        "only the main worktree may remain registered:\n{registered}"
    );
    assert!(
        branch_exists(&repo, "keep-me"),
        "rollback must never delete the branch"
    );
}

#[test]
fn create_never_lets_a_branch_or_base_name_reach_git_as_a_flag() {
    // `branch` and `base` come straight from the caller (`--branch`, `--base`) and go to
    // `git worktree add` as positionals, so anything starting with `-` would be read by git
    // as an option rather than a ref: `--base --lock` would lock the worktree, and
    // `--branch --detach` would change what is created entirely. A `--` terminator keeps
    // them refs, and it is what this pins.
    let repo_tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(repo_tmp.path());

    let server = Server::start(&repo);
    let t = &server.0;

    let state_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(state_dir.path());
    let worktrees_dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.worktrees.directory = worktrees_dir.path().to_path_buf();
    let taw_bin = std::path::PathBuf::from("/bin/true");

    // `--lock` as a *base* would, without `--`, be consumed by `git worktree add` as its
    // own `--lock` flag and the worktree would be created locked from HEAD instead.
    let err = worktree::create(
        t,
        &cfg,
        &paths,
        &taw_bin,
        "repo",
        "feat/flagged",
        Some("--lock"),
        false,
        false,
    )
    .expect_err("a base that is not a ref must fail, not be read as a flag");
    assert!(
        !format!("{err:#}").contains("session"),
        "it must fail in git, before any tmux session is made: {err:#}"
    );

    // Nothing was left behind, and in particular nothing locked.
    let listed = taw::git::command()
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        !listed.contains("locked"),
        "no locked worktree may have been created:\n{listed}"
    );
    assert_eq!(
        listed.matches("worktree ").count(),
        1,
        "only the main worktree may remain:\n{listed}"
    );
    assert!(worktree::list(t, &paths).unwrap().is_empty());

    // A dash-leading branch name is not a legal git branch name at all, and must be
    // refused by git rather than acted on: `-b <value>` is an option's argument, where
    // git's own parser rejects a dash-leading value, so this needs no `--` of its own —
    // pinned here so that stays true.
    let err = worktree::create(
        t, &cfg, &paths, &taw_bin, "repo", "--detach", None, false, false,
    )
    .expect_err("a branch name that is a git flag must be refused");
    assert!(
        format!("{err:#}").contains("git"),
        "git must be the one refusing it: {err:#}"
    );
    assert_eq!(
        worktree::list(t, &paths).unwrap().len(),
        0,
        "and nothing may be recorded"
    );

    // Positive control: an ordinary base still resolves, so `--` did not break the happy
    // path it guards.
    let record = worktree::create(
        t,
        &cfg,
        &paths,
        &taw_bin,
        "repo",
        "feat/ok",
        Some("main"),
        false,
        false,
    )
    .expect("an ordinary base must still work");
    assert_eq!(record.base.as_deref(), Some("main"));
    assert!(record.path.is_dir());
    let listed = taw::git::command()
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&listed.stdout).contains("locked"),
        "and it must not be locked"
    );
}

#[test]
fn create_rolls_everything_back_when_the_record_cannot_be_saved() {
    // Rollback has to cover more than `new-session`. A failure *after* the session exists —
    // at the read-back, or writing `worktrees.json` — left a live tmux session and a git
    // worktree with no record: a workspace nothing associates with a worktree, and a
    // directory `open`/`remove` can never find. Both are unreachable afterwards, so both
    // have to go.
    let repo_tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(repo_tmp.path());
    let repo_basename = repo.file_name().unwrap().to_str().unwrap();

    let server = Server::start(&repo);
    let t = &server.0;

    let state_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(state_dir.path());
    let worktrees_dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.worktrees.directory = worktrees_dir.path().to_path_buf();
    let taw_bin = std::path::PathBuf::from("/bin/true");

    // `save_records` writes `<sessions_dir>/../worktrees.json` under the state dir. A
    // *file* where its parent directory belongs makes that write fail, and nothing
    // earlier in `create` touches it — so the failure lands exactly where this test needs
    // it: after the session and the worktree both exist.
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    let plugins_as_file = paths.state_dir.join("worktrees.json");
    std::fs::create_dir_all(&plugins_as_file).unwrap(); // a directory where a file goes

    let err = worktree::create(
        t,
        &cfg,
        &paths,
        &taw_bin,
        "repo",
        "feat/rollback",
        None,
        false,
        false,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("recording the worktree"),
        "the failure must be reported as what it was: {err:#}"
    );

    let session = format!("{repo_basename}--feat-rollback");
    assert!(
        !t.list_sessions()
            .unwrap()
            .iter()
            .any(|s| s.session_name == session),
        "the session this call created must be gone"
    );
    assert!(
        !worktrees_dir.path().join(&session).exists(),
        "and so must the worktree directory"
    );
    let registered = taw::git::command()
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    let registered = String::from_utf8_lossy(&registered.stdout);
    assert_eq!(
        registered.matches("worktree ").count(),
        1,
        "only the main worktree may remain registered:\n{registered}"
    );
    assert!(
        branch_exists(&repo, "feat/rollback"),
        "the branch is never rolled back"
    );
}

/// `--trust-repository` (`-c safe.directory=<repo_root>`, ported from herdr) must not
/// change ordinary create/open/remove behaviour against a repository git is already happy
/// with — [`session::worktree::tests::git_command_adds_safe_directory_only_when_trusted`]
/// pins the exact args it adds.
#[test]
fn trust_repository_does_not_change_ordinary_create_open_remove_behaviour() {
    let repo_tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(repo_tmp.path());
    let repo_basename = repo.file_name().unwrap().to_str().unwrap();

    let server = Server::start(&repo);
    let t = &server.0;

    let state_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(state_dir.path());
    let worktrees_dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.worktrees.directory = worktrees_dir.path().to_path_buf();
    let taw_bin = std::path::PathBuf::from("/bin/true");

    let record = worktree::create(
        t,
        &cfg,
        &paths,
        &taw_bin,
        "repo",
        "feat/trusted",
        None,
        false,
        true,
    )
    .expect("create with trust_repository must still succeed");
    let expected_session_name = format!("{repo_basename}--feat-trusted");
    assert_eq!(record.session_name, expected_session_name);
    assert!(branch_exists(&record.repo_root, "feat/trusted"));

    let (opened, already_open) =
        worktree::open(t, &cfg, &paths, &taw_bin, "feat/trusted", false, true).unwrap();
    assert!(already_open);
    assert_eq!(opened.workspace_id, record.workspace_id);

    let removed = worktree::remove(t, &cfg, &paths, &taw_bin, &record.workspace_id, false, true)
        .expect("remove with trust_repository must still succeed");
    assert!(!removed.path.exists());
    assert!(branch_exists(&repo, "feat/trusted"));
}

// -----------------------------------------------------------------------------------
// The worktree sessions closing a workspace would strand — herdr v0.9.1's "linked
// worktree workspaces" of a primary workspace (`workspace_close_indices`, #2874), which
// `taw workspace close` asks `--group` for.
// -----------------------------------------------------------------------------------

/// A running session, by name.
fn session_named(t: &Tmux, name: &str) -> taw::tmux::SessionInfo {
    t.list_sessions()
        .unwrap()
        .into_iter()
        .find(|s| s.session_name == name)
        .unwrap_or_else(|| panic!("no session named {name}"))
}

fn names(linked: &[worktree::LinkedSession]) -> Vec<String> {
    let mut names: Vec<String> = linked.iter().map(|l| l.session_name.clone()).collect();
    names.sort();
    names
}

#[test]
fn linked_sessions_are_the_running_worktree_sessions_created_from_a_workspace() {
    let repo_tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(repo_tmp.path());
    let repo_basename = repo.file_name().unwrap().to_str().unwrap();
    let server = Server::start(&repo);
    let t = &server.0;
    let state_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(state_dir.path());
    let worktrees_dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.worktrees.directory = worktrees_dir.path().to_path_buf();
    let taw_bin = std::path::PathBuf::from("/bin/true");

    let primary = session_named(t, "repo");
    assert!(
        worktree::linked_sessions(t, &paths, &primary)
            .unwrap()
            .is_empty(),
        "a workspace nothing was created from has nothing linked"
    );

    for branch in ["feat/a", "feat/b"] {
        worktree::create(
            t, &cfg, &paths, &taw_bin, "repo", branch, None, false, false,
        )
        .unwrap();
    }
    let a = format!("{repo_basename}--feat-a");
    let b = format!("{repo_basename}--feat-b");

    let linked = worktree::linked_sessions(t, &paths, &primary).unwrap();
    assert_eq!(names(&linked), vec![a.clone(), b.clone()]);
    for l in &linked {
        assert_eq!(
            l.session_id,
            session_named(t, &l.session_name).session_id,
            "the id of the session that is running, whatever the record remembers"
        );
    }

    // A worktree workspace is not the primary of the others: nothing hangs off it.
    assert!(
        worktree::linked_sessions(t, &paths, &session_named(t, &a))
            .unwrap()
            .is_empty()
    );

    // Only a *running* session counts: a worktree whose session is closed has nothing left
    // to strand.
    t.run_ok(&["kill-session", "-t", &a]).unwrap();
    let linked = worktree::linked_sessions(t, &paths, &primary).unwrap();
    assert_eq!(names(&linked), vec![b]);
}

#[test]
fn a_worktree_cut_from_a_worktree_belongs_to_the_group_of_the_first_workspace() {
    let repo_tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(repo_tmp.path());
    let repo_basename = repo.file_name().unwrap().to_str().unwrap();
    let server = Server::start(&repo);
    let t = &server.0;
    let state_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(state_dir.path());
    let worktrees_dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.worktrees.directory = worktrees_dir.path().to_path_buf();
    let taw_bin = std::path::PathBuf::from("/bin/true");

    let first = worktree::create(
        t, &cfg, &paths, &taw_bin, "repo", "feat/a", None, false, false,
    )
    .unwrap();
    // Cut from the worktree session itself, not from the primary.
    worktree::create(
        t,
        &cfg,
        &paths,
        &taw_bin,
        &first.session_name,
        "feat/nested",
        None,
        false,
        false,
    )
    .unwrap();

    let primary = session_named(t, "repo");
    let linked = worktree::linked_sessions(t, &paths, &primary).unwrap();
    assert_eq!(
        names(&linked),
        vec![
            format!("{repo_basename}--feat-a"),
            format!("{repo_basename}--feat-a--feat-nested"),
        ],
        "closing the primary strands the nested worktree too"
    );
    // A worktree workspace closes on its own, whatever was cut from it: herdr's
    // `workspace_close_indices` expands a group only from the workspace that is not a
    // linked worktree itself, and every worktree of the repository is in that group.
    let middle = session_named(t, &first.session_name);
    assert!(
        worktree::linked_sessions(t, &paths, &middle)
            .unwrap()
            .is_empty(),
        "a worktree workspace has no group of its own"
    );
}

/// herdr keeps every worktree of a repository in the group of its primary workspace, so
/// closing one worktree never takes another out of it. Here a worktree is linked to the
/// workspace it was cut from, and that link has to hold through a worktree that is closed:
/// otherwise closing the middle of a chain (which closes on its own) would leave the
/// one cut from it running, and out of reach of the primary's `--group`.
#[test]
fn a_nested_worktree_stays_in_the_primarys_group_when_the_one_it_was_cut_from_is_closed() {
    let repo_tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(repo_tmp.path());
    let repo_basename = repo.file_name().unwrap().to_str().unwrap();
    let server = Server::start(&repo);
    let t = &server.0;
    let state_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(state_dir.path());
    let worktrees_dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.worktrees.directory = worktrees_dir.path().to_path_buf();
    let taw_bin = std::path::PathBuf::from("/bin/true");
    let first = worktree::create(
        t, &cfg, &paths, &taw_bin, "repo", "feat/a", None, false, false,
    )
    .unwrap();
    worktree::create(
        t,
        &cfg,
        &paths,
        &taw_bin,
        &first.session_name,
        "feat/nested",
        None,
        false,
        false,
    )
    .unwrap();

    t.run_ok(&["kill-session", "-t", &first.session_name])
        .unwrap();

    let primary = session_named(t, "repo");
    assert_eq!(
        names(&worktree::linked_sessions(t, &paths, &primary).unwrap()),
        vec![format!("{repo_basename}--feat-a--feat-nested")],
        "the closed worktree is not in the group, and the one cut from it still is"
    );
}

/// A record names the workspace it was cut from by the public id it had *then*, and tmux
/// hands ids out afresh after a server restart, so an old `w<n>` can later name a session
/// that has nothing to do with the worktree. A session outside the repository is never the
/// primary of a worktree of that repository, whatever the id says.
#[test]
fn a_stale_source_id_does_not_link_a_session_outside_the_repository() {
    let repo_tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(repo_tmp.path());
    let server = Server::start(&repo);
    let t = &server.0;
    let state_dir = tempfile::tempdir().unwrap();
    let paths = test_paths(state_dir.path());
    let worktrees_dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.worktrees.directory = worktrees_dir.path().to_path_buf();
    let taw_bin = std::path::PathBuf::from("/bin/true");
    worktree::create(
        t, &cfg, &paths, &taw_bin, "repo", "feat/a", None, false, false,
    )
    .unwrap();

    // Another session, elsewhere, that ends up with the id the record remembers.
    let elsewhere = tempfile::tempdir().unwrap();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "elsewhere",
        "-c",
        elsewhere.path().to_str().unwrap(),
    ])
    .unwrap();
    let other = session_named(t, "elsewhere");
    let records_path = paths.state_dir.join("worktrees.json");
    let mut records: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&records_path).unwrap()).unwrap();
    records["worktrees"][0]["source_workspace_id"] =
        serde_json::Value::String(taw::tmux::ids::workspace_id(&other.session_id));
    std::fs::write(&records_path, serde_json::to_string(&records).unwrap()).unwrap();

    assert!(
        worktree::linked_sessions(t, &paths, &other)
            .unwrap()
            .is_empty(),
        "a session outside the repository must not be treated as its primary"
    );
}

// -----------------------------------------------------------------------------------
// A removal git refuses, and one that failed partway (herdr v0.9.1, #3766, #3314, #1797):
// a dirty or submodule worktree is refused without `force` and removed with it, and a
// removal that got as far as letting go of the worktree, but not of every file in it, can
// be tried again.
// -----------------------------------------------------------------------------------

use taw::session::worktree::{RemoveRefusal, WorktreeError};

/// A repository, a server with its session, and the state a worktree verb needs.
struct Removal {
    _repo_tmp: tempfile::TempDir,
    _state_dir: tempfile::TempDir,
    _worktrees_dir: tempfile::TempDir,
    server: Server,
    repo: std::path::PathBuf,
    paths: Paths,
    cfg: Config,
    taw_bin: std::path::PathBuf,
}

impl Removal {
    fn start() -> Removal {
        Removal::start_in("repo")
    }

    /// [`Removal::start`] with the repository in a folder called `folder`.
    fn start_in(folder: &str) -> Removal {
        let repo_tmp = tempfile::tempdir().unwrap();
        let repo = repo_tmp.path().join(folder);
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let server = Server::start(&repo);
        let state_dir = tempfile::tempdir().unwrap();
        let worktrees_dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.worktrees.directory = worktrees_dir.path().to_path_buf();
        Removal {
            paths: test_paths(state_dir.path()),
            _repo_tmp: repo_tmp,
            _state_dir: state_dir,
            _worktrees_dir: worktrees_dir,
            server,
            repo,
            cfg,
            taw_bin: std::path::PathBuf::from("/bin/true"),
        }
    }

    fn t(&self) -> &Tmux {
        &self.server.0
    }

    fn create(&self, branch: &str) -> worktree::WorktreeRecord {
        worktree::create(
            self.t(),
            &self.cfg,
            &self.paths,
            &self.taw_bin,
            "repo",
            branch,
            None,
            false,
            false,
        )
        .expect("create should succeed for a real git repository")
    }

    fn remove(
        &self,
        record: &worktree::WorktreeRecord,
        force: bool,
    ) -> anyhow::Result<worktree::WorktreeRecord> {
        worktree::remove(
            self.t(),
            &self.cfg,
            &self.paths,
            &self.taw_bin,
            &record.workspace_id,
            force,
            false,
        )
    }

    fn session_running(&self, record: &worktree::WorktreeRecord) -> bool {
        self.t()
            .list_sessions()
            .unwrap()
            .iter()
            .any(|s| s.session_name == record.session_name)
    }

    fn tracked(&self) -> usize {
        worktree::list(self.t(), &self.paths).unwrap().len()
    }

    /// git's own list of worktrees, as paths.
    fn git_lists(&self, path: &std::path::Path) -> bool {
        let listed = taw::git::command()
            .arg("-C")
            .arg(&self.repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&listed.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .any(|listed| {
                std::fs::canonicalize(listed).unwrap_or_else(|_| listed.into())
                    == std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
            })
    }
}

/// The folder a refused removal would have deleted, from its error.
fn refusal_folder(err: &anyhow::Error) -> Option<std::path::PathBuf> {
    err.chain()
        .find_map(|cause| match cause.downcast_ref::<WorktreeError>() {
            Some(WorktreeError::RemoveRefused { path, .. }) => Some(path.clone()),
            _ => None,
        })
}

/// What `remove` refused for, from its error.
fn refusal(err: &anyhow::Error) -> Option<(RemoveRefusal, String, String)> {
    err.chain()
        .find_map(|cause| match cause.downcast_ref::<WorktreeError>() {
            Some(WorktreeError::RemoveRefused {
                why,
                branch,
                message,
                ..
            }) => Some((*why, branch.clone(), message.clone())),
            _ => None,
        })
}

/// Whether this process runs as root, who is not stopped by a directory's permissions: a test
/// that makes a folder read-only so that a file in it cannot be deleted has nothing to show then.
fn running_as_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "0")
}

/// Makes `dir` unwritable, so a file in it cannot be deleted, and puts it right on drop so
/// the temporary directory can be cleaned up whatever the test did.
struct Locked(std::path::PathBuf);

impl Locked {
    fn new(dir: &std::path::Path) -> Locked {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("kept.txt"), "x\n").unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        Locked(dir.to_path_buf())
    }

    fn unlock(&self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        self.unlock();
    }
}

#[test]
fn a_dirty_worktree_is_refused_without_force_and_nothing_is_touched() {
    let r = Removal::start();
    let record = r.create("feat/dirty");
    std::fs::write(record.path.join("README.md"), "changed\n").unwrap();
    std::fs::write(record.path.join("new.txt"), "untracked\n").unwrap();

    let err = r.remove(&record, false).unwrap_err();

    let (why, branch, message) =
        refusal(&err).unwrap_or_else(|| panic!("a typed refusal: {err:#}"));
    assert_eq!(why, RemoveRefusal::Dirty);
    assert_eq!(branch, "feat/dirty");
    // git's own words, in English whatever the locale: the refusal is told from them.
    assert!(
        message.contains("contains modified or untracked files, use --force to delete it"),
        "{message}"
    );
    assert!(
        record.path.join("new.txt").exists(),
        "the files are still there"
    );
    assert!(r.session_running(&record), "and so is the session");
    assert_eq!(r.tracked(), 1, "and the record");
    assert!(branch_exists(&r.repo, "feat/dirty"));
}

#[test]
fn a_dirty_worktree_is_removed_when_forced_and_its_branch_is_kept() {
    let r = Removal::start();
    let record = r.create("feat/dirty");
    std::fs::write(record.path.join("new.txt"), "untracked\n").unwrap();

    let removed = r
        .remove(&record, true)
        .expect("forced removal goes through");

    assert_eq!(removed.branch, "feat/dirty");
    assert!(!record.path.exists());
    assert!(!r.session_running(&record));
    assert_eq!(r.tracked(), 0);
    assert!(
        branch_exists(&r.repo, "feat/dirty"),
        "the branch is never deleted"
    );
}

#[test]
fn a_clean_worktree_with_a_submodule_is_refused_without_force_and_removed_with_it() {
    let r = Removal::start();
    // A repository to be the submodule, added to the main one.
    let sub_tmp = tempfile::tempdir().unwrap();
    let sub = sub_tmp.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    init_repo(&sub);
    run_git(
        &r.repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            sub.to_str().unwrap(),
            "sub",
        ],
    );
    run_git(
        &r.repo,
        &[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=Test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "add a submodule",
        ],
    );
    let record = r.create("feat/sub");
    run_git(
        &record.path,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--quiet",
        ],
    );

    let err = r.remove(&record, false).unwrap_err();

    let (why, _, message) = refusal(&err).unwrap_or_else(|| panic!("a typed refusal: {err:#}"));
    assert_eq!(why, RemoveRefusal::Submodules);
    assert!(
        message.contains("working trees containing submodules cannot be moved or removed"),
        "{message}"
    );
    assert!(record.path.join("sub").exists());
    assert_eq!(r.tracked(), 1);

    r.remove(&record, true)
        .expect("forced removal goes through");
    assert!(!record.path.exists());
    assert_eq!(r.tracked(), 0);
}

/// Other failures are not refusals a force would cure, and come out as they always did.
#[test]
fn a_worktree_git_cannot_find_is_no_refusal_and_a_missing_one_is_no_worktree() {
    let r = Removal::start();
    let err = worktree::remove(
        r.t(),
        &r.cfg,
        &r.paths,
        &r.taw_bin,
        "feat/never-made",
        false,
        false,
    )
    .unwrap_err();
    assert!(refusal(&err).is_none(), "{err:#}");
    assert!(err.downcast_ref::<WorktreeError>().is_some(), "{err:#}");
}

/// A forced removal that fails partway leaves the session and the record, so there is
/// something to try again; git has by then let go of the worktree, so it says it is no
/// worktree any more. Without force that is a refusal a force cures, and the forced retry
/// clears what is left, once what stopped it is out of the way.
#[test]
fn a_removal_that_failed_partway_can_be_tried_again() {
    if running_as_root() {
        eprintln!("skipped: root is not stopped by a read-only directory");
        return;
    }
    let r = Removal::start();
    let record = r.create("feat/stuck");
    let locked = Locked::new(&record.path.join("locked"));

    let err = r.remove(&record, true).unwrap_err();

    assert!(
        format!("{err:#}").contains("Permission denied"),
        "the first failure says what stopped it: {err:#}"
    );
    assert!(
        refusal(&err).is_none(),
        "a forced removal is never offered itself again"
    );
    assert!(record.path.exists(), "part of the folder is still there");
    assert!(!r.git_lists(&record.path), "git has let go of the worktree");
    assert!(r.session_running(&record), "the session is left");
    assert_eq!(
        r.tracked(),
        1,
        "and so is the record, which is what a retry works from"
    );

    // Without force, the retry is refused, and what would cure it is a force.
    let err = r.remove(&record, false).unwrap_err();
    let (why, branch, _) = refusal(&err).unwrap_or_else(|| panic!("a typed refusal: {err:#}"));
    assert_eq!(
        (why, branch.as_str()),
        (RemoveRefusal::Leftover, "feat/stuck")
    );
    assert_eq!(
        refusal_folder(&err),
        Some(record.path.clone()),
        "the refusal names the folder a force would delete"
    );
    assert!(record.path.exists(), "a refusal deletes nothing");

    // Forced, while the same thing still stops it: the same failure, nothing lost track of.
    let err = r.remove(&record, true).unwrap_err();
    assert!(format!("{err:#}").contains("Permission denied"), "{err:#}");
    assert_eq!(r.tracked(), 1);
    assert!(r.session_running(&record));

    // Out of the way, the forced retry finishes the job.
    locked.unlock();
    let removed = r.remove(&record, true).expect("the retry goes through");
    assert_eq!(removed.branch, "feat/stuck");
    assert!(!record.path.exists());
    assert!(!r.session_running(&record));
    assert_eq!(r.tracked(), 0);
    assert!(branch_exists(&r.repo, "feat/stuck"));
}

/// A worktree git has already let go of, whose folder is gone too, has nothing left to
/// remove: the removal finishes what it was asked to, the session and the record.
#[test]
fn a_worktree_that_is_already_gone_is_finished_off_not_failed() {
    let r = Removal::start();
    let record = r.create("feat/gone");
    run_git(
        &r.repo,
        &[
            "worktree",
            "remove",
            "--force",
            record.path.to_str().unwrap(),
        ],
    );
    assert!(!record.path.exists());

    let removed = r
        .remove(&record, false)
        .expect("nothing left to remove is not a failure");

    assert_eq!(removed.branch, "feat/gone");
    assert!(!r.session_running(&record));
    assert_eq!(r.tracked(), 0);
}

/// A worktree whose folder has a `#` in its path gets its session in that folder, from `create` and
/// from `open` once its session is closed: tmux expands formats in a start directory, where `#S` was
/// the session's name, and a folder that is not there is the home folder.
#[test]
fn a_worktree_whose_folder_has_a_hash_in_its_path_gets_its_session_there() {
    let r = Removal::start();
    let directory = r.cfg.worktrees.directory.join("C#Sharp");
    std::fs::create_dir(&directory).unwrap();
    let mut cfg = r.cfg.clone();
    cfg.worktrees.directory = directory;
    let record = worktree::create(
        r.t(),
        &cfg,
        &r.paths,
        &r.taw_bin,
        "repo",
        "feat/hash",
        None,
        false,
        false,
    )
    .unwrap();
    let folder = record.path.canonicalize().unwrap().display().to_string();
    let opened_in = || {
        r.t()
            .display(
                &format!("={}:", record.session_name),
                "#{pane_current_path}",
            )
            .unwrap_or_default()
    };
    let wait = std::time::Duration::from_secs(10);
    let came = |what: &str| {
        let deadline = std::time::Instant::now() + wait;
        while opened_in() != folder && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(opened_in(), folder, "{what}");
    };
    came("create");

    r.t()
        .run_ok(&["kill-session", "-t", &format!("={}", record.session_name)])
        .unwrap();
    worktree::open(
        r.t(),
        &cfg,
        &r.paths,
        &r.taw_bin,
        &record.workspace_id,
        false,
        false,
    )
    .unwrap();
    came("open");
}

/// A worktree of a repository whose folder has a `#` in its name gets a session of its name
/// (`<repo>--<branch>`), from `create` and from `open` once its session is closed: tmux expands formats
/// in the name it makes a session with (`#S`, the name of a session not yet made, is nothing). The
/// record names the session tmux made, and `open` makes no other. (A branch holds neither `#` nor `;`,
/// which `create` refuses.)
#[test]
fn a_worktree_of_a_repository_with_a_hash_in_its_folder_gets_a_session_of_its_name() {
    let r = Removal::start_in("C#Sharp");
    let sessions = || {
        let mut names: Vec<String> = r
            .t()
            .list_sessions()
            .unwrap()
            .into_iter()
            .map(|session| session.session_name)
            .filter(|name| name != "repo")
            .collect();
        names.sort();
        names
    };
    let record = r.create("feat/x");
    assert_eq!(record.session_name, "C#Sharp--feat-x");
    assert_eq!(sessions(), ["C#Sharp--feat-x"], "create");

    r.t()
        .run_ok(&[
            "kill-session",
            "-t",
            &taw::tmux::session_target(&record.session_name),
        ])
        .unwrap();
    assert!(sessions().is_empty(), "{:?}", sessions());
    worktree::open(
        r.t(),
        &r.cfg,
        &r.paths,
        &r.taw_bin,
        &record.workspace_id,
        false,
        false,
    )
    .unwrap();
    assert_eq!(sessions(), ["C#Sharp--feat-x"], "open");
}

/// `open` of a worktree whose session is closed and whose checkout is gone makes no session: tmux
/// would open one in the home folder and say nothing. It is refused, as herdr refuses to open a
/// worktree whose checkout is gone (`worktree_not_found`), and the refusal names the folder. `list`
/// drops such a worktree, as it did.
#[test]
fn opening_a_worktree_whose_checkout_is_gone_is_refused_and_names_the_folder() {
    let r = Removal::start();
    let record = r.create("feat/gone");
    r.t()
        .run_ok(&["kill-session", "-t", &format!("={}", record.session_name)])
        .unwrap();
    std::fs::remove_dir_all(&record.path).unwrap();

    let err = worktree::open(
        r.t(),
        &r.cfg,
        &r.paths,
        &r.taw_bin,
        &record.workspace_id,
        false,
        false,
    )
    .unwrap_err();

    assert_eq!(taw::cli::envelope::code_for(&err), "not_found", "{err:#}");
    assert!(
        err.to_string().contains(&record.path.display().to_string()),
        "{err:#}"
    );
    assert!(!r.session_running(&record), "a session was made");
    assert_eq!(r.tracked(), 0);
}

/// Only what git's own bookkeeping says was the worktree's folder is deleted: a folder that
/// has come to stand where it was, with something else in it, is left alone (herdr's
/// `forced_worktree_remove_recovery_keeps_unrelated_replacement_directory`).
#[test]
fn a_folder_that_is_not_the_worktrees_is_never_deleted() {
    let r = Removal::start();
    let record = r.create("feat/replaced");
    run_git(
        &r.repo,
        &[
            "worktree",
            "remove",
            "--force",
            record.path.to_str().unwrap(),
        ],
    );
    std::fs::create_dir_all(&record.path).unwrap();
    std::fs::write(record.path.join("unrelated"), "do not delete\n").unwrap();

    for force in [false, true] {
        let err = r.remove(&record, force).unwrap_err();
        assert!(refusal(&err).is_none(), "force={force}: {err:#}");
        assert!(
            format!("{err:#}").contains("is not a working tree"),
            "force={force}: git's own words: {err:#}"
        );
        assert!(record.path.join("unrelated").exists(), "force={force}");
    }
    assert_eq!(r.tracked(), 1);
    assert!(r.session_running(&record));
}

/// A folder git has let go of is cleared only when its own `.git` file says it was a checkout of
/// this repository, one that points into this repository's `worktrees` directory. One that
/// points into another repository's worktree entry, into this repository's git directory but
/// outside `worktrees`, or at nothing at all, is not the worktree's, whatever force says: nothing
/// is deleted, and git's own words are the failure.
#[test]
fn a_folder_whose_dot_git_points_elsewhere_is_never_deleted() {
    let r = Removal::start();
    let record = r.create("feat/elsewhere");
    run_git(
        &r.repo,
        &[
            "worktree",
            "remove",
            "--force",
            record.path.to_str().unwrap(),
        ],
    );
    // Another repository, with a worktree of its own: the entry git keeps for it.
    let other_tmp = tempfile::tempdir().unwrap();
    let other = other_tmp.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    init_repo(&other);
    let other_checkout = other_tmp.path().join("other-checkout");
    run_git(
        &other,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            other_checkout.to_str().unwrap(),
        ],
    );
    let other_entry = other.join(".git").join("worktrees").join("other-checkout");
    assert!(other_entry.is_dir(), "{}", other_entry.display());

    std::fs::create_dir_all(&record.path).unwrap();
    std::fs::write(record.path.join("unrelated"), "do not delete\n").unwrap();
    for (what, target) in [
        ("another repository's worktree", other_entry),
        (
            "this repository's git directory, outside worktrees",
            r.repo.join(".git"),
        ),
        (
            "nothing at all",
            std::path::PathBuf::from("/nonexistent/taw-test-gitdir"),
        ),
    ] {
        std::fs::write(
            record.path.join(".git"),
            format!("gitdir: {}\n", target.display()),
        )
        .unwrap();
        for force in [false, true] {
            let err = r.remove(&record, force).unwrap_err();
            assert!(
                refusal(&err).is_none(),
                "{what}, force={force}: no question is asked about it: {err:#}"
            );
            assert!(
                format!("{err:#}").contains("is not a working tree"),
                "{what}, force={force}: git's own words: {err:#}"
            );
            assert!(
                record.path.join("unrelated").exists(),
                "{what}, force={force}: the folder is untouched"
            );
        }
    }
    assert_eq!(r.tracked(), 1);
    assert!(r.session_running(&record));
}

/// `[hooks] pre_worktree_remove` is asked before git is, every time, so a forced removal
/// can still be refused by it.
#[test]
fn the_pre_worktree_remove_hook_is_asked_before_a_forced_removal_too() {
    let mut r = Removal::start();
    let record = r.create("feat/dirty");
    std::fs::write(record.path.join("new.txt"), "untracked\n").unwrap();
    let marker = r._state_dir.path().join("hook-refuses");
    r.cfg = toml::from_str(&format!(
        "[hooks]\npre_worktree_remove = \"[ ! -e '{}' ] || {{ echo 'not while it works' >&2; exit 1; }}\"\n",
        marker.display()
    ))
    .unwrap();
    r.cfg.worktrees.directory = r._worktrees_dir.path().to_path_buf();

    // The hook agrees, git refuses.
    let err = r.remove(&record, false).unwrap_err();
    assert!(refusal(&err).is_some(), "{err:#}");

    // Now it does not: the forced removal is stopped before git is asked.
    std::fs::write(&marker, "").unwrap();
    let err = r.remove(&record, true).unwrap_err();
    assert!(format!("{err:#}").contains("not while it works"), "{err:#}");
    assert!(record.path.join("new.txt").exists());
    assert!(r.session_running(&record));
    assert_eq!(r.tracked(), 1);

    // And when it agrees again the forced removal goes through.
    std::fs::remove_file(&marker).unwrap();
    r.remove(&record, true)
        .expect("forced removal goes through");
    assert!(!record.path.exists());
}

/// No git that these tests run reaches the repository that git's variables of whoever runs them point
/// at (git runs the command of a `rebase -x` with `GIT_DIR` set): every test of this file but this one, since nearly all make a repository, run again
/// with each of the variables pointing into a decoy, pass and leave it byte for byte as it was
/// (tests/common/git_decoy.rs).
#[test]
fn the_tests_that_make_a_repository_leave_the_one_gits_variables_point_at_as_it_was() {
    common::git_decoy::rerun(&[], "leave_the_one_gits_variables_point_at");
}

/// herdr's `worktree create --cwd <repo>` names a folder inside the repository: the session of
/// that repository is found from it (the fixture's session `repo` is rooted at the repository's top,
/// and the folder given is one inside it), and a repository with no session gets one made at its root.
#[test]
fn a_folder_inside_a_repository_finds_the_session_of_that_repository_or_makes_one() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let inside = repo.join("src");
    std::fs::create_dir_all(&inside).unwrap();
    let server = Server::start(&repo);
    let known = server.0.list_sessions().unwrap().remove(0);

    let found =
        worktree::session_of_repository(&server.0, inside.to_str().unwrap(), false).unwrap();
    assert_eq!(
        found, known.session_id,
        "the session rooted at the repository"
    );

    // A second repository with no session: one is made, named for it, rooted at its top.
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let other = git_repo(&other);
    let made = worktree::session_of_repository(&server.0, other.to_str().unwrap(), false).unwrap();
    assert_ne!(made, known.session_id);
    let session = server
        .0
        .list_sessions()
        .unwrap()
        .into_iter()
        .find(|s| s.session_id == made)
        .expect("the session was made");
    assert_eq!(
        std::fs::canonicalize(&session.session_path).unwrap(),
        std::fs::canonicalize(&other).unwrap()
    );
    // Asked again, the one just made is found, not a third.
    let again = worktree::session_of_repository(&server.0, other.to_str().unwrap(), false).unwrap();
    assert_eq!(again, made);

    // A folder that is in no repository is `not_a_git_repo`, and so is one that is not there.
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    for dir in [plain.to_str().unwrap(), "/no/such/folder/here"] {
        let err = worktree::session_of_repository(&server.0, dir, false).unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<worktree::WorktreeError>(),
                Some(worktree::WorktreeError::NotAGitRepo(_))
            ),
            "{dir}: {err:#}"
        );
    }
}
