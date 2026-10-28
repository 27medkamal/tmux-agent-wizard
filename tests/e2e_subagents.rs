//! A Claude at its prompt while its sub-agents still run, end to end through the built
//! `taw` binary.
//!
//! When the main agent's turn ends, Claude Code's prompt box is back at rest, and herdr's
//! rules read that idle: the watcher wrote `done`, and a toast, with the sub-agents' work
//! unfinished. taw adds a rule that reads the list Claude draws under the box, one row for
//! each sub-agent, and a row that is still counting keeps the pane `working`.
//!
//! The fake is a shell script named `node`, as npm's Claude Code is, and the pane is given
//! its hint by hand, as `agent start` gives it. It draws the screens of those moments, the
//! captures `tests/detect_fixtures.rs` reads, and moves to the next on each SIGUSR1: the
//! prompt box at rest, a turn, the box back with a sub-agent still counting, and the box
//! with every sub-agent finished. The watcher is a real `taw watch run` on a throwaway tmux
//! server, and every state is read from tmux's `@agent_*` options.

mod common;

use std::process::Command;
use std::time::Duration;

use common::{Server, ServerOptions, read_pid, repo_root, wait_for, write_script};

/// Ceiling for "the watcher noticed and wrote the new state". A new agent reads `unknown`
/// for 3 s and one more tick, and `cargo test` runs these files in parallel, so the
/// ceiling is well above what an idle machine needs.
const SETTLE: Duration = Duration::from_secs(20);

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// `/bin/sh` body of the fake `node`. `PIDFILE` is replaced by where it records its pid and
/// `SCREENS` by the directory of the screens it draws. The title is Claude's: a spinner
/// frame while a turn runs, the resting `✳` otherwise.
const FAKE_CLAUDE: &str = r#"printf '%s\n' "$$" > 'PIDFILE'
title() { printf '\033]2;%s\007' "$1"; }
draw() { printf '\033[2J\033[H'; cat "SCREENS/$1"; }
step=0
render() {
  case "$step" in
    0) title '✳ fake task'; draw claude-idle.txt ;;
    1) title '✻ fake task'; draw claude-2.1.227-working.txt ;;
    2) title '✳ fake task'; draw claude-subagents-running.txt ;;
    3) title '✳ fake task'; draw claude-subagents-idle.txt ;;
  esac
}
trap 'step=$(( (step + 1) % 4 )); render' USR1
trap 'exit 0' TERM INT
render
while :; do sleep 0.2; done
"#;

/// Waits for `@agent_state` to reach `state` with `@agent_rule` at `rule`.
fn expect(server: &Server, pane: &str, state: &str, rule: &str) {
    assert!(
        wait_for(
            || server.pane_opt(pane, "@agent_state") == state
                && server.pane_opt(pane, "@agent_rule") == rule,
            SETTLE
        ),
        "wanted {state:?} by {rule:?}, got {:?} by {:?}\n--- screen ---\n{}\n--- log ---\n{}",
        server.pane_opt(pane, "@agent_state"),
        server.pane_opt(pane, "@agent_rule"),
        server.capture(pane),
        watcher_logs(server),
    );
}

/// The watcher's own log, for a failure message: it is a child of the tmux server, so its
/// `TAW_LOG` is the one the server was started with.
fn watcher_logs(server: &Server) -> String {
    let mut out = String::new();
    if let Ok(entries) = std::fs::read_dir(server.state_dir().join("logs")) {
        for entry in entries.flatten() {
            out.push_str(&format!("== {}\n", entry.path().display()));
            out.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
        }
    }
    out
}

fn signal(pid: &str) {
    let status = Command::new("kill").args(["-USR1", pid]).status().unwrap();
    assert!(status.success(), "kill -USR1 {pid} failed");
}

#[test]
fn a_claude_at_its_prompt_stays_working_while_a_sub_agent_runs_and_is_done_when_the_last_stops() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("fake-bin");
    let pid_file = tmp.path().join("claude.pid");
    let screens = repo_root().join("tests/fixtures/screens");
    write_script(
        &bin_dir.join("node"),
        &FAKE_CLAUDE
            .replace("PIDFILE", &pid_file.display().to_string())
            .replace("SCREENS", &screens.display().to_string()),
    );
    // Wide enough for the sub-agent list: its rows are 100 columns, and a narrower pane
    // would wrap them into lines no row is on.
    let server = Server::start_with(ServerOptions {
        width: 120,
        height: 30,
        path_prefix: Some(bin_dir),
        env: vec![("TAW_LOG".to_string(), "info".to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();

    // What `agent start --kind claude` leaves for a Claude that npm runs as `node`: the
    // hint, and a pane whose command names no agent.
    server.set_pane_opt(&pane, "@agent_kind_hint", "claude");
    server.tmux_ok(&["send-keys", "-t", &pane, "node", "Enter"]);
    assert!(
        wait_for(
            || server.capture(&pane).contains("Done. The tests pass"),
            SETTLE
        ),
        "the fake never drew its prompt box:\n{}",
        server.capture(&pane)
    );
    assert!(
        wait_for(|| read_pid(&pid_file).is_some(), SETTLE),
        "the fake never recorded its pid"
    );
    let pid = read_pid(&pid_file).unwrap();
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    // The prompt box at rest, no sub-agents: idle, as herdr reads it.
    expect(&server, &pane, "idle", "live_prompt_box");
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "claude");

    // A turn.
    signal(&pid);
    expect(&server, &pane, "working", "live_turn_working");

    // The turn ends with a sub-agent still counting under the prompt box: the pane is
    // still working, by taw's rule, and `agent explain` says whose rule it is.
    signal(&pid);
    expect(&server, &pane, "working", "taw_subagent_running");
    let explained = server.ok(
        &["agent", "explain", &pane],
        "cli:agent:explain",
        "agent_explain",
    );
    assert_eq!(explained["state"], "working", "{explained}");
    assert_eq!(
        explained["matched_rule"]["id"], "taw_subagent_running",
        "{explained}"
    );
    assert_eq!(explained["matched_rule"]["source"], "taw", "{explained}");

    // It stays there: the watcher rechecks a quiet agent pane every 5 s, and the pane is
    // still working after it has, with no `done` written in between.
    assert!(
        !wait_for(
            || server.pane_opt(&pane, "@agent_state") != "working",
            Duration::from_millis(6500)
        ),
        "the pane left working with a sub-agent still running: {:?} by {:?}",
        server.pane_opt(&pane, "@agent_state"),
        server.pane_opt(&pane, "@agent_rule"),
    );

    // The sub-agent's row goes to `idle`: the box at rest with nothing counting is the
    // work finished, and a finish behind the user's back is `done`, not `idle`.
    signal(&pid);
    expect(&server, &pane, "done", "live_prompt_box");
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "claude");
}
