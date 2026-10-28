//! An agent taw names by its process goes when that process does, even after its own hook
//! reported its session: end to end through the built `taw` binary and a real `taw watch run`
//! on a throwaway tmux server.
//!
//! Claude's hook reports the session with `taw pane report-agent-session`, which marks the
//! pane with the kind the agent reported. A pane held that way is kept until its shell is back
//! at its prompt, which is right for an agent whose command taw cannot read (a Letta run under
//! `node`; see `tests/e2e_letta_session.rs`) and wrong for one it can. herdr keeps an agent it
//! cannot identify by its process and judges the rest by its ordinary rules, and so must taw:
//! quit Claude inside a shell of your own and its record, session id included, goes at once.
//!
//! The Claude is a copy of tmux called `claude`, so that tmux reports `claude` as the pane's
//! command for as long as it runs, and it reports its session as the real hook does, with
//! Claude's own shape of session id (a UUID). The OpenCode is the same kind of copy, whose plugin
//! reports its state, not only its session: a pane a hook owns is judged by the same split.

mod common;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::{Server, ServerOptions, tmux_named, wait_for};

/// Ceiling for "the watcher noticed and wrote the new state", well above what an idle machine
/// needs because `cargo test` runs these files in parallel.
const SETTLE: Duration = Duration::from_secs(25);

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// Claude's session ids are UUIDs.
const SESSION: &str = "3f0c2a9e-7d41-4b6e-9c55-2b1e8a6d0f42";

/// OpenCode's session ids: `ses_`, twelve hexadecimal digits and fourteen letters and digits.
const OPENCODE_SESSION: &str = "ses_3e1a04b21ffeQ7mZk2VtXc9LwH";

