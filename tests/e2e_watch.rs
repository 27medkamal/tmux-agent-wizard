//! The watcher, the notification path and the CLI's read verbs, end to end **through the
//! built `taw` binary** against a throwaway tmux server running a fake Claude Code.
//!
//! `tests/tmux_watch.rs` drives `watch::run_with_reader` in-process, with a stubbed
//! binary and a pipe this test suite writes into. That pins the loop's logic. What it
//! cannot prove is that the *assembled product* works: that `taw watch attach` really
//! opens a `pipe-pane` onto a real `taw watch run`, that the watcher process tmux spawns
//! finds its config and state (it is a child of the tmux server, not of any `taw`
//! invocation), that `taw agent start` launches an agent the watcher then classifies, and
//! that `taw agent wait` / `taw seen` / `taw watch status` / `taw watch detach` agree with
//! the `@agent_*` options tmux is actually holding.
//!
//! Everything here goes through `env!("CARGO_BIN_EXE_taw")` and is verified by reading
//! tmux directly, never through `taw`'s library API.

mod common;

use std::sync::Mutex;
use std::time::Duration;

use common::{FakeAgent, NotifyRecorder, Server, ServerOptions, wait_for};

/// Serialises the tests in this file, the same way `tests/tmux_watch.rs` does.
///
/// `cargo test` runs them as concurrent threads of one process, and each of these five
/// holds a tmux server running a real fake agent plus one or two real `taw watch run`
/// watcher processes. Run at once they contend for the machine hard enough that the poll
/// ceilings below — which are ceilings, not sleeps, and stay as they are — start being
/// reached before the thing they wait for happens: about three of the five failed under
/// that load. Serialising is what makes them measure the product rather than the host.
static SERIAL: Mutex<()> = Mutex::new(());

/// Ceiling for "the watcher noticed and wrote the new state".
///
/// A change the pane prints is written on the next tick (`Timing::default()`: 300 ms)
/// plus a few tmux round trips — well under a second on an idle machine. Two waits are
/// longer by design, and the ceiling has to clear both with room to spare: an agent
/// `agent start` has just launched reads `unknown` for the 3 s startup grace and one
/// more tick before its first real state, and a change the pane does not print, such
/// as `@agent_rule` unset behind the watcher's back, waits for the identity recheck,
/// 5 s after the last read. `cargo test` runs these files in parallel, each test holding
/// its own tmux server and one or two real watcher processes, and a ceiling tuned to an
/// idle machine is how a suite becomes flaky. It stays inside the suite's 10 s cap
/// for any poll.
const SETTLE: Duration = Duration::from_secs(8);

/// Ceiling for a one-shot side effect that does not wait on an evaluation (a pipe
/// opening, an option being written by a command that already returned).
const IMMEDIATE: Duration = Duration::from_secs(3);

/// Ceiling for "the agent process exited and the watcher cleared the pane's `@agent_*`
/// options". Kept as its own constant rather than reusing `SETTLE`: this figure is
/// chosen for this specific check, not derived headroom over an observed latency.
const TERM_CLEARS: Duration = Duration::from_secs(3);

/// `[update] manifest_check = false` keeps the watcher's manifest refresh off (the
/// harness also pre-touches the marker, so nothing in this suite reaches the network);
/// `[ui.sound] enabled = false` keeps `afplay`/`paplay` out of a test run.
const CONFIG_NO_TOASTS: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// The notification config of the `system` delivery, with both notification
/// delays removed so the test does not have to wait either out. `$TAW_NOTIFY_CMD` (set on
/// the server, so the watcher inherits it) stands in for the OS notifier.
const CONFIG_SYSTEM_TOASTS: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"system\"
delay_seconds = 0
done_delay_seconds = 0

[ui.sound]
enabled = false
";

/// Starts a server whose panes can run the fake agent as `claude`, and returns it
/// alongside the launcher. The tempdir holding the wrapper has to outlive the server, so
/// it is returned too.
fn server_with_fake_agent(
    config: &str,
    extra_env: Vec<(String, String)>,
) -> (Server, FakeAgent, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let fake = FakeAgent::new(tmp.path());
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(fake.bin_dir.clone()),
        env: extra_env,
        ..ServerOptions::default()
    });
    server.write_config(config);
    (server, fake, tmp)
}

/// Waits for `@agent_state` to reach `want`, reporting what it was stuck on.
fn expect_state(server: &Server, pane: &str, want: &str, timeout: Duration) {
    assert!(
        wait_for(|| server.pane_opt(pane, "@agent_state") == want, timeout),
        "@agent_state never reached {want:?} (still {:?}, rule {:?})\n--- screen ---\n{}",
        server.pane_opt(pane, "@agent_state"),
        server.pane_opt(pane, "@agent_rule"),
        server.capture(pane),
    );
}

