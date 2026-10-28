//! `[experimental] pane_history`, end to end: the screen a pane had is saved with the
//! layout and painted back by `taw session restore`.
//!
//! Tested both ways round, because "off" is the default and the more important of the
//! two: with the key absent, nothing may be captured, nothing written, and a restore
//! must produce exactly the plain shells it produced before the feature existed.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Server, wait_for, write_script};

/// Ceiling for "the restored pane's shell ran the replay and drew it".
const SETTLE: Duration = Duration::from_secs(10);

const CONFIG_OFF: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"
";

const CONFIG_ON: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[experimental]
pane_history = true
";

fn history_file(server: &Server, name: &str) -> PathBuf {
    server
        .state_dir()
        .join("sessions")
        .join("history")
        .join(format!("{name}.json"))
}

/// A second session, so the server still has a current target after the one under test
/// is killed. `session restore` calls `list-windows -a`, which fails outright on a server
/// with no sessions at all — the same reason `tests/tmux_session.rs` keeps one.
fn keepalive(server: &Server) {
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "200",
        "-y",
        "50",
        "/bin/sh",
    ]);
}

/// A server whose new panes' shells have a narrow `PATH`: the fake agents a restore types the
/// command of (`claude` and `pi`, which stay up and print nothing, as an agent waiting for its first
/// key does), then `/usr/bin` and `/bin`, and nothing of the runner's. The panes a restore makes
/// take it from `default-command`, so the `claude --resume` or `pi --session` it types runs a fake
/// on any machine: on the runner's `PATH` it started whatever agent the machine had installed.
fn server_with_fake_agents(dir: &Path) -> Server {
    let bin = dir.join("bin");
    for agent in ["claude", "pi"] {
        write_script(&bin.join(agent), "exec cat\n");
    }
    let server = Server::start();
    server.tmux_ok(&[
        "set-option",
        "-g",
        "default-command",
        &format!(
            "export PATH='{}:/usr/bin:/bin'; exec /bin/sh",
            bin.display()
        ),
    ]);
    server
}

/// Prints a recognisable line into a pane and waits for it to be on screen, so the
/// capture has something specific to find.
fn print_marker(server: &Server, pane: &str, marker: &str) {
    server.tmux_ok(&["send-keys", "-t", pane, &format!("echo {marker}"), "Enter"]);
    assert!(
        wait_for(|| server.capture(pane).contains(marker), SETTLE),
        "'{marker}' never appeared in the pane:\n{}",
        server.capture(pane)
    );
}

#[test]
fn with_pane_history_off_nothing_is_captured_and_a_restore_gives_plain_shells() {
    let server = Server::start();
    server.write_config(CONFIG_OFF);
    keepalive(&server);
    let pane = server.pane();
    print_marker(&server, &pane, "SECRET-TOKEN-OFF");

    let saved = server.ok(&["session", "save"], "cli:session:save", "session_saved");
    assert_eq!(saved["pane_history"], false);
    assert_eq!(saved["panes_captured"], 0);
    assert!(
        !history_file(&server, "default").exists(),
        "a history file was written with the feature off"
    );

    // Nothing on disk holds the screen: the whole point of the default.
    let snapshot =
        std::fs::read_to_string(server.state_dir().join("sessions").join("default.json")).unwrap();
    assert!(
        !snapshot.contains("SECRET-TOKEN-OFF"),
        "the layout snapshot captured screen text"
    );

    server.tmux_ok(&["kill-session", "-t", "one"]);
    let report = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    assert_eq!(report["created"].as_array().unwrap().len(), 1, "{report}");
    let restored = server.pane_of("one");
    // A plain shell, not a `sh -c cat …` wrapper.
    assert!(
        wait_for(
            || !server
                .display(&restored, "#{pane_current_command}")
                .is_empty(),
            SETTLE
        ),
        "the restored pane never started a command"
    );
    assert!(
        !server.capture(&restored).contains("SECRET-TOKEN-OFF"),
        "a screen was replayed with the feature off:\n{}",
        server.capture(&restored)
    );
}

