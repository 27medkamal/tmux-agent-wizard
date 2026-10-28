//! A Claude pane comes back under the profile it ran under, end to end through the built `taw`
//! binary on a private socket: the transcript path Claude reported is
//! `<profile>/projects/<project>/<id>.jsonl`, and restore types `CLAUDE_CONFIG_DIR=<profile> claude
//! --resume <id>` for any profile but the default (`$HOME/.claude`, the bare line).
//!
//! The fake `claude` records the arguments it was given and the `CLAUDE_CONFIG_DIR` it was started
//! with into a file under the test's temp folder, named for the session it was resumed into.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{Server, wait_for, write_script};

const SETTLE: Duration = Duration::from_secs(15);

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"
";

/// A server whose panes find a fake `claude` first, which writes what it got to `<records>/<id>` and
/// then stays up.
fn server_with_fake_claude(dir: &Path, records: &Path) -> Server {
    std::fs::create_dir_all(records).unwrap();
    let bin = dir.join("bin");
    write_script(
        &bin.join("claude"),
        &format!(
            "{{ printf 'ARGS=%s\\n' \"$*\"; printf 'CONFIG=%s\\n' \"${{CLAUDE_CONFIG_DIR-unset}}\"; }} > '{}'/\"$2\"\nexec cat\n",
            records.display()
        ),
    );
    let server = Server::start();
    server.write_config(CONFIG);
    server.tmux_ok(&[
        "set-option",
        "-g",
        "default-command",
        &format!(
            "export PATH='{}:/usr/bin:/bin'; exec /bin/sh",
            bin.display()
        ),
    ]);
    // A session that stays, so the server has a current target while the others are killed.
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "100",
        "-y",
        "30",
        "/bin/sh",
    ]);
    server
}

/// The pane's screen with wrapped lines joined: a long path wraps in a pane of 100 columns.
fn joined(server: &Server, pane: &str) -> String {
    server.tmux_ok(&["capture-pane", "-p", "-J", "-t", pane])
}

fn read_record(records: &Path, id: &str) -> Option<String> {
    std::fs::read_to_string(records.join(id))
        .ok()
        .filter(|text| text.contains("CONFIG="))
}

/// A saved Claude pane in session `name`: it reported `session` and the transcript at `transcript`.
fn claude_pane(server: &Server, name: &str, session: &str, transcript: &Path) {
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
    let pane = server.pane_of(name);
    server.set_pane_opt(&pane, "@agent_kind", "claude");
    server.set_pane_opt(&pane, "@agent_session_id", session);
    server.set_pane_opt(
        &pane,
        "@agent_session_path",
        &transcript.display().to_string(),
    );
}

#[test]
fn a_claude_pane_comes_back_under_its_profile_and_a_default_one_gets_the_bare_line() {
    let tmp = tempfile::tempdir().unwrap();
    let records = tmp.path().join("records");
    let server = server_with_fake_claude(tmp.path(), &records);
    let team = tmp.path().join(".claude-home");
    let default = server.home_dir().join(".claude");
    claude_pane(
        &server,
        "teamed",
        "sess-team",
        &team.join("projects/-work-repo/sess-team.jsonl"),
    );
    claude_pane(
        &server,
        "plain",
        "sess-default",
        &default.join("projects/-work-repo/sess-default.jsonl"),
    );
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "teamed"]);
    server.tmux_ok(&["kill-session", "-t", "plain"]);

    // The dry run shows the exact line for each pane, and makes nothing.
    let dry = server.ok(
        &["session", "restore", "--dry-run"],
        "cli:session:restore",
        "session_restored",
    );
    let team_line = format!(
        "CLAUDE_CONFIG_DIR={} claude --resume sess-team",
        team.display()
    );
    let plan: Vec<&str> = dry["plan"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert!(
        plan.iter()
            .any(|l| l.ends_with(&format!("would type: {team_line}"))),
        "{plan:?}"
    );
    assert!(
        plan.iter()
            .any(|l| l.ends_with("would type: claude --resume sess-default")),
        "{plan:?}"
    );
    let would: Vec<&str> = dry["would_type"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["line"].as_str().unwrap())
        .collect();
    assert!(would.contains(&team_line.as_str()), "{dry}");
    assert!(would.contains(&"claude --resume sess-default"), "{dry}");
    assert!(server.tmux(&["has-session", "-t", "teamed"]).is_err());
    assert!(!records.join("sess-team").exists());

    let restored = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    assert_eq!(
        restored["resumed"].as_array().unwrap().len(),
        2,
        "{restored}"
    );
    assert!(
        restored["would_type"].as_array().unwrap().is_empty(),
        "{restored}"
    );

    assert!(
        wait_for(
            || read_record(&records, "sess-team").is_some()
                && read_record(&records, "sess-default").is_some(),
            SETTLE
        ),
        "the fakes were not started: {:?} {:?}\n{}\n{}",
        read_record(&records, "sess-team"),
        read_record(&records, "sess-default"),
        joined(&server, &server.pane_of("teamed")),
        joined(&server, &server.pane_of("plain")),
    );
    assert_eq!(
        read_record(&records, "sess-team").unwrap(),
        format!("ARGS=--resume sess-team\nCONFIG={}\n", team.display())
    );
    // The bare line: the fake got what the pane's shell has, the harness's own default.
    assert_eq!(
        read_record(&records, "sess-default").unwrap(),
        format!("ARGS=--resume sess-default\nCONFIG={}\n", default.display())
    );
    // What was typed is what the dry run said.
    assert!(
        joined(&server, &server.pane_of("teamed")).contains(&team_line),
        "{}",
        joined(&server, &server.pane_of("teamed"))
    );
    // The restore put the transcript path back with the session (a pane with a profile and no flags
    // too), so its end save and every later one keep the profile for a second crash.
    let teamed = server.pane_of("teamed");
    assert!(
        wait_for(
            || !server.pane_opt(&teamed, "@agent_session_path").is_empty(),
            SETTLE
        ),
        "the restore did not put the transcript path back"
    );
    assert_eq!(
        server.pane_opt(&teamed, "@agent_session_path"),
        team.join("projects/-work-repo/sess-team.jsonl")
            .display()
            .to_string()
    );
    assert!(
        !joined(&server, &server.pane_of("plain")).contains("CLAUDE_CONFIG_DIR"),
        "{}",
        joined(&server, &server.pane_of("plain"))
    );
}
