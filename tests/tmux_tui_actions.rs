//! The popup's side effects, against a real tmux server.
//!
//! `tui::actions::exec` is unit-tested by pointing it at a `tmux` binary that is not
//! there, which pins the argv each arm builds but never what tmux does with it. This is
//! the other half: the arms that are more than one command — `OpenProject`, which runs
//! `session::create::open_or_create` and then the popup's own `switch-client`, and
//! `NextUp`, which asks `notify::open_target` which agent is next — checked by what
//! the server looks like afterwards.
//!
//! Every test runs on its own `-L` socket and tears it down on drop; the user's real tmux
//! server is never addressed.

mod clients;

mod common;

use common::shell_env::ShellHome;
use std::path::{Path, PathBuf};

use clients::{Client, sessions_of};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Text;
use taw::config::Config;
use taw::paths::Paths;
use taw::tmux::{PaneInfo, Tmux};
use taw::tui::actions::{self, Action, Outcome};
use taw::tui::app::App;
use taw::tui::data::{self, GitWorker};
use taw::tui::{follow_preview, theme};

/// A throwaway tmux server on its own `-L` socket, torn down on drop. Same shape as the
/// guard in `tests/tmux_tui_data.rs`, including the per-test counter that keeps
/// concurrently running tests on separate sockets.
struct Server(Tmux, #[allow(dead_code)] ShellHome);

impl Server {
    /// One session, `name`, whose directory is `at` — the session a later `OpenProject`
    /// has to either find, ignore, or collide with.
    fn start(name: &str, at: &Path) -> Server {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let socket = format!("taw-test-act-{}-{n}", std::process::id());
        let t = Tmux::with_socket_name(&socket);
        let home = ShellHome::new();
        // `-f /dev/null` so the host's own tmux.conf (base-index, hooks, TPM autoload)
        // cannot change what this test observes, and `/bin/sh` so no login shell rc file
        // runs in the panes.
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

/// The session the one attached client is looking at.
fn client_session(t: &Tmux) -> String {
    let clients = t.list_clients().unwrap();
    assert_eq!(clients.len(), 1, "one attached client: {clients:?}");
    clients[0].session_name.clone()
}

/// The config every test here runs with: `[sessions] zoxide` off.
///
/// It defaults to *on*, and `open_or_create` then runs the real `zoxide add` on whatever
/// directory it just opened — which would write a temp path into the developer's own
/// zoxide database every time this file ran.
fn cfg() -> Config {
    let mut cfg = Config::default();
    cfg.sessions.zoxide = false;
    cfg
}

/// Runs one action the way the popup's run loop does. `paths` and `taw_bin` reach only the
/// worktree arms, which this file does not exercise (`tests/tmux_worktree.rs` does).
fn exec(t: &Tmux, cfg: &Config, action: &Action) -> Outcome {
    exec_on(t, cfg, action, None)
}

/// [`exec`] for a popup whose launcher named the client it was opened on.
fn exec_on(t: &Tmux, cfg: &Config, action: &Action, client: Option<&str>) -> Outcome {
    actions::exec(
        t,
        cfg,
        &Paths::from_env(),
        Path::new("/nonexistent/taw"),
        action,
        client,
    )
    .expect("exec reports failure as a toast, never as an error")
}

/// `mkdir -p` under `tmp`, canonicalised — on macOS a temp directory is reached through a
/// `/var` symlink, and tmux reports the resolved path.
fn dir(tmp: &tempfile::TempDir, rel: &str) -> PathBuf {
    let path = tmp.path().join(rel);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::canonicalize(&path).unwrap()
}

/// The session running in `at`, if any.
fn session_at(t: &Tmux, at: &Path) -> Option<String> {
    t.list_sessions()
        .unwrap()
        .into_iter()
        .find(|s| Path::new(&s.session_path) == at)
        .map(|s| s.session_name)
}

#[test]
fn open_project_creates_a_session_in_that_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let project = dir(&tmp, "projects/notes");
    let s = Server::start("main", &home);

    let outcome = exec(
        &s.0,
        &cfg(),
        &Action::OpenProject {
            path: project.to_str().unwrap().to_string(),
        },
    );

    assert_eq!(
        session_at(&s.0, &project).as_deref(),
        Some("notes"),
        "the directory's own name, in `[sessions] name = \"directory\"`"
    );
    // Nothing is attached to this server, so the switch that would close the popup has no
    // client to move: the session still exists, and the popup stays open to say so.
    assert!(!outcome.exit);
    assert_eq!(
        outcome.toast.as_deref(),
        Some("no attached client to switch")
    );
}

#[test]
fn open_project_switches_to_the_session_already_running_there_instead_of_a_second_one() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let project = dir(&tmp, "projects/notes");
    let s = Server::start("main", &home);
    s.0.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "scratch",
        "-c",
        project.to_str().unwrap(),
    ])
    .unwrap();

    exec(
        &s.0,
        &cfg(),
        &Action::OpenProject {
            path: project.to_str().unwrap().to_string(),
        },
    );

    let names: Vec<String> =
        s.0.list_sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.session_name)
            .collect();
    assert_eq!(
        names,
        ["main", "scratch"],
        "the session already in that directory is switched to under its own name"
    );
}