#[test]
fn with_pane_history_on_the_screen_is_saved_and_painted_back_on_restore() {
    let server = Server::start();
    server.write_config(CONFIG_ON);
    keepalive(&server);
    let pane = server.pane();
    print_marker(&server, &pane, "REPLAY-ME-PLEASE");

    let saved = server.ok(&["session", "save"], "cli:session:save", "session_saved");
    assert_eq!(saved["pane_history"], true);
    // Both this pane and the keepalive session's, which the save also sees.
    assert_eq!(saved["panes_captured"], 2, "{saved}");

    let raw = std::fs::read_to_string(history_file(&server, "default")).unwrap();
    let history: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(history["version"], 1);
    // The history mirrors the snapshot's session order, which is by name — so the pane
    // under test is found by its content rather than by a position the keepalive session
    // shifts.
    let saved_pane = history["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|session| session["windows"].as_array().unwrap())
        .flat_map(|window| window["panes"].as_object().unwrap().values())
        .find(|pane| {
            pane["ansi"]
                .as_str()
                .unwrap_or_default()
                .contains("REPLAY-ME-PLEASE")
        })
        .unwrap_or_else(|| panic!("no saved pane holds the marker: {history}"));
    assert!(saved_pane["lines"].as_u64().unwrap() >= 1);

    server.tmux_ok(&["kill-session", "-t", "one"]);
    server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    let restored = server.pane_of("one");
    assert!(
        wait_for(
            || server.capture(&restored).contains("REPLAY-ME-PLEASE"),
            SETTLE
        ),
        "the saved screen was never painted back:\n{}",
        server.capture(&restored)
    );

    // The pane's own shell took over: `exec` in the replay command means the wrapper is
    // gone, so the pane is not reported as running `sh -c …` and `exit` closes it.
    assert!(
        wait_for(
            || {
                let command = server.display(&restored, "#{pane_current_command}");
                !command.is_empty() && command != "cat"
            },
            SETTLE
        ),
        "the restored pane is still running {:?}",
        server.display(&restored, "#{pane_current_command}")
    );
}

#[test]
fn turning_pane_history_off_removes_the_screens_already_saved() {
    let server = Server::start();
    server.write_config(CONFIG_ON);
    keepalive(&server);
    let pane = server.pane();
    print_marker(&server, &pane, "TO-BE-FORGOTTEN");
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    assert!(history_file(&server, "default").exists());

    // Changing your mind has to stop the screens being *kept*, not only stop new ones
    // being written.
    server.write_config(CONFIG_OFF);
    let saved = server.ok(&["session", "save"], "cli:session:save", "session_saved");
    assert_eq!(saved["pane_history"], false);
    assert!(
        !history_file(&server, "default").exists(),
        "the saved screens survived turning the feature off"
    );
}

#[test]
fn a_multi_pane_layout_replays_each_panes_own_screen_into_that_pane() {
    let server = Server::start();
    server.write_config(CONFIG_ON);
    keepalive(&server);
    let pane = server.pane();
    let public = server.public_pane_id(&pane);
    let split = server.ok(
        &["pane", "split", &public, "--direction", "down"],
        "cli:pane:split",
        "pane_split",
    );
    let second = split["pane"]["terminal_id"].as_str().unwrap().to_string();

    print_marker(&server, &pane, "TOP-PANE-MARK");
    print_marker(&server, &second, "BOTTOM-PANE-MARK");

    let saved = server.ok(&["session", "save"], "cli:session:save", "session_saved");
    assert_eq!(saved["panes_captured"], 3, "{saved}");

    server.tmux_ok(&["kill-session", "-t", "one"]);
    server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );

    let panes = server.tmux_ok(&["list-panes", "-t", "one", "-F", "#{pane_id}"]);
    let panes: Vec<&str> = panes.lines().collect();
    assert_eq!(panes.len(), 2, "restored panes: {panes:?}");
    // Each marker lands in exactly one pane, and they are not the same pane — history is
    // matched to a pane by position, so a swap here would be a real bug.
    for marker in ["TOP-PANE-MARK", "BOTTOM-PANE-MARK"] {
        assert!(
            wait_for(
                || panes.iter().any(|p| server.capture(p).contains(marker)),
                SETTLE
            ),
            "'{marker}' was never replayed; panes:\n{}\n---\n{}",
            server.capture(panes[0]),
            server.capture(panes[1])
        );
    }
    let top_has = server.capture(panes[0]).contains("TOP-PANE-MARK");
    let bottom_has = server.capture(panes[1]).contains("BOTTOM-PANE-MARK");
    assert!(
        top_has && bottom_has,
        "each pane must get its own screen, not the other's:\n{}\n---\n{}",
        server.capture(panes[0]),
        server.capture(panes[1])
    );
}

