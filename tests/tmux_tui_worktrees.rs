//! What `x` does on a worktree and on a session that has worktree sessions, against a real tmux
//! server and real git repositories (herdr v0.9.1: a removal git refuses offers a forced one,
//! and a close of a primary workspace with worktree workspaces asks for the group).
//!
//! The popup is driven the way its run loop drives it, without the terminal: a key goes to
//! [`App::on_key`], the action it returns runs through [`actions::exec`] against the server,
//! and [`finish`] does what the loop does next (the toast, the question, reading tmux again).
//! What is asserted is what the user sees (the confirmation's words, the toast) and what is
//! left on disk and in tmux afterwards.
//!
//! Every test runs on its own `-L` socket, in its own temporary directories, and tears the
//! server down on drop; the user's real tmux server, repositories and HOME are never touched.

mod common;

use common::shell_env::ShellHome;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use taw::config::Config;
use taw::paths::Paths;
use taw::session::worktree::{self, WorktreeRecord};
use taw::tmux::Tmux;
use taw::tui::actions::{self, Action};
use taw::tui::app::{App, SETTLE};
use taw::tui::data::{self, GitWorker};
use taw::tui::overlays::{Answers, Confirm, Overlay};
use taw::tui::theme;
use taw::tui::tree::Row;
use taw::tui::{Finished, finish};

/// A throwaway tmux server on its own `-L` socket, torn down on drop. It starts with two
/// sessions: `repo`, in the repository, and `keeper`, which only keeps the server alive when
/// everything else is closed.
struct Server(Tmux, #[allow(dead_code)] ShellHome);

impl Server {
    fn start(repo: &Path, elsewhere: &Path) -> Server {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let t = Tmux::with_socket_name(&format!("taw-test-wtui-{}-{n}", std::process::id()));
        let home = ShellHome::new();
        // `-f /dev/null` so the host's own tmux.conf cannot change what this test observes,
        // and `/bin/sh` so no login shell's rc file runs in a pane.
        home.start_server(
            &t,
            &[
                "-d",
                "-s",
                "repo",
                "-c",
                repo.to_str().unwrap(),
                "-x",
                "80",
                "-y",
                "24",
                "/bin/sh",
            ],
        );
        t.run_ok(&["set-option", "-g", "default-shell", "/bin/sh"])
            .unwrap();
        t.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "keeper",
            "-c",
            elsewhere.to_str().unwrap(),
        ])
        .unwrap();
        Server(t, home)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.run_ok(&["kill-server"]);
    }
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = taw::git::command()
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

fn commit(dir: &Path, message: &str) {
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
            message,
        ],
    );
}

fn init_repo(dir: &Path) {
    run_git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    run_git(dir, &["add", "."]);
    commit(dir, "first");
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
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

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ch(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

/// Whether this process runs as root, who is not stopped by a directory's permissions: a test
/// that makes a folder read-only so that a file in it cannot be deleted has nothing to show then.
fn running_as_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "0")
}

/// Makes `dir` unwritable, so a file in it cannot be deleted, and puts it right on drop so the
/// temporary directory can be cleaned up whatever the test did.
struct Locked(PathBuf);

impl Locked {
    fn new(dir: &Path) -> Locked {
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

/// A repository with its session, a server, and what the popup needs: the config (zoxide off,
/// since it defaults to on and opening a session would run the real `zoxide add`), state
/// directories of their own, and the worktree verbs to make worktrees with.
struct World {
    _repo_tmp: tempfile::TempDir,
    _state_tmp: tempfile::TempDir,
    _worktrees_tmp: tempfile::TempDir,
    _elsewhere: tempfile::TempDir,
    server: Server,
    repo: PathBuf,
    paths: Paths,
    cfg: Config,
    git: GitWorker,
}

impl World {
    fn start() -> World {
        World::start_with("")
    }

    /// [`World::start`] with `extra` config TOML (a `[hooks]` table, say).
    fn start_with(extra: &str) -> World {
        let repo_tmp = tempfile::tempdir().unwrap();
        // A name of our own under the temporary directory: `tempfile`'s are dot-prefixed, and
        // tmux rewrites a leading dot in a session name.
        let repo = repo_tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let repo = std::fs::canonicalize(&repo).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let state_tmp = tempfile::tempdir().unwrap();
        let worktrees_tmp = tempfile::tempdir().unwrap();
        let mut cfg: Config = toml::from_str(extra).expect("the test config parses");
        cfg.sessions.zoxide = false;
        cfg.worktrees.directory = worktrees_tmp.path().to_path_buf();
        let dir = state_tmp.path();
        let plugins_dir = dir.join("plugins");
        let paths = Paths {
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
        };
        World {
            server: Server::start(&repo, elsewhere.path()),
            _repo_tmp: repo_tmp,
            _state_tmp: state_tmp,
            _worktrees_tmp: worktrees_tmp,
            _elsewhere: elsewhere,
            repo,
            paths,
            cfg,
            git: GitWorker::new(Duration::from_secs(2)),
        }
    }

    fn t(&self) -> &Tmux {
        &self.server.0
    }

    fn taw_bin(&self) -> PathBuf {
        PathBuf::from("/bin/true")
    }

    /// A worktree of `repo` on `branch`, with its session.
    fn worktree(&self, branch: &str) -> WorktreeRecord {
        worktree::create(
            self.t(),
            &self.cfg,
            &self.paths,
            &self.taw_bin(),
            "repo",
            branch,
            None,
            false,
            false,
        )
        .expect("a worktree of a real repository")
    }

    fn sessions(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .t()
            .list_sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.session_name)
            .collect();
        names.sort();
        names
    }

    fn tracked(&self) -> usize {
        worktree::list(self.t(), &self.paths).unwrap().len()
    }

    /// The popup over the server as it is now.
    fn popup(&self) -> App<'_> {
        let snap = data::build(self.t(), &self.cfg, &self.git, None, None).unwrap();
        let theme = theme::resolve(&self.cfg.theme.name, &self.cfg.theme.custom).theme;
        let mut app = App::new(&self.cfg, theme, String::new(), snap);
        app.set_worktrees(worktree::list(self.t(), &self.paths).unwrap());
        app
    }

