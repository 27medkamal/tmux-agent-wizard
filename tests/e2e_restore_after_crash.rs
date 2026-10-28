//! A tmux server that died and a new one that autosaved, end to end through the built `taw`
//! binary on a private socket: the old layout is kept as a copy, and a plain `taw session restore`
//! finds it.
//!
//! The server is ended with `kill-server` and a new one is started on the same socket, two seconds
//! later (a snapshot's `taken_unix` and tmux's `#{start_time}` are whole seconds, and "before" is
//! strict). The new server's first autosave replaced `sessions/default.json` with its one session,
//! and a plain restore could not find the layout the old server had last saved.

mod common;

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use common::{Server, ServerOptions, wait_for};
use serde_json::Value;

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

fn new_session(server: &Server, name: &str) {
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        name,
        "-x",
        "100",
        "-y",
        "30",
        "-c",
        server.work_dir().to_str().unwrap(),
    ]);
}

fn default_json(server: &Server) -> PathBuf {
    server.state_dir().join("sessions").join("default.json")
}

/// Makes `default.json` older than an autosave's debounce, as it is when the next autosave is due.
fn age_default(server: &Server) {
    std::fs::File::options()
        .write(true)
        .open(default_json(server))
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(120))
        .unwrap();
}

/// The session names `snapshot` (a file) holds, in file order.
fn names_in(path: &std::path::Path) -> Vec<String> {
    let snapshot: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    snapshot["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect()
}

fn copies(server: &Server) -> Vec<PathBuf> {
    let dir = server
        .state_dir()
        .join("sessions")
        .join("session-snapshots");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("session-default-") && n.ends_with(".json"))
        })
        .collect();
    files.sort();
    files
}

fn open_sessions(server: &Server) -> Vec<String> {
    let mut names: Vec<String> = server
        .tmux(&["list-sessions", "-F", "#{session_name}"])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    names.sort();
    names
}

/// The server dies, and a new one starts on the same socket with one session, `fresh`, through the
/// fixture (which pauses two seconds, for the whole-second `#{start_time}`).
fn crash_and_start_afresh(server: &mut Server) {
    server.crash_and_start_afresh(&ServerOptions {
        session: "fresh".to_string(),
        width: 100,
        height: 30,
        ..ServerOptions::default()
    });
}