/// The whole watcher lifecycle, driven only by the CLI and signals.
#[test]
fn watch_attach_classifies_a_real_agent_through_its_whole_lifecycle_and_detaches() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let (server, fake, _tmp) = server_with_fake_agent(CONFIG_NO_TOASTS, Vec::new());
    let pane = server.pane();

    // --- attach: a pipe onto a real `taw watch run` ------------------------------------
    assert_eq!(server.piped(&pane), "0", "a fresh pane is not piped");
    let attached = server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    assert_eq!(attached["attached"], true, "{attached}");
    assert_eq!(attached["terminal_id"], pane, "{attached}");
    assert!(
        wait_for(|| server.piped(&pane) == "1", IMMEDIATE),
        "`watch attach` did not open a pipe-pane"
    );
    // The watcher records its own pid, which is what `watch status` reports liveness from
    // and what proves a real process (not just a pipe) is on the other end.
    assert!(
        wait_for(
            || server
                .pane_opt(&pane, "@agent_watch_pid")
                .parse::<u32>()
                .is_ok(),
            IMMEDIATE
        ),
        "@agent_watch_pid was never set to a pid (got {:?})",
        server.pane_opt(&pane, "@agent_watch_pid")
    );
    // A watcher on a plain shell records no agent: identity comes from the pane's command
    // and `@agent_kind_hint`, never from the screen.
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "");

    // --- the product launches the agent ------------------------------------------------
    // `agent start` sets `@agent_kind_hint`, types the canonical executable (`claude`,
    // which the harness put on this pane's PATH as the fake) and waits until the agent is
    // there. It is the only supported way to make a `#!`-script fixture readable as an
    // agent: on macOS `#{pane_current_command}` reports the interpreter, which names no
    // agent, so what says the agent has started is the watcher attached above, which reads
    // a state off the screen once the 3 seconds a new agent is given are over.
    let started = server.ok(
        &[
            "agent", "start", "fake", "--kind", "claude", "--pane", &pane,
        ],
        "cli:agent:start",
        "agent",
    );
    assert_eq!(started["agent"]["agent"], "claude", "{started}");
    assert_eq!(started["agent"]["name"], "fake", "{started}");
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "claude");
    assert_eq!(server.pane_opt(&pane, "@agent_name"), "fake");

    expect_state(&server, &pane, "idle", SETTLE);
    assert_eq!(
        server.pane_opt(&pane, "@agent_source"),
        "manifest",
        "the state came from the manifest engine, not a hook"
    );
    assert!(
        server
            .pane_opt(&pane, "@agent_state_since")
            .parse::<u64>()
            .is_ok_and(|since| since > 1_700_000_000),
        "@agent_state_since should be a unix timestamp, got {:?}",
        server.pane_opt(&pane, "@agent_state_since")
    );
    // `agent start` returned only because the watcher had read a state off the screen, so
    // the first `idle` is already the drawn prompt box's, and carries the rule that read it.
    assert!(
        !server.pane_opt(&pane, "@agent_rule").is_empty(),
        "the state the watcher read off the screen has no rule behind it"
    );
    let agent_pid = fake.wait_until_drawn(&server, &pane, SETTLE);

    // --- working ------------------------------------------------------------------------
    fake.signal("-USR1");
    expect_state(&server, &pane, "working", SETTLE);
    assert_eq!(
        server.pane_opt(&pane, "@agent_rule"),
        "live_turn_working",
        "the working state should name claude.toml's live-turn rule"
    );
    // A new turn resets `@agent_seen`: that flag decides, at the *next* `working -> idle`,
    // whether the user is told the agent finished.
    assert_eq!(server.pane_opt(&pane, "@agent_seen"), "0");

    // --- blocked, and the CLI's own wait ------------------------------------------------
    fake.signal("-USR2");
    expect_state(&server, &pane, "blocked", SETTLE);
    // The screen the fake draws is a permission prompt, so the rule behind `blocked` must
    // be one that raises the visible-blocker evidence flag, not the priority-300
    // `legacy_no_prompt_blocker` fallback (which cannot tell a real prompt from
    // prompt-like scrollback). Which of the visible-blocker rules wins is claude.toml's
    // business, so the assertion is on the set.
    let blocked_rule = server.pane_opt(&pane, "@agent_rule");
    assert!(
        [
            "bash_permission_prompt",
            "generic_permission_prompt",
            "live_blocked_form",
        ]
        .contains(&blocked_rule.as_str()),
        "blocked should be explained by a visible-blocker rule, got {blocked_rule:?}"
    );

    // `agent wait` addresses the agent by the name `agent start` gave it, and returns 0
    // once tmux is holding the requested state.
    let (code, env) = server.taw(&[
        "agent",
        "wait",
        "fake",
        "--state",
        "blocked",
        "--timeout",
        "3000",
    ]);
    assert_eq!(code, 0, "`agent wait --state blocked` failed: {env}");
    assert_eq!(env["result"]["agent"]["agent_status"], "blocked", "{env}");

    // Every transition recomputes the rail; one blocked agent is a non-zero count in it.
    assert!(
        wait_for(
            || {
                let rail = server.global_opt("@agent_wizard_status");
                !rail.is_empty() && rail.contains('1')
            },
            SETTLE
        ),
        "@agent_wizard_status should carry a non-zero count, got {:?}",
        server.global_opt("@agent_wizard_status")
    );

    // --- back at its prompt: the first transition that arrives from a drawn screen -------
    // Nothing is attached to this server, so every pane in it is background by definition
    // (`notify::is_background`), and `idle` after `blocked` is a completion, as herdr
    // counts them: the agent's return to its prompt, with nobody looking, is `done`.
    assert_eq!(
        server.display(&pane, "#{session_attached}"),
        "0",
        "no client may be attached, or this would be a foreground finish"
    );
    fake.signal("-USR1");
    expect_state(&server, &pane, "done", SETTLE);
    assert_eq!(
        server.pane_opt(&pane, "@agent_rule"),
        "live_prompt_box",
        "arriving from the drawn prompt box should name claude.toml's idle rule"
    );

    // --- a fresh turn, glanced at mid-turn, finished while nobody is looking: `done` ----
    fake.signal("-USR1");
    expect_state(&server, &pane, "working", SETTLE);
    // The user looks in while it works: `taw seen`, as tmux's focus hooks run it. Where
    // they are looking when the turn ends is what counts, so the finish, which nobody
    // sees, is still `done` — and a `done` is unseen, whatever the glance left.
    server.ok(&["seen", &pane], "cli:seen", "ok");
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_seen") == "1", IMMEDIATE),
        "`taw seen` must record the glance"
    );
    fake.signal("-USR1");
    expect_state(&server, &pane, "done", SETTLE);
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_seen") == "0", IMMEDIATE),
        "a `done` must be unseen, got @agent_seen {:?}",
        server.pane_opt(&pane, "@agent_seen")
    );

    // --- `taw seen` releases done -------------------------------------------------------
    let seen = server.ok(&["seen", &pane], "cli:seen", "ok");
    assert_eq!(seen["terminal_id"], pane, "{seen}");
    assert_eq!(
        server.pane_opt(&pane, "@agent_state"),
        "idle",
        "`taw seen` must demote done to idle"
    );
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_seen") == "1", IMMEDIATE),
        "`taw seen` must record the pane as seen"
    );

    // --- `watch status` sees a live watcher on this pane --------------------------------
    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    assert_eq!(status["watched"], 1, "{status}");
    let row = status["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["terminal_id"] == pane.as_str())
        .unwrap_or_else(|| panic!("pane {pane} missing from watch status: {status}"));
    assert_eq!(row["piped"], true, "{row}");
    assert_eq!(row["watcher_alive"], true, "{row}");
    let watch_pid = row["watch_pid"].as_u64().unwrap_or_else(|| panic!("{row}"));
    assert_eq!(
        watch_pid.to_string(),
        server.pane_opt(&pane, "@agent_watch_pid"),
        "`watch status` should report the pid tmux is holding: {row}"
    );
    assert_ne!(
        watch_pid.to_string(),
        agent_pid,
        "the watcher is its own process, not the agent it watches"
    );
    assert_eq!(row["agent"]["agent"], "claude", "{row}");

    // --- detach closes the pipe and clears the pid --------------------------------------
    let detached = server.ok(&["watch", "detach", &pane], "cli:watch:detach", "ok");
    assert_eq!(detached["terminal_id"], pane, "{detached}");
    assert!(
        wait_for(|| server.piped(&pane) == "0", IMMEDIATE),
        "`watch detach` did not close the pipe"
    );
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_watch_pid").is_empty(),
            IMMEDIATE
        ),
        "@agent_watch_pid must be cleared once the watcher exits, got {:?}",
        server.pane_opt(&pane, "@agent_watch_pid")
    );
}