    /// One key, then what the run loop does with the action it asks for. What the action said
    /// comes back as the toast the user would read, if any.
    fn press(&self, app: &mut App, key: KeyEvent) -> Option<(Action, Finished)> {
        let action = app.on_key(key)?;
        let outcome = actions::exec(
            self.t(),
            &self.cfg,
            &self.paths,
            &self.taw_bin(),
            &action,
            None,
        )
        .expect("an action's failure is a toast, never an error");
        let done = finish(
            self.t(),
            &self.cfg,
            &self.paths,
            &[],
            &self.git,
            app,
            &action,
            outcome,
            None,
            None,
        );
        Some((action, done))
    }

    /// Waits out the moment after a close in which the popup takes no `y` for a removal that
    /// loses work.
    fn let_the_popup_settle(&self) {
        std::thread::sleep(SETTLE + Duration::from_millis(100));
    }
}

fn confirmation<'a>(app: &'a App<'a>) -> &'a Confirm {
    match &app.overlay {
        Overlay::Confirm(confirm) => confirm,
        other => panic!("expected a confirmation, got {other:?}"),
    }
}

/// Puts the cursor on the parked worktree row.
fn cursor_to_parked(app: &mut App) {
    app.sel = app
        .rows
        .rows
        .iter()
        .position(|row| matches!(row, Row::ClosedWorktree { .. }))
        .expect("a parked worktree in the tree");
}

fn cursor_to_header(app: &mut App, session: &str) {
    app.sel = app
        .rows
        .rows
        .iter()
        .position(|row| match row {
            Row::Session(space) => app.snap.spaces[*space].session_name == session,
            _ => false,
        })
        .unwrap_or_else(|| panic!("no header for {session}"));
}

fn parked_rows(app: &App) -> usize {
    app.rows
        .rows
        .iter()
        .filter(|row| matches!(row, Row::ClosedWorktree { .. }))
        .count()
}

/// A worktree whose session is closed: a parked row, which is what `x` removes.
fn parked(world: &World, branch: &str) -> WorktreeRecord {
    let record = world.worktree(branch);
    world
        .t()
        .run_ok(&["kill-session", "-t", &record.session_name])
        .unwrap();
    record
}

// ------------------------------------------------------------------ a removal git refuses

/// The words the popup asks in for a worktree git will not remove because it has changes.
fn dirty_question(branch: &str) -> String {
    format!(
        "git will not remove {branch}: it has modified or untracked files. remove anyway? uncommitted changes are lost"
    )
}

/// Everything the user sees and what is left, from the first `x` to the removal: git refuses
/// a dirty worktree, the popup asks whether to remove it anyway, anything but `y` keeps it, and
/// `y` removes it and its session and record, never its branch.
#[test]
fn a_dirty_worktree_git_refuses_is_offered_a_forced_removal_that_only_y_accepts() {
    let world = World::start();
    let record = parked(&world, "feat/dirty");
    std::fs::write(record.path.join("new.txt"), "untracked\n").unwrap();
    std::fs::write(record.path.join("README.md"), "changed\n").unwrap();
    let mut app = world.popup();
    cursor_to_parked(&mut app);

    // The ordinary confirmation comes first, as it always did.
    assert!(world.press(&mut app, ch('x')).is_none());
    assert_eq!(
        confirmation(&app).message,
        "git worktree remove feat/dirty? (the branch is kept)"
    );
    assert_eq!(confirmation(&app).answers, Answers::Plain);

    // `y` runs it, and git refuses: the popup asks, in place of showing git's reason.
    let (action, done) = world.press(&mut app, ch('y')).expect("y runs the removal");
    assert_eq!(
        action,
        Action::RemoveWorktree {
            workspace: record.workspace_id.clone(),
            force: false
        }
    );
    assert_eq!(
        done,
        Finished {
            exit: false,
            closed: true
        }
    );
    assert_eq!(confirmation(&app).message, dirty_question("feat/dirty"));
    assert_eq!(confirmation(&app).answers, Answers::Strict);
    assert_eq!(confirmation(&app).answers.line(), "y / N");
    assert_eq!(app.live_toast(), None, "the question says it, not a toast");
    assert!(record.path.join("new.txt").exists(), "nothing was touched");
    assert_eq!(world.tracked(), 1);

    // Esc, `n`, `q` and Enter each keep it, and each leave the row where it was.
    for answer in [key(KeyCode::Esc), ch('n'), ch('q'), key(KeyCode::Enter)] {
        assert!(world.press(&mut app, answer).is_none(), "{answer:?}");
        assert!(!app.overlay.is_open(), "{answer:?}");
        assert!(record.path.join("new.txt").exists(), "{answer:?}");
        assert_eq!(parked_rows(&app), 1, "{answer:?}");
        // Ask again: `x`, then `y` (past the moment after the last close), and the same
        // question comes back.
        world.let_the_popup_settle();
        world.press(&mut app, ch('x'));
        world.press(&mut app, ch('y'));
        assert_eq!(
            confirmation(&app).message,
            dirty_question("feat/dirty"),
            "{answer:?}"
        );
        world.let_the_popup_settle();
    }

    // `y` removes it anyway.
    let (action, _) = world.press(&mut app, ch('y')).expect("y runs it forced");
    assert_eq!(
        action,
        Action::RemoveWorktree {
            workspace: record.workspace_id.clone(),
            force: true
        }
    );
    assert!(
        !record.path.exists(),
        "the folder and what was in it are gone"
    );
    assert_eq!(world.tracked(), 0, "and the record");
    assert_eq!(parked_rows(&app), 0, "and the row");
    assert_eq!(app.live_toast(), Some("removed worktree feat/dirty"));
    assert!(
        branch_exists(&world.repo, "feat/dirty"),
        "the branch is kept"
    );
    assert!(!app.overlay.is_open());
}