#[test]
fn open_project_reports_the_name_it_had_to_take_to_dodge_a_collision() {
    let tmp = tempfile::tempdir().unwrap();
    let running = dir(&tmp, "work/api");
    let wanted = dir(&tmp, "oss/api");
    // The running session is called `api`, which is the name `~/oss/api` wants too.
    let s = Server::start("api", &running);

    let outcome = exec(
        &s.0,
        &cfg(),
        &Action::OpenProject {
            path: wanted.to_str().unwrap().to_string(),
        },
    );

    assert_eq!(
        session_at(&s.0, &wanted).as_deref(),
        Some("oss/api"),
        "the shortest distinguishing suffix"
    );
    // The switch had no client to move, so the popup stayed open — and its toast names
    // the session it made as well as tmux's reason for still being here.
    let toast = outcome.toast.expect("a failed switch toasts");
    assert!(
        toast.starts_with("session oss/api (\"api\" is taken by ")
            && toast.ends_with("(no attached client to switch)"),
        "{toast}"
    );
}

/// The home directory's session is called `~`, which tmux reads as the marked pane unless
/// it is asked for by exact name. Enter on the home directory in the picker said `no marked
/// target` and left the popup open, with the session made and nobody in it.
#[test]
fn open_project_goes_to_the_home_directorys_session_by_its_exact_name() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let elsewhere = dir(&tmp, "elsewhere");
    let s = Server::start("main", &elsewhere);
    // Running in the home directory, as taw names the session of that directory.
    s.0.run_ok(&["new-session", "-d", "-s", "~", "-c", home.to_str().unwrap()])
        .unwrap();
    let _client = Client::attach(&s.0, "main");

    let outcome = exec(
        &s.0,
        &cfg(),
        &Action::OpenProject {
            path: home.to_str().unwrap().to_string(),
        },
    );

    assert_eq!(outcome.toast, None, "{outcome:?}");
    assert!(outcome.exit, "the popup closes once the client has moved");
    assert_eq!(client_session(&s.0), "~");
}

/// With a pane marked in another session, a bare `~` is that pane, and the popup's client
/// went to the marked pane's session, with nothing to say about it.
#[test]
fn open_project_does_not_take_the_home_directory_for_the_marked_pane() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let elsewhere = dir(&tmp, "elsewhere");
    let s = Server::start("main", &elsewhere);
    s.0.run_ok(&["new-session", "-d", "-s", "~", "-c", home.to_str().unwrap()])
        .unwrap();
    s.0.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "marked",
        "-c",
        elsewhere.to_str().unwrap(),
    ])
    .unwrap();
    s.0.run_ok(&["select-pane", "-m", "-t", "marked"]).unwrap();
    let _client = Client::attach(&s.0, "main");

    let outcome = exec(
        &s.0,
        &cfg(),
        &Action::OpenProject {
            path: home.to_str().unwrap().to_string(),
        },
    );

    assert!(outcome.exit, "{outcome:?}");
    assert_eq!(client_session(&s.0), "~");
}