/// The watcher's process-exit path: stopping the agent process with `SIGTERM`
/// must leave the pane no longer claiming to hold a working agent.
///
/// What that means depends on where the pane's identity came from, and the two cases are
/// genuinely different:
///
/// * **Process-identified.** Nothing claims the pane once the process is gone, so
///   `Decision::NoAgent { clear: true }` clears every `@agent_*` option. Covered by
///   `tests/tmux_watch.rs`, which can control `#{pane_current_command}` directly.
/// * **Hint-identified**, which is this test. `taw agent start` writes
///   `@agent_kind_hint`, and `store::CLEARED_ON_AGENT_EXIT` deliberately excludes it —
///   the hint outlives any one occupant, as it does in herdr. So `detect::identify` keeps
///   answering `Some("claude")` after the process exits and the `NoAgent` path is
///   unreachable, and the pane would sit at whatever state the agent left behind, for ever.
///   The command-name fallback cannot rescue it either, at least on macOS: the fixture's
///   `#!/usr/bin/env bash` shebang makes `#{pane_current_command}` report "bash" while it
///   runs, and macOS's `/bin/sh` is bash underneath, so an idle prompt reports "bash" too.
///
/// The rule (`src/watch/state.rs`, step 5 of `decide`) keeps the hint and reports what
/// is actually known: identity from the hint alone, a foreground command that is not an
/// agent, and no manifest rule matching the screen means `unknown` with no rule. `unknown`
/// does not toast — it is the watcher admitting it cannot tell, not news.
#[test]
fn stopping_a_hinted_agent_with_term_leaves_the_pane_unknown_rather_than_stale() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let (server, fake, _tmp) = server_with_fake_agent(CONFIG_NO_TOASTS, Vec::new());
    let pane = server.pane();

    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    server.ok(
        &[
            "agent", "start", "fake", "--kind", "claude", "--pane", &pane,
        ],
        "cli:agent:start",
        "agent",
    );
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "claude");
    fake.wait_until_drawn(&server, &pane, SETTLE);
    expect_state(&server, &pane, "idle", SETTLE);
    assert_eq!(
        server.pane_opt(&pane, "@agent_rule"),
        "live_prompt_box",
        "precondition: the drawn screen explains the idle state"
    );

    fake.signal("-TERM");

    // Two things outlive the process and keep saying "claude is here": its last screen,
    // and the OSC 2 title it set. On a real machine both go on their own — the user types
    // something, and an interactive shell's prompt hook sets the title on every prompt.
    // This pane runs `/bin/sh` with no rc file, so neither happens by itself; `clear` and
    // `select-pane -T` stand in for the shell taking its terminal back. Only then is the
    // pane in the state this finding is about: agent gone, nothing on the pane saying
    // otherwise, and `@agent_kind_hint` the only thing left claiming one.
    server.tmux_ok(&["send-keys", "-t", &pane, "clear", "Enter"]);
    server.tmux_ok(&["select-pane", "-t", &pane, "-T", ""]);
    assert!(
        wait_for(
            || !server.capture(&pane).contains("? for shortcuts"),
            IMMEDIATE
        ),
        "the agent's screen should be gone after `clear`, got:\n{}",
        server.capture(&pane)
    );

    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_state") == "unknown",
            TERM_CLEARS
        ),
        "TERM should leave the pane `unknown` within {TERM_CLEARS:?}, got state={:?} rule={:?}",
        server.pane_opt(&pane, "@agent_state"),
        server.pane_opt(&pane, "@agent_rule"),
    );
    assert_eq!(
        server.pane_opt(&pane, "@agent_rule"),
        "",
        "no rule can explain a state nothing on screen showed"
    );
    // The hint is what keeps the identity, and it is deliberately kept: the pane is still
    // the place a claude belongs, which is why this is `unknown` and not "no agent".
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "claude");
    assert_eq!(server.pane_opt(&pane, "@agent_kind_hint"), "claude");
}