/// A worktree with nothing in it git minds is removed at the first `y`, asking nothing.
#[test]
fn a_clean_worktree_is_removed_at_the_first_y_with_no_second_question() {
    let world = World::start();
    let record = parked(&world, "feat/clean");
    let mut app = world.popup();
    cursor_to_parked(&mut app);

    world.press(&mut app, ch('x'));
    world.press(&mut app, ch('y'));

    assert!(!record.path.exists());
    assert!(!app.overlay.is_open(), "no second question");
    assert_eq!(app.live_toast(), Some("removed worktree feat/clean"));
    assert_eq!(world.tracked(), 0);
}

/// The forced removal asks `[hooks] pre_worktree_remove` again, and the hook can refuse it: the
/// worktree stays, and the toast is the hook's own line.
#[test]
fn the_pre_worktree_remove_hook_can_still_refuse_the_forced_removal() {
    let world = World::start_with("");
    let marker = world._state_tmp.path().join("refuse");
    let world = World {
        cfg: {
            let mut cfg: Config = toml::from_str(&format!(
                "[hooks]\npre_worktree_remove = \"[ ! -e '{}' ] || {{ echo 'feat/dirty is not merged' >&2; exit 1; }}\"\n",
                marker.display()
            ))
            .unwrap();
            cfg.sessions.zoxide = false;
            cfg.worktrees.directory = world._worktrees_tmp.path().to_path_buf();
            cfg
        },
        ..world
    };
    let record = parked(&world, "feat/dirty");
    std::fs::write(record.path.join("new.txt"), "untracked\n").unwrap();
    let mut app = world.popup();
    cursor_to_parked(&mut app);

    // The hook agrees, and git refuses, and the popup asks.
    world.press(&mut app, ch('x'));
    world.press(&mut app, ch('y'));
    assert_eq!(confirmation(&app).message, dirty_question("feat/dirty"));
    world.let_the_popup_settle();

    // Now the hook does not: the forced removal stops before git is asked.
    std::fs::write(&marker, "").unwrap();
    world.press(&mut app, ch('y'));
    assert_eq!(
        app.live_toast(),
        Some("pre-worktree-remove hook: feat/dirty is not merged")
    );
    assert!(record.path.join("new.txt").exists());
    assert_eq!(world.tracked(), 1);
    assert_eq!(parked_rows(&app), 1);
    assert!(!app.overlay.is_open());
}

/// `[ui] confirm_close = false` skips the confirmation of a removal, not the question about
/// what it would lose: a dirty worktree is still asked about, and nothing is removed without
/// the `y`.
#[test]
fn with_confirmations_off_a_removal_git_refused_is_still_asked_about() {
    let world = World::start_with("[ui]\nconfirm_close = false\n");
    let record = parked(&world, "feat/dirty");
    std::fs::write(record.path.join("new.txt"), "untracked\n").unwrap();
    let mut app = world.popup();
    cursor_to_parked(&mut app);

    // `x` runs the removal at once, git refuses, and the question is the first thing asked.
    let (action, _) = world.press(&mut app, ch('x')).expect("x removes at once");
    assert_eq!(
        action,
        Action::RemoveWorktree {
            workspace: record.workspace_id.clone(),
            force: false
        }
    );
    assert_eq!(confirmation(&app).message, dirty_question("feat/dirty"));
    assert!(record.path.join("new.txt").exists());

    world.let_the_popup_settle();
    assert!(world.press(&mut app, key(KeyCode::Enter)).is_none());
    assert!(record.path.join("new.txt").exists(), "Enter keeps it");
}

/// A refusal no force cures is shown as it always was: git's reason, in a toast, with no
/// question.
#[test]
fn a_refusal_a_force_does_not_cure_is_a_toast_and_no_question() {
    let world = World::start();
    let record = parked(&world, "feat/odd");
    // The folder is gone and something else stands where it was: git says it is no
    // worktree, and the folder is not the worktree's to delete.
    run_git(
        &world.repo,
        &[
            "worktree",
            "remove",
            "--force",
            record.path.to_str().unwrap(),
        ],
    );
    std::fs::create_dir_all(&record.path).unwrap();
    std::fs::write(record.path.join("unrelated"), "do not delete\n").unwrap();
    let mut app = world.popup();
    cursor_to_parked(&mut app);

    world.press(&mut app, ch('x'));
    world.press(&mut app, ch('y'));

    assert!(!app.overlay.is_open(), "{:?}", app.overlay);
    assert!(
        app.live_toast()
            .is_some_and(|t| t.contains("is not a working tree")),
        "{:?}",
        app.live_toast()
    );
    assert!(record.path.join("unrelated").exists());
}