fn autosave(server: &Server) -> Value {
    let (code, env) = server.taw(&["session", "autosave"]);
    assert_eq!(code, 0, "{env}");
    env["result"].clone()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

/// Tests 1 and 3: three sessions autosaved, the server dies, the new one autosaves
/// its one session over them, and a plain restore brings the three back. A second plain restore
/// has nothing to do.
#[test]
fn a_plain_restore_after_a_crash_brings_back_the_old_servers_layout() {
    let mut server = Server::start();
    server.write_config(CONFIG);
    new_session(&server, "two");
    new_session(&server, "three");
    assert!(autosaved(&server));
    assert_eq!(names_in(&default_json(&server)), ["one", "three", "two"]);

    crash_and_start_afresh(&mut server);
    age_default(&server);
    assert!(autosaved(&server));
    assert_eq!(
        names_in(&default_json(&server)),
        ["fresh"],
        "precondition: the new server's autosave replaced the layout"
    );

    // The dry run names the layout it would restore, and makes nothing.
    let dry = server.ok(
        &["session", "restore", "--dry-run"],
        "cli:session:restore",
        "session_restored",
    );
    let line = dry["plan"][0].as_str().unwrap();
    assert!(
        line.starts_with("would restore the layout saved at ")
            && line.contains(", before this tmux server started at "),
        "{dry}"
    );
    assert!(
        dry["snapshot_file"]
            .as_str()
            .unwrap()
            .contains("session-snapshots/session-default-"),
        "{dry}"
    );
    assert_eq!(open_sessions(&server), ["fresh"], "{dry}");

    let restored = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    assert_eq!(
        strings(&restored["created"]),
        ["one", "three", "two"],
        "{restored}"
    );
    assert_eq!(
        strings(&restored["skipped"]),
        Vec::<String>::new(),
        "{restored}"
    );
    assert_eq!(
        open_sessions(&server),
        ["fresh", "one", "three", "two"],
        "{restored}"
    );
    let used = restored["snapshot_file"].as_str().unwrap();
    assert!(
        used.contains("session-snapshots/session-default-"),
        "{restored}"
    );
    assert!(
        restored["snapshot_taken_unix"].as_u64().unwrap() > 0,
        "{restored}"
    );
    assert!(
        restored["snapshot_taken_unix"].as_u64().unwrap()
            < restored["server_start_unix"].as_u64().unwrap(),
        "{restored}"
    );
    let line = restored["plan"][0].as_str().unwrap();
    assert!(
        line.starts_with("restored the layout saved at ")
            && line.contains(", before this tmux server started at "),
        "{restored}"
    );

    // Test 3: the three are open, so there is nothing to do, and nothing is brought back from any
    // other layout. The layout the look-back found, the old server's, has all three open.
    let again = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    assert_eq!(strings(&again["skipped"]), ["fresh"], "{again}");
    assert_eq!(strings(&again["created"]), Vec::<String>::new(), "{again}");
    assert!(
        again["plan"][0]
            .as_str()
            .unwrap()
            .starts_with("nothing to restore"),
        "{again}"
    );
    assert_eq!(
        open_sessions(&server),
        ["fresh", "one", "three", "two"],
        "{again}"
    );
}

fn autosaved(server: &Server) -> bool {
    autosave(server)["saved"] == true
}

/// Test 2: the layout the old server last saved is a third session newer than the history's newest
/// copy, which is under 15 minutes old, so no copy of it was made. The new server's autosave still
/// keeps it.
#[test]
fn the_old_layout_is_kept_though_the_newest_copy_is_under_fifteen_minutes_old() {
    let mut server = Server::start();
    server.write_config(CONFIG);
    new_session(&server, "two");
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    assert_eq!(copies(&server).len(), 1, "the first save is copied at once");
    assert_eq!(names_in(&copies(&server)[0]), ["one", "two"]);

    new_session(&server, "three");
    age_default(&server);
    assert!(autosaved(&server));
    assert_eq!(names_in(&default_json(&server)), ["one", "three", "two"]);
    assert_eq!(
        copies(&server).len(),
        1,
        "precondition: the history holds only the two sessions' layout"
    );

    crash_and_start_afresh(&mut server);
    age_default(&server);
    assert!(autosaved(&server));
    assert_eq!(names_in(&default_json(&server)), ["fresh"]);
    let kept: Vec<Vec<String>> = copies(&server).iter().map(|c| names_in(c)).collect();
    assert!(
        kept.iter().any(|names| names == &["one", "three", "two"]),
        "the layout the old server last saved was not copied: {kept:?}"
    );

    let restored = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    assert_eq!(
        strings(&restored["created"]),
        ["one", "three", "two"],
        "{restored}"
    );
    assert_eq!(
        open_sessions(&server),
        ["fresh", "one", "three", "two"],
        "{restored}"
    );
}

/// Test 5, after a crash: the new server with no session at all (its autosave never replaces a
/// layout with an empty one) does not touch the old layout, and makes no copy of it either.
/// The guards themselves are in `tests/tmux_session.rs`
/// (`autosave_if_due_never_overwrites_a_saved_layout_with_an_empty_one` and the
/// `autosave_if_due_waits_while_a_restore_is_running_...` tests).
#[test]
fn a_new_server_with_no_session_leaves_the_old_layout_alone() {
    let mut server = Server::start();
    server.write_config(CONFIG);
    new_session(&server, "two");
    assert!(autosaved(&server));
    let before = std::fs::read(default_json(&server)).unwrap();
    let copied = copies(&server);

    crash_and_start_afresh(&mut server);
    server.tmux_ok(&["kill-session", "-t", "fresh"]);
    // The positive sign first: the server answers and holds no session.
    assert!(
        wait_for(
            || server
                .tmux(&["list-sessions"])
                .unwrap_or_default()
                .is_empty(),
            Duration::from_secs(20)
        ),
        "the new server still has a session"
    );
    age_default(&server);
    assert!(!autosaved(&server), "an empty layout was saved");
    assert_eq!(std::fs::read(default_json(&server)).unwrap(), before);
    assert_eq!(copies(&server), copied);
}