fn have(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn server() -> Server {
    let server = Server::start_with(ServerOptions {
        width: 100,
        height: 30,
        env: vec![("TAW_LOG".to_string(), "debug".to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    server
}

/// Types a line into the pane's shell, the pane's own or one started in it, once the shell is at its prompt:
/// typed sooner, the line is cut at the terminal's line buffer. No caller types while a program is in front.
fn type_line(server: &Server, pane: &str, line: &str) {
    server.wait_for_prompt(pane);
    server.tmux_ok(&["send-keys", "-t", pane, line, "Enter"]);
}

/// A shell the user started inside the pane: `bash` typed at its prompt, the shape of `nix develop`
/// or `sudo -s`. It has a prompt of its own to wait for.
fn nest_a_shell(server: &Server, pane: &str) {
    type_line(server, pane, "bash --norc --noprofile");
    type_line(server, pane, "PS1='nested$ '");
    assert!(
        wait_for(
            || server.capture(pane).trim_end().ends_with("nested$"),
            SETTLE
        ),
        "the nested shell never prompted:\n{}",
        server.capture(pane)
    );
}

/// What Claude's hook does when a session starts: the same call, with the same source and
/// the same shape of id.
fn report_session(server: &Server, pane: &str) {
    server.ok(
        &[
            "pane",
            "report-agent-session",
            pane,
            "--source",
            "integration:claude",
            "--agent",
            "claude",
            "--agent-session-id",
            SESSION,
        ],
        "cli:pane:report-agent-session",
        "ok",
    );
}

/// The record the hook and the watcher leave: kind, state, session id and the kind the hook
/// reported, in that order.
fn record(server: &Server, pane: &str) -> [String; 4] {
    [
        "@agent_kind",
        "@agent_state",
        "@agent_session_id",
        "@agent_reported_kind",
    ]
    .map(|name| server.pane_opt(pane, name))
}

fn gone() -> [String; 4] {
    [String::new(), String::new(), String::new(), String::new()]
}

/// A pane running a Claude that has reported its session, with a shell of the user's own
/// (`bash` typed at the prompt, the shape of `nix develop` or `sudo -s`) between it and the
/// pane's shell, and a watcher on the pane.
fn claude_in_a_nested_shell(server: &Server, tmp: &Path, channel: &str) -> String {
    let claude = tmux_named(tmp, "claude");
    let pane = server.pane();
    nest_a_shell(server, &pane);
    type_line(server, &pane, &server.wait_line(&claude, channel));
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "claude",
            SETTLE
        ),
        "Claude never took the terminal (the pane runs {:?})",
        server.display(&pane, "#{pane_current_command}")
    );
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    // The watcher names it from its process, and after its startup grace reads it at rest.
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_kind") == "claude"
                && server.pane_opt(&pane, "@agent_state") == "idle",
            SETTLE
        ),
        "the watcher never took Claude for idle: {:?}\n{}",
        record(server, &pane),
        server.watcher_logs()
    );
    report_session(server, &pane);
    assert_eq!(
        record(server, &pane),
        ["claude", "idle", SESSION, "claude"].map(str::to_string),
        "the hook's report did not land"
    );
    pane
}

#[test]
fn a_claude_that_reported_its_session_goes_when_it_quits_into_a_shell_started_in_the_pane() {
    if !have("bash") {
        eprintln!("skipping: bash is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let server = server();
    let pane = claude_in_a_nested_shell(&server, tmp.path(), "quit");

    // Claude quits, and what is in front of the pane is `bash`, which the user started: not
    // the pane's own shell, but taw names Claude by its process, so a shell in front of it is
    // the end of it.
    server.wake("quit");
    assert!(
        wait_for(|| record(&server, &pane) == gone(), SETTLE),
        "Claude quit into a nested shell and the record stayed: {:?}\n{}",
        record(&server, &pane),
        server.watcher_logs()
    );
}

/// What OpenCode's plugin does when a turn starts: `taw pane report-agent`, with the shape of
/// session id OpenCode gives its sessions.
fn report_opencode_working(server: &Server, pane: &str) {
    server.ok(
        &[
            "pane",
            "report-agent",
            pane,
            "--source",
            "integration:opencode",
            "--agent",
            "opencode",
            "--state",
            "working",
            "--agent-session-id",
            OPENCODE_SESSION,
        ],
        "cli:pane:report-agent",
        "ok",
    );
}

/// The same split for an agent whose hook reports its state, which is the pane's to give up on
/// its shell coming back: an OpenCode run from a prompt is named by its process, so quitting it
/// inside a shell of the user's own ends its record, and the turn it was in the middle of is
/// ended first, as for any exit.
#[test]
fn an_opencode_that_reports_its_state_goes_when_it_quits_into_a_shell_started_in_the_pane() {
    if !have("bash") {
        eprintln!("skipping: bash is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let server = server();
    let opencode = tmux_named(tmp.path(), "opencode");
    let pane = server.pane();
    nest_a_shell(&server, &pane);
    type_line(&server, &pane, &server.wait_line(&opencode, "quit"));
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "opencode",
            SETTLE
        ),
        "OpenCode never took the terminal (the pane runs {:?})",
        server.display(&pane, "#{pane_current_command}")
    );
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    // The watcher names it from its process, and the plugin reports a turn under way.
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_kind") == "opencode",
            SETTLE
        ),
        "the watcher never named OpenCode from its process\n{}",
        server.watcher_logs()
    );
    report_opencode_working(&server, &pane);
    assert_eq!(
        server.pane_opt(&pane, "@agent_source"),
        "hook:integration:opencode"
    );
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "working");

    server.wake("quit");
    assert!(
        wait_for(
            || [
                "@agent_kind",
                "@agent_state",
                "@agent_source",
                "@agent_session_id"
            ]
            .iter()
            .all(|name| server.pane_opt(&pane, name).is_empty()),
            SETTLE
        ),
        "OpenCode quit into a nested shell and the record stayed\n{}",
        server.watcher_logs()
    );
    let logs = server.watcher_logs();
    assert!(
        logs.contains("process_exited"),
        "no end written for the turn OpenCode was in\n{logs}"
    );
}
