// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//
// The screens here are those of herdr's Letta tests (src/detect/mod.rs, v0.9.1). The fake below
// draws them into a tmux pane with a shell script instead of handing them to `explain`; the
// working screen gains a composer and a footer under its status lines.

//! Letta Code, end to end through the built `taw` binary: `taw agent start --kind letta`
//! launches it in a pane, and the watcher reads its state from the chrome Letta draws and
//! from the title it sets.
//!
//! The fake is a shell script named `letta` that draws the screens herdr's own Letta tests
//! were written from, and moves to the next one on each SIGUSR1: the composer at rest, a
//! turn in its status line, a turn that only its title spinner shows, and a command
//! approval. The watcher is a real `taw watch run` on a throwaway tmux server, and every
//! state is read from tmux's `@agent_*` options, never from `taw`'s own library.

mod common;

use std::process::Command;
use std::time::Duration;

use common::{Server, ServerOptions, read_pid, wait_for, write_script};

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

/// `/bin/sh` body of the fake `letta`; `PIDFILE` is replaced by where it records its pid.
const FAKE_LETTA: &str = r#"printf '%s\n' "$$" > 'PIDFILE'
title() { printf '\033]2;%s\007' "$1"; }
draw() { printf '\033[2J\033[H%s\n' "$1"; }
step=0
render() {
  case "$step" in
    0) title 'Tutor'
       draw '────────────────
› Try "debug this error"
────────────────
Tutor · No model selected' ;;
    1) title 'Tutor'
       draw '✻ Thinking…
Tutor is reflecting… (esc to interrupt · 2m 3s)

────────────────
›
────────────────
Tutor · GPT-5.5' ;;
    2) title '⠋ Tutor'
       draw '────────────────
› Try "debug this error"
────────────────
Tutor · GPT-5.5' ;;
    3) title 'Tutor'
       draw '✻ Thinking…

────────────────────────────────────────────────────────────────
Run this command?

  $ rm -f /var/tmp/herdr-blocked-capture-never-created

❯ 1. Yes
  2. No, and tell Letta Code what to do differently

Enter to select · Esc to cancel' ;;
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
fn agent_start_launches_letta_and_the_watcher_reads_its_chrome() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("fake-bin");
    let pid_file = tmp.path().join("letta.pid");
    write_script(
        &bin_dir.join("letta"),
        &FAKE_LETTA.replace("PIDFILE", &pid_file.display().to_string()),
    );
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin_dir),
        env: vec![("TAW_LOG".to_string(), "info".to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();

    // The manifest is bundled.
    let shown = server.ok(
        &["manifest", "show", "letta"],
        "cli:manifest:show",
        "manifest",
    );
    assert_eq!(shown["id"], "letta", "{shown}");

    // The fake is a script, so tmux names its interpreter and not Letta: the watcher is what says
    // the agent is there, from the chrome it draws, and `agent start` returns only once it has.
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    // `agent start --kind letta` types `letta`, which is the fake on this pane's PATH.
    let started = server.ok(
        &[
            "agent", "start", "tutor", "--kind", "letta", "--pane", &pane,
        ],
        "cli:agent:start",
        "agent",
    );
    assert_eq!(started["agent"]["agent"], "letta", "{started}");
    assert_eq!(started["agent"]["name"], "tutor", "{started}");
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "letta");
    assert!(
        wait_for(
            || server.capture(&pane).contains("No model selected"),
            SETTLE
        ),
        "the fake never drew its composer:\n{}",
        server.capture(&pane)
    );
    // The file exists as soon as the shell opens it, and holds nothing until the pid lands.
    assert!(
        wait_for(|| read_pid(&pid_file).is_some(), SETTLE),
        "the fake never recorded its pid"
    );
    let pid = read_pid(&pid_file).unwrap();

    // The empty composer is idle.
    expect(&server, &pane, "idle", "composer_idle");
    assert_eq!(server.pane_opt(&pane, "@agent_source"), "manifest");

    // A turn shows in its status line.
    signal(&pid);
    expect(&server, &pane, "working", "active_status");

    // A turn that only the title's spinner shows is working too, by the title's rule.
    signal(&pid);
    expect(&server, &pane, "working", "osc_title_working");

    // A command approval is blocked.
    signal(&pid);
    expect(&server, &pane, "blocked", "command_approval");

    // Back at the composer after that, with nobody looking at the pane: the work is
    // finished, and a finish behind the user's back is `done`, not `idle`.
    signal(&pid);
    expect(&server, &pane, "done", "composer_idle");

    // Nothing above needed a hook or a hint beyond the one `agent start` set.
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "letta");
    assert_eq!(server.pane_opt(&pane, "@agent_source"), "manifest");
}