/// `@agent_rule` is the pane's *evidence*: the manifest rule that explains the state, and
/// what `taw agent get`, the popup and `watch status` all surface as the reason it is in
/// that state. It is written whenever the rule changes, not on a state transition only: a
/// rule that went stale while the state stood still would never get corrected, and for an
/// agent waiting at its prompt the state stands still for most of a session.
///
/// Both halves of that are exercised here, in the order a real pane meets them:
///
/// 1. A pane with a kind hint and no agent yet is `unknown` with no rule — nothing but the
///    hint says a claude belongs here, and nothing on screen says what it is doing.
/// 2. The agent starts and draws its prompt box: `unknown -> idle`, with the rule that
///    explains it.
/// 3. The evidence is then destroyed while the state stays exactly where it is. Only
///    `Decision::RuleChanged` puts it back; a transition-only writer cannot, because there
///    is no transition. This is also the mechanism behind the load-sensitive `@agent_rule`
///    assertion in `tests/tmux_watch.rs`, where a capture landing mid-redraw recorded an
///    empty rule that then became permanent.
#[test]
fn agent_rule_should_explain_the_state_once_the_agent_has_drawn_its_screen() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let (server, fake, _tmp) = server_with_fake_agent(CONFIG_NO_TOASTS, Vec::new());
    let pane = server.pane();

    // Identity before the agent exists: this is the documented hint mechanism, and it is
    // what `session restore` leaves behind on a pane whose agent is about to be resumed.
    server.set_pane_opt(&pane, "@agent_kind_hint", "claude");
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    expect_state(&server, &pane, "unknown", SETTLE);
    assert_eq!(
        server.pane_opt(&pane, "@agent_rule"),
        "",
        "a shell prompt matches no claude.toml rule, so nothing explains anything yet"
    );

    // Now the agent really starts and draws its prompt box, which `live_prompt_box`
    // matches.
    server.ok(
        &[
            "agent", "start", "fake", "--kind", "claude", "--pane", &pane,
        ],
        "cli:agent:start",
        "agent",
    );
    fake.wait_until_drawn(&server, &pane, SETTLE);
    // The engine agrees a rule now matches — `agent explain` reads the same screen.
    let explained = server.ok(
        &["agent", "explain", &pane],
        "cli:agent:explain",
        "agent_explain",
    );
    assert_eq!(
        explained["matched_rule"]["id"], "live_prompt_box",
        "the live screen does match the idle rule: {explained}"
    );
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_rule") == "live_prompt_box",
            SETTLE
        ),
        "@agent_rule still explains nothing ({:?}) although `agent explain` matches \
         live_prompt_box on the same screen",
        server.pane_opt(&pane, "@agent_rule")
    );
    expect_state(&server, &pane, "idle", SETTLE);
    let since = server.pane_opt(&pane, "@agent_state_since");
    assert_ne!(since, "", "precondition: a transition recorded when it was");

    // The state now stands still while the evidence is destroyed. Nothing about the pane's
    // status changes, so no transition will ever be written — the rule has to be restored
    // on its own or it stays empty for as long as the agent sits at its prompt.
    server.tmux_ok(&["set-option", "-u", "-p", "-t", &pane, "@agent_rule"]);
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_rule") == "live_prompt_box",
            SETTLE
        ),
        "@agent_rule was never restored ({:?}) although the screen still matches it",
        server.pane_opt(&pane, "@agent_rule")
    );
    assert_eq!(
        server.pane_opt(&pane, "@agent_state"),
        "idle",
        "restoring the evidence must not move the state"
    );
    assert_eq!(
        server.pane_opt(&pane, "@agent_state_since"),
        since,
        "and must not claim the state changed just now"
    );
}

