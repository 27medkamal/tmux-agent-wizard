//! A test server takes its socket file with it.
//!
//! tmux leaves the socket file of a server that has ended where it was (in `tmux-<uid>` under
//! `$TMUX_TMPDIR`, or `/tmp`, which is `/private/tmp` on a Mac). Every server a test started, and every
//! harness run, added one and nothing took it away, so a directory of sockets could grow
//! to more than a hundred thousand dead ones.
//!
//! Each helper that starts a server removes its file once the server is gone, by the exact path the
//! server said it listens on (`#{socket_path}`) and never by a pattern. These tests start a server
//! through each of the helpers, look for the file while it is up (a test that looked for nothing
//! would pass whatever the helper did), and look again once it is dropped.

mod clients;
mod common;

use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::harness::harness_up;
use common::shell_env::{ShellHome, take_the_socket_away};
use common::{
    Server, end_what_a_server_left, make_executable, plugin_script, taw_bin, write_script,
};
use taw::tmux::{Socket, Tmux};

/// The path of the file the server `t` listens on, as the server says it.
fn socket_file_of(t: &Tmux) -> PathBuf {
    let path = t
        .run(&["display-message", "-p", "#{socket_path}"])
        .unwrap_or_else(|err| panic!("the server does not say its socket: {err:#}"));
    PathBuf::from(path)
}

fn is_a_socket_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_socket())
}

fn assert_up(file: &Path, what: &str) {
    assert!(
        is_a_socket_file(file),
        "{what}: a server that is up has a socket file, and there is none at {file:?}"
    );
}

fn assert_gone(file: &Path, what: &str) {
    assert!(
        !file.exists(),
        "{what} left its socket file behind: {file:?}"
    );
}

