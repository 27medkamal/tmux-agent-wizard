//! Enter on a directory the `/` filter lists, against a real tmux server.
//!
//! `[ui] projects_in_filter` puts the directories the new session picker lists under the
//! sessions while the filter is open. The unit tests pin that Enter on one asks for
//! `Action::OpenProject`; `tests/tmux_tui_actions.rs` pins what that action does to a
//! server. This drives the two together the way the popup does, from a snapshot of a real
//! server through the keys to the server afterwards: a name typed at the filter creates the
//! session for its directory when nothing runs there, and switches to the one that does
//! instead of making a second.
//!
//! Every test runs on its own `-L` socket and tears it down on drop; the user's real tmux
//! server is never addressed.

mod common;

use common::shell_env::ShellHome;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use taw::config::{Config, Depth};
use taw::paths::Paths;
use taw::tmux::Tmux;
use taw::tui::actions::{self, Action, Outcome};
use taw::tui::app::App;
use taw::tui::data::{self, GitWorker};
use taw::tui::picker::Tag;
use taw::tui::theme;
use taw::tui::tree::Row;

/// A throwaway tmux server on its own `-L` socket, torn down on drop: one session, `name`,
/// in `at`.
struct Server(Tmux, #[allow(dead_code)] ShellHome);

impl Server {
    fn start(name: &str, at: &Path) -> Server {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let socket = format!("taw-test-projects-{}-{n}", std::process::id());
        let t = Tmux::with_socket_name(&socket);
        let home = ShellHome::new();
        // `-f /dev/null` so the host's own tmux.conf cannot change what this test observes,
        // and `/bin/sh` so no login shell rc file runs in the panes.
        home.start_server(
            &t,
            &[
                "-d",
                "-s",
                name,
                "-c",
                at.to_str().unwrap(),
                "-x",
                "80",
                "-y",
                "24",
                "/bin/sh",
            ],
        );
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

/// Where the test server's own session runs: `/`, and not the scratch home. The filter
/// fuzzy-matches a session's header against its whole path, a scratch directory's name is
/// random letters, and "notes" is a subsequence of some of them, which would put the cursor
/// on this session instead of the directory the test types for.
const ROOT: &str = "/";

/// `mkdir -p` under `tmp`, canonicalised: on macOS a temp directory is reached through a
/// `/var` symlink, and tmux reports the resolved path.
fn dir(tmp: &tempfile::TempDir, rel: &str) -> PathBuf {
    let path = tmp.path().join(rel);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::canonicalize(&path).unwrap()
}

/// The config the popup runs with: the filter lists directories, `projects` holds the
/// projects, and zoxide is off, because it defaults to on and `open_or_create` would then
/// run the real `zoxide add` and write a temp path into the developer's own database.
fn cfg(projects: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.ui.projects_in_filter = true;
    cfg.ui.depth = Depth::All;
    cfg.sessions.zoxide = false;
    cfg.sessions.project_dirs = vec![projects.to_path_buf()];
    cfg
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn type_str(app: &mut App, text: &str) {
    for c in text.chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
}

/// The popup over `t` as it is now, with `home` as `$HOME`, the filter open and `typed`
/// typed into it.
fn filtering<'a>(t: &Tmux, cfg: &'a Config, home: &Path, typed: &str) -> App<'a> {
    let snap = data::build(t, cfg, &GitWorker::new(Duration::from_secs(2)), None, None).unwrap();
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let mut app = App::new(cfg, theme, String::new(), snap);
    app.home = home.to_path_buf();
    app.on_key(key(KeyCode::Char('/')));
    type_str(&mut app, typed);
    app
}

/// Runs one action the way the popup's run loop does.
fn exec(t: &Tmux, cfg: &Config, action: &Action) -> Outcome {
    actions::exec(
        t,
        cfg,
        &Paths::from_env(),
        Path::new("/nonexistent/taw"),
        action,
        None,
    )
    .expect("exec reports failure as a toast, never as an error")
}

fn session_names(t: &Tmux) -> Vec<String> {
    t.list_sessions()
        .unwrap()
        .into_iter()
        .map(|s| s.session_name)
        .collect()
}

#[test]
fn a_name_typed_at_the_filter_creates_the_session_for_that_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let projects = dir(&tmp, "projects");
    let notes = dir(&tmp, "projects/notes");
    let s = Server::start("main", Path::new(ROOT));
    let cfg = cfg(&projects);

    let mut app = filtering(&s.0, &cfg, &home, "notes");

    // Nothing running matches "notes", so the cursor is on the directory.
    let Some(&Row::Project(at)) = app.current() else {
        panic!("the cursor is on {:?}", app.current());
    };
    assert_eq!(app.projects[at].path, notes.to_str().unwrap());
    assert_eq!(app.projects[at].tag, Some(Tag::ProjectDir));

    let open = app.on_key(key(KeyCode::Enter)).expect("Enter opens it");
    assert_eq!(
        open,
        Action::OpenProject {
            path: notes.to_str().unwrap().to_string()
        }
    );
    let outcome = exec(&s.0, &cfg, &open);

    assert_eq!(session_names(&s.0), ["main", "notes"], "one session made");
    let made = s.0.list_sessions().unwrap();
    assert_eq!(Path::new(&made[1].session_path), notes);
    // Nothing is attached to this server, so the switch that would close the popup has no
    // client to move: the session exists, and the popup stays to say so.
    assert!(!outcome.exit);
    assert_eq!(
        outcome.toast.as_deref(),
        Some("no attached client to switch")
    );
}

#[test]
fn a_directory_with_a_session_is_switched_to_and_no_second_one_is_made() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let projects = dir(&tmp, "projects");
    let notes = dir(&tmp, "projects/notes");
    let s = Server::start("main", &home);
    // The session is running in the directory under another name than its own.
    s.0.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "scratch",
        "-c",
        notes.to_str().unwrap(),
    ])
    .unwrap();
    let cfg = cfg(&projects);

    let mut app = filtering(&s.0, &cfg, &home, "");
    // Walk down to the directory: it is listed all the same, tagged running.
    let at = app
        .projects
        .iter()
        .position(|p| p.path == notes.to_str().unwrap())
        .expect("a directory with a session is listed");
    assert_eq!(app.projects[at].tag, Some(Tag::Running));
    while app.current() != Some(&Row::Project(at)) {
        app.on_key(key(KeyCode::Down));
    }

    let open = app.on_key(key(KeyCode::Enter)).expect("Enter opens it");
    exec(&s.0, &cfg, &open);

    assert_eq!(
        session_names(&s.0),
        ["main", "scratch"],
        "the session already there is the one switched to"
    );
}

/// A directory that is not there any more is a toast and creates nothing: the list is read
/// when the text changes, and a directory can go between that and the Enter.
#[test]
fn a_directory_that_has_gone_is_a_toast_and_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let projects = dir(&tmp, "projects");
    let notes = dir(&tmp, "projects/notes");
    let s = Server::start("main", Path::new(ROOT));
    let cfg = cfg(&projects);
    let mut app = filtering(&s.0, &cfg, &home, "notes");
    std::fs::remove_dir_all(&notes).unwrap();

    let open = app.on_key(key(KeyCode::Enter)).expect("Enter opens it");
    let outcome = exec(&s.0, &cfg, &open);

    assert_eq!(session_names(&s.0), ["main"]);
    assert!(!outcome.exit);
    assert!(
        outcome
            .toast
            .as_deref()
            .is_some_and(|t| t.starts_with("no such directory")),
        "{:?}",
        outcome.toast
    );
}
