//! The copies a save keeps in `session-snapshots/` (herdr's 48 snapshots), seen from the real
//! binary on a private server with a scratch state directory.

mod common;

use std::path::PathBuf;

use common::Server;

fn copies(server: &Server, name: &str) -> Vec<PathBuf> {
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
                .is_some_and(|n| n.starts_with(&format!("session-{name}-")) && n.ends_with(".json"))
        })
        .collect();
    files.sort();
    files
}

/// Two saves in quick succession leave one copy (the first save is copied at once, the second is
/// inside the 15 minutes), and `session list` shows the saved name and not the copy.
#[test]
fn two_quick_saves_leave_one_copy_and_list_does_not_show_it() {
    let server = Server::start();
    server.write_config("[update]\nmanifest_check = false\n\n[ui.toast]\ndelivery = \"off\"\n");

    server.ok(
        &["session", "save", "--name", "proj"],
        "cli:session:save",
        "session_saved",
    );
    let first = copies(&server, "proj");
    assert_eq!(
        first.len(),
        1,
        "the first save is copied at once: {first:?}"
    );
    let kept = std::fs::read(&first[0]).unwrap();

    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "second",
        "-x",
        "100",
        "-y",
        "30",
        "/bin/sh",
    ]);
    server.ok(
        &["session", "save", "--name", "proj"],
        "cli:session:save",
        "session_saved",
    );
    assert_eq!(
        copies(&server, "proj"),
        first,
        "a second save inside 15 minutes adds none"
    );
    assert_eq!(std::fs::read(&first[0]).unwrap(), kept);

    let listed = server.ok(&["session", "list"], "cli:session:list", "session_list");
    let names: Vec<&str> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["proj"], "{listed}");

    server.ok(&["session", "delete", "proj"], "cli:session:delete", "ok");
    assert_eq!(copies(&server, "proj"), first, "a delete leaves the copy");
    let after = server.ok(&["session", "list"], "cli:session:list", "session_list");
    assert!(after["sessions"].as_array().unwrap().is_empty(), "{after}");
}