#[test]
fn a_pane_that_gets_an_agent_resumed_into_it_is_not_also_painted_over() {
    // A resumed agent restores its own conversation. A replayed screen would sit above
    // it as a second, stale copy of the same transcript — so the replay is suppressed for
    // exactly the panes `resume_agents` will type into. (herdr suppresses it the same
    // way, in `pane_restore_startup`.)
    let dir = tempfile::tempdir().unwrap();
    let server = server_with_fake_agents(dir.path());
    server.write_config(CONFIG_ON);
    keepalive(&server);
    let pane = server.pane();
    print_marker(&server, &pane, "STALE-TRANSCRIPT");
    server.set_pane_opt(&pane, "@agent_kind", "claude");
    server.set_pane_opt(&pane, "@agent_session_id", "abc123");

    let saved = server.ok(&["session", "save"], "cli:session:save", "session_saved");
    // The screen *is* captured — suppression is a restore-time decision, so turning the
    // resume off later still has a screen to replay.
    assert_eq!(saved["pane_history"], true);

    server.tmux_ok(&["kill-session", "-t", "one"]);
    let report = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    assert_eq!(
        report["resumed"].as_array().unwrap().len(),
        1,
        "the pane should have been resumed: {report}"
    );

    let restored = server.pane_of("one");
    // The resume command was typed; the saved screen was not painted.
    assert!(
        wait_for(
            || server.capture(&restored).contains("claude --resume abc123"),
            SETTLE
        ),
        "the resume command was never typed:
{}",
        server.capture(&restored)
    );
    assert!(
        !server.capture(&restored).contains("STALE-TRANSCRIPT"),
        "the old screen was replayed over a resumed agent:
{}",
        server.capture(&restored)
    );
}

#[test]
fn a_pane_resumed_from_its_session_path_is_not_painted_over_either() {
    // `pi` and `omp` report the path of their session file and no id, and are resumed from it.
    // That is a resumed agent like any other: its own conversation comes back, and the saved
    // screen would sit above it as a second, stale copy of it.
    let dir = tempfile::tempdir().unwrap();
    // Its fake `pi` stays up, as the agent would: a `pi` of the machine's own must not be the one
    // that runs.
    let server = server_with_fake_agents(dir.path());
    server.write_config(CONFIG_ON);
    keepalive(&server);
    let pane = server.pane();
    print_marker(&server, &pane, "STALE-TRANSCRIPT");
    server.set_pane_opt(&pane, "@agent_kind", "pi");
    server.set_pane_opt(
        &pane,
        "@agent_session_path",
        &dir.path().join("pi-session.json").display().to_string(),
    );

    let saved = server.ok(&["session", "save"], "cli:session:save", "session_saved");
    assert_eq!(saved["pane_history"], true);

    server.tmux_ok(&["kill-session", "-t", "one"]);
    let report = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    assert_eq!(
        report["resumed"].as_array().unwrap().len(),
        1,
        "the pane should have been resumed: {report}"
    );

    let restored = server.pane_of("one");
    assert!(
        wait_for(
            || server.capture(&restored).contains("pi --session"),
            SETTLE
        ),
        "the resume command was never typed:\n{}",
        server.capture(&restored)
    );
    assert!(
        !server.capture(&restored).contains("STALE-TRANSCRIPT"),
        "the old screen was replayed over a resumed agent:\n{}",
        server.capture(&restored)
    );
}

#[test]
fn a_dry_run_restore_says_it_would_replay_and_changes_nothing() {
    let server = Server::start();
    server.write_config(CONFIG_ON);
    keepalive(&server);
    let pane = server.pane();
    print_marker(&server, &pane, "DRY-RUN-MARK");
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "one"]);

    let dry = server.ok(
        &["session", "restore", "--dry-run"],
        "cli:session:restore",
        "session_restored",
    );
    assert_eq!(dry["dry_run"], true);
    assert!(dry["created"].as_array().unwrap().is_empty(), "{dry}");
    let plan = dry["plan"].as_array().unwrap();
    assert!(
        plan.iter()
            .any(|line| line.as_str().unwrap().contains("replay the saved screen")),
        "the plan should mention the replay: {plan:?}"
    );
    // Nothing was created, and no replay file was written out either.
    assert!(server.tmux(&["has-session", "-t", "one"]).is_err());
    assert!(
        !server
            .state_dir()
            .join("sessions")
            .join("history")
            .join("default.replay")
            .exists(),
        "a dry run wrote replay files"
    );
}

#[test]
fn a_hostile_session_path_is_refused_and_never_typed_into_a_restored_pane() {
    // `@agent_session_path` is written with no validation (`pane report-agent-session
    // --agent-session-path` stores it as given), so a snapshot can hold anything. The
    // restore is the only guard between that value and a `send-keys … Enter` line.
    let server = Server::start();
    server.write_config(CONFIG_OFF);
    keepalive(&server);
    let pane = server.pane();
    server.set_pane_opt(&pane, "@agent_kind", "pi");
    server.set_pane_opt(
        &pane,
        "@agent_session_path",
        "/tmp/s.json; touch /tmp/taw-pwned",
    );
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "one"]);

    let report = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    assert!(
        report["resumed"].as_array().unwrap().is_empty(),
        "a hostile path must not resume: {report}"
    );
    let plan = report["plan"].as_array().unwrap();
    assert!(
        plan.iter().any(|line| {
            let line = line.as_str().unwrap();
            line.contains("not a usable session path") && line.contains("skip resume")
        }),
        "the refusal should be reported: {plan:?}"
    );
    // Nothing was typed, so nothing ran.
    let restored = server.pane_of("one");
    assert!(
        !server.capture(&restored).contains("taw-pwned"),
        "the path reached the pane:\n{}",
        server.capture(&restored)
    );
    assert!(!std::path::Path::new("/tmp/taw-pwned").exists());
}