/// A session on two screens: the popup's Enter moves the client the popup is on, which its
/// launcher named, and no other, whichever of the two the server lists first, not
/// the first the server lists, which is the other screen when the popup is on the second.
#[test]
fn open_project_moves_the_client_the_popup_is_on_and_no_other() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let project = dir(&tmp, "projects/two-screens");
    let s = Server::start("main", &home);
    let first = Client::attach_as(&s.0, "main", "-first");
    let second = Client::attach_as(&s.0, "main", "-second");

    for (on, other) in [(&second, &first), (&first, &second)] {
        for client in [&first, &second] {
            s.0.run_ok(&["switch-client", "-c", &client.name, "-t", "main"])
                .unwrap();
        }

        let outcome = exec_on(
            &s.0,
            &cfg(),
            &Action::OpenProject {
                path: project.to_str().unwrap().to_string(),
            },
            Some(&on.name),
        );

        assert!(outcome.exit && outcome.toast.is_none(), "{outcome:?}");
        assert_eq!(
            sessions_of(&s.0, &[on, other]),
            ["two-screens", "main"],
            "the popup is on {}",
            on.name
        );
    }
}

/// The tree's Enter on a session, which hands over its id, and on a pane, go the same way.
#[test]
fn a_jump_moves_the_client_the_popup_is_on_and_no_other() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let s = Server::start("main", &home);
    s.0.run_ok(&["new-session", "-d", "-s", "other", "/bin/sh"])
        .unwrap();
    let other_id = s.0.display("other", "#{session_id}").unwrap();
    let other_pane = s.0.display("other", "#{pane_id}").unwrap();
    let other_window = s.0.display("other", "#{window_id}").unwrap();
    let first = Client::attach_as(&s.0, "main", "-first");
    let second = Client::attach_as(&s.0, "main", "-second");

    for action in [
        Action::SwitchWorkspace {
            session: other_id.clone(),
        },
        Action::Jump {
            session: other_id.clone(),
            window: other_window,
            pane: other_pane,
        },
    ] {
        for client in [&first, &second] {
            s.0.run_ok(&["switch-client", "-c", &client.name, "-t", "main"])
                .unwrap();
        }

        let outcome = exec_on(&s.0, &cfg(), &action, Some(&second.name));

        assert!(
            outcome.exit && outcome.toast.is_none(),
            "{action:?}: {outcome:?}"
        );
        assert_eq!(
            sessions_of(&s.0, &[&first, &second]),
            ["main", "other"],
            "{action:?}"
        );
    }
}

#[test]
fn open_project_refuses_a_directory_that_is_not_there() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let s = Server::start("main", &home);
    let missing = tmp.path().join("nope");

    let outcome = exec(
        &s.0,
        &cfg(),
        &Action::OpenProject {
            path: missing.to_str().unwrap().to_string(),
        },
    );

    assert!(!outcome.exit);
    assert_eq!(
        outcome.toast,
        Some(format!("no such directory: {}", missing.display()))
    );
    assert_eq!(
        s.0.list_sessions().unwrap().len(),
        1,
        "a refused path creates nothing"
    );
}

/// [`cfg`] with a pre-create hook, set the way a config file sets it.
fn cfg_with_pre_hook(hook: &Path) -> Config {
    cfg_with_pre_hook_text(hook, "")
}

/// [`cfg_with_pre_hook`] with `[sessions] hook_timeout_seconds` set to `seconds`.
fn cfg_with_pre_hook_and_limit(hook: &Path, seconds: &str) -> Config {
    cfg_with_pre_hook_text(hook, &format!("hook_timeout_seconds = {seconds}\n"))
}

fn cfg_with_pre_hook_text(hook: &Path, more: &str) -> Config {
    let text = format!(
        "[sessions]\nzoxide = false\npre_create_hook = {}\n{more}",
        toml::Value::String(hook.to_str().unwrap().to_string())
    );
    toml::from_str(&text).expect("a config file")
}