/// The notification path, with the OS notifier replaced by the documented
/// `$TAW_NOTIFY_CMD` seam.
#[test]
fn a_blocked_agent_earns_a_request_notification_and_a_finished_one_earns_done() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    // The recorder has to live where the *tmux server* can run it, and its path has to be
    // known before the server starts (the watcher is a child of the server and inherits
    // its environment, so `$TAW_NOTIFY_CMD` cannot be added afterwards).
    let notify_tmp = tempfile::tempdir().unwrap();
    let recorder = NotifyRecorder::new(notify_tmp.path());
    let (server, fake, _tmp) = server_with_fake_agent(
        CONFIG_SYSTEM_TOASTS,
        vec![(
            "TAW_NOTIFY_CMD".to_string(),
            recorder.script.display().to_string(),
        )],
    );
    let pane = server.pane();

    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    assert!(wait_for(|| server.piped(&pane) == "1", IMMEDIATE));
    server.ok(
        &[
            "agent", "start", "fake", "--kind", "claude", "--pane", &pane,
        ],
        "cli:agent:start",
        "agent",
    );
    expect_state(&server, &pane, "idle", SETTLE);
    fake.wait_until_drawn(&server, &pane, SETTLE);

    // Nothing has happened that earns a notification yet: `idle` is not a transition that
    // earns a notification.
    assert!(
        recorder.lines().is_empty(),
        "an idle arrival must not notify: {:?}",
        recorder.lines()
    );

    // --- idle -> working -> blocked: one `request` -------------------------------------
    fake.signal("-USR1");
    expect_state(&server, &pane, "working", SETTLE);
    assert!(
        recorder.lines().is_empty(),
        "entering working must not notify: {:?}",
        recorder.lines()
    );

    fake.signal("-USR2");
    expect_state(&server, &pane, "blocked", SETTLE);
    assert!(
        wait_for(|| recorder.matching("blocked").len() == 1, SETTLE),
        "expected exactly one request notification, got {:?}",
        recorder.lines()
    );
    let request = recorder.matching("blocked").remove(0);
    // `notify::for_transition`'s documented title, with the body being the first non-empty
    // line of the region the matched rule looked at.
    assert!(
        request.starts_with("fake blocked \u{b7} one:0|"),
        "unexpected request title: {request:?}"
    );
    assert!(
        request.contains("rm -rf build/"),
        "the body should quote the prompt the agent is blocked on: {request:?}"
    );

    // --- blocked -> idle: one `done` ----------------------------------------------------
    // herdr counts `idle` after `blocked` as the agent's work ending, as it counts the end
    // of a turn: in the background it is `done`, and it earns the `done` notification.
    fake.signal("-USR1");
    expect_state(&server, &pane, "done", SETTLE);
    assert!(
        wait_for(|| recorder.matching("done").len() == 1, SETTLE),
        "expected one done notification for blocked -> idle, got {:?}",
        recorder.lines()
    );
    assert!(
        recorder.matching("done")[0].starts_with("fake done \u{b7} one:0|"),
        "unexpected done title: {:?}",
        recorder.matching("done")[0]
    );

    // --- working -> idle: another `done` ------------------------------------------------
    fake.signal("-USR1");
    expect_state(&server, &pane, "working", SETTLE);
    assert_eq!(
        recorder.matching("done").len(),
        1,
        "nothing new has finished yet: {:?}",
        recorder.lines()
    );
    fake.signal("-USR1");
    // Background, so the finish is recorded as `done` rather than `idle`.
    expect_state(&server, &pane, "done", SETTLE);
    assert!(
        wait_for(|| recorder.matching("done").len() == 2, SETTLE),
        "expected a second done notification, got {:?}",
        recorder.lines()
    );
    // Still exactly one request: a `done` must not re-fire the earlier one.
    assert_eq!(
        recorder.matching("blocked").len(),
        1,
        "{:?}",
        recorder.lines()
    );

    // --- the jump key's verb ------------------------------------------------------------
    let opened = server.ok(
        &["notification", "open-target"],
        "cli:notification:open-target",
        "ok",
    );
    assert_eq!(opened["pane_id"], pane, "{opened}");
    // It really selected the pane it reported.
    assert_eq!(server.display(&pane, "#{pane_active}"), "1");
}