/// A worktree with a submodule is refused by git even when it is clean, and the popup asks
/// the same question, saying why.
#[test]
fn a_worktree_with_a_submodule_is_offered_a_forced_removal_too() {
    let world = World::start();
    let sub_tmp = tempfile::tempdir().unwrap();
    let sub = sub_tmp.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    init_repo(&sub);
    run_git(
        &world.repo,
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
    commit(&world.repo, "add a submodule");
    let record = parked(&world, "feat/sub");
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
    let mut app = world.popup();
    cursor_to_parked(&mut app);

    world.press(&mut app, ch('x'));
    world.press(&mut app, ch('y'));

    assert_eq!(
        confirmation(&app).message,
        "git will not remove feat/sub: it has submodules. remove anyway? uncommitted changes are lost"
    );
    world.let_the_popup_settle();
    world.press(&mut app, ch('y'));
    assert!(!record.path.exists());
    assert_eq!(app.live_toast(), Some("removed worktree feat/sub"));
}

/// A removal that failed partway can be tried again: the first forced removal stops on a file
/// it may not delete, the row is still there, and the next `x` asks about what is left, which
/// `y` clears once the file can go.
#[test]
fn a_removal_that_failed_partway_can_be_tried_again_from_the_popup() {
    if running_as_root() {
        eprintln!("skipped: root is not stopped by a read-only directory");
        return;
    }
    let world = World::start();
    let record = parked(&world, "feat/stuck");
    let locked = Locked::new(&record.path.join("locked"));
    let mut app = world.popup();
    cursor_to_parked(&mut app);

    // Dirty (the locked folder is untracked), so the first answer is the usual question; its
    // `y` runs the forced removal, which stops on the folder it may not empty.
    world.press(&mut app, ch('x'));
    world.press(&mut app, ch('y'));
    assert_eq!(confirmation(&app).message, dirty_question("feat/stuck"));
    world.let_the_popup_settle();
    world.press(&mut app, ch('y'));
    assert!(!app.overlay.is_open());
    assert!(
        app.live_toast()
            .is_some_and(|t| t.contains("Permission denied")),
        "{:?}",
        app.live_toast()
    );
    assert!(record.path.exists(), "part of it is still there");
    assert_eq!(world.tracked(), 1, "so is its record");
    assert_eq!(
        parked_rows(&app),
        1,
        "and its row, which is what a retry works from"
    );

    // The retry: git has let go of the worktree, and says what is left.
    world.let_the_popup_settle();
    world.press(&mut app, ch('x'));
    world.press(&mut app, ch('y'));
    // It names the folder, since that is what `y` deletes, as a path under home is written.
    let folder = match dirs::home_dir() {
        Some(home) => taw::session::naming::tilde(&record.path, &home),
        None => record.path.display().to_string(),
    };
    assert_eq!(
        confirmation(&app).message,
        format!(
            "git no longer lists feat/stuck, but part of its folder is still there: {folder}. remove anyway? uncommitted changes are lost"
        )
    );
    assert_eq!(confirmation(&app).answers, Answers::Strict);
    world.let_the_popup_settle();

    // Out of the way, it goes.
    locked.unlock();
    world.press(&mut app, ch('y'));
    assert!(!record.path.exists());
    assert_eq!(world.tracked(), 0);
    assert_eq!(app.live_toast(), Some("removed worktree feat/stuck"));
    assert!(branch_exists(&world.repo, "feat/stuck"));
}

// ------------------------------------------ a session with worktree sessions open

/// The question `x` on `repo` asks while its two worktree sessions are open.
const GROUP_QUESTION: &str = "kill-session repo? It has 2 worktree sessions open (repo--feat-a, repo--feat-b). Close them too?";

fn two_worktree_world(extra: &str) -> World {
    let world = World::start_with(extra);
    world.worktree("feat/a");
    world.worktree("feat/b");
    assert_eq!(
        world.sessions(),
        ["keeper", "repo", "repo--feat-a", "repo--feat-b"]
    );
    world
}

fn ask_the_group_question<'a>(world: &'a World) -> App<'a> {
    let mut app = world.popup();
    cursor_to_header(&mut app, "repo");
    assert!(
        world.press(&mut app, ch('x')).is_none(),
        "the question comes first"
    );
    assert_eq!(confirmation(&app).message, GROUP_QUESTION);
    assert_eq!(confirmation(&app).answers.line(), "y / N");
    app
}

/// `y`: the session and its two worktree sessions close, and the worktrees themselves stay.
#[test]
fn y_closes_the_session_and_its_worktree_sessions() {
    let world = two_worktree_world("");
    let mut app = ask_the_group_question(&world);

    let (action, done) = world.press(&mut app, ch('y')).expect("y runs it");

    assert!(matches!(action, Action::KillGroup { .. }), "{action:?}");
    assert_eq!(
        done,
        Finished {
            exit: false,
            closed: true
        }
    );
    assert_eq!(world.sessions(), ["keeper"]);
    assert_eq!(world.tracked(), 2, "closing a session removes no worktree");
    assert!(world.repo.exists());
    assert_eq!(
        app.live_toast(),
        Some("closed repo and its 2 worktree sessions")
    );
    assert!(!app.overlay.is_open());
}

