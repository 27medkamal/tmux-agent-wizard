//! An agent that a shell or a runtime runs, end to end through the built `taw` binary.
//!
//! An npm Codex is `node /path/to/bin/codex`, and a script is `sh /path/to/codex`: tmux names the pane's
//! command `node` or `sh`, which names no agent. The watcher reads the job in front of such a pane and
//! names the agent its arguments are. The fake here is a shell script called `codex`, run as
//! `sh <dir>/codex` (tmux names it `sh`, or `bash` on macOS) with no hint on the pane, so the job's arguments are the one thing that names it;
//! a script called `notes` run the same way names nothing and stays no agent.

mod common;

use std::time::Duration;

use common::{Server, ServerOptions, wait_for, write_script};

/// Ceiling for "the watcher noticed and wrote the new state".
const SETTLE: Duration = Duration::from_secs(20);

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// `/bin/sh` body of the fake: it draws `SCREEN` once and waits.
const FAKE: &str = r#"printf '\033[2J\033[H%s\n' 'SCREEN'
trap 'exit 0' TERM INT
while :; do sleep 0.2; done
"#;

/// The watcher's own log, for a failure message and for the positive wait of the no-agent case.
fn watcher_logs(server: &Server) -> String {
    let mut out = String::new();
    if let Ok(entries) = std::fs::read_dir(server.state_dir().join("logs")) {
        for entry in entries.flatten() {
            out.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
        }
    }
    out
}

/// A pane on a fresh server running `sh <dir>/<name>`, drawing `screen`, with its watcher attached.
fn wrapped(name: &str, screen: &str) -> (tempfile::TempDir, Server, String) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("scripts");
    let script = dir.join(name);
    write_script(&script, &FAKE.replace("SCREEN", screen));
    let server = Server::start_with(ServerOptions {
        env: vec![("TAW_LOG".to_string(), "info".to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();
    let line = format!("sh '{}'", script.display());
    server.tmux_ok(&["send-keys", "-t", &pane, &line, "Enter"]);
    assert!(
        wait_for(|| server.capture(&pane).contains(screen), SETTLE),
        "the fake never drew its screen:\n{}",
        server.capture(&pane)
    );
    // macOS's `/bin/sh` is bash, so tmux names the pane's command `sh` or `bash`: a shell either way,
    // which names no agent.
    let command = server.display(&pane, "#{pane_current_command}");
    assert!(
        command == "sh" || command == "bash",
        "the pane's command is {command:?}, not a shell"
    );
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    (tmp, server, pane)
}

#[test]
fn a_codex_that_sh_runs_is_named_codex_by_the_job_in_front_of_the_pane() {
    let (_tmp, server, pane) = wrapped("codex", "> Ask Codex to do anything");
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_kind") == "codex", SETTLE),
        "@agent_kind is {:?}, not codex\n--- screen ---\n{}\n--- log ---\n{}",
        server.pane_opt(&pane, "@agent_kind"),
        server.capture(&pane),
        watcher_logs(&server),
    );
    assert_eq!(server.pane_opt(&pane, "@agent_kind_hint"), "");
}

#[test]
fn a_script_that_names_no_agent_stays_no_agent() {
    let (_tmp, server, pane) = wrapped("notes", "my notes");
    // The positive wait: the watcher has read the job in front of the pane and found no agent in it.
    let read = format!("pane {pane}: the job in front of it names no agent");
    assert!(
        wait_for(|| watcher_logs(&server).contains(&read), SETTLE),
        "the watcher never read the job in front of {pane}\n--- log ---\n{}",
        watcher_logs(&server),
    );
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "");
}