/// The notification config with the done notification held back `done_delay` seconds, as the
/// default config holds it back five: long enough for the agent's session to change first.
fn config_with_done_delay(done_delay: u32) -> String {
    CONFIG_SYSTEM_TOASTS.replace(
        "done_delay_seconds = 0",
        &format!("done_delay_seconds = {done_delay}"),
    )
}

/// A `claude` on a pane shell's `$PATH` that is `tests/fixtures/fake-agents/claude-trust`: a Claude
/// that starts at its folder trust question, and is idle once `SIGUSR1` answers it (then each
/// `SIGUSR1` starts or ends a turn). Its pid is recorded as [`FakeAgent`] records the other fake's.
struct TrustingAgent {
    bin_dir: std::path::PathBuf,
    pid_file: std::path::PathBuf,
}

impl TrustingAgent {
    fn new(dir: &std::path::Path) -> TrustingAgent {
        let bin_dir = dir.join("fake-bin");
        let pid_file = dir.join("fake-agent.pid");
        let fixture = common::repo_root().join("tests/fixtures/fake-agents/claude-trust");
        common::write_script(
            &bin_dir.join("claude"),
            &format!(
                "printf '%s\\n' \"$$\" > '{}'\nexec '{}' \"$@\"\n",
                pid_file.display(),
                fixture.display(),
            ),
        );
        TrustingAgent { bin_dir, pid_file }
    }

    fn signal(&self, sig: &str) {
        let pid = common::read_pid(&self.pid_file).expect("the fake agent recorded no pid yet");
        let status = std::process::Command::new("kill")
            .args([sig, &pid])
            .status()
            .unwrap();
        assert!(status.success(), "kill {sig} {pid} failed");
    }
}

/// herdr's completion guard (#4457), on a real server: a Claude started in a session nobody is
/// attached to stops at its folder trust question, and is answered later. That is not `done`, with
/// a done notification for a turn that never ran: it is a request while it waits, and `idle` once
/// it is answered; the agent's first real turn is `done`, with its notification.
#[test]
fn a_trust_question_answered_in_a_background_session_ends_idle_with_no_done_notification() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let notify_tmp = tempfile::tempdir().unwrap();
    let recorder = NotifyRecorder::new(notify_tmp.path());
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let fake = TrustingAgent::new(tmp.path());
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(fake.bin_dir.clone()),
        env: vec![
            (
                "TAW_NOTIFY_CMD".to_string(),
                recorder.script.display().to_string(),
            ),
            ("HOME".to_string(), home.display().to_string()),
        ],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG_SYSTEM_TOASTS);
    let pane = server.pane();
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    assert!(wait_for(|| server.piped(&pane) == "1", IMMEDIATE));
    assert_eq!(
        server.display(&pane, "#{session_attached}"),
        "0",
        "no client may be attached: the session is in the background"
    );

    // `agent start` returns once the watcher has read a state off the screen: the question.
    server.ok(
        &[
            "agent", "start", "fake", "--kind", "claude", "--pane", &pane,
        ],
        "cli:agent:start",
        "agent",
    );
    expect_state(&server, &pane, "blocked", SETTLE);
    assert_eq!(server.pane_opt(&pane, "@agent_rule"), "live_blocked_form");
    // The agent is still being taken while it asks: the question is not the agent being ready.
    assert_eq!(server.pane_opt(&pane, "@agent_acquiring"), "taken");
    assert!(
        wait_for(|| recorder.matching("blocked").len() == 1, SETTLE),
        "the question is a request: {:?}",
        recorder.lines()
    );

    // --- the question answered: ready, not finished ------------------------------------
    fake.signal("-USR1");
    expect_state(&server, &pane, "idle", SETTLE);
    assert_eq!(server.pane_opt(&pane, "@agent_rule"), "live_prompt_box");
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_acquiring").is_empty(),
            IMMEDIATE
        ),
        "the agent is ready, and no longer being taken"
    );

    // --- its first real turn ------------------------------------------------------------
    // A notification is titled with the label the agent had when it was earned: from here on
    // that is "answered", so the turn's notification is told from one the question earned.
    server.set_pane_opt(&pane, "@agent_display", "answered");
    fake.signal("-USR1");
    expect_state(&server, &pane, "working", SETTLE);
    fake.signal("-USR1");
    expect_state(&server, &pane, "done", SETTLE);
    let titled = |prefix: &str| {
        recorder
            .lines()
            .into_iter()
            .filter(|line| line.starts_with(prefix))
            .count()
    };
    assert!(
        wait_for(|| titled("answered done \u{b7} ") == 1, SETTLE),
        "the turn's end earns a done notification: {:?}",
        recorder.lines()
    );
    // Earned a second or more before it, with no delay either, it would have gone out first.
    assert_eq!(
        titled("fake done \u{b7} "),
        0,
        "the answered question is no finished turn: {:?}",
        recorder.lines()
    );
    assert_eq!(
        recorder.matching("blocked").len(),
        1,
        "{:?}",
        recorder.lines()
    );
}