/// An executable `/bin/sh` script at `path`.
fn hook_script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    crate::common::make_executable(path);
}

/// The pre-create hook runs for a directory that already has a session, and a hook that
/// refuses stops the open there too: the popup stays open and says why, in the hook's own
/// words behind the name of the hook, and nobody is switched anywhere.
#[test]
fn open_project_stops_when_the_pre_create_hook_refuses_even_for_a_running_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let project = dir(&tmp, "projects/notes");
    let s = Server::start("main", &home);
    s.0.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "scratch",
        "-c",
        project.to_str().unwrap(),
    ])
    .unwrap();
    let hook = tmp.path().join("pre.sh");
    hook_script(&hook, "echo 'not on a Friday' >&2\nexit 1\n");

    let outcome = exec(
        &s.0,
        &cfg_with_pre_hook(&hook),
        &Action::OpenProject {
            path: project.to_str().unwrap().to_string(),
        },
    );

    assert!(!outcome.exit);
    assert_eq!(
        outcome.toast.as_deref(),
        Some("pre-create hook: not on a Friday")
    );
    assert_eq!(s.0.list_sessions().unwrap().len(), 2, "nothing was created");
}

/// A script says its verdict last, so that is the line the toast carries: what came before
/// it is the hook thinking aloud, and a blank line at the end says nothing.
#[test]
fn open_project_shows_the_last_line_a_refusing_pre_create_hook_wrote() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let project = dir(&tmp, "projects/notes");
    let s = Server::start("main", &home);
    let hook = tmp.path().join("pre.sh");
    hook_script(
        &hook,
        "echo 'looking at the calendar' >&2\necho 'not on a Friday' >&2\necho >&2\nexit 1\n",
    );

    let outcome = exec(
        &s.0,
        &cfg_with_pre_hook(&hook),
        &Action::OpenProject {
            path: project.to_str().unwrap().to_string(),
        },
    );

    assert!(!outcome.exit);
    assert_eq!(
        outcome.toast.as_deref(),
        Some("pre-create hook: not on a Friday")
    );
    assert_eq!(s.0.list_sessions().unwrap().len(), 1, "nothing was created");
}

/// A hook that refuses without a word still gets named, and its exit status is all there is
/// to say about it. The toast has no room for the path of the script.
#[test]
fn open_project_names_the_exit_status_when_a_refusing_pre_create_hook_said_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let project = dir(&tmp, "projects/notes");
    let s = Server::start("main", &home);
    let hook = tmp.path().join("pre.sh");
    hook_script(&hook, "exit 3\n");

    let outcome = exec(
        &s.0,
        &cfg_with_pre_hook(&hook),
        &Action::OpenProject {
            path: project.to_str().unwrap().to_string(),
        },
    );

    assert!(!outcome.exit);
    assert_eq!(
        outcome.toast.as_deref(),
        Some("pre-create hook refused (exit status 3)")
    );
    assert_eq!(s.0.list_sessions().unwrap().len(), 1, "nothing was created");
}

/// A pre-create hook that runs past `[sessions] hook_timeout_seconds` is stopped, so the popup
/// does not sit unanswering for as long as the hook likes: it stays open and says the hook took
/// too long, and nothing is created.
#[test]
fn open_project_says_so_when_the_pre_create_hook_runs_past_the_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let project = dir(&tmp, "projects/notes");
    let s = Server::start("main", &home);
    let hook = tmp.path().join("pre.sh");
    hook_script(&hook, "sleep 30\n");

    let started = std::time::Instant::now();
    let outcome = exec(
        &s.0,
        &cfg_with_pre_hook_and_limit(&hook, "1"),
        &Action::OpenProject {
            path: project.to_str().unwrap().to_string(),
        },
    );

    assert!(!outcome.exit);
    assert_eq!(
        outcome.toast.as_deref(),
        Some("pre-create hook took longer than 1 s")
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(9),
        "the popup waited {:?} with a limit of 1 s",
        started.elapsed()
    );
    assert_eq!(s.0.list_sessions().unwrap().len(), 1, "nothing was created");
}