#[test]
fn a_pi_pane_with_only_a_session_path_is_accepted_for_a_path_resume() {
    // The decision this covers is `resume_target` choosing the path over nothing; the
    // command it turns into comes from `resume_command`, whose `pi` arm lands with the
    // integrations work. So this asserts the pane is *not refused* — the failure this
    // replaces was a valid path being ignored outright — and the exhaustive choice table
    // is in `session::restore`'s unit tests.
    let dir = tempfile::tempdir().unwrap();
    let server = server_with_fake_agents(dir.path());
    server.write_config(CONFIG_OFF);
    keepalive(&server);
    let pane = server.pane();
    server.set_pane_opt(&pane, "@agent_kind", "pi");
    server.set_pane_opt(&pane, "@agent_session_path", "/tmp/taw-pi-session.json");
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "one"]);

    let report = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    let plan = report["plan"].as_array().unwrap();
    assert!(
        !plan
            .iter()
            .any(|line| line.as_str().unwrap().contains("not a usable session path")),
        "a valid absolute path must not be refused: {plan:?}"
    );
    // Either it resumed (once `resume_command` knows `pi`) or it said no command is known
    // — never "the path is unusable", which is what this guards.
    let resumed = !report["resumed"].as_array().unwrap().is_empty();
    let no_command = plan.iter().any(|line| {
        line.as_str()
            .unwrap()
            .contains("no resume command is known for pi")
    });
    assert!(
        resumed || no_command,
        "expected a resume or an explicit 'no resume command' line: {report}"
    );
}

/// What `session restore --dry-run` says of the screens: the line a session that would be made gets
/// when its saved screens will be replayed.
fn replays_in_plan(server: &Server, name: &str) -> bool {
    let report = server.ok(
        &["session", "restore", "--name", name, "--dry-run"],
        "cli:session:restore",
        "session_restored",
    );
    report["plan"]
        .as_array()
        .unwrap()
        .iter()
        .any(|line| line.as_str().unwrap().contains("replay the saved screen"))
}

#[test]
fn saved_screens_are_replayed_only_onto_the_layout_they_were_taken_with() {
    let server = Server::start();
    server.write_config(CONFIG_ON);
    keepalive(&server);
    let pane = server.pane();
    print_marker(&server, &pane, "ONLY-ITS-OWN-LAYOUT");
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    let history: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(history_file(&server, "default")).unwrap())
            .unwrap();
    assert!(
        history["layout_fingerprint"]
            .as_str()
            .is_some_and(|f| f.len() == 64),
        "a save records the layout its screens were taken with: {history}"
    );
    server.tmux_ok(&["kill-session", "-t", "one"]);
    assert!(
        replays_in_plan(&server, "default"),
        "the screens of the layout they were taken with are replayed"
    );

    // A snapshot recovered by hand: the same file under another name has no history of its own,
    // and the same history beside a snapshot of another layout is not replayed.
    let sessions = server.state_dir().join("sessions");
    std::fs::copy(
        sessions.join("default.json"),
        sessions.join("recovered.json"),
    )
    .unwrap();
    assert!(!replays_in_plan(&server, "recovered"));
    std::fs::copy(
        history_file(&server, "default"),
        sessions.join("history").join("recovered.json"),
    )
    .unwrap();
    assert!(
        replays_in_plan(&server, "recovered"),
        "the same layout and the same screens: replayed"
    );
    let mut other: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sessions.join("recovered.json")).unwrap())
            .unwrap();
    other["sessions"][0]["windows"][0]["name"] = "another-layout".into();
    std::fs::write(sessions.join("recovered.json"), other.to_string()).unwrap();
    assert!(
        !replays_in_plan(&server, "recovered"),
        "screens taken with another layout were replayed"
    );

    // A history written before fingerprints: no field, never replayed.
    let mut old = history.clone();
    old.as_object_mut().unwrap().remove("layout_fingerprint");
    std::fs::write(history_file(&server, "default"), old.to_string()).unwrap();
    assert!(!replays_in_plan(&server, "default"));
}
