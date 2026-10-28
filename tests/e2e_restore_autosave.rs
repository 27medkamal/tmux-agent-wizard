//! A restore with the plugin's hooks installed, end to end through the built `taw` binary.
//!
//! The hooks autosave on every new window and every split, and a restore makes both by the
//! dozen. An autosave that ran in the middle of one took a snapshot of a half-made layout, the
//! sessions still to come missing from it and the panes made so far with no agent in them yet,
//! and wrote it over the snapshot the restore was reading its layout from: the next restore had
//! nothing to resume. A restore holds autosave off while it runs, saves once when it is done, and
//! marks the panes it resumed as holding their agent as it types, so what is saved keeps them.

mod common;

use std::time::{Duration, SystemTime};

use common::{Server, ServerOptions, wait_for, write_script};
use serde_json::Value;

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// The agents the saved snapshot holds for `session`, as `(agent_kind, agent_session_id)` of
/// each pane that has an `agent_kind`.
fn saved_agents(server: &Server, session: &str) -> Vec<(String, Option<String>)> {
    let path = server.state_dir().join("sessions").join("default.json");
    let snapshot: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let mut agents = Vec::new();
    for saved in snapshot["sessions"].as_array().unwrap() {
        if saved["name"] != session {
            continue;
        }
        for window in saved["windows"].as_array().unwrap() {
            for pane in window["panes"].as_array().unwrap() {
                if let Some(kind) = pane["agent_kind"].as_str() {
                    agents.push((
                        kind.to_string(),
                        pane["agent_session_id"].as_str().map(str::to_string),
                    ));
                }
            }
        }
    }
    agents
}

#[test]
fn a_restore_with_the_plugins_hooks_installed_leaves_the_saved_snapshot_holding_its_agents() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    // Stays up, so the pane is still the agent's when the snapshot is read.
    write_script(&bin.join("claude"), "exec cat\n");
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);

    // Session `one`: two windows, an agent in the first. A second session keeps the server up
    // while `one` is gone.
    let front = server.pane();
    server.tmux_ok(&["rename-window", "-t", "one:", "front"]);
    server.tmux_ok(&["new-window", "-d", "-t", "one:", "-n", "back"]);
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "180",
        "-y",
        "45",
    ]);
    server.set_pane_opt(&front, "@agent_kind", "claude");
    server.set_pane_opt(&front, "@agent_session_id", "abc");
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    assert_eq!(
        saved_agents(&server, "one"),
        vec![("claude".to_string(), Some("abc".to_string()))],
        "precondition: the snapshot holds the agent"
    );
    server.tmux_ok(&["kill-session", "-t", "one"]);

    // Older than an autosave's debounce, so a hook that fires may rewrite it.
    let snapshot = server.state_dir().join("sessions").join("default.json");
    std::fs::File::options()
        .write(true)
        .open(&snapshot)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(120))
        .unwrap();

    // The hooks go in last, so that nothing above fires them.
    server.source_plugin();

    // What the saved snapshot holds is read all through the restore and after it, as often as a
    // file can be read: a snapshot written in the middle of it, with the agent missing, is
    // there to be seen for as long as nothing overwrites it, and the one saved when it is done
    // does not make up for it.
    let expected = vec![("claude".to_string(), Some("abc".to_string()))];
    let stop = std::sync::atomic::AtomicBool::new(false);
    let (restored, seen) = std::thread::scope(|scope| {
        let reading = scope.spawn(|| {
            let mut seen = Vec::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                seen.push(saved_agents(&server, "one"));
                std::thread::sleep(Duration::from_millis(5));
            }
            seen
        });
        let restored = server.ok(
            &["session", "restore"],
            "cli:session:restore",
            "session_restored",
        );
        // The hooks a restore fires run on their own, a little after the commands that fired
        // them.
        assert!(
            wait_for(
                || server.display("one:front", "#{pane_current_command}") == "cat",
                Duration::from_secs(20)
            ),
            "the agent was never started in the restored pane"
        );
        std::thread::sleep(Duration::from_secs(2));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        (restored, reading.join().unwrap())
    });
    assert_eq!(
        restored["resumed"].as_array().unwrap().len(),
        1,
        "{restored}"
    );

    assert!(
        seen.len() > 20,
        "the snapshot was read only {} times",
        seen.len()
    );
    if let Some(at) = seen.iter().position(|agents| *agents != expected) {
        panic!(
            "the saved snapshot lost the agent at read {at} of {} ({:?}), during or after the restore",
            seen.len(),
            seen[at]
        );
    }
    assert_eq!(
        saved_agents(&server, "one"),
        expected,
        "the restore, or a hook it fired, saved a snapshot without the agent"
    );
    assert!(
        std::fs::metadata(&snapshot).unwrap().modified().unwrap()
            > SystemTime::now() - Duration::from_secs(60),
        "the layout the restore made was never saved"
    );
    assert_eq!(
        server.global_opt("@agent_wizard_restoring"),
        "",
        "the restore left autosave held off"
    );
}

#[test]
fn a_restore_that_was_killed_does_not_leave_autosave_held_off() {
    let server = Server::start();
    server.write_config(CONFIG);

    // A session with an agent in it, and a second to keep the server up while it is gone.
    let pane = server.pane();
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "180",
        "-y",
        "45",
    ]);
    server.set_pane_opt(&pane, "@agent_kind", "claude");
    server.set_pane_opt(&pane, "@agent_session_id", "abc");
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "one"]);

    // Its pane is a program and not a shell, which no prompt ever comes from: the restore waits
    // for it, for as long as it is allowed, which is time enough to end it in the middle.
    server.tmux_ok(&["set-option", "-g", "default-command", "exec cat"]);
    let mut restore = server.spawn_taw(&["session", "restore"]);
    assert!(
        wait_for(
            || !server.global_opt("@agent_wizard_restoring").is_empty(),
            Duration::from_secs(20)
        ),
        "the restore never held autosave off"
    );

    // Ctrl-C, a closed terminal and a kill all end a process without a word, and a mark that is
    // cleared by the process that set it is not cleared by any of them.
    restore.kill().unwrap();
    restore.wait().unwrap();
    assert_ne!(
        server.global_opt("@agent_wizard_restoring"),
        "",
        "precondition: the killed restore had no chance to clear its mark"
    );

    // What a hook does on the next window or split, with the snapshot older than the debounce.
    let snapshot = server.state_dir().join("sessions").join("default.json");
    let long_ago = SystemTime::now() - Duration::from_secs(120);
    std::fs::File::options()
        .write(true)
        .open(&snapshot)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();
    let (code, autosaved) = server.taw(&["session", "autosave"]);
    assert_eq!(code, 0, "{autosaved}");
    assert_eq!(
        autosaved["result"]["saved"], true,
        "a restore that was killed left autosave held off: {autosaved}"
    );
    assert!(
        std::fs::metadata(&snapshot).unwrap().modified().unwrap() > long_ago,
        "nothing was saved"
    );
}