/// A directory the pre-create hook prints that is not there stops the open with a message
/// that says the hook chose it; no session is made.
#[test]
fn open_project_says_so_when_the_pre_create_hook_names_a_directory_that_is_not_there() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let project = dir(&tmp, "projects/notes");
    let s = Server::start("main", &home);
    let missing = tmp.path().join("nope");
    let hook = tmp.path().join("pre.sh");
    hook_script(
        &hook,
        &format!("echo custom\necho '{}'\n", missing.display()),
    );

    let outcome = exec(
        &s.0,
        &cfg_with_pre_hook(&hook),
        &Action::OpenProject {
            path: project.to_str().unwrap().to_string(),
        },
    );

    assert!(!outcome.exit);
    assert_eq!(
        outcome.toast,
        Some(format!(
            "no such directory: {} (the pre-create hook chose it)",
            missing.display()
        ))
    );
    assert_eq!(s.0.list_sessions().unwrap().len(), 1, "nothing was created");
}

/// `prefix+O` on a server with no agent panes at all: next up has no group to walk.
#[test]
fn next_up_says_so_when_there_are_no_agents() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let s = Server::start("main", &home);

    let outcome = exec(&s.0, &cfg(), &Action::NextUp { own: None });

    assert!(!outcome.exit, "there is nowhere to go, so the popup stays");
    assert_eq!(outcome.toast.as_deref(), Some(actions::NO_AGENTS));
}

/// `Action::NextUp` carries the pane the popup was opened from — `App` fills it in
/// from the snapshot, which got it from `taw ui --from`, resolved by `taw ui open` before
/// the popup existed. `exec` hands it to `next_up` as the pane to walk from
/// (`notify::open_target`'s `after`): two agent panes, both blocked, in tmux order; naming
/// the first, the popup walks past it to the second.
#[test]
fn next_up_walks_from_the_popups_own_pane() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let s = Server::start("main", &home);
    let (first, second) = two_blocked_agents(&s.0);

    let outcome = exec(
        &s.0,
        &cfg(),
        &Action::NextUp {
            own: Some(first.pane_id.clone()),
        },
    );

    // No client is attached to this throwaway server, so the client-switch half of a
    // successful jump fails and the popup stays open with a toast — open_target's own
    // select-window/select-pane already ran before that, which is the half this test is
    // actually about.
    assert!(
        !outcome.exit,
        "no attached client to switch, so the popup stays open: {outcome:?}"
    );
    assert_eq!(
        s.0.display("main", "#{pane_id}").unwrap(),
        second.pane_id,
        "walked from the popup's own pane (first) to the next hot one (second)"
    );
}

/// No origin — `taw ui` run where neither `--from` nor `$TMUX_PANE` named a pane, or a
/// `--from` naming one that has since gone. The popup invents nothing: `notify` asks tmux
/// for its own current pane and walks from there, which here is the second window, so the
/// walk wraps to the first.
#[test]
fn next_up_without_an_origin_walks_from_tmuxs_current_pane() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let s = Server::start("main", &home);
    let (first, second) = two_blocked_agents(&s.0);
    // Pin the premise rather than leaning on `new-window` having selected it.
    s.0.run_ok(&["select-window", "-t", &second.window_id])
        .unwrap();

    let outcome = exec(&s.0, &cfg(), &Action::NextUp { own: None });

    assert!(
        !outcome.exit,
        "no attached client to switch, so the popup stays open: {outcome:?}"
    );
    assert_eq!(
        s.0.display("main", "#{pane_id}").unwrap(),
        first.pane_id,
        "walked from tmux's own current pane (second) and wrapped to the first"
    );
}