/// herdr's completion reset (#4457), on a real server: a turn ends behind the user's back,
/// `done`, and its notification waits out its delay. Before that is up the agent's session
/// changes (Claude's `/clear`, which its `SessionStart` hook reports as a new session id): the
/// pane goes back to `idle`, marked seen, and that notification is never sent. The next turn's is.
#[test]
fn a_new_session_inside_the_done_delay_turns_done_back_to_idle_and_drops_its_notification() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let notify_tmp = tempfile::tempdir().unwrap();
    let recorder = NotifyRecorder::new(notify_tmp.path());
    let home = tempfile::tempdir().unwrap();
    let (server, fake, _tmp) = server_with_fake_agent(
        &config_with_done_delay(3),
        vec![
            (
                "TAW_NOTIFY_CMD".to_string(),
                recorder.script.display().to_string(),
            ),
            ("HOME".to_string(), home.path().display().to_string()),
        ],
    );
    let pane = server.pane();
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    assert!(wait_for(|| server.piped(&pane) == "1", IMMEDIATE));
    server.ok(
        &[
            "agent", "start", "fake", "--kind", "claude", "--pane", &pane,
        ],
        "cli:agent:start",
        "agent",
    );
    expect_state(&server, &pane, "idle", SETTLE);
    fake.wait_until_drawn(&server, &pane, SETTLE);
    // What Claude's `SessionStart` hook sends (`taw-agent-state-claude.sh`).
    let session = |id: &str, reason: &str| {
        server.ok(
            &[
                "pane",
                "report-agent-session",
                &pane,
                "--source",
                "integration:claude",
                "--agent",
                "claude",
                "--agent-session-id",
                id,
                "--session-start-source",
                reason,
            ],
            "cli:pane:report-agent-session",
            "ok",
        )
    };
    session("before-clear", "startup");

    // --- a turn ends behind the user's back ---------------------------------------------
    fake.signal("-USR1");
    expect_state(&server, &pane, "working", SETTLE);
    fake.signal("-USR1");
    expect_state(&server, &pane, "done", SETTLE);

    // --- `/clear`, inside the three seconds the notification waits ---------------------
    // A report of a session is no nudge, and nothing prints here, so the watcher has not read
    // the pane when the notification comes due: what drops it is the session the pane holds
    // then. The watcher's identity recheck, five seconds after its last read, finds the new
    // session and the `done` the old one left.
    session("after-clear", "clear");
    expect_state(&server, &pane, "idle", SETTLE);
    assert_eq!(server.pane_opt(&pane, "@agent_seen"), "1");
    assert_eq!(server.pane_opt(&pane, "@agent_session_id"), "after-clear");

    // --- the next turn, whose notification is sent -------------------------------------
    // A notification is titled with the label the agent had when it was earned: from here on
    // that is "cleared", so the new session's notification is told from the old one's.
    server.set_pane_opt(&pane, "@agent_display", "cleared");
    fake.signal("-USR1");
    expect_state(&server, &pane, "working", SETTLE);
    fake.signal("-USR1");
    expect_state(&server, &pane, "done", SETTLE);
    let titled = |prefix: &str| {
        recorder
            .lines()
            .into_iter()
            .filter(|line| line.starts_with(prefix))
            .count()
    };
    assert!(
        wait_for(|| titled("cleared done \u{b7} ") == 1, SETTLE),
        "the new session's turn earns its notification: {:?}",
        recorder.lines()
    );
    // The old one waited out the same delay from an earlier start, so it came due first.
    assert_eq!(
        titled("fake done \u{b7} "),
        0,
        "the old session's notification must not be sent: {:?}",
        recorder.lines()
    );
}

