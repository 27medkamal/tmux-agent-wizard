//! An agent that reports its own state, end to end through the built `taw` binary: a real `taw
//! watch run` on a throwaway tmux server, told of each state by a real `taw pane report-agent` from
//! outside the pane, as OpenCode's plugin tells it from inside.
//!
//! `tests/tmux_watch.rs` runs the watcher in this process and feeds it output. What only the real
//! thing shows is the nudge: a report writes a pane option, and a watcher whose pane prints nothing
//! reads nothing (a pane with no agent in it never, one with an agent on its five-second recheck),
//! so the report has to wake it, across processes, in the watcher that tmux started. The program
//! standing in for the agent prints nothing at all.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{NotifyRecorder, Server, ServerOptions, wait_for};

/// Ceiling for "the watcher took the report in". The watcher's tick is 300 ms; this allows for
/// `cargo test` running the other files alongside, and a pane the recheck would only read in five
/// seconds is what it is being told apart from.
const SETTLE: Duration = Duration::from_secs(8);

/// Hooks that write each event they hear to `heard`, toasts delivered through `$TAW_NOTIFY_CMD` with
/// no delay.
fn config(heard: &Path) -> String {
    let event = toml_string(&format!(
        "echo \"$TAW_EVENT $TAW_STATE/$TAW_PREVIOUS_STATE\" >> {}",
        heard.display()
    ));
    let notification = toml_string(&format!(
        "echo \"$TAW_EVENT $TAW_KIND\" >> {}",
        heard.display()
    ));
    format!(
        "[update]\nmanifest_check = false\n\n[ui.toast]\ndelivery = \"system\"\ndelay_seconds = 0\ndone_delay_seconds = 0\n\n[ui.sound]\nenabled = false\n\n[hooks]\n\"pane.agent_status_changed\" = {event}\n\"agent.blocked\" = {event}\n\"agent.done\" = {event}\n\"agent.idle\" = {event}\n\"agent.working\" = {event}\nnotification = {notification}\n"
    )
}

fn toml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn heard(path: &Path) -> Vec<String> {
    let mut lines: Vec<String> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    lines.sort();
    lines
}

/// What an integration does: `taw pane report-agent` for `pane`, from outside it.
fn report(server: &Server, pane: &str, state: &str) {
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
            state,
        ],
        "cli:pane:report-agent",
        "ok",
    );
}

#[test]
fn a_report_wakes_the_watcher_of_a_pane_that_prints_nothing_and_each_change_is_announced_once() {
    let tmp = tempfile::tempdir().unwrap();
    let recorder = NotifyRecorder::new(tmp.path());
    let heard_file = tmp.path().join("heard.txt");
    let server = Server::start_with(ServerOptions {
        env: vec![
            (
                "TAW_NOTIFY_CMD".to_string(),
                recorder.script.display().to_string(),
            ),
            ("TAW_LOG".to_string(), "debug".to_string()),
        ],
        ..ServerOptions::default()
    });
    server.write_config(&config(&heard_file));
    let pane = server.pane();

    // A program in front of the pane's shell that prints nothing, as a real agent thinking does:
    // the agent this stands in for is kept only until the pane's own shell is back at its prompt.
    server.tmux_ok(&["send-keys", "-t", &pane, "sleep 300", "Enter"]);
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "sleep",
            SETTLE
        ),
        "`sleep` never took the pane"
    );
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    assert!(
        wait_for(|| server.watcher_reads(&pane) >= 1, SETTLE),
        "the watcher never read the pane\n{}",
        server.watcher_logs()
    );
    let reads = server.watcher_reads(&pane);

    // The pane has no agent and prints nothing: no read is owed until the report comes.
    report(&server, &pane, "working");
    assert!(
        wait_for(
            || heard(&heard_file)
                == [
                    "agent.working working/",
                    "pane.agent_status_changed working/"
                ],
            SETTLE
        ),
        "the first report of a quiet pane was never announced: {:?}\n{}",
        heard(&heard_file),
        server.watcher_logs()
    );
    assert!(
        server.watcher_reads(&pane) > reads,
        "the watcher read nothing"
    );

    // The same report again is nothing; `idle` ends the turn behind the user's back.
    report(&server, &pane, "working");
    report(&server, &pane, "idle");
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_state") == "done", SETTLE),
        "the finished turn never became done (state {:?})\n{}",
        server.pane_opt(&pane, "@agent_state"),
        server.watcher_logs()
    );
    assert!(
        wait_for(|| recorder.matching("done").len() == 1, SETTLE),
        "one done toast was expected, got {:?}",
        recorder.lines()
    );
    let want = [
        "agent.done done/working",
        "agent.working working/",
        "notification done",
        "pane.agent_status_changed done/working",
        "pane.agent_status_changed working/",
    ];
    assert!(
        wait_for(|| heard(&heard_file) == want, SETTLE),
        "what the hooks heard: {:?}",
        heard(&heard_file)
    );
    // Nothing more comes: a second `idle` leaves the `done` as it is, and is announced no more.
    report(&server, &pane, "idle");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(heard(&heard_file), want, "a repeated idle was announced");
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "done");
    assert_eq!(recorder.lines().len(), 1, "{:?}", recorder.lines());
    assert!(
        recorder.lines()[0].starts_with("opencode done \u{b7} one:0|"),
        "{:?}",
        recorder.lines()
    );
    assert_eq!(
        server.pane_opt(&pane, "@agent_source"),
        "hook:integration:opencode",
        "the watcher's done leaves the agent its owner"
    );
}