/// A second window, and a blocked claude in each pane: two agents that need the user, in
/// tmux order (first window first).
fn two_blocked_agents(t: &Tmux) -> (PaneInfo, PaneInfo) {
    t.run_ok(&["new-window", "-t", "main", "/bin/sh"]).unwrap();
    let mut panes = t.list_panes().unwrap();
    panes.sort_by_key(|p| p.window_index);
    let (first, second) = (panes[0].clone(), panes[1].clone());
    for pane in [&first, &second] {
        t.set_pane_option(&pane.pane_id, "@agent_kind", "claude")
            .unwrap();
        t.set_pane_option(&pane.pane_id, "@agent_state", "blocked")
            .unwrap();
    }
    (first, second)
}

/// `x` in the popup, against a real server: `[hooks] pre_close` is asked before tmux kills
/// anything, and a refusal is the toast, with the session and the pane still there.
#[test]
fn a_pre_close_hook_that_refuses_keeps_the_session_and_the_pane_open() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let other = dir(&tmp, "other");
    let s = Server::start("main", &home);
    s.0.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "other",
        "-c",
        other.to_str().unwrap(),
    ])
    .unwrap();
    s.0.run_ok(&["split-window", "-d", "-t", "other"]).unwrap();
    let session = s.0.display("other", "#{session_id}").unwrap();
    let pane =
        s.0.list_panes()
            .unwrap()
            .into_iter()
            .find(|p| p.session_name == "other")
            .unwrap()
            .pane_id;
    let mut cfg = cfg();
    cfg.hooks = toml::from_str::<Config>(
        "[hooks]\npre_close = \"echo 'agent is still working' >&2; exit 1\"\n",
    )
    .unwrap()
    .hooks;

    for action in [
        Action::KillWorkspace {
            session: session.clone(),
        },
        Action::KillPane { pane: pane.clone() },
    ] {
        let outcome = exec(&s.0, &cfg, &action);

        assert_eq!(
            outcome.toast.as_deref(),
            Some("pre-close hook: agent is still working"),
            "{action:?}"
        );
        assert!(!outcome.exit);
    }
    let sessions: Vec<String> =
        s.0.list_sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.session_name)
            .collect();
    assert!(sessions.contains(&"other".to_string()), "{sessions:?}");
    assert!(
        s.0.list_panes().unwrap().iter().any(|p| p.pane_id == pane),
        "the pane is gone"
    );

    // A hook that agrees lets the same actions through.
    cfg.hooks = toml::from_str::<Config>("[hooks]\npre_close = \"exit 0\"\n")
        .unwrap()
        .hooks;
    exec(&s.0, &cfg, &Action::KillPane { pane: pane.clone() });
    assert!(!s.0.list_panes().unwrap().iter().any(|p| p.pane_id == pane));
    exec(&s.0, &cfg, &Action::KillWorkspace { session });
    assert!(
        !s.0.list_sessions()
            .unwrap()
            .iter()
            .any(|s| s.session_name == "other")
    );
}