/// The read verbs, over a server with a live agent in it.
#[test]
fn the_cli_read_verbs_describe_the_assembled_system() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let (server, fake, _tmp) = server_with_fake_agent(CONFIG_NO_TOASTS, Vec::new());
    let pane = server.pane();
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    server.ok(
        &[
            "agent", "start", "reviewer", "--kind", "claude", "--pane", &pane,
        ],
        "cli:agent:start",
        "agent",
    );
    expect_state(&server, &pane, "idle", SETTLE);
    fake.wait_until_drawn(&server, &pane, SETTLE);

    // --- pane list ---------------------------------------------------------------------
    let panes = server.ok(&["pane", "list"], "cli:pane:list", "pane_list");
    let rows = panes["panes"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{panes}");
    let row = &rows[0];
    assert_eq!(row["terminal_id"], pane, "{row}");
    assert!(
        regex::Regex::new(r"^w\d+:p\d+$")
            .unwrap()
            .is_match(row["pane_id"].as_str().unwrap()),
        "pane_id is not w<n>:p<n>: {row}"
    );
    assert_eq!(
        row["agent"], "claude",
        "a watched agent pane names its kind"
    );
    assert_eq!(row["agent_status"], "idle", "{row}");
    assert_eq!(row["name"], "reviewer", "{row}");

    // --- agent list --------------------------------------------------------------------
    let agents = server.ok(&["agent", "list"], "cli:agent:list", "agent_list");
    let agents = agents["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1, "one pane holds an agent: {agents:?}");
    assert_eq!(agents[0]["pane_id"], row["pane_id"]);
    assert_eq!(agents[0]["agent"], "claude");
    assert_eq!(agents[0]["agent_status"], "idle");

    // A pane with no agent is in `pane list` and not in `agent list`.
    server.tmux_ok(&["split-window", "-t", "one"]);
    assert!(
        wait_for(
            || server.ok(&["pane", "list"], "cli:pane:list", "pane_list")["panes"]
                .as_array()
                .unwrap()
                .len()
                == 2,
            IMMEDIATE
        ),
        "the split should show up in `pane list`"
    );
    let agents = server.ok(&["agent", "list"], "cli:agent:list", "agent_list");
    assert_eq!(
        agents["agents"].as_array().unwrap().len(),
        1,
        "a plain shell is not an agent: {agents}"
    );

    // --- agent explain, on the live screen ----------------------------------------------
    let explained = server.ok(
        &["agent", "explain", &pane],
        "cli:agent:explain",
        "agent_explain",
    );
    assert_eq!(explained["state"], "idle", "{explained}");
    assert!(
        !explained["evaluated_rules"].as_array().unwrap().is_empty(),
        "explain should report every rule it evaluated: {explained}"
    );

    // --- status ------------------------------------------------------------------------
    let status = server.ok(&["status"], "cli:status:show", "status");
    assert_eq!(status["counts"]["idle"], 1, "{status}");
    assert_eq!(status["counts"]["blocked"], 0, "{status}");
    assert!(!status["text"].as_str().unwrap().is_empty(), "{status}");

    // --- manifests, config, and the two raw-output verbs --------------------------------
    let manifests = server.ok(&["manifest", "list"], "cli:manifest:list", "manifest_list");
    assert_eq!(
        manifests["manifests"].as_array().unwrap().len(),
        22,
        "herdr publishes 22 screen manifests"
    );
    let claude = manifests["manifests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "claude")
        .unwrap_or_else(|| panic!("no claude manifest: {manifests}"));
    assert_eq!(claude["active_source"], "bundled", "{claude}");

    let check = server.ok(&["config", "check"], "cli:config:check", "config_check");
    assert_eq!(
        check["errors"], 0,
        "the test config must validate cleanly: {check}"
    );

    // `taw skill` and `taw config default` are the two documented exceptions to the
    // envelope: both exist to be redirected into a file.
    let (code, skill, stderr) = server.taw_raw(&["skill"]);
    assert_eq!(code, 0, "`taw skill` failed: {stderr}");
    let first = skill.lines().next().unwrap_or_default();
    assert!(
        first == "---" || first.starts_with('#'),
        "`taw skill` should print raw markdown, got {first:?}"
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(&skill).is_err(),
        "`taw skill` must not be wrapped in an envelope"
    );

    let (code, toml_text, stderr) = server.taw_raw(&["config", "default"]);
    assert_eq!(code, 0, "`taw config default` failed: {stderr}");
    toml_text
        .parse::<toml::Table>()
        .unwrap_or_else(|e| panic!("`taw config default` did not parse as TOML: {e}"));

    let (code, version, stderr) = server.taw_raw(&["--version"]);
    assert_eq!(code, 0, "`taw --version` failed: {stderr}");
    assert!(
        version.starts_with("taw "),
        "unexpected --version output: {version:?}"
    );
}