/// Enter, the default, and `n`: just this session, the way `x` always was. The worktree
/// sessions are left running.
#[test]
fn enter_and_n_close_just_this_session() {
    for answer in [key(KeyCode::Enter), ch('n')] {
        let world = two_worktree_world("");
        let mut app = ask_the_group_question(&world);

        let (action, _) = world.press(&mut app, answer).expect("the answer runs it");

        assert!(matches!(action, Action::KillWorkspace { .. }), "{action:?}");
        assert_eq!(
            world.sessions(),
            ["keeper", "repo--feat-a", "repo--feat-b"],
            "{answer:?}"
        );
        assert!(!app.overlay.is_open());
    }
}

/// Esc, and `q`, close nothing.
#[test]
fn esc_closes_nothing() {
    for answer in [key(KeyCode::Esc), ch('q')] {
        let world = two_worktree_world("");
        let mut app = ask_the_group_question(&world);

        assert!(world.press(&mut app, answer).is_none(), "{answer:?}");

        assert_eq!(
            world.sessions(),
            ["keeper", "repo", "repo--feat-a", "repo--feat-b"],
            "{answer:?}"
        );
        assert!(!app.overlay.is_open());
    }
}

/// A world with a `[hooks] pre_close` of `hook` (a shell line that can read `$TAW_SESSION`),
/// two worktree sessions, and the file every asking is logged to.
fn world_with_pre_close(hook: &str) -> (World, PathBuf) {
    let world = World::start();
    let log = world._state_tmp.path().join("asked");
    let hook = hook.replace("$LOG", &log.display().to_string());
    let world = World {
        cfg: {
            let mut cfg: Config = toml::from_str(&format!(
                "[ui]\ndepth = \"all\"\n[hooks]\npre_close = {hook:?}\n"
            ))
            .unwrap();
            cfg.sessions.zoxide = false;
            cfg.worktrees.directory = world._worktrees_tmp.path().to_path_buf();
            cfg
        },
        ..world
    };
    world.worktree("feat/a");
    world.worktree("feat/b");
    (world, log)
}

/// The veto hook is asked about every session a group close would close, before any of them is
/// closed, and about just the one when only it is.
#[test]
fn the_pre_close_hook_is_asked_about_each_session_that_would_close() {
    let (world, log) = world_with_pre_close("echo \"$TAW_SESSION\" >> '$LOG'");
    let mut app = ask_the_group_question(&world);

    world.press(&mut app, ch('y'));

    let asked = std::fs::read_to_string(&log).unwrap();
    let mut asked: Vec<&str> = asked.lines().collect();
    asked.sort();
    assert_eq!(asked, ["repo", "repo--feat-a", "repo--feat-b"]);
    assert_eq!(world.sessions(), ["keeper"]);

    // Just this one: the hook is asked about that one alone.
    let (world, log) = world_with_pre_close("echo \"$TAW_SESSION\" >> '$LOG'");
    let mut app = ask_the_group_question(&world);
    world.press(&mut app, key(KeyCode::Enter));
    assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), "repo");
    assert_eq!(world.sessions(), ["keeper", "repo--feat-a", "repo--feat-b"]);
}

/// One refusal keeps the whole group open, so a hook is never left having agreed to half of
/// it: the worktree sessions are asked first, in the order they would close, and the asking
/// ends at the refusal. The toast is the hook's own line.
#[test]
fn a_pre_close_hook_refusing_one_of_them_keeps_all_of_them_open() {
    let (world, log) = world_with_pre_close(
        "echo \"$TAW_SESSION\" >> '$LOG'; [ \"$TAW_SESSION\" != repo--feat-b ] || { echo 'feat-b is still working' >&2; exit 1; }",
    );
    let mut app = ask_the_group_question(&world);

    world.press(&mut app, ch('y'));

    assert_eq!(
        world.sessions(),
        ["keeper", "repo", "repo--feat-a", "repo--feat-b"],
        "nothing was closed"
    );
    assert_eq!(
        app.live_toast(),
        Some("pre-close hook: feat-b is still working")
    );
    let asked = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        asked.lines().collect::<Vec<_>>(),
        ["repo--feat-a", "repo--feat-b"]
    );
    assert!(!app.overlay.is_open());
}

/// With `[ui] confirm_close = false` the popup asks nothing of a close: `x` closes just this
/// session, as it always did.
#[test]
fn with_confirmations_off_x_closes_just_the_session_and_asks_nothing() {
    let world = two_worktree_world("[ui]\nconfirm_close = false\n");
    let mut app = world.popup();
    cursor_to_header(&mut app, "repo");

    let (action, _) = world.press(&mut app, ch('x')).expect("x closes it at once");

    assert!(matches!(action, Action::KillWorkspace { .. }), "{action:?}");
    assert!(!app.overlay.is_open());
    assert_eq!(world.sessions(), ["keeper", "repo--feat-a", "repo--feat-b"]);
}

// ------------------------------ the last pane of a session with worktree sessions open

/// The end of the question `x` on `repo`'s only pane asks while its two worktree sessions are
/// open: the pane's own target and what runs in it come before, and depend on the shell.
const LAST_PANE_QUESTION_END: &str = "It is the session's last pane, so the session closes with it. It has 2 worktree sessions open (repo--feat-a, repo--feat-b). Close them too?";

/// A world as [`two_worktree_world`], whose popup lists every pane, plain shells too.
fn last_pane_world() -> World {
    two_worktree_world("[ui]\ndepth = \"all\"\n")
}