/// Sets `name` in this process while it lives, and takes it out again after.
///
/// The popup runs `[hooks]` commands from the process it is, so what the hook inherits beside
/// tmux's own environment is this process's. Only one test here does this, under a name no other
/// code reads, and every child another test starts is started under `std`'s own environment lock,
/// which `set_var` takes too.
struct ProcessVar(&'static str);

impl ProcessVar {
    fn set(name: &'static str, value: &str) -> ProcessVar {
        // SAFETY: see the type. Nothing in this binary reads the variable but the hook under test.
        unsafe { std::env::set_var(name, value) };
        ProcessVar(name)
    }
}

impl Drop for ProcessVar {
    fn drop(&mut self) {
        // SAFETY: as in `set`.
        unsafe { std::env::remove_var(self.0) };
    }
}

/// `x` in the popup: the `[hooks] pre_close` it asks is started with what tmux would give a new
/// pane of the session being closed, as a hook a watcher or a `taw` command starts is. The popup
/// is a process with an environment of its own, from wherever the client was started, so a hook
/// that reached an ssh agent through it would reach the popup's, and not the one the session was
/// last attached with. A variable the session has removed is taken out of what the popup has, and
/// one the session says nothing of is kept.
#[test]
fn a_pre_close_hook_the_popup_asks_gets_the_environment_of_the_session_it_is_about() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let s = Server::start("main", &home);
    for name in ["other", "third"] {
        s.0.run_ok(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-c",
            dir(&tmp, name).to_str().unwrap(),
        ])
        .unwrap();
    }
    s.0.run_ok(&["set-environment", "-g", "SSH_AUTH_SOCK", "global-sock"])
        .unwrap();
    s.0.run_ok(&[
        "set-environment",
        "-t",
        "other",
        "SSH_AUTH_SOCK",
        "sock-of-other",
    ])
    .unwrap();
    s.0.run_ok(&["set-environment", "-t", "other", "-r", "STALE_IN_POPUP"])
        .unwrap();
    s.0.run_ok(&[
        "set-environment",
        "-t",
        "third",
        "SSH_AUTH_SOCK",
        "sock-of-third",
    ])
    .unwrap();
    let asked = tmp.path().join("asked.txt");
    let mut cfg = cfg();
    cfg.hooks = toml::from_str::<Config>(&format!(
        "[hooks]\npre_close = {}\n",
        toml::Value::String(format!(
            "printf '%s|%s\\n' \"$SSH_AUTH_SOCK\" \"${{STALE_IN_POPUP-unset}}\" >> '{}'",
            asked.display()
        ))
    ))
    .unwrap()
    .hooks;
    let _popup_has = ProcessVar::set("STALE_IN_POPUP", "the-popups-own");
    let id_of = |name: &str| s.0.display(name, "#{session_id}").unwrap();

    for name in ["other", "third"] {
        let outcome = exec(
            &s.0,
            &cfg,
            &Action::KillWorkspace {
                session: id_of(name),
            },
        );
        assert_eq!(outcome.toast, None, "{name}");
    }

    assert_eq!(
        std::fs::read_to_string(&asked).unwrap(),
        "sock-of-other|unset\nsock-of-third|the-popups-own\n"
    );
}

/// A preview's text as one string, a line to a row.
fn plain(text: &Text<'_>) -> String {
    text.lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Types `line` into `session`'s pane and waits until the pane shows it, so the capture a
/// test then takes has the text on screen.
fn say(t: &Tmux, session: &str, line: &str) {
    let target = format!("={session}:");
    t.run_ok(&["send-keys", "-t", &target, "-l", &format!("echo {line}")])
        .unwrap();
    t.run_ok(&["send-keys", "-t", &target, "Enter"]).unwrap();
    let shown = common::wait_for(
        || {
            t.capture_pane(&target, false)
                .is_ok_and(|screen| screen.lines().any(|row| row == line))
        },
        std::time::Duration::from_secs(10),
    );
    assert!(shown, "{session}'s pane never printed {line}");
}

/// Moves the popup's cursor with Down until the preview would show `pane`.
fn move_cursor_to(app: &mut App, pane: &str) {
    for _ in 0..20 {
        if app.preview_pane().as_deref() == Some(pane) {
            return;
        }
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    panic!("the cursor never reached a row previewing {pane}");
}

#[test]
fn the_preview_follows_the_cursor_to_another_pane_before_the_next_tick() {
    let tmp = tempfile::tempdir().unwrap();
    let home = dir(&tmp, "home");
    let s = Server::start("alpha", &home);
    s.0.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "beta",
        "-c",
        home.to_str().unwrap(),
    ])
    .unwrap();
    say(&s.0, "alpha", "text-of-the-first-pane");
    say(&s.0, "beta", "text-of-the-second-pane");
    let first = s.0.display("=alpha:", "#{pane_id}").unwrap();
    let second = s.0.display("=beta:", "#{pane_id}").unwrap();

    let cfg = cfg();
    let git = GitWorker::new(std::time::Duration::from_secs(2));
    let snap = data::build(&s.0, &cfg, &git, None, None).unwrap();
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let mut app = App::new(&cfg, theme, String::new(), snap);
    move_cursor_to(&mut app, &first);
    follow_preview(&s.0, &mut app);
    assert_eq!(app.preview_shows(), Some(first.as_str()));
    assert!(plain(app.preview.as_ref().unwrap()).contains("text-of-the-first-pane"));

    move_cursor_to(&mut app, &second);
    follow_preview(&s.0, &mut app);

    assert_eq!(
        app.preview_shows(),
        Some(second.as_str()),
        "the title's pane"
    );
    let body = plain(app.preview.as_ref().expect("a capture of the second pane"));
    assert!(body.contains("text-of-the-second-pane"), "{body}");
    assert!(!body.contains("text-of-the-first-pane"), "{body}");
}