/// A server started through the helper the way the fixtures of the test files start theirs: a
/// `Tmux`, the home that goes with it, and a kill-server on drop.
struct Fixture(Tmux, #[allow(dead_code)] ShellHome);

impl Fixture {
    fn start() -> Fixture {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmux =
            Tmux::with_socket_name(&format!("taw-test-socket-files-{}-{n}", std::process::id()));
        let home = ShellHome::new();
        home.start_server(
            &tmux,
            &["-d", "-s", "one", "-x", "80", "-y", "24", "/bin/sh"],
        );
        Fixture(tmux, home)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.0.run_ok(&["kill-server"]);
    }
}

#[test]
fn a_dropped_server_takes_its_socket_file_with_it() {
    let server = Server::start();
    let file = socket_file_of(&Tmux::with_socket_name(server.socket()));
    assert_up(&file, "common::Server");

    drop(server);

    assert_gone(&file, "a dropped common::Server");
}

/// Nor does a dropped `Server` leave any of its directories behind. The plugin's hooks run `taw` when
/// a session or a pane closes (an autosave, which first marks itself wanted in the state directory),
/// and a test that drops its server right after it closed one had the directories gone before that
/// `taw` got to its mark, which made them again: a whole run of the tests left a couple of dozen of
/// them so. Everything a server set off has ended before they go, so none comes back in the seconds
/// after. The plugin's `taw` here is a wrapper that waits a second before it runs, so that the mark
/// comes after the drop for certain, unless the drop ended it first.
#[test]
fn a_dropped_server_leaves_none_of_its_directories_behind() {
    drop_a_server_after_a_session_closed(false);
}

/// The same when the test ended its server itself before the drop, which then cannot ask the server
/// for its pid: the group of the server is known from its start.
#[test]
fn a_server_a_test_ended_itself_leaves_none_of_its_directories_behind() {
    drop_a_server_after_a_session_closed(true);
}

/// The pid a Server read at its start stands for its group when the server no longer answers (a
/// test ended it itself). But a pid is given to another process once its own has gone, and a
/// process that still has it after the server has had its time to go is not the server: its group
/// is not the server's, and is not signalled. A sleep in a group of its own stands for one here.
#[test]
fn the_group_of_a_process_that_has_the_pid_a_server_started_with_is_left_alone() {
    let mut other = Command::new("sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = other.id() as libc::pid_t;

    end_what_a_server_left(None, Some(pid));

    let still_there = matches!(other.try_wait(), Ok(None));
    let _ = other.kill();
    let _ = other.wait();
    assert!(
        still_there,
        "the group of a process that has the pid a server started with was signalled"
    );
}

/// Starts a Server with the plugin loaded (its `taw` a wrapper that waits a second before it runs),
/// closes a session, ends the server first when `end_it` says so, drops the Server, and watches its
/// four directories for three seconds after: none may come back.
fn drop_a_server_after_a_session_closed(end_it: bool) {
    let server = Server::start();
    let bin = tempfile::tempdir().unwrap();
    let slow_taw = bin.path().join("taw");
    write_script(
        &slow_taw,
        &format!("sleep 1\nexec '{}' \"$@\"\n", taw_bin()),
    );
    server.tmux_ok(&[
        "run-shell",
        &format!(
            "TAW_BIN='{}' bash '{}'",
            slow_taw.display(),
            plugin_script().display()
        ),
    ]);
    server.tmux_ok(&["new-session", "-d", "-s", "two", "/bin/sh"]);
    server.tmux_ok(&["kill-session", "-t", "two"]);
    if end_it {
        server.tmux_ok(&["kill-server"]);
    }
    let dirs: Vec<PathBuf> = [
        server.config_dir(),
        server.state_dir(),
        server.work_dir(),
        server.home_dir(),
    ]
    .iter()
    .map(|dir| dir.to_path_buf())
    .collect();

    drop(server);

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let left: Vec<&PathBuf> = dirs.iter().filter(|dir| dir.exists()).collect();
        assert!(
            left.is_empty(),
            "a dropped Server left directories behind, or something it set off made them again: \
             {left:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_server_of_the_helper_and_the_second_server_of_a_client_take_theirs_with_them() {
    let fixture = Fixture::start();
    let terminal_of = Server::start();
    let inner = Tmux::with_socket_name(terminal_of.socket());
    let client = clients::Client::attach(&inner, "one");
    let files = [
        ("the helper's server", socket_file_of(&fixture.0)),
        (
            "the second server of a Client",
            socket_file_of(client.outer()),
        ),
    ];
    for (what, file) in &files {
        assert_up(file, what);
    }

    drop(client);
    drop(fixture);
    drop(terminal_of);

    for (what, file) in &files {
        assert_gone(file, what);
    }
}

#[test]
fn the_harness_takes_its_socket_files_down_with_its_servers() {
    let tmp = tempfile::tempdir().unwrap();
    let mut harness = harness_up(tmp.path(), &[]);
    let files = [
        (
            "the inner server of the harness",
            socket_file_of(&harness.inner),
        ),
        (
            "the outer server of the harness",
            socket_file_of(&harness.outer),
        ),
    ];
    for (what, file) in &files {
        assert_up(file, what);
    }

    harness.down();

    for (what, file) in &files {
        assert_gone(file, what);
    }
}

/// What is taken away is a socket file of a test server's, the one the `Tmux` it is given asks for:
/// another name, a file that is no socket, and the file called `default` (the server of whoever runs
/// the tests) stay. Nothing here has a server behind it.
#[test]
fn only_the_socket_file_of_a_test_server_is_taken_away() {
    let dir = tempfile::tempdir().unwrap();
    let id = std::process::id();
    let bind = |name: &str| {
        let path = dir.path().join(name);
        (UnixListener::bind(&path).unwrap(), path)
    };
    let (_default, default) = bind("default");
    let (_other, other) = bind(&format!("taw-other-{id}"));
    let (_ours, ours) = bind(&format!("taw-ours-{id}"));
    let plain = dir.path().join(format!("taw-plain-{id}"));
    std::fs::write(&plain, "").unwrap();
    let asking_for = |name: &str| Tmux::with_socket_name(&format!("{name}-{id}"));

    take_the_socket_away(&asking_for("taw-asked"), &default);
    take_the_socket_away(&asking_for("taw-asked"), &other);
    take_the_socket_away(&asking_for("taw-plain"), &plain);
    take_the_socket_away(&asking_for("taw-ours"), &ours);

    assert!(default.exists(), "the file called default was removed");
    assert!(other.exists(), "the file of another server was removed");
    assert!(plain.exists(), "a file that is no socket was removed");
    assert!(
        !ours.exists(),
        "the socket file of the server asked for stayed"
    );
}

/// The file called `default` is the socket of the tmux server of whoever runs the tests, and it is
/// never taken away, whichever way the `Tmux` asks for it: by that name (`-L default`) or by the path
/// of a file of that name (`-S .../default`). The `Tmux` runs a stand-in that notes what it is asked
/// and fails, so that nothing could reach a server even if the name were let through; and it is asked
/// nothing at all.
#[test]
fn the_socket_called_default_is_never_taken_away() {
    let dir = tempfile::tempdir().unwrap();
    let asked = dir.path().join("asked.log");
    let stand_in = dir.path().join("tmux");
    std::fs::write(
        &stand_in,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 1\n", asked.display()),
    )
    .unwrap();
    make_executable(&stand_in);
    let default = dir.path().join("default");
    let _listening = UnixListener::bind(&default).unwrap();
    let by_name = Tmux {
        bin: stand_in.clone(),
        ..Tmux::with_socket_name("default")
    };
    let by_path = Tmux {
        bin: stand_in,
        socket: Some(Socket::Path(default.clone())),
    };

    take_the_socket_away(&by_name, &default);
    take_the_socket_away(&by_path, &default);

    assert!(
        is_a_socket_file(&default),
        "the file called default was removed"
    );
    assert!(
        !asked.exists(),
        "a tmux was run against the socket called default: {}",
        std::fs::read_to_string(&asked).unwrap_or_default()
    );
}