/// Puts the cursor on the row of `session`'s one pane.
fn cursor_to_only_pane(app: &mut App, session: &str) {
    let id = app
        .snap
        .spaces
        .iter()
        .find(|s| s.session_name == session)
        .map(|s| s.session_id.clone())
        .unwrap_or_else(|| panic!("no session {session}"));
    app.sel = app
        .rows
        .rows
        .iter()
        .position(|row| match row {
            Row::Pane(pane) => app.snap.panes[*pane].session_id == id,
            _ => false,
        })
        .unwrap_or_else(|| panic!("no pane row for {session}"));
}

fn ask_the_last_pane_question<'a>(world: &'a World) -> App<'a> {
    let mut app = world.popup();
    cursor_to_only_pane(&mut app, "repo");
    assert!(
        world.press(&mut app, ch('x')).is_none(),
        "the question comes first"
    );
    let message = &confirmation(&app).message;
    assert!(message.starts_with("kill-pane repo:"), "{message}");
    assert!(message.ends_with(LAST_PANE_QUESTION_END), "{message}");
    assert_eq!(confirmation(&app).answers.line(), "y / N");
    app
}

/// `x` on the pane that is the session's last: closing it closes the session, so it asks what
/// closing the session asks. `y` closes the session and its worktree sessions, the worktrees
/// stay.
#[test]
fn y_on_a_last_panes_question_closes_the_session_and_its_worktree_sessions() {
    let world = last_pane_world();
    let mut app = ask_the_last_pane_question(&world);

    let (action, done) = world.press(&mut app, ch('y')).expect("y runs it");

    assert!(matches!(action, Action::KillGroup { .. }), "{action:?}");
    assert_eq!(
        done,
        Finished {
            exit: false,
            closed: true
        }
    );
    assert_eq!(world.sessions(), ["keeper"]);
    assert_eq!(world.tracked(), 2, "closing a session removes no worktree");
    assert_eq!(
        app.live_toast(),
        Some("closed repo and its 2 worktree sessions")
    );
}

/// Enter, the default, and `n`: just this pane, which is what `x` always did, and the session
/// goes with its last pane. The worktree sessions are left running.
#[test]
fn enter_and_n_on_a_last_panes_question_close_just_the_pane() {
    for answer in [key(KeyCode::Enter), ch('n')] {
        let world = last_pane_world();
        let mut app = ask_the_last_pane_question(&world);

        let (action, _) = world.press(&mut app, answer).expect("the answer runs it");

        assert!(matches!(action, Action::KillPane { .. }), "{action:?}");
        assert_eq!(
            world.sessions(),
            ["keeper", "repo--feat-a", "repo--feat-b"],
            "{answer:?}"
        );
    }
}

/// Esc, and `q`, close nothing.
#[test]
fn esc_on_a_last_panes_question_closes_nothing() {
    for answer in [key(KeyCode::Esc), ch('q')] {
        let world = last_pane_world();
        let mut app = ask_the_last_pane_question(&world);

        assert!(world.press(&mut app, answer).is_none(), "{answer:?}");

        assert_eq!(
            world.sessions(),
            ["keeper", "repo", "repo--feat-a", "repo--feat-b"],
            "{answer:?}"
        );
        assert!(!app.overlay.is_open());
    }
}

/// The veto hook is asked about each session a `y` would close before any closes, and a
/// refusal keeps them all open; for just the pane it is asked about that pane alone.
#[test]
fn the_pre_close_hook_is_asked_the_way_a_session_close_asks_it() {
    let hook = "echo \"$TAW_TARGET $TAW_SESSION\" >> '$LOG'";
    let (world, log) = world_with_pre_close(hook);
    let mut app = ask_the_last_pane_question(&world);
    world.press(&mut app, ch('y'));
    let asked = std::fs::read_to_string(&log).unwrap();
    let mut asked: Vec<&str> = asked.lines().collect();
    asked.sort();
    assert_eq!(
        asked,
        [
            "session repo",
            "session repo--feat-a",
            "session repo--feat-b"
        ]
    );
    assert_eq!(world.sessions(), ["keeper"]);

    let (world, log) = world_with_pre_close(hook);
    let mut app = ask_the_last_pane_question(&world);
    world.press(&mut app, key(KeyCode::Enter));
    assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), "pane repo");
    assert_eq!(world.sessions(), ["keeper", "repo--feat-a", "repo--feat-b"]);

    let (world, _) = world_with_pre_close(
        "[ \"$TAW_SESSION\" != repo--feat-b ] || { echo 'feat-b is still working' >&2; exit 1; }",
    );
    let mut app = ask_the_last_pane_question(&world);
    world.press(&mut app, ch('y'));
    assert_eq!(
        world.sessions(),
        ["keeper", "repo", "repo--feat-a", "repo--feat-b"],
        "one refusal keeps them all"
    );
    assert_eq!(
        app.live_toast(),
        Some("pre-close hook: feat-b is still working")
    );
}