/// A popup on `main`, a server of `main`, `other`, `third` (in that order) and a real client that went
/// to `third` and came back, so tmux's last session is `third` and the first other session is `other`.
fn popup_on_main(s: &Server, client: &Client) -> (Config, App<'static>) {
    for to in ["third", "main"] {
        s.0.run_ok(&["switch-client", "-c", &client.name, "-t", to])
            .unwrap();
    }
    let cfg: &'static Config = Box::leak(Box::new(cfg()));
    let git = GitWorker::new(std::time::Duration::from_secs(2));
    let snap = data::build(&s.0, cfg, &git, None, Some(&client.name)).unwrap();
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    (cfg.clone(), App::new(cfg, theme, String::new(), snap))
}

fn three_sessions(tmp: &tempfile::TempDir) -> Server {
    let home = dir(tmp, "home");
    let s = Server::start("main", &home);
    for name in ["other", "third"] {
        s.0.run_ok(&["new-session", "-d", "-s", name, "/bin/sh"])
            .unwrap();
    }
    s
}

/// `x` on the session the popup is on asks where you go, and `y` moves the client there first, so it
/// is not dropped out of tmux: tmux's last session, not the first one in its list.
#[test]
fn closing_your_own_session_from_the_popup_takes_you_to_the_last_session() {
    let tmp = tempfile::tempdir().unwrap();
    let s = three_sessions(&tmp);
    let client = Client::attach(&s.0, "main");
    let (cfg, mut app) = popup_on_main(&s, &client);
    // The cursor on main's own header.
    app.sel = app
        .rows
        .rows
        .iter()
        .position(|row| {
            matches!(row, taw::tui::tree::Row::Session(space)
                if app.snap.spaces[*space].session_name == "main")
        })
        .expect("main has a header");

    app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    let said = match &app.overlay {
        taw::tui::overlays::Overlay::Confirm(confirm) => confirm.message.clone(),
        other => panic!("expected the confirmation, got {other:?}"),
    };
    assert_eq!(said, "kill-session main? You go to third.");
    let action = app
        .on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
        .expect("y runs the close");

    exec_on(&s.0, &cfg, &action, Some(&client.name));

    assert_eq!(sessions_of(&s.0, &[&client]), ["third"]);
    let left: Vec<String> =
        s.0.list_sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.session_name)
            .collect();
    assert_eq!(left, ["other", "third"]);
}

/// The last pane of your own session closes the session, and takes you to the last session the same way.
#[test]
fn closing_the_last_pane_of_your_own_session_from_the_popup_takes_you_to_the_last_session() {
    let tmp = tempfile::tempdir().unwrap();
    let s = three_sessions(&tmp);
    let client = Client::attach(&s.0, "main");
    let (cfg, mut app) = popup_on_main(&s, &client);
    let pane = s.0.display("=main:", "#{pane_id}").unwrap();
    move_cursor_to(&mut app, &pane);

    app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    let said = match &app.overlay {
        taw::tui::overlays::Overlay::Confirm(confirm) => confirm.message.clone(),
        other => panic!("expected the confirmation, got {other:?}"),
    };
    assert!(said.ends_with(" You go to third."), "{said}");
    let action = app
        .on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
        .expect("y runs the close");

    exec_on(&s.0, &cfg, &action, Some(&client.name));

    assert_eq!(sessions_of(&s.0, &[&client]), ["third"]);
}
