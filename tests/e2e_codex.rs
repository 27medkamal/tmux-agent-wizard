//! A Codex that only its launch hint names, end to end through the built `taw` binary.
//!
//! npm's Codex runs as `node`, so tmux reports `node` for its pane, and the one thing that
//! says the pane holds Codex is the hint `taw agent start --kind codex` records. herdr's
//! Codex manifest reads such a pane idle through its terminal title: any title that is not a
//! spinner or "Action Required" is `osc_title_idle`. Where the title says nothing, the pane
//! matches nothing at its own empty composer and rests at `unknown`, so taw adds a rule for
//! the composer. A Codex that never sets a title is such a pane: tmux titles it with
//! the host name, which taw reads as no title. Both are run here: a Codex with a title of
//! its own, and one that sets none and keeps tmux's.
//!
//! The fake is a shell script named `node`, so the pane's command names no agent, and the
//! hint is set on the pane by hand, as `agent start` sets it. It draws the screens a Codex
//! draws, and moves to the next one on each SIGUSR1: the composer at rest, a turn in its
//! status line, an approval, and the composer again. The watcher is a real `taw watch run`
//! on a throwaway tmux server, and every state is read from tmux's `@agent_*` options.

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

/// `/bin/sh` body of the fake `node`; `PIDFILE` is replaced by where it records its pid and
/// `TITLE_CALL` by the call that sets its title, or by nothing. The title stays the same
/// throughout, the project's name as it is when Codex's title spinner is off, or tmux's own,
/// so it is the screen that says what Codex is doing.
const FAKE_CODEX: &str = r#"printf '%s\n' "$$" > 'PIDFILE'
title() { printf '\033]2;%s\007' "$1"; }
draw() { printf '\033[2J\033[H%s\n' "$1"; }
step=0
render() {
  TITLE_CALL
  case "$step" in
    0) draw '╭────────────────────────────────────────────────╮
│ >_ OpenAI Codex (v0.128.0)                     │
│                                                │
│ model:     gpt-5.6-sol high   /model to change │
│ directory: ~/code/project                      │
╰────────────────────────────────────────────────╯

  To get started, describe a task or try one of these commands:

  /init - create an AGENTS.md file with instructions for Codex
  /status - show current session configuration

› Explain this codebase

  ? for shortcuts                                100% context left' ;;
    1) draw '• Working (12s • esc to interrupt)

› Ask Codex to do anything

  gpt-5.6-sol default · /work' ;;
    2) draw '  Would you like to run the following command?

  $ rm -rf build

› 1. Yes, proceed (y)
  2. Yes, and don'\''t ask again for this command (a)
  3. No, and tell Codex what to do differently (esc)

  Press enter to confirm or esc to cancel' ;;
    3) draw '• All three tests pass now.

─ Worked for 1m 16s ─

› Ask Codex to do anything

  gpt-5.6-sol default · /work' ;;
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

/// A hint-only Codex that sets `title`, or none, through one rest, a turn, an approval and the
/// end of the turn. `idle_rule` is the rule that reads its composer at rest and `rule_source`
/// whose rule that is, as `agent explain` names it.
fn run_hint_only_codex(title: Option<&str>, idle_rule: &str, rule_source: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("fake-bin");
    let pid_file = tmp.path().join("codex.pid");
    write_script(
        &bin_dir.join("node"),
        &FAKE_CODEX
            .replace("PIDFILE", &pid_file.display().to_string())
            .replace(
                "TITLE_CALL",
                &title.map_or_else(String::new, |title| format!("title '{title}'")),
            ),
    );
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin_dir),
        env: vec![("TAW_LOG".to_string(), "info".to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();

    // What `agent start --kind codex` leaves for a Codex that npm runs as `node`: the
    // hint, and a pane whose command names no agent.
    server.set_pane_opt(&pane, "@agent_kind_hint", "codex");
    server.tmux_ok(&["send-keys", "-t", &pane, "node", "Enter"]);
    assert!(
        wait_for(
            || server.capture(&pane).contains("Explain this codebase"),
            SETTLE
        ),
        "the fake never drew its composer:\n{}",
        server.capture(&pane)
    );
    assert!(
        wait_for(|| read_pid(&pid_file).is_some(), SETTLE),
        "the fake never recorded its pid"
    );
    let pid = read_pid(&pid_file).unwrap();
    let command = server.display(&pane, "#{pane_current_command}");
    assert!(
        !command.to_lowercase().contains("codex"),
        "the pane's command must not name Codex, it is {command:?}"
    );
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    // The empty composer is idle, by the rule that can read it: not `unknown`, which is what
    // a hint with no rule behind it reads as.
    expect(&server, &pane, "idle", idle_rule);
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "codex");
    assert_eq!(server.pane_opt(&pane, "@agent_source"), "manifest");

    // `agent explain` reads the same screen the same way, and says whose rule it is.
    let explained = server.ok(
        &["agent", "explain", &pane],
        "cli:agent:explain",
        "agent_explain",
    );
    assert_eq!(explained["state"], "idle", "{explained}");
    assert_eq!(explained["matched_rule"]["id"], idle_rule, "{explained}");
    assert_eq!(
        explained["matched_rule"]["source"], rule_source,
        "{explained}"
    );

    // It rests there: the watcher rechecks a quiet agent pane every 5 s, and the pane is
    // still idle after it has.
    assert!(
        !wait_for(
            || server.pane_opt(&pane, "@agent_state") != "idle",
            Duration::from_millis(6500)
        ),
        "the pane left idle at rest: {:?} by {:?}",
        server.pane_opt(&pane, "@agent_state"),
        server.pane_opt(&pane, "@agent_rule"),
    );

    // A turn shows in its status line, and the composer under it does not make it idle.
    signal(&pid);
    expect(&server, &pane, "working", "screen_working_fallback");

    // An approval is blocked.
    signal(&pid);
    expect(&server, &pane, "blocked", "live_strong_blocker");

    // The composer again, with nobody looking at the pane: the work is finished, and a
    // finish behind the user's back is `done`, not `idle`.
    signal(&pid);
    expect(&server, &pane, "done", idle_rule);
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "codex");
}

/// A Codex with a title of its own: herdr's `osc_title_idle` reads it, and taw's composer
/// rule loses the tie at the same priority.
#[test]
fn a_codex_named_only_by_its_hint_rests_at_idle_and_reads_its_turns() {
    run_hint_only_codex(Some("project"), "osc_title_idle", "manifest");
}

/// A Codex that never sets a title, so the pane keeps the host name tmux gave it: taw reads
/// that as no title, herdr's rule has nothing to read, and the composer rule is what keeps the
/// pane at idle, and at `done` when a turn ends. `agent explain` reads it the same way.
#[test]
fn a_codex_that_sets_no_title_rests_at_idle_by_the_composer_rule() {
    run_hint_only_codex(None, "taw_composer_idle", "taw");
}