/// A pane that does not end its session, and a session with confirmations off, are not asked.
#[test]
fn a_pane_with_a_window_beside_it_and_confirmations_off_are_not_asked_the_question() {
    // A second window: the pane is not the session's last, so this is the pane close it was.
    let world = last_pane_world();
    world
        .t()
        .run_ok(&["new-window", "-d", "-t", "repo:"])
        .unwrap();
    let mut app = world.popup();
    cursor_to_only_pane(&mut app, "repo");
    world.press(&mut app, ch('x'));
    assert_eq!(confirmation(&app).answers, Answers::Plain);
    assert!(
        confirmation(&app).message.starts_with("kill-pane repo:"),
        "{}",
        confirmation(&app).message
    );
    assert!(
        !confirmation(&app).message.contains("Close them too?"),
        "{}",
        confirmation(&app).message
    );

    // Confirmations off: nobody to ask, and just this pane closes at once.
    let world = two_worktree_world("[ui]\ndepth = \"all\"\nconfirm_close = false\n");
    let mut app = world.popup();
    cursor_to_only_pane(&mut app, "repo");
    let (action, _) = world.press(&mut app, ch('x')).expect("x closes it at once");
    assert!(matches!(action, Action::KillPane { .. }), "{action:?}");
    assert_eq!(world.sessions(), ["keeper", "repo--feat-a", "repo--feat-b"]);
}

/// The question says what really closes with a last pane, on a real server: the session with
/// the last pane of its last window (nothing cut from `keeper`, so this is the plain question,
/// not the group's), only the window with the last pane of a window that has a neighbour. `y`
/// then does exactly that.
#[test]
fn x_on_a_last_pane_says_what_closes_with_it_and_that_is_what_closes() {
    let world = World::start_with("[ui]\ndepth = \"all\"\n");

    // `keeper` has the one window with the one pane.
    let mut app = world.popup();
    cursor_to_only_pane(&mut app, "keeper");
    assert!(world.press(&mut app, ch('x')).is_none(), "asked first");
    let message = confirmation(&app).message.clone();
    assert!(message.starts_with("kill-pane keeper:"), "{message}");
    assert!(
        message.ends_with("? It is the session's last pane, so the session closes with it."),
        "{message}"
    );
    assert_eq!(confirmation(&app).answers, Answers::Plain);
    world.press(&mut app, ch('y')).expect("y runs it");
    assert_eq!(
        world.sessions(),
        ["repo"],
        "the session went with its last pane"
    );

    // A second window beside the pane: only the window closes.
    let world = World::start_with("[ui]\ndepth = \"all\"\n");
    world
        .t()
        .run_ok(&["new-window", "-d", "-t", "keeper:"])
        .unwrap();
    let mut app = world.popup();
    cursor_to_only_pane(&mut app, "keeper"); // the first window's pane
    assert!(world.press(&mut app, ch('x')).is_none(), "asked first");
    let message = confirmation(&app).message.clone();
    assert!(
        message.ends_with("? It is the window's last pane, so the window closes too."),
        "{message}"
    );
    world.press(&mut app, ch('y')).expect("y runs it");
    assert_eq!(
        world.sessions(),
        ["keeper", "repo"],
        "the session stayed with its other window"
    );
}

/// The sessions of a tmux session group (`new-session -t`) share their windows, so the last pane of
/// the last window closes every one of them, and the question says that. `y` then does exactly that.
/// With a second window it says only the window and every session stays.
#[test]
fn x_on_a_last_pane_of_a_session_group_says_the_sessions_grouped_with_it_close_too() {
    let world = World::start_with("[ui]\ndepth = \"all\"\n");
    world
        .t()
        .run_ok(&["new-session", "-d", "-t", "keeper", "-s", "keeper-b"])
        .unwrap();
    let mut app = world.popup();
    cursor_to_only_pane(&mut app, "keeper");
    assert!(world.press(&mut app, ch('x')).is_none(), "asked first");
    let message = confirmation(&app).message.clone();
    assert!(
        message.ends_with(
            "? It is the session's last pane, so the session and the sessions grouped with it close."
        ),
        "{message}"
    );
    assert_eq!(confirmation(&app).answers, Answers::Plain);
    world.press(&mut app, ch('y')).expect("y runs it");
    assert_eq!(
        world.sessions(),
        ["repo"],
        "both sessions of the group went with the last pane"
    );

    // A second window, which both sessions have: only the window closes.
    let world = World::start_with("[ui]\ndepth = \"all\"\n");
    world
        .t()
        .run_ok(&["new-session", "-d", "-t", "keeper", "-s", "keeper-b"])
        .unwrap();
    world
        .t()
        .run_ok(&["new-window", "-d", "-t", "keeper:"])
        .unwrap();
    let mut app = world.popup();
    cursor_to_only_pane(&mut app, "keeper"); // the first window's pane
    assert!(world.press(&mut app, ch('x')).is_none(), "asked first");
    let message = confirmation(&app).message.clone();
    assert!(
        message.ends_with("? It is the window's last pane, so the window closes too."),
        "{message}"
    );
    world.press(&mut app, ch('y')).expect("y runs it");
    assert_eq!(
        world.sessions(),
        ["keeper", "keeper-b", "repo"],
        "the sessions stayed with their other window"
    );
}

// ------------------------------- a group close from a popup inside one of its own sessions

/// A `tmux` that writes each call's arguments on a line of `log` and runs the real one, so a
/// test can see how many calls an action made and in what order it asked for things.
fn recording_tmux(real: &Tmux, dir: &Path) -> (Tmux, PathBuf) {
    let log = dir.join("tmux-calls");
    let shim = dir.join("tmux-shim");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real.bin.display()
        ),
    )
    .unwrap();
    crate::common::make_executable(&shim);
    let mut recording = real.clone();
    recording.bin = shim;
    (recording, log)
}

/// A control-mode client of the server attached to `session`, which is what a terminal's client
/// is to tmux without a terminal: it is listed by `list-clients`, and it goes when the session it
/// is attached to does, if `detach-on-destroy` is on. Its stdin is held open.
struct Client {
    child: std::process::Child,
    name: String,
}

impl Client {
    fn attach(world: &World, session: &str) -> Client {
        let mut cmd = world.t().command();
        cmd.args(["-C", "attach-session", "-t", &format!("={session}")])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().expect("a control-mode client");
        for _ in 0..60 {
            let attached = world
                .t()
                .list_clients()
                .unwrap()
                .into_iter()
                .find(|c| c.session_name == session);
            if let Some(client) = attached {
                return Client {
                    child,
                    name: client.client_name,
                };
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // It never attached: end it, and wait for it, so that the failure leaves no process behind.
        let _ = child.kill();
        let _ = child.wait();
        panic!("no client attached to {session}");
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// With tmux's default `detach-on-destroy`, closing the session a client is attached to takes
/// the client with it, and a popup dies with its client. A group close from a popup opened in one
/// of the sessions it closes must therefore be asked of tmux in one command, which the server
/// runs to the end whatever becomes of the client that sent it, and must close that session
/// last, so that nothing is left to close once the popup is gone. With a session that stays open the
/// client is first moved to it, so it is not detached at all (`keeper` here); the order matters
/// only when nothing would be left (`client_session_last`'s unit test).
#[test]
fn a_group_close_is_one_tmux_command_and_leaves_the_popups_client_in_a_session_that_stays() {
    let world = two_worktree_world("");
    world
        .t()
        .run_ok(&["set-option", "-g", "detach-on-destroy", "on"])
        .unwrap();
    let ids: std::collections::HashMap<String, String> = world
        .t()
        .list_sessions()
        .unwrap()
        .into_iter()
        .map(|s| (s.session_name, s.session_id))
        .collect();
    let client = Client::attach(&world, "repo--feat-a");
    let (recording, log) = recording_tmux(world.t(), world._state_tmp.path());

    let outcome = actions::exec(
        &recording,
        &world.cfg,
        &world.paths,
        &world.taw_bin(),
        &Action::KillGroup {
            session: ids["repo"].clone(),
        },
        Some(&client.name),
    )
    .expect("an action's failure is a toast");

    assert_eq!(
        world.sessions(),
        ["keeper"],
        "all three closed: {:?}",
        outcome.toast
    );
    let calls = std::fs::read_to_string(&log).unwrap();
    let kills: Vec<&str> = calls
        .lines()
        .filter(|call| call.contains("kill-session"))
        .collect();
    assert_eq!(
        kills.len(),
        1,
        "one tmux command for the whole close: {calls}"
    );
    let mut closed: Vec<&str> = kills[0]
        .split(" ; ")
        .map(|kill| kill.rsplit(' ').next().unwrap())
        .collect();
    closed.sort();
    let mut all = [
        ids["repo--feat-b"].as_str(),
        ids["repo"].as_str(),
        ids["repo--feat-a"].as_str(),
    ];
    all.sort();
    assert_eq!(closed, all, "the whole group goes: {calls}");
    let on: Vec<String> = world
        .t()
        .list_clients()
        .unwrap()
        .into_iter()
        .map(|c| c.session_name)
        .collect();
    assert_eq!(on, ["keeper"], "the client was moved, not detached");
}

/// A worktree session's own header, and a session nothing was cut from, are asked nothing.
#[test]
fn a_worktree_session_and_a_session_with_none_are_closed_as_before() {
    let world = two_worktree_world("");
    let mut app = world.popup();

    cursor_to_header(&mut app, "repo--feat-a");
    world.press(&mut app, ch('x'));
    assert_eq!(confirmation(&app).message, "kill-session repo--feat-a?");
    assert_eq!(confirmation(&app).answers, Answers::Plain);
    world.press(&mut app, key(KeyCode::Esc));

    cursor_to_header(&mut app, "keeper");
    world.press(&mut app, ch('x'));
    assert_eq!(confirmation(&app).message, "kill-session keeper?");
}

/// What the group question says follows what is running: with one of the worktree sessions
/// already closed, it is asked about one.
#[test]
fn the_question_names_only_the_worktree_sessions_that_are_running() {
    let world = two_worktree_world("");
    world
        .t()
        .run_ok(&["kill-session", "-t", "repo--feat-a"])
        .unwrap();
    let mut app = world.popup();
    cursor_to_header(&mut app, "repo");

    world.press(&mut app, ch('x'));

    assert_eq!(
        confirmation(&app).message,
        "kill-session repo? It has 1 worktree session open (repo--feat-b). Close them too?"
    );
    // And a group close takes that one, and only it, with the session.
    world.press(&mut app, ch('y'));
    assert_eq!(world.sessions(), ["keeper"]);
    assert_eq!(
        app.live_toast(),
        Some("closed repo and its 1 worktree session")
    );
}

/// No git that these tests run reaches the repository that git's variables of whoever runs them point
/// at (git runs the command of a `rebase -x` with `GIT_DIR` set): the tests of this file that make a repository, run again
/// with each of the variables pointing into a decoy, pass and leave it byte for byte as it was
/// (tests/common/git_decoy.rs).
#[test]
fn the_tests_that_make_a_repository_leave_the_one_gits_variables_point_at_as_it_was() {
    common::git_decoy::rerun(
        &["worktree", "removal", "refusal"],
        "leave_the_one_gits_variables_point_at",
    );
}
