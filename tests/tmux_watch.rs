//! The watcher, end to end, against a real tmux server and a real (fake) agent.
//!
//! The unit tests in `src/watch/` pin the decision table and the OSC scanner; what only a
//! live tmux server can prove is the loop itself: that pane output drives an evaluation,
//! that the evaluation writes the documented `@agent_*` options, that a background
//! `working -> idle` becomes `done` and `taw seen` releases it, that a `blocked` arrival
//! actually delivers a notification, and that EOF on the pipe shuts the watcher down
//! cleanly.
//!
//! The fake agent (`tests/fixtures/fake-agents/claude`) is a shell script that renders
//! the captured Claude Code screens and switches between them on `SIGUSR1`/`SIGUSR2`, so
//! the manifest rules see exactly the text `tests/detect_fixtures.rs` pins.

mod common;

use common::shell_env::ShellHome;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use taw::config::{Config, ToastDelivery};
use taw::paths::Paths;
use taw::tmux::Tmux;
use taw::watch::{self, Timing};

/// Serialises the tests in this file.
///
/// `cargo test` runs them as concurrent threads of one process, and the notification test
/// has to set `$TAW_NOTIFY_CMD` — a process-global mutation that must not race another
/// thread reading the environment (every `Tmux::run` spawns a child, which does).
static SERIAL: Mutex<()> = Mutex::new(());

/// A throwaway tmux server on its own `-L` socket, torn down on drop. Never touches the
/// user's real server. Same shape as `tests/tmux_client.rs`'s guard — see its comments
/// for why `-f /dev/null` and a per-`Server` socket name both matter.
struct Server(Tmux, #[allow(dead_code)] ShellHome);

impl Server {
    /// Starts a detached 80x24 session. `command`, when given, becomes the pane's own
    /// command instead of a shell — which is how the fake agent ends up being the pane's
    /// process (and so `#{pane_pid}` is the fake's own pid, signallable from here).
    fn start(command: Option<&str>) -> Server {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("taw-test-watch-{}-{n}", std::process::id());
        let t = Tmux::with_socket_name(&name);
        let home = ShellHome::new();
        let mut args = vec!["-d", "-s", "one", "-x", "80", "-y", "24"];
        // `/bin/sh` when the caller names no command, *and* `default-shell` below: `-f
        // /dev/null` isolates tmux's config but not the *shell's*. The host login shell may
        // print banners, run prompt tools, or open an interactive prompt of its own — in
        // this repo, `mise`'s "trust this config?" question in a fresh worktree — which
        // makes any test that types into a pane flaky for reasons that have nothing to do
        // with `taw`. `/bin/sh` reads no rc file. It has to be named twice because the
        // server does not outlive a `start-server` with no sessions, so there is no earlier
        // moment at which to set the option for this first pane. (Shared with
        // `tests/common/mod.rs`, which does the same for the `e2e_*` files.)
        args.push(command.unwrap_or("/bin/sh"));
        home.start_server(&t, &args);
        t.run_ok(&["set-option", "-g", "default-shell", "/bin/sh"])
            .unwrap();
        Server(t, home)
    }

    fn pane(&self) -> String {
        self.0.display("one", "#{pane_id}").unwrap()
    }

    /// One `@agent_*`/tmux pane option, `""` when unset — the shape every assertion here
    /// wants (tmux draws no distinction between unset and empty for our purposes).
    fn option(&self, pane: &str, name: &str) -> String {
        self.0
            .get_pane_option(pane, name)
            .unwrap()
            .unwrap_or_default()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.run_ok(&["kill-server"]);
    }
}

fn fake_agent() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/fake-agents/claude"
    )
}

fn test_paths(dir: &Path) -> Paths {
    let agent_detection = dir.join("state").join("agent-detection");
    let plugins_dir = dir.join("state").join("plugins");
    Paths {
        config_dir: dir.join("config"),
        config_file: dir.join("config").join("config.toml"),
        state_dir: dir.join("state"),
        override_dir: dir.join("config").join("agent-detection"),
        remote_manifest_dir: agent_detection.join("remote"),
        manifest_marker: agent_detection.join("last-check"),
        sessions_dir: dir.join("state").join("sessions"),
        plugin_state_dir: plugins_dir.join("state"),
        plugins_dir,
        logs_dir: dir.join("state").join("logs"),
    }
}

/// Config for these tests: no network, no sound and no toast, and no delay before a toast for
/// a test that turns them on ([`recording_config`]).
///
/// No toast, because the watchers here run in this process, so a `system` toast goes to
/// `$TAW_NOTIFY_CMD` only while a test has set it, and to the desktop's own notifier
/// (`osascript`) otherwise: a test that left a stale record on a plain shell (as the one that
/// survives tmux failures does) put "claude done" on the screen of whoever ran the tests.
fn test_config() -> Config {
    let mut cfg = Config::default();
    cfg.update.manifest_check = false;
    cfg.ui.toast.delivery = ToastDelivery::Off;
    cfg.ui.toast.delay_seconds = 0.0;
    cfg.ui.sound.enabled = false;
    cfg
}

/// [`test_config`] with toasts delivered through the `system` path, for a test that records
/// them: it sets `$TAW_NOTIFY_CMD` to its recorder before its watcher starts.
fn recording_config() -> Config {
    let mut cfg = test_config();
    cfg.ui.toast.delivery = ToastDelivery::System;
    cfg
}

/// Ticks short enough to keep the test quick, and an identity recheck long enough that
/// state changes have to come through the *byte* path — output making the pane eligible
/// for its next tick — rather than being caught by the periodic re-check.
/// `Timing::default()` (herdr's 300 ms / 500 ms / 100 ms / 5 s) is pinned by a unit test.
fn test_timing() -> Timing {
    Timing {
        identified: Duration::from_millis(150),
        unidentified: Duration::from_millis(150),
        hold_recheck: Duration::from_millis(50),
        identity_recheck: Duration::from_secs(5),
    }
}

/// One ceiling for every wait in this file: how long a side effect may take to show up
/// before the test calls it a failure.
///
/// Deliberately not tuned to an idle machine, and deliberately shared. Every test here is
/// serialised by `SERIAL`, but serialising them does not make the machine quiet: a test
/// leaves a fake agent (a `sleep 0.2` loop) and a real watcher winding down behind it, and
/// `cargo test` runs the other integration binaries alongside this one. A 3 s budget for
/// `pipe-pane`'s child to be scheduled and produce output failed about one run in six
/// under exactly that. The product promises none of these numbers — the watcher's own
/// tick here is 150 ms — so a generous ceiling costs nothing: `poll_until` returns
/// the moment its condition holds. Same reasoning as `SETTLE` in `tests/common/mod.rs`.
const SETTLE: Duration = Duration::from_secs(15);

fn poll_until(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if ready() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn signal(pid: &str, sig: &str) {
    let status = Command::new("kill").args([sig, pid]).status().unwrap();
    assert!(status.success(), "kill {sig} {pid} failed");
}

/// A real client attached to session `one` of `server`, as the user's terminal is: a
/// second throwaway server whose only pane runs `tmux attach` against the first (the e2e
/// harness's trick). With it the session is attached, and the pane's window is the one it
/// shows. Dropping it kills the second server, which detaches the client.
struct Client(Tmux, #[allow(dead_code)] ShellHome);

impl Client {
    fn attach(server: &Server) -> Client {
        let Some(taw::tmux::Socket::Name(socket)) = &server.0.socket else {
            panic!("the test server should use a named socket");
        };
        let terminal = Tmux::with_socket_name(&format!("{socket}-terminal"));
        let home = ShellHome::new();
        let bin = server.0.bin.display().to_string();
        // Separate arguments: tmux runs the command itself rather than through a shell.
        home.start_server(
            &terminal,
            &[
                "-d",
                "-s",
                "terminal",
                "-x",
                "80",
                "-y",
                "24",
                &bin,
                "-L",
                socket,
                "attach-session",
                "-t",
                "one",
            ],
        );
        let client = Client(terminal, home);
        assert!(
            poll_until(SETTLE, || server
                .0
                .display("one", "#{session_attached}")
                .is_ok_and(|attached| attached == "1")),
            "no client attached to the test server"
        );
        client
    }

    /// The user's terminal reports gaining or losing focus, as a terminal does once tmux
    /// asks it to (`focus-events on`): `ESC [ I` or `ESC [ O`, typed into the client's tty.
    fn report_focus(&self, focused: bool) {
        let report = if focused { "49" } else { "4f" };
        self.0
            .run_ok(&["send-keys", "-t", "terminal", "-H", "1b", "5b", report])
            .unwrap();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.0.run_ok(&["kill-server"]);
    }
}

/// A test that records toasts puts `$TAW_NOTIFY_CMD` back as it found it (the gate's refusing recorder, for
/// every test after it, or none), whether it ends or fails: a `remove_var` took away what the gate gave.
#[test]
fn a_recording_test_gives_back_the_notifier_it_found() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let had = std::env::var_os("TAW_NOTIFY_CMD");
    {
        let _gate = common::EnvVar::set("TAW_NOTIFY_CMD", "/the/gates/recorder");
        {
            let _mine = common::EnvVar::set("TAW_NOTIFY_CMD", "/my/recorder");
            assert_eq!(
                std::env::var_os("TAW_NOTIFY_CMD").as_deref(),
                Some(std::ffi::OsStr::new("/my/recorder"))
            );
        }
        assert_eq!(
            std::env::var_os("TAW_NOTIFY_CMD").as_deref(),
            Some(std::ffi::OsStr::new("/the/gates/recorder")),
            "the recorder the gate gave was not put back"
        );
    }
    assert_eq!(std::env::var_os("TAW_NOTIFY_CMD"), had);
    // And where there was none, none is put back.
    {
        let _probe = common::EnvVar::set("TAW_GUARD_PROBE", "x");
    }
    assert_eq!(std::env::var_os("TAW_GUARD_PROBE"), None);
}

/// A `$TAW_NOTIFY_CMD` script in `dir` that appends `<title>|<body>` to a file, one line
/// per `system` notification; returns the script and the file.
fn notification_recorder(dir: &Path) -> (PathBuf, PathBuf) {
    let recorded = dir.join("notifications.txt");
    let recorder = dir.join("record-notification.sh");
    std::fs::write(
        &recorder,
        format!(
            "#!/bin/sh\nprintf '%s|%s\\n' \"$1\" \"$2\" >> {}\n",
            recorded.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        crate::common::make_executable(&recorder);
    }
    (recorder, recorded)
}

/// Nudges the watcher with a byte of pane output until `@agent_state` reads `want`.
fn await_state(server: &Server, pane: &str, writer: &mut std::io::PipeWriter, want: &str) -> bool {
    let deadline = Instant::now() + SETTLE;
    loop {
        let _ = writer.write_all(b".");
        let _ = writer.flush();
        // Longer than a tick, so the write it follows actually gets read.
        if poll_until(Duration::from_millis(400), || {
            server.option(pane, "@agent_state") == want
        }) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}

/// What the watcher leaves on a pane where Claude sits idle at its prompt, so a watcher
/// started on it takes up from there, with no startup grace. The fake agent is `bash` to
/// tmux, so the hint is what names it Claude.
fn seed_idle_claude(server: &Server, pane: &str) {
    for (option, value) in [
        ("@agent_kind_hint", "claude"),
        ("@agent_kind", "claude"),
        ("@agent_state", "idle"),
        ("@agent_rule", "live_prompt_box"),
        ("@agent_source", "manifest"),
        ("@agent_seen", "1"),
    ] {
        server.0.set_pane_option(pane, option, value).unwrap();
    }
}

/// herdr's process-exit report, end to end: Claude exits mid-turn in a session nobody is
/// attached to. Its `done` is written first, "so notifications ... observe completion", and
/// the record is cleared a tick later — well before the done delay is up. When it is, the
/// pane holds no agent at all, and the done toast is delivered all the same.
///
/// The Claude here is a copy of this suite's tmux named `claude`, run from the pane's shell
/// over Claude's working screen: `#{pane_current_command}` says `claude`, and `wait-for`
/// keeps it in front of the shell until the test lets it go.
#[test]
fn an_agent_that_exits_mid_turn_is_toasted_done_after_its_record_is_cleared() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let (recorder, recorded) = notification_recorder(tmp.path());
    // Copied once, before anything runs it: see `a_name_goes_with_its_agent_when_another_
    // takes_the_pane`.
    let claude = tmp.path().join("claude");
    std::fs::copy(Tmux::with_socket_name("unused").bin, &claude).unwrap();
    crate::common::make_executable(&claude);
    let _notifier = common::EnvVar::set("TAW_NOTIFY_CMD", &recorder);

    let server = Server::start(None);
    let pane = server.pane();
    let Some(taw::tmux::Socket::Name(socket)) = &server.0.socket else {
        panic!("the test server should use a named socket");
    };
    let working = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/screens/claude-2.1.227-working.txt"
    );
    server
        .0
        .run_ok(&[
            "send-keys",
            "-t",
            &pane,
            &format!(
                "clear; cat {working}; {} -L {socket} wait-for taw-test-exit",
                claude.display()
            ),
            "Enter",
        ])
        .unwrap();
    let command = || server.0.display(&pane, "#{pane_current_command}").unwrap();
    assert!(
        poll_until(SETTLE, || command() == "claude"
            && server
                .0
                .capture_pane(&pane, false)
                .unwrap()
                .contains("esc to interrupt")),
        "Claude never took the pane (it runs {:?})",
        command()
    );
    // What the watcher leaves on a Claude mid-turn, so it takes up from there.
    for (option, value) in [
        ("@agent_kind", "claude"),
        ("@agent_state", "working"),
        ("@agent_rule", "live_turn_working"),
        ("@agent_source", "manifest"),
        ("@agent_seen", "0"),
    ] {
        server.0.set_pane_option(&pane, option, value).unwrap();
    }

    let mut cfg = recording_config();
    cfg.ui.toast.done_delay_seconds = 1.0;
    // What the hooks hear of it: a line for each of the four agent events and for the
    // notification hook.
    let heard = tmp.path().join("hooks-heard.txt");
    let line = |what: &str| toml::Value::String(format!("echo \"{what}\" >> {}", heard.display()));
    cfg.hooks = toml::from_str::<Config>(&format!(
        "[hooks]\n\"agent.blocked\" = {}\n\"agent.done\" = {}\n\"agent.idle\" = {}\n\"agent.working\" = {}\nnotification = {}\n",
        line("$TAW_EVENT $TAW_STATE/$TAW_PREVIOUS_STATE"),
        line("$TAW_EVENT $TAW_STATE/$TAW_PREVIOUS_STATE"),
        line("$TAW_EVENT $TAW_STATE/$TAW_PREVIOUS_STATE"),
        line("$TAW_EVENT $TAW_STATE/$TAW_PREVIOUS_STATE"),
        line("$TAW_EVENT $TAW_KIND"),
    ))
    .unwrap()
    .hooks;
    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let paths = paths.clone();
        let pane = pane.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };
    // The watcher has read the pane once it records its pid and a read keeps the turn.
    assert!(
        poll_until(SETTLE, || !server
            .option(&pane, "@agent_watch_pid")
            .is_empty()),
        "the watcher never started"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(server.option(&pane, "@agent_state"), "working");

    // Claude exits mid-turn: the shell is back in front of the pane.
    server
        .0
        .run_ok(&["wait-for", "-S", "taw-test-exit"])
        .unwrap();
    // `/bin/sh`, which macOS reports as the `bash` underneath it.
    assert!(
        poll_until(SETTLE, || ["sh", "bash"].contains(&command().as_str())),
        "the shell never came back (the pane runs {:?})",
        command()
    );
    let _ = writer.write_all(b".");
    let _ = writer.flush();
    assert!(
        poll_until(SETTLE, || server.option(&pane, "@agent_kind").is_empty()),
        "the exited agent's record was never cleared (state {:?})",
        server.option(&pane, "@agent_state")
    );
    assert!(
        poll_until(SETTLE, || std::fs::read_to_string(&recorded)
            .is_ok_and(|text| text.contains("claude done"))),
        "an agent that exited mid-turn behind the user's back was never toasted done: {:?}",
        std::fs::read_to_string(&recorded).unwrap_or_default()
    );
    let delivered = std::fs::read_to_string(&recorded).unwrap();
    assert!(
        delivered.starts_with("claude done \u{b7} one:0|"),
        "unexpected notification: {delivered:?}"
    );
    assert_eq!(
        server.option(&pane, "@agent_state"),
        "",
        "the record stays cleared"
    );
    // The hooks heard the turn end, which is what the README says of an agent that leaves
    // mid-turn: `agent.done` (the user is away) and the done notification, and the agent
    // leaving its pane after that is the status event alone, so no other agent event.
    let heard_now = || std::fs::read_to_string(&heard).unwrap_or_default();
    assert!(
        poll_until(SETTLE, || {
            let text = heard_now();
            text.contains("agent.done done/working") && text.contains("notification done")
        }),
        "the hooks never heard the turn end: {:?}",
        heard_now()
    );
    std::thread::sleep(Duration::from_millis(500));
    let mut lines: Vec<String> = heard_now().lines().map(str::to_string).collect();
    lines.sort();
    assert_eq!(
        lines,
        ["agent.done done/working", "notification done"],
        "an agent that left mid-turn was heard as more than its turn ending"
    );

    drop(writer);
    assert!(
        poll_until(SETTLE, || watcher.is_finished()),
        "run_with_reader did not return on EOF"
    );
    watcher.join().unwrap().unwrap();
}

/// herdr decides a done toast when the work ends, by where the user is looking then. The
/// user answers a permission prompt they are looking at with Esc — `blocked -> idle` in
/// front of them — and goes to another window straight after, well inside the done delay.
/// When the delay is up the pane is in the background and still idle, which is what a
/// check at delivery alone would toast; but the finish was seen as it happened, so nothing is.
#[test]
fn a_prompt_answered_in_front_of_the_user_earns_no_done_toast_when_they_leave() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let (recorder, recorded) = notification_recorder(tmp.path());
    let _notifier = common::EnvVar::set("TAW_NOTIFY_CMD", &recorder);

    let server = Server::start(Some(fake_agent()));
    let pane = server.pane();
    let agent_pid = server.0.display(&pane, "#{pane_pid}").unwrap();
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("? for shortcuts")),
        "the fake agent never drew its idle screen"
    );
    seed_idle_claude(&server, &pane);
    let _client = Client::attach(&server);

    let mut cfg = recording_config();
    cfg.ui.toast.done_delay_seconds = 2.0;
    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let paths = paths.clone();
        let pane = pane.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };

    signal(&agent_pid, "-USR1");
    assert!(
        await_state(&server, &pane, &mut writer, "working"),
        "never reached working (state {:?})",
        server.option(&pane, "@agent_state")
    );
    signal(&agent_pid, "-USR2");
    assert!(
        await_state(&server, &pane, &mut writer, "blocked"),
        "never reached blocked (state {:?})",
        server.option(&pane, "@agent_state")
    );
    // Esc on the prompt: back at the prompt, in front of the user.
    signal(&agent_pid, "-USR1");
    assert!(
        await_state(&server, &pane, &mut writer, "idle"),
        "the answered prompt should be an idle the user has seen (state {:?})",
        server.option(&pane, "@agent_state")
    );
    // Polled: `@agent_seen` is written in a tmux call of its own, after the state.
    assert!(
        poll_until(SETTLE, || server.option(&pane, "@agent_seen") == "1"),
        "the answered prompt was not recorded as seen"
    );
    // Straight to another window: the pane is in the background from here on.
    server.0.run_ok(&["new-window", "-t", "one"]).unwrap();
    assert_eq!(server.0.display(&pane, "#{window_active}").unwrap(), "0");

    std::thread::sleep(Duration::from_millis(3500));
    let delivered = std::fs::read_to_string(&recorded).unwrap_or_default();
    assert!(
        !delivered.contains("claude done"),
        "a finish the user watched was toasted once they left: {delivered:?}"
    );
    assert_eq!(server.option(&pane, "@agent_state"), "idle");

    drop(writer);
    assert!(
        poll_until(SETTLE, || watcher.is_finished()),
        "run_with_reader did not return on EOF"
    );
    watcher.join().unwrap().unwrap();
}

/// herdr does not count a pane in the active tab as looked at while the terminal it runs
/// in is known to have lost focus (`outer_terminal_focus`): the user is in another app.
/// tmux knows the same of each client once `focus-events` is on. So a turn that ends in the
/// very window the user's client shows, while their terminal is in the background, is
/// `done`, with its done toast; back in the terminal, the next read finds them looking at
/// it and writes the idle they see.
#[test]
fn a_finish_while_the_users_terminal_is_in_the_background_is_done() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let (recorder, recorded) = notification_recorder(tmp.path());
    let _notifier = common::EnvVar::set("TAW_NOTIFY_CMD", &recorder);

    let server = Server::start(Some(fake_agent()));
    let pane = server.pane();
    let agent_pid = server.0.display(&pane, "#{pane_pid}").unwrap();
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("? for shortcuts")),
        "the fake agent never drew its idle screen"
    );
    seed_idle_claude(&server, &pane);
    server
        .0
        .run_ok(&["set-option", "-g", "focus-events", "on"])
        .unwrap();
    let client = Client::attach(&server);
    // The pane's window is the one the user's client shows.
    assert_eq!(server.0.display(&pane, "#{window_active}").unwrap(), "1");
    let flags = || {
        server
            .0
            .run(&["list-clients", "-F", "#{client_flags}"])
            .unwrap()
    };

    let mut cfg = recording_config();
    cfg.ui.toast.done_delay_seconds = 1.0;
    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let paths = paths.clone();
        let pane = pane.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };

    signal(&agent_pid, "-USR1");
    assert!(
        await_state(&server, &pane, &mut writer, "working"),
        "never reached working (state {:?})",
        server.option(&pane, "@agent_state")
    );
    // The user goes to another app mid-turn.
    client.report_focus(false);
    assert!(
        poll_until(SETTLE, || !flags().contains("focused")),
        "tmux never took the focus-out report: {}",
        flags()
    );
    signal(&agent_pid, "-USR1");
    assert!(
        await_state(&server, &pane, &mut writer, "done"),
        "a finish while the user's terminal was in the background is not one they saw \
         (state {:?})",
        server.option(&pane, "@agent_state")
    );
    // Polled: `@agent_seen` is written in a tmux call of its own, after the state.
    assert!(
        poll_until(SETTLE, || server.option(&pane, "@agent_seen") == "0"),
        "the done was marked seen"
    );
    assert!(
        poll_until(SETTLE, || std::fs::read_to_string(&recorded)
            .is_ok_and(|text| text.contains("claude done"))),
        "the finish was never toasted: {:?}",
        std::fs::read_to_string(&recorded).unwrap_or_default()
    );

    // Back in the terminal: the pane is in front of them again.
    client.report_focus(true);
    assert!(
        poll_until(SETTLE, || flags().contains("focused")),
        "tmux never took the focus-in report: {}",
        flags()
    );
    assert!(
        await_state(&server, &pane, &mut writer, "idle"),
        "the done the user came back to was left standing (state {:?})",
        server.option(&pane, "@agent_state")
    );
    assert!(
        poll_until(SETTLE, || server.option(&pane, "@agent_seen") == "1"),
        "the idle they came back to was not marked seen"
    );

    drop(writer);
    assert!(
        poll_until(SETTLE, || watcher.is_finished()),
        "run_with_reader did not return on EOF"
    );
    watcher.join().unwrap().unwrap();
}

#[test]
fn watches_a_pane_through_its_agents_whole_lifecycle() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();

    // `$TAW_NOTIFY_CMD` records every `system` notification as `<title>|<body>` lines.
    let recorded = tmp.path().join("notifications.txt");
    let recorder = tmp.path().join("record-notification.sh");
    std::fs::write(
        &recorder,
        format!(
            "#!/bin/sh\nprintf '%s|%s\\n' \"$1\" \"$2\" >> {}\n",
            recorded.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        crate::common::make_executable(&recorder);
    }
    let _notifier = common::EnvVar::set("TAW_NOTIFY_CMD", &recorder);

    let server = Server::start(Some(fake_agent()));
    let pane = server.pane();
    let agent_pid = server.0.display(&pane, "#{pane_pid}").unwrap();

    // Wait for the fake to draw its idle screen before the watcher's first look, so the
    // first evaluation is against a real agent screen rather than a half-drawn one.
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("? for shortcuts")),
        "the fake agent never drew its idle screen"
    );

    // A shell-script fixture cannot make `#{pane_current_command}` say `claude` — the
    // kernel reports the interpreter (`bash`) — so identity comes from the other half of
    // the rule, `@agent_kind_hint`. `identify` itself is pinned by
    // `tests/detect_fixtures.rs`.
    server
        .0
        .set_pane_option(&pane, "@agent_kind_hint", "claude")
        .unwrap();

    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let cfg = recording_config();
        let paths = paths.clone();
        let pane = pane.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };

    // One byte of pane output is all the watcher needs: it is an activity signal, and the
    // screen itself is read back from tmux.
    let nudge = |writer: &mut std::io::PipeWriter| {
        writer.write_all(b".").unwrap();
        writer.flush().unwrap();
    };
    let await_option = |name: &str, want: &str| -> bool {
        poll_until(SETTLE, || server.option(&pane, name) == want)
    };
    let await_state = |writer: &mut std::io::PipeWriter, want: &str| -> bool {
        let deadline = Instant::now() + SETTLE;
        loop {
            let _ = writer.write_all(b".");
            let _ = writer.flush();
            // Longer than a tick, so the write it follows actually gets read.
            if poll_until(Duration::from_millis(400), || {
                server.option(&pane, "@agent_state") == want
            }) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
        }
    };

    // --- idle: the first evaluation classifies an already-running agent ---------------
    nudge(&mut writer);
    assert!(
        await_state(&mut writer, "idle"),
        "never reached idle (state {:?})",
        server.option(&pane, "@agent_state")
    );
    assert!(
        await_option("@agent_kind", "claude"),
        "@agent_kind should be claude"
    );
    assert!(
        await_option("@agent_rule", "live_prompt_box"),
        "@agent_rule should name the idle rule"
    );
    assert!(
        await_option("@agent_source", "manifest"),
        "@agent_source should be manifest"
    );
    assert!(
        poll_until(SETTLE, || server
            .option(&pane, "@agent_state_since")
            .parse::<u64>()
            .is_ok_and(|since| since > 1_700_000_000)),
        "@agent_state_since should be a unix timestamp"
    );
    assert!(
        await_option("@agent_watch_pid", &std::process::id().to_string()),
        "the watcher must record its own pid"
    );

    // --- working ---------------------------------------------------------------------
    signal(&agent_pid, "-USR1");
    assert!(
        await_state(&mut writer, "working"),
        "never reached working (state {:?})",
        server.option(&pane, "@agent_state")
    );
    assert!(
        await_option("@agent_rule", "live_turn_working"),
        "@agent_rule should name the working rule"
    );
    // A new turn resets `@agent_seen`: this is the flag that decides, at the *next*
    // `working -> idle`, whether the user is told the agent finished.
    assert!(
        await_option("@agent_seen", "0"),
        "entering working must reset @agent_seen"
    );

    // --- blocked, and the notification it earns ---------------------------------------
    signal(&agent_pid, "-USR2");
    assert!(
        await_state(&mut writer, "blocked"),
        "never reached blocked (state {:?})",
        server.option(&pane, "@agent_state")
    );
    // The status rail is recomputed on every transition.
    assert!(
        poll_until(SETTLE, || !server
            .0
            .get_global_option("@agent_wizard_status")
            .unwrap()
            .unwrap_or_default()
            .is_empty()),
        "@agent_wizard_status should not be empty"
    );
    // And every transition asks the session snapshot to be saved again (debounced by
    // `session::autosave_if_due`, which is not this module's job).
    assert!(
        poll_until(SETTLE, || paths
            .sessions_dir
            .join(".autosave-requested")
            .exists()),
        "a transition should request a session autosave"
    );

    assert!(
        poll_until(SETTLE, || std::fs::read_to_string(&recorded)
            .is_ok_and(|text| text.contains("blocked"))),
        "no request notification was delivered (file: {:?})",
        std::fs::read_to_string(&recorded)
    );
    let delivered = std::fs::read_to_string(&recorded).unwrap();
    let line = delivered.lines().next().unwrap();
    assert!(
        line.starts_with("claude blocked \u{b7} one:0|"),
        "unexpected notification title: {line:?}"
    );
    assert!(
        line.contains("Bash(rm -rf build/)"),
        "the body should be the first line of the matched region: {line:?}"
    );

    // --- back at the prompt, then a fresh turn, then done again -----------------------
    // `idle` after `blocked` is a completion, as herdr counts them, and nobody is attached
    // to this session: the agent's return to its prompt is `done`, like a turn's end.
    signal(&agent_pid, "-USR1"); // blocked -> idle
    assert!(
        await_state(&mut writer, "done"),
        "a background blocked -> idle should become done (state {:?})",
        server.option(&pane, "@agent_state")
    );
    signal(&agent_pid, "-USR1"); // idle -> working
    assert!(
        await_state(&mut writer, "working"),
        "never reached working again (state {:?})",
        server.option(&pane, "@agent_state")
    );
    signal(&agent_pid, "-USR1"); // working -> idle, in a session nobody is attached to
    assert!(
        await_state(&mut writer, "done"),
        "a background working -> idle should become done (state {:?})",
        server.option(&pane, "@agent_state")
    );
    // `done` is not a manifest state, so the rule recorded with it is whichever *idle*
    // rule produced it. Either of claude.toml's two is legitimate here: the fake sets its
    // OSC title before redrawing the screen, so a capture landing inside that window
    // matches `osc_title_idle` rather than `live_prompt_box`. A later evaluation of the
    // finished screen is the same *state* but a different rule, which is refreshed on
    // its own (`Decision::RuleChanged`) — so both spellings are transient and polling is
    // what makes this assertion stable rather than load-sensitive.
    //
    // `store::write_state` batches all four options into one `tmux` invocation, so a
    // reader never sees a new `@agent_state` beside a stale `@agent_rule`; the poll is
    // for the *evaluation* to happen, not for a torn write to settle.
    assert!(
        poll_until(SETTLE, || matches!(
            server.option(&pane, "@agent_rule").as_str(),
            "live_prompt_box" | "osc_title_idle"
        )),
        "@agent_rule should name the idle rule behind done, got {:?}",
        server.option(&pane, "@agent_rule")
    );
    let done_rule = server.option(&pane, "@agent_rule");

    // --- `taw seen` releases done ------------------------------------------------------
    watch::mark_seen(&server.0, &test_config(), &paths, &pane).unwrap();
    assert_eq!(server.option(&pane, "@agent_state"), "idle");
    // `taw seen` keeps the evidence: it did not change the state because the screen
    // changed, so the rule and source behind it still stand.
    assert_eq!(server.option(&pane, "@agent_rule"), done_rule);
    assert_eq!(server.option(&pane, "@agent_source"), "manifest");
    assert!(
        await_option("@agent_seen", "1"),
        "`taw seen` must record the pane as seen"
    );
    // And it stays idle: the watcher keeps evaluating the same screen.
    nudge(&mut writer);
    assert!(
        !poll_until(Duration::from_millis(600), || server
            .option(&pane, "@agent_state")
            != "idle"),
        "the state should stay idle once the pane has been seen"
    );

    // --- EOF shuts the watcher down ----------------------------------------------------
    drop(writer);
    let joined = poll_until(SETTLE, || watcher.is_finished());
    assert!(joined, "run_with_reader did not return on EOF");
    watcher.join().unwrap().unwrap();
    assert_eq!(
        server.option(&pane, "@agent_watch_pid"),
        "",
        "@agent_watch_pid must be cleared on exit"
    );
}

/// `[hooks]`, against a real watcher: each state the fake agent goes through is one
/// `agent.<state>` event, told which agent moved between which states in which session and
/// pane, and the states that are not a change earn none. `taw seen` moves a `done` back to
/// `idle` without the watcher's event, so it is no hook either.
#[test]
fn the_agent_hooks_fire_for_each_state_the_watcher_writes_and_for_nothing_else() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let recorded = tmp.path().join("agent-events.txt");
    let record = format!(
        "echo \"$TAW_EVENT $TAW_STATE/$TAW_PREVIOUS_STATE $TAW_AGENT $TAW_SESSION $TAW_PANE\" >> {}",
        recorded.display()
    );
    let mut cfg = test_config();
    // No toast: this test is about the hooks, and `system` delivery would run the OS's own
    // notifier for the blocked agent.
    cfg.ui.toast.delivery = ToastDelivery::Off;
    cfg.hooks = toml::from_str::<Config>(&format!(
        "[hooks]\n\"agent.idle\" = {r}\n\"agent.working\" = {r}\n\"agent.blocked\" = {r}\n\"agent.done\" = {r}\n",
        r = toml::Value::String(record)
    ))
    .unwrap()
    .hooks;

    let server = Server::start(Some(fake_agent()));
    let pane = server.pane();
    let agent_pid = server.0.display(&pane, "#{pane_pid}").unwrap();
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("? for shortcuts")),
        "the fake agent never drew its idle screen"
    );
    server
        .0
        .set_pane_option(&pane, "@agent_kind_hint", "claude")
        .unwrap();

    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let (cfg, paths, pane) = (cfg.clone(), paths.clone(), pane.clone());
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };
    let lines = || -> Vec<String> {
        std::fs::read_to_string(&recorded)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    };
    // A line is `<event> <state>/<previous state> <agent> <session> <pane>`. The state before
    // is whatever the watcher had written, and a screen caught half-drawn as the fake agent
    // redraws reads `unknown` for a moment, so the test asks only that it was another state:
    // what it pins is that each state the watcher writes is one agent event, told where.
    let tail = format!("claude one {pane}");
    let fired = |event: &str, state: &str| {
        lines().iter().any(|line| {
            line.strip_prefix(&format!("{event} {state}/"))
                .and_then(|rest| rest.strip_suffix(&format!(" {tail}")))
                .is_some_and(|previous| previous != state)
        })
    };
    let expect = |event: &str, state: &str| {
        assert!(
            poll_until(SETTLE, || fired(event, state)),
            "no {event} for {state}: got {:?}, log {:?}",
            lines(),
            std::fs::read_to_string(paths.logs_dir.join("hooks.log")).unwrap_or_default()
        );
    };

    assert!(await_state(&server, &pane, &mut writer, "idle"));
    // The first thing the watcher writes for an agent only its hint names is `unknown`, which
    // is no agent event: the idle that follows it is the one.
    expect("agent.idle", "idle");
    signal(&agent_pid, "-USR1");
    assert!(await_state(&server, &pane, &mut writer, "working"));
    expect("agent.working", "working");
    signal(&agent_pid, "-USR2");
    assert!(await_state(&server, &pane, &mut writer, "blocked"));
    expect("agent.blocked", "blocked");
    // Back at the prompt in a session nobody is attached to: a completion, so `done`.
    signal(&agent_pid, "-USR1");
    assert!(await_state(&server, &pane, &mut writer, "done"));
    expect("agent.done", "done");

    // The user looks: `done` goes back to `idle`, which is no state change of the
    // watcher's, and the screen has not changed either. Nothing more fires.
    watch::mark_seen(&server.0, &cfg, &paths, &pane).unwrap();
    assert_eq!(server.option(&pane, "@agent_state"), "idle");
    let before = lines();
    let _ = writer.write_all(b".");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(lines(), before, "taw seen fired a hook");

    drop(writer);
    assert!(poll_until(SETTLE, || watcher.is_finished()));
    watcher.join().unwrap().unwrap();
}

/// Runs `sleep` in the pane, in front of its shell: a program that names no agent and is no shell.
///
/// An agent whose integration reports its state, and that no process in the pane names, is kept
/// only until the pane's own shell is back at its prompt with nothing running in it (herdr's
/// `report_self_reported_agent_shell_return`). A test that reports a state for one on a pane that
/// holds nothing but an idle shell reports an agent that has already gone, and the watcher says so
/// and lets the pane go. A real one is running, so this is what stands in for it while the
/// reports come.
fn run_in_front_of_the_shell(server: &Server, pane: &str) {
    server
        .0
        .run_ok(&["send-keys", "-t", pane, "sleep 300", "Enter"])
        .unwrap();
    let command = || server.0.display(pane, "#{pane_current_command}").unwrap();
    assert!(
        poll_until(SETTLE, || command() == "sleep"),
        "`sleep` never took the pane (it runs {:?})",
        command()
    );
}

/// Starts a shell inside the pane's own, the shape of `nix develop` or `sudo -s`: a program that
/// is named a shell, in front of the pane's shell, which is then not at its prompt with the terminal
/// to itself.
///
/// An agent that reports for itself and that no process names is judged by the pane's own shell
/// alone, so a shell in front does not release it. One that is not hook-owned yet (an agent a
/// report has written and not yet given its owner) is judged by the command in front, and a shell
/// there is an exit: what a watcher reading in the gap between the two writes of a report would
/// take it for.
fn nest_a_shell(server: &Server, pane: &str) {
    for line in ["sh", "PS1='nested$ '"] {
        server
            .0
            .run_ok(&["send-keys", "-t", pane, line, "Enter"])
            .unwrap();
    }
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(pane, false)
            .unwrap()
            .trim_end()
            .ends_with("nested$")),
        "the nested shell never prompted:\n{}",
        server.0.capture_pane(pane, false).unwrap()
    );
}

/// A `tmux` that runs the real one, except that the first call that writes `done` over an agent's
/// report (`write_state_over`, an `if-shell` that sets `@agent_state done`) fails without running
/// anything, as a busy server's can. Returns it and the file it creates when it has failed that one
/// call, so a test can tell that the failure happened. `pausing_tmux` is the same kind of stand-in.
fn failing_the_first_done_write(server: &Server, dir: &Path) -> (PathBuf, PathBuf) {
    let shim = dir.join("tmux-failing-one-write");
    let failed = dir.join("the-write-failed");
    std::fs::write(
        &shim,
        format!(
            r#"#!/bin/sh
case "$*" in
  *"@agent_state done"*)
    if [ ! -e '{failed}' ]; then
      : > '{failed}'
      echo 'tmux: the write was refused (a test)' >&2
      exit 1
    fi;;
esac
exec '{tmux}' "$@"
"#,
            failed = failed.display(),
            tmux = server.0.bin.display()
        ),
    )
    .unwrap();
    {
        crate::common::make_executable(&shim);
    }
    (shim, failed)
}

/// Counts the reads each watcher thread of this process has made, from the `decision` line the
/// watcher logs at debug level for each. What a test waits on when a watcher has to have looked at
/// the pane before the pane changes: a report that beat the watcher's first read is the state it
/// found, and no change.
struct CountReads;

static READS: Mutex<Vec<(std::thread::ThreadId, usize)>> = Mutex::new(Vec::new());

impl log::Log for CountReads {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Debug
    }

    fn log(&self, record: &log::Record) {
        if !record.args().to_string().contains(" decision: ") {
            return;
        }
        let me = std::thread::current().id();
        let mut reads = READS.lock().unwrap_or_else(|err| err.into_inner());
        match reads.iter_mut().find(|(thread, _)| *thread == me) {
            Some((_, count)) => *count += 1,
            None => reads.push((me, 1)),
        }
    }

    fn flush(&self) {}
}

/// Starts counting reads, once for the process.
fn count_reads() {
    static START: std::sync::Once = std::sync::Once::new();
    START.call_once(|| {
        log::set_logger(&CountReads).expect("no other logger is set in this test binary");
        log::set_max_level(log::LevelFilter::Debug);
    });
}

/// How many times the watcher that runs on `thread` has read its pane.
fn reads_of(thread: std::thread::ThreadId) -> usize {
    READS
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .iter()
        .find(|(reader, _)| *reader == thread)
        .map_or(0, |(_, count)| *count)
}

/// A pane whose agent reports its own state, as OpenCode does, read by a watcher in this process.
///
/// A program that prints nothing stands in for the agent in front of the pane's shell (an agent no
/// process names is kept only until the pane's own shell is back at its prompt, which a real one
/// running is not). Hooks write each event they hear to a file, toasts go to `$TAW_NOTIFY_CMD` at
/// once, and `taw pane report-agent` is run as an integration runs it, from outside the pane. Whoever
/// holds it holds [`SERIAL`]: the recorder is a process-global variable.
struct Reporting {
    _tmp: tempfile::TempDir,
    server: Server,
    pane: String,
    socket: String,
    paths: Paths,
    cfg: Config,
    heard: PathBuf,
    toasts: PathBuf,
    writer: Option<std::io::PipeWriter>,
    watcher: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
    /// The user's terminal, when they are looking at the pane.
    _client: Option<Client>,
    /// The file the watcher's failing `tmux` writes when it has failed its one write, when it has one.
    failed: Option<PathBuf>,
    /// `$TAW_NOTIFY_CMD`, which names `toasts`' recorder while this lives and is put back as it was after
    /// everything else here is gone (it is the last field, so it is dropped last).
    _notifier: common::EnvVar,
}

impl Reporting {
    /// The pane, and its watcher, which has read it once by the time this returns: a report that
    /// comes after that is a change, where one that came before would be the state it found.
    fn start(in_front: bool) -> Reporting {
        Reporting::start_with(in_front, test_timing())
    }

    /// [`Reporting::start`] for a watcher on `timing`.
    fn start_with(in_front: bool, timing: Timing) -> Reporting {
        Reporting::start_using(in_front, timing, false)
    }

    /// [`Reporting::start`] for a watcher whose `tmux` fails the first write of a `done` over an
    /// agent's report, as a busy server's can, and runs everything else as it is (see
    /// [`failing_the_first_done_write`]).
    fn start_failing_the_first_done_write(in_front: bool) -> Reporting {
        Reporting::start_using(in_front, test_timing(), true)
    }

    fn start_using(in_front: bool, timing: Timing, failing: bool) -> Reporting {
        let tmp = tempfile::tempdir().unwrap();
        let paths = test_paths(tmp.path());
        paths.ensure().unwrap();
        let (recorder, toasts) = notification_recorder(tmp.path());
        let notifier = common::EnvVar::set("TAW_NOTIFY_CMD", &recorder);
        let heard = tmp.path().join("hooks-heard.txt");
        let event = toml::Value::String(format!(
            "echo \"$TAW_EVENT $TAW_STATE/$TAW_PREVIOUS_STATE\" >> {}",
            heard.display()
        ));
        let notification = toml::Value::String(format!(
            "echo \"$TAW_EVENT $TAW_KIND\" >> {}",
            heard.display()
        ));
        let config = format!(
            "[ui.toast]\ndelivery = \"system\"\ndelay_seconds = 0.0\ndone_delay_seconds = 0.0\n\n[hooks]\n\"pane.agent_status_changed\" = {event}\n\"agent.blocked\" = {event}\n\"agent.done\" = {event}\n\"agent.idle\" = {event}\n\"agent.working\" = {event}\nnotification = {notification}\n"
        );
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(&paths.config_file, &config).unwrap();
        let mut cfg = recording_config();
        cfg.ui.toast.done_delay_seconds = 0.0;
        cfg.hooks = toml::from_str::<Config>(&config).unwrap().hooks;

        let server = Server::start(None);
        let pane = server.pane();
        run_in_front_of_the_shell(&server, &pane);
        let Some(taw::tmux::Socket::Name(socket)) = &server.0.socket else {
            panic!("the test server should use a named socket");
        };
        let socket = socket.clone();
        let client = in_front.then(|| Client::attach(&server));

        count_reads();
        let (reader, writer) = std::io::pipe().unwrap();
        // The watcher's own `tmux`, which is the real one unless this rig fails a write for it. The
        // reports come from `taw` processes, which run the real one either way.
        let (watcher_tmux, failed) = if failing {
            let (shim, failed) = failing_the_first_done_write(&server, tmp.path());
            (
                Tmux {
                    bin: shim,
                    socket: server.0.socket.clone(),
                },
                Some(failed),
            )
        } else {
            (server.0.clone(), None)
        };
        let watcher = {
            let t = watcher_tmux;
            let (cfg, paths, pane) = (cfg.clone(), paths.clone(), pane.clone());
            let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
            std::thread::spawn(move || {
                watch::run_with_reader(&t, &cfg, &paths, &taw_bin, &pane, timing, Box::new(reader))
            })
        };
        let thread = watcher.thread().id();
        let rig = Reporting {
            _tmp: tmp,
            server,
            pane,
            socket,
            paths,
            cfg,
            heard,
            toasts,
            writer: Some(writer),
            watcher: Some(watcher),
            _client: client,
            failed,
            _notifier: notifier,
        };
        assert!(
            poll_until(SETTLE, || !rig
                .server
                .option(&rig.pane, "@agent_watch_pid")
                .is_empty()),
            "the watcher never started"
        );
        assert!(
            poll_until(SETTLE, || reads_of(thread) >= 1),
            "the watcher never read the pane"
        );
        rig
    }

    /// A byte of output, and time for the watcher to read the pane with it.
    fn read(&mut self) {
        let writer = self.writer.as_mut().unwrap();
        let _ = writer.write_all(b".");
        let _ = writer.flush();
        std::thread::sleep(Duration::from_millis(400));
    }

    /// What an integration does: `taw pane report-agent` for the pane, from outside it.
    fn report(&self, state: &str) {
        self.report_as("opencode", state);
    }

    /// [`Reporting::report`] by the integration of the agent `agent`.
    fn report_as(&self, agent: &str, state: &str) {
        let out = Command::new(env!("CARGO_BIN_EXE_taw"))
            .args([
                "pane",
                "report-agent",
                &self.pane,
                "--source",
                &format!("integration:{agent}"),
                "--agent",
                agent,
                "--state",
                state,
            ])
            .env("TAW_TMUX_SOCKET_NAME", &self.socket)
            .env("TAW_CONFIG_DIR", &self.paths.config_dir)
            .env("TAW_STATE_DIR", &self.paths.state_dir)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "taw pane report-agent --state {state}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    /// Every line the hooks have written, sorted: the order is the hooks', which are separate
    /// processes, so what is told is what was heard and how often.
    fn heard(&self) -> Vec<String> {
        let mut lines: Vec<String> = std::fs::read_to_string(&self.heard)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        lines.sort();
        lines
    }

    /// The `<title>|<body>` of every toast delivered.
    fn toasts(&self) -> Vec<String> {
        std::fs::read_to_string(&self.toasts)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Feeds the watcher output until the hooks have heard exactly `lines`, and then some more, to
    /// show that nothing else comes: what is announced is announced once.
    fn expect_heard(&mut self, lines: &[&str]) {
        let mut want: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        want.sort();
        let deadline = Instant::now() + SETTLE;
        while self.heard() != want && Instant::now() < deadline {
            let writer = self.writer.as_mut().unwrap();
            let _ = writer.write_all(b".");
            let _ = writer.flush();
            std::thread::sleep(Duration::from_millis(200));
        }
        assert_eq!(self.heard(), want, "what the hooks heard");
        self.read();
        self.read();
        assert_eq!(self.heard(), want, "something more was announced");
    }

    /// Waits, feeding the watcher nothing, for the hooks to have heard exactly `lines`: what reaches a
    /// watcher whose pane prints nothing is what a report sends it, and its own rechecks.
    fn expect_heard_unprompted(&self, lines: &[&str]) {
        let mut want: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        want.sort();
        assert!(
            poll_until(SETTLE, || self.heard() == want),
            "what the hooks heard of a report to a pane that printed nothing: {:?}, not {want:?}",
            self.heard()
        );
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(self.heard(), want, "something more was announced");
    }

    fn option(&self, name: &str) -> String {
        self.server.option(&self.pane, name)
    }

    /// Ends the watcher the way tmux does, by closing its pipe.
    fn finish(mut self) {
        drop(self.writer.take());
        let watcher = self.watcher.take().unwrap();
        assert!(
            poll_until(SETTLE, || watcher.is_finished()),
            "run_with_reader did not return on EOF"
        );
        watcher.join().unwrap().unwrap();
    }
}

/// An agent whose integration reports its own state (opencode, kilo, pi, omp, kimi, mastracode)
/// gets what any agent gets for a change of state: the status event, the agent event of the state it
/// went into, and the notification hook where a toast is delivered. `taw pane report-agent` writes
/// the state, and the watcher takes each report in and announces the change, once: the same report
/// again (OpenCode reports `working` for every tool it runs, and `idle` from two of its events) is
/// nothing, and an `idle` that ends a turn behind the user's back is `done`. The README says so,
/// and this keeps it true: it was the other way round, reports announced nothing, until the watcher
/// took them in.
#[test]
fn an_agent_whose_integration_reports_its_state_fires_the_agent_hooks_and_the_notification_hook() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let mut rig = Reporting::start(false);

    // A turn starts, tool after tool: announced once.
    rig.report("working");
    let mut heard = vec![
        "agent.working working/",
        "pane.agent_status_changed working/",
    ];
    rig.expect_heard(&heard);
    rig.report("working");
    rig.report("working");
    rig.expect_heard(&heard);

    // It asks for a permission, twice: a request, once.
    rig.report("blocked");
    heard.extend([
        "agent.blocked blocked/working",
        "pane.agent_status_changed blocked/working",
        "notification request",
    ]);
    rig.expect_heard(&heard);
    rig.report("blocked");
    rig.expect_heard(&heard);

    // It finishes behind the user's back, `idle` from two events: one `done`, which the second
    // `idle` does not undo.
    rig.report("idle");
    heard.extend([
        "agent.done done/blocked",
        "pane.agent_status_changed done/blocked",
        "notification done",
    ]);
    rig.expect_heard(&heard);
    assert_eq!(rig.option("@agent_state"), "done");
    rig.report("idle");
    rig.expect_heard(&heard);
    assert_eq!(rig.option("@agent_state"), "done", "unseen, still");
    // The state is the agent's, and the watcher, which has read the pane since, is not its owner.
    assert_eq!(rig.option("@agent_source"), "hook:integration:opencode");
    assert_eq!(rig.option("@agent_seen"), "0");

    // The user looks: `done` goes back to `idle`, which nobody reported, and nothing more fires.
    watch::mark_seen(&rig.server.0, &rig.cfg, &rig.paths, &rig.pane).unwrap();
    assert_eq!(rig.option("@agent_state"), "idle");
    rig.expect_heard(&heard);

    // The toasts are what the notification hook heard, with the words a toast has.
    let toasts = rig.toasts();
    assert_eq!(toasts.len(), 2, "{toasts:?}");
    assert!(
        toasts[0].starts_with("opencode blocked \u{b7} one:0|"),
        "{toasts:?}"
    );
    assert!(
        toasts[1].starts_with("opencode done \u{b7} one:0|"),
        "{toasts:?}"
    );

    // The hooks are wired to fire: the same command runs when an event is fired at it.
    let out = Command::new(env!("CARGO_BIN_EXE_taw"))
        .args(["hooks", "test", "agent.idle"])
        .env("TAW_TMUX_SOCKET_NAME", &rig.socket)
        .env("TAW_CONFIG_DIR", &rig.paths.config_dir)
        .env("TAW_STATE_DIR", &rig.paths.state_dir)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        poll_until(SETTLE, || rig
            .heard()
            .contains(&"agent.idle idle/working".to_string())),
        "the hooks of this test never ran, so their silence above proves nothing: {:?}",
        rig.heard()
    );
    rig.finish();
}

/// A turn an agent reports, behind the user's back: one `done` toast, `agent.working`,
/// `agent.done` and `notification` once each, `done` on the pane with its hook still the user, and
/// no `agent.idle`.
#[test]
fn a_reported_turn_the_user_was_away_for_is_done_toasted_once_and_heard_once() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let mut rig = Reporting::start(false);

    rig.report("working");
    rig.expect_heard(&[
        "agent.working working/",
        "pane.agent_status_changed working/",
    ]);
    assert!(rig.toasts().is_empty(), "{:?}", rig.toasts());

    rig.report("idle");
    rig.expect_heard(&[
        "agent.working working/",
        "pane.agent_status_changed working/",
        "agent.done done/working",
        "pane.agent_status_changed done/working",
        "notification done",
    ]);
    assert_eq!(rig.option("@agent_state"), "done");
    assert_eq!(rig.option("@agent_source"), "hook:integration:opencode");
    assert_eq!(rig.option("@agent_seen"), "0");
    let toasts = rig.toasts();
    assert_eq!(toasts.len(), 1, "one finish, one toast: {toasts:?}");
    assert!(
        toasts[0].starts_with("opencode done \u{b7} one:0|"),
        "{toasts:?}"
    );
    rig.finish();
}

/// The same turn with the user in front of the pane: the agent's `idle` is an idle they saw, so
/// `agent.idle` fires with the status event, and no toast and no notification hook: a finish they
/// watched is not news.
#[test]
fn the_same_reported_turn_in_front_of_the_user_is_idle_and_earns_no_toast() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let mut rig = Reporting::start(true);

    rig.report("working");
    rig.expect_heard(&[
        "agent.working working/",
        "pane.agent_status_changed working/",
    ]);
    rig.report("idle");
    rig.expect_heard(&[
        "agent.working working/",
        "pane.agent_status_changed working/",
        "agent.idle idle/working",
        "pane.agent_status_changed idle/working",
    ]);
    assert_eq!(rig.option("@agent_state"), "idle");
    assert_eq!(rig.option("@agent_seen"), "1");
    assert!(rig.toasts().is_empty(), "{:?}", rig.toasts());
    rig.finish();
}

/// A reported `blocked` is a request: the needs-you toast and its notification hook, and
/// `agent.blocked`, once, however often the agent says it; answering it is no toast.
#[test]
fn a_reported_blocked_earns_the_needs_you_toast_and_agent_blocked_once() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let mut rig = Reporting::start(false);

    rig.report("working");
    rig.expect_heard(&[
        "agent.working working/",
        "pane.agent_status_changed working/",
    ]);
    rig.report("blocked");
    let mut heard = vec![
        "agent.working working/",
        "pane.agent_status_changed working/",
        "agent.blocked blocked/working",
        "pane.agent_status_changed blocked/working",
        "notification request",
    ];
    rig.expect_heard(&heard);
    rig.report("blocked");
    rig.expect_heard(&heard);
    let toasts = rig.toasts();
    assert_eq!(toasts.len(), 1, "{toasts:?}");
    assert!(
        toasts[0].starts_with("opencode blocked \u{b7} one:0|"),
        "{toasts:?}"
    );

    // The permission is given: a change back to `working`, which is news to no one.
    rig.report("working");
    heard.extend([
        "agent.working working/blocked",
        "pane.agent_status_changed working/blocked",
    ]);
    rig.expect_heard(&heard);
    assert_eq!(rig.toasts().len(), 1);
    rig.finish();
}

/// The report is what wakes the watcher: a pane that prints nothing is read only on the identity
/// recheck (a pane with no agent not at all), and an agent that reports its state is often quiet
/// while it thinks. The recheck here is ten minutes, so nothing but the nudge a report sends can
/// explain a report heard within seconds, and nothing is fed to the watcher after its first read.
#[test]
fn a_report_is_noticed_though_the_pane_prints_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let timing = Timing {
        identity_recheck: Duration::from_secs(600),
        ..test_timing()
    };
    let rig = Reporting::start_with(false, timing);

    rig.report("working");
    rig.expect_heard_unprompted(&[
        "agent.working working/",
        "pane.agent_status_changed working/",
    ]);
    rig.report("blocked");
    rig.expect_heard_unprompted(&[
        "agent.working working/",
        "pane.agent_status_changed working/",
        "agent.blocked blocked/working",
        "pane.agent_status_changed blocked/working",
        "notification request",
    ]);
    rig.report("idle");
    rig.expect_heard_unprompted(&[
        "agent.working working/",
        "pane.agent_status_changed working/",
        "agent.blocked blocked/working",
        "pane.agent_status_changed blocked/working",
        "notification request",
        "agent.done done/blocked",
        "pane.agent_status_changed done/blocked",
        "notification done",
    ]);
    assert_eq!(rig.option("@agent_state"), "done");
    rig.finish();
}

/// A different agent takes the pane and reports the state it is in: the status event says who is
/// there (`kind_changed`, the state unchanged), as it does for an agent a screen read finds in the
/// pane's place, and no agent event and no toast follow. The name the last agent was given goes with
/// it, and the same agent reporting again is nothing.
#[test]
fn a_different_agent_reporting_the_state_the_pane_is_in_fires_the_status_event_alone() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let mut rig = Reporting::start(false);

    rig.report("working");
    let mut heard = vec![
        "agent.working working/",
        "pane.agent_status_changed working/",
    ];
    rig.expect_heard(&heard);
    rig.server
        .0
        .set_pane_option(&rig.pane, "@agent_name", "reviewer")
        .unwrap();

    rig.report_as("kimi", "working");
    heard.push("pane.agent_status_changed working/working");
    rig.expect_heard(&heard);
    assert_eq!(rig.option("@agent_kind"), "kimi");
    assert_eq!(
        rig.option("@agent_name"),
        "",
        "the name went with the agent it was given to"
    );
    assert!(rig.toasts().is_empty(), "{:?}", rig.toasts());

    // The same agent again, any number of times: nothing.
    rig.report_as("kimi", "working");
    rig.expect_heard(&heard);
    rig.finish();
}

/// A turn shorter than one tick of the watcher: `working` and `idle` reported back to back, from an
/// agent the watcher has announced as idle. The state at the watcher's next read is the one it had
/// taken in, and only the turn stamp has moved: the start and the end are announced all the same, as
/// herdr, which applies each report, announces them, with the one done toast. The tick here is long,
/// so that the two reports land inside one; a watcher that read between them would announce the same
/// lines from two reads.
#[test]
fn a_turn_shorter_than_a_tick_is_heard_to_start_and_to_end_and_toasted_once() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let timing = Timing {
        identified: Duration::from_secs(2),
        unidentified: Duration::from_secs(2),
        ..test_timing()
    };
    let rig = Reporting::start_with(false, timing);

    rig.report("idle");
    let mut heard = vec!["agent.idle idle/", "pane.agent_status_changed idle/"];
    rig.expect_heard_unprompted(&heard);

    rig.report("working");
    rig.report("idle");
    heard.extend([
        "agent.working working/idle",
        "pane.agent_status_changed working/idle",
        "agent.done done/working",
        "pane.agent_status_changed done/working",
        "notification done",
    ]);
    rig.expect_heard_unprompted(&heard);
    assert_eq!(rig.option("@agent_state"), "done");
    assert_eq!(rig.option("@agent_seen"), "0");
    let toasts = rig.toasts();
    assert_eq!(toasts.len(), 1, "one finish, one toast: {toasts:?}");
    assert!(
        toasts[0].starts_with("opencode done \u{b7} one:0|"),
        "{toasts:?}"
    );
    rig.finish();
}

/// A write that fails after a report is retried, and the change it found is announced exactly once:
/// the watcher's tmux refuses the first write of the `done` over the agent's `idle`, the read fails,
/// and the one after it, a backoff later, finds the `idle` still on the pane and the turn still
/// announced as under way, and writes and announces the `done`. A watcher that kept what the failed
/// read had taken in would find the `done` already taken, put it back without a word, and never
/// announce it.
#[test]
fn a_write_that_fails_after_a_report_is_retried_and_the_done_is_announced_exactly_once() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let mut rig = Reporting::start_failing_the_first_done_write(false);

    rig.report("working");
    let mut heard = vec![
        "agent.working working/",
        "pane.agent_status_changed working/",
    ];
    rig.expect_heard(&heard);

    rig.report("idle");
    heard.extend([
        "agent.done done/working",
        "pane.agent_status_changed done/working",
        "notification done",
    ]);
    rig.expect_heard(&heard);
    let failed = rig.failed.clone().unwrap();
    assert!(
        failed.exists(),
        "the watcher's tmux never refused the write, so no retry was exercised"
    );
    assert_eq!(rig.option("@agent_state"), "done");
    assert_eq!(rig.option("@agent_seen"), "0");
    let toasts = rig.toasts();
    assert_eq!(toasts.len(), 1, "one finish, one toast: {toasts:?}");
    rig.finish();
}

/// `opencode run` and the agents like it end with their last report: the pane's shell is back at
/// its prompt as the `idle` lands. The turn is announced as started and is owed its end: one `done`,
/// one toast and one of each hook, whichever of the two the watcher reads first, and then the record
/// goes.
#[test]
fn an_agent_that_reports_its_last_state_and_quits_is_heard_done_once_and_toasted() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let mut rig = Reporting::start(false);

    rig.report("working");
    rig.expect_heard(&[
        "agent.working working/",
        "pane.agent_status_changed working/",
    ]);
    rig.report("idle");
    rig.server
        .0
        .run_ok(&["send-keys", "-t", &rig.pane, "C-c"])
        .unwrap();
    let deadline = Instant::now() + SETTLE;
    while !rig.option("@agent_kind").is_empty() && Instant::now() < deadline {
        rig.read();
    }
    assert!(
        rig.option("@agent_kind").is_empty(),
        "the record was never let go"
    );
    rig.expect_heard(&[
        "agent.working working/",
        "pane.agent_status_changed working/",
        "agent.done done/working",
        "pane.agent_status_changed done/working",
        // The agent leaves its pane after that: the status event alone.
        "pane.agent_status_changed /done",
        "notification done",
    ]);
    let toasts = rig.toasts();
    assert_eq!(toasts.len(), 1, "{toasts:?}");
    assert!(
        toasts[0].starts_with("opencode done \u{b7} one:0|"),
        "{toasts:?}"
    );
    rig.finish();
}

/// Its end, for an agent that reports for itself. When the pane's own shell comes back at its
/// prompt (the agent quit, or was stopped) and the last state the agent reported was `working` or
/// `blocked`, the watcher writes the turn's end as it does for any agent that exits, `done` while
/// nobody is looking at the pane, through the path every other change takes: the status event,
/// `agent.done` and the done notification fire, and the record goes a read later, which is the
/// status event alone. The report that started the turn fired its own (`agent.working` and the
/// status event) when the watcher took it in, and the release must not fire those again, nor its
/// own twice: the agent is heard to start once and to end once.
#[test]
fn an_agent_that_reports_for_itself_is_heard_to_start_once_and_end_once_when_its_shell_comes_back()
{
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let heard = tmp.path().join("hooks-heard.txt");
    let line = toml::Value::String(format!(
        "echo \"$TAW_EVENT $TAW_STATE\" >> {}",
        heard.display()
    ));
    let config = format!(
        "[ui.toast]\ndelivery = \"off\"\ndelay_seconds = 0.0\ndone_delay_seconds = 0.0\n\n[hooks]\n\"pane.agent_status_changed\" = {line}\n\"agent.blocked\" = {line}\n\"agent.done\" = {line}\n\"agent.idle\" = {line}\n\"agent.working\" = {line}\nnotification = {line}\n"
    );
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(&paths.config_file, &config).unwrap();
    let mut cfg = test_config();
    cfg.ui.toast.delivery = ToastDelivery::Off;
    cfg.ui.toast.done_delay_seconds = 0.0;
    cfg.hooks = toml::from_str::<Config>(&config).unwrap().hooks;

    let server = Server::start(None);
    let pane = server.pane();
    run_in_front_of_the_shell(&server, &pane);
    let Some(taw::tmux::Socket::Name(socket)) = &server.0.socket else {
        panic!("the test server should use a named socket");
    };
    count_reads();
    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let (cfg, paths, pane) = (cfg.clone(), paths.clone(), pane.clone());
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };
    assert!(
        poll_until(SETTLE, || !server
            .option(&pane, "@agent_watch_pid")
            .is_empty()),
        "the watcher never started"
    );
    // A report that beat the watcher's first read would be the state it found, and no change.
    let thread = watcher.thread().id();
    assert!(
        poll_until(SETTLE, || reads_of(thread) >= 1),
        "the watcher never read the pane"
    );
    let lines = || -> Vec<String> {
        std::fs::read_to_string(&heard)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    };

    // What the integration does when a turn starts.
    let reported = Command::new(env!("CARGO_BIN_EXE_taw"))
        .args([
            "pane",
            "report-agent",
            &pane,
            "--source",
            "integration:opencode",
            "--agent",
            "opencode",
            "--state",
            "working",
        ])
        .env("TAW_TMUX_SOCKET_NAME", socket)
        .env("TAW_CONFIG_DIR", &paths.config_dir)
        .env("TAW_STATE_DIR", &paths.state_dir)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .unwrap();
    assert!(
        reported.status.success(),
        "{}",
        String::from_utf8_lossy(&reported.stdout)
    );
    // The watcher has read the pane with the turn under way, and announced it once.
    assert!(
        await_state(&server, &pane, &mut writer, "working"),
        "the reported working never showed"
    );
    let deadline = Instant::now() + SETTLE;
    while lines().len() < 2 && Instant::now() < deadline {
        let _ = writer.write_all(b".");
        let _ = writer.flush();
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(800));
    let mut started = lines();
    started.sort();
    assert_eq!(
        started,
        ["agent.working working", "pane.agent_status_changed working"],
        "what the agent's own report fired"
    );

    // The agent is stopped and the pane's shell is at its prompt again. The watcher, fed output
    // as it reads, writes the turn's end and then lets the pane go.
    server.0.run_ok(&["send-keys", "-t", &pane, "C-c"]).unwrap();
    let deadline = Instant::now() + SETTLE;
    while lines().len() < 6 && Instant::now() < deadline {
        let _ = writer.write_all(b".");
        let _ = writer.flush();
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        poll_until(SETTLE, || server.option(&pane, "@agent_kind").is_empty()),
        "the pane was never let go (kind {:?})",
        server.option(&pane, "@agent_kind")
    );
    // The hooks are separate processes, so the order they write in is theirs: what matters is what
    // was heard, and that nothing more comes. The last is the agent leaving: a status event with no
    // state.
    std::thread::sleep(Duration::from_millis(500));
    let mut heard_now = lines();
    heard_now.sort();
    assert_eq!(
        heard_now,
        [
            "agent.done done",
            "agent.working working",
            "notification done",
            "pane.agent_status_changed ",
            "pane.agent_status_changed done",
            "pane.agent_status_changed working",
        ],
        "what the agent fired: its start once, and the release of it once"
    );

    drop(writer);
    assert!(poll_until(SETTLE, || watcher.is_finished()));
    watcher.join().unwrap().unwrap();
}

/// A `tmux` that runs the real one and then, after a call that sets `@agent_kind`, waits `pause`
/// seconds: whatever that call left behind is what anything reading the pane sees for that long.
fn pausing_tmux(server: &Server, dir: &Path, pause: &str) -> PathBuf {
    let shim = dir.join("tmux-pausing");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\n{} \"$@\"\nstatus=$?\ncase \"$*\" in *set-option*@agent_kind*) sleep {pause};; esac\nexit $status\n",
            server.0.bin.display()
        ),
    )
    .unwrap();
    {
        crate::common::make_executable(&shim);
    }
    shim
}

/// A watcher that reads the pane all the while `taw pane report-agent` writes must never see the
/// agent without its owner. Written in separate steps (the agent, then its state and owner), a
/// watcher that read in between took the half-written agent for one that had gone (a shell is in
/// front of this pane) and cleared it, and the report then wrote a state on a pane with no agent.
/// The gap is held open here by a `tmux` that waits after the call that sets the agent, and the
/// watcher is fed a byte of output every few milliseconds, so it reads in the gap if there is one.
/// The shell in front of the pane is one started inside it: that is no release of a finished
/// report (only the pane's own shell at its prompt is), and it is the exit of a half-written one.
#[test]
fn a_watcher_reading_all_the_while_never_takes_a_report_in_progress_for_an_agent_that_left() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let heard = tmp.path().join("hooks-heard.txt");
    let line = toml::Value::String(format!(
        "echo \"$TAW_EVENT $TAW_STATE\" >> {}",
        heard.display()
    ));
    let config = format!(
        "[ui.toast]\ndelivery = \"off\"\n\n[hooks]\n\"pane.agent_status_changed\" = {line}\n\"agent.idle\" = {line}\n\"agent.done\" = {line}\nnotification = {line}\n"
    );
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(&paths.config_file, &config).unwrap();
    let mut cfg = test_config();
    cfg.ui.toast.delivery = ToastDelivery::Off;
    cfg.hooks = toml::from_str::<Config>(&config).unwrap().hooks;

    let server = Server::start(None);
    let pane = server.pane();
    nest_a_shell(&server, &pane);
    let Some(taw::tmux::Socket::Name(socket)) = &server.0.socket else {
        panic!("the test server should use a named socket");
    };
    let shim = pausing_tmux(&server, tmp.path(), "0.6");
    count_reads();
    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let (cfg, paths, pane) = (cfg.clone(), paths.clone(), pane.clone());
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };
    assert!(
        poll_until(SETTLE, || !server
            .option(&pane, "@agent_watch_pid")
            .is_empty()),
        "the watcher never started"
    );
    // A report that beat the watcher's first read would be the state it found, and no change.
    let thread = watcher.thread().id();
    assert!(
        poll_until(SETTLE, || reads_of(thread) >= 1),
        "the watcher never read the pane"
    );
    // Output every 25 ms until the report is done, then the writer is dropped: the EOF that ends
    // the watcher.
    let done = Arc::new(AtomicBool::new(false));
    let feeder = {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let _ = writer.write_all(b".");
                let _ = writer.flush();
                std::thread::sleep(Duration::from_millis(25));
            }
        })
    };

    let out = Command::new(env!("CARGO_BIN_EXE_taw"))
        .args([
            "pane",
            "report-agent",
            &pane,
            "--source",
            "integration:opencode",
            "--agent",
            "opencode",
            "--state",
            "working",
        ])
        .env("TAW_TMUX_SOCKET_NAME", socket)
        .env("TAW_CONFIG_DIR", &paths.config_dir)
        .env("TAW_STATE_DIR", &paths.state_dir)
        .env("TAW_TMUX_BIN", &shim)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .unwrap();
    // A few more reads of the finished report.
    std::thread::sleep(Duration::from_millis(400));
    done.store(true, Ordering::Relaxed);
    feeder.join().unwrap();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(poll_until(SETTLE, || watcher.is_finished()));
    watcher.join().unwrap().unwrap();
    assert_eq!(server.option(&pane, "@agent_kind"), "opencode");
    assert_eq!(server.option(&pane, "@agent_state"), "working");
    assert_eq!(
        server.option(&pane, "@agent_source"),
        "hook:integration:opencode"
    );
    // What the finished report earns, once: a pane that gained an agent that is working. Nothing
    // of a report in progress: no agent that left, no `done`, no `idle`. The hook is a process of
    // its own, so it is waited for, and then waited out for anything that follows it.
    let heard_now = || std::fs::read_to_string(&heard).unwrap_or_default();
    assert!(
        poll_until(SETTLE, || heard_now()
            == "pane.agent_status_changed working\n"),
        "the finished report was never announced as a pane that gained a working agent: {:?}",
        heard_now()
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        heard_now(),
        "pane.agent_status_changed working\n",
        "the watcher announced more than the finished report"
    );
}

/// A config whose `[hooks] notification` appends `kind|message|session|pane|state` to
/// `recorded`, with the toast delivered nowhere (`off`) and the request and done delays as
/// given.
fn notification_hook_config(recorded: &Path, request_delay: f64, done_delay: f64) -> Config {
    let mut cfg = test_config();
    cfg.ui.toast.delivery = ToastDelivery::Off;
    cfg.ui.toast.delay_seconds = request_delay;
    cfg.ui.toast.done_delay_seconds = done_delay;
    let record = format!(
        "echo \"$TAW_KIND|$TAW_MESSAGE|$TAW_SESSION|$TAW_PANE|$TAW_STATE\" >> {}",
        recorded.display()
    );
    cfg.hooks = toml::from_str::<Config>(&format!(
        "[hooks]\nnotification = {}\n",
        toml::Value::String(record)
    ))
    .unwrap()
    .hooks;
    cfg
}

/// The `notification` hook, against a real watcher: an agent that blocks while the user is
/// away earns its hook when the toast's delay is up and not before, told the toast's own
/// text, that it is a request, and which pane; its finish earns one after the done delay.
#[test]
fn the_notification_hook_runs_when_a_toast_would_after_its_delay_and_says_what_it_is() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let recorded = tmp.path().join("notification-hook.txt");
    let cfg = notification_hook_config(&recorded, 3.0, 3.0);

    let server = Server::start(Some(fake_agent()));
    let pane = server.pane();
    let agent_pid = server.0.display(&pane, "#{pane_pid}").unwrap();
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("? for shortcuts")),
        "the fake agent never drew its idle screen"
    );
    seed_idle_claude(&server, &pane);

    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let (cfg, paths, pane) = (cfg.clone(), paths.clone(), pane.clone());
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };
    let lines = || -> Vec<String> {
        std::fs::read_to_string(&recorded)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    };

    signal(&agent_pid, "-USR1");
    assert!(await_state(&server, &pane, &mut writer, "working"));
    let blocked_at = Instant::now();
    signal(&agent_pid, "-USR2");
    assert!(await_state(&server, &pane, &mut writer, "blocked"));
    // The request toast waits 3 s: a hook that had run at the transition would be here now.
    assert!(
        lines().is_empty(),
        "the hook ran before the delay: {:?}",
        lines()
    );
    assert!(
        poll_until(SETTLE, || !lines().is_empty()),
        "the request never reached the hook; log {:?}",
        std::fs::read_to_string(paths.logs_dir.join("hooks.log")).unwrap_or_default()
    );
    assert!(
        blocked_at.elapsed() >= Duration::from_millis(2800),
        "the hook ran {:?} after the transition, before its 3 s delay",
        blocked_at.elapsed()
    );
    let request = lines()[0].clone();
    let fields: Vec<&str> = request.split('|').collect();
    assert_eq!(fields[0], "request", "{request}");
    assert!(
        fields[1].starts_with("claude blocked \u{b7} one:0 \u{b7} "),
        "{request}"
    );
    assert_eq!(&fields[2..], ["one", pane.as_str(), "blocked"], "{request}");

    // Back at the prompt in a session nobody is attached to is a finish: a done, after its own
    // delay.
    signal(&agent_pid, "-USR1");
    assert!(await_state(&server, &pane, &mut writer, "done"));
    assert_eq!(
        lines().len(),
        1,
        "the done ran before its delay: {:?}",
        lines()
    );
    assert!(poll_until(SETTLE, || lines().len() == 2), "{:?}", lines());
    let done = lines()[1].clone();
    assert!(done.starts_with("done|claude done \u{b7} one:0"), "{done}");

    drop(writer);
    assert!(poll_until(SETTLE, || watcher.is_finished()));
    watcher.join().unwrap().unwrap();
}

/// Never while the user looks: the agent's pane is the active window of an attached session,
/// so a request that comes up when the delay is over is for someone who is looking, and there
/// is no toast, and so no hook.
#[test]
fn the_notification_hook_does_not_run_while_the_user_is_looking_at_the_pane() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let recorded = tmp.path().join("notification-hook.txt");
    let cfg = notification_hook_config(&recorded, 1.0, 1.0);

    let server = Server::start(Some(fake_agent()));
    let pane = server.pane();
    let agent_pid = server.0.display(&pane, "#{pane_pid}").unwrap();
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("? for shortcuts")),
        "the fake agent never drew its idle screen"
    );
    seed_idle_claude(&server, &pane);
    let _client = Client::attach(&server);
    assert_eq!(server.0.display(&pane, "#{window_active}").unwrap(), "1");

    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let (cfg, paths, pane) = (cfg.clone(), paths.clone(), pane.clone());
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };

    signal(&agent_pid, "-USR1");
    assert!(await_state(&server, &pane, &mut writer, "working"));
    signal(&agent_pid, "-USR2");
    assert!(await_state(&server, &pane, &mut writer, "blocked"));
    // Well past the 1 s delay.
    std::thread::sleep(Duration::from_millis(3000));
    assert_eq!(
        std::fs::read_to_string(&recorded).unwrap_or_default(),
        "",
        "the hook ran for a pane the user was looking at"
    );

    drop(writer);
    assert!(poll_until(SETTLE, || watcher.is_finished()));
    watcher.join().unwrap().unwrap();
}

/// What a hook a watcher starts is given beside the `TAW_*` variables: tmux's global
/// environment, where a person keeps the credentials a push needs (`set-environment -g` in
/// `tmux.conf`), and `TMUX` for the server that fired it. A watcher is a `pipe-pane` child, which
/// has neither: only the environment the server was launched with. Both hooks a watcher fires
/// are checked, the agent's and the notification's, and a same-named global variable does not
/// replace the event's own.
#[test]
fn the_hooks_a_watcher_starts_see_tmuxs_global_environment_and_the_server_that_fired_them() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let agent_hook = tmp.path().join("agent-hook.txt");
    let notification_hook = tmp.path().join("notification-hook.txt");
    // The record is `topic|TMUX|event`, and is one line: nothing else of the environment is
    // written, since it may hold secrets.
    let record = |file: &Path| {
        toml::Value::String(format!(
            "echo \"$NTFY_TOPIC|$TMUX|$TAW_EVENT\" >> {}",
            file.display()
        ))
    };
    let mut cfg = test_config();
    cfg.ui.toast.delivery = ToastDelivery::Off;
    cfg.ui.toast.delay_seconds = 0.0;
    cfg.ui.toast.done_delay_seconds = 0.0;
    cfg.hooks = toml::from_str::<Config>(&format!(
        "[hooks]\n\"agent.done\" = {}\nnotification = {}\n",
        record(&agent_hook),
        record(&notification_hook)
    ))
    .unwrap()
    .hooks;

    let server = Server::start(Some(fake_agent()));
    let pane = server.pane();
    let agent_pid = server.0.display(&pane, "#{pane_pid}").unwrap();
    server
        .0
        .run_ok(&[
            "set-environment",
            "-g",
            "NTFY_TOPIC",
            "taw-hooks-test-topic",
        ])
        .unwrap();
    server
        .0
        .run_ok(&[
            "set-environment",
            "-g",
            "TAW_EVENT",
            "from-the-global-environment",
        ])
        .unwrap();
    let socket_path = server.0.display(&pane, "#{socket_path}").unwrap();
    let server_pid = server.0.display(&pane, "#{pid}").unwrap();
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("? for shortcuts")),
        "the fake agent never drew its idle screen"
    );
    seed_idle_claude(&server, &pane);

    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let (cfg, paths, pane) = (cfg.clone(), paths.clone(), pane.clone());
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };

    signal(&agent_pid, "-USR1");
    assert!(await_state(&server, &pane, &mut writer, "working"));
    signal(&agent_pid, "-USR1");
    assert!(await_state(&server, &pane, &mut writer, "done"));

    for (file, event) in [
        (&agent_hook, "agent.done"),
        (&notification_hook, "notification"),
    ] {
        assert!(
            poll_until(SETTLE, || std::fs::read_to_string(file)
                .is_ok_and(|text| !text.is_empty())),
            "the {event} hook never ran; log {:?}",
            std::fs::read_to_string(paths.logs_dir.join("hooks.log")).unwrap_or_default()
        );
        let text = std::fs::read_to_string(file).unwrap();
        let fields: Vec<&str> = text.trim().split('|').collect();
        assert_eq!(fields.len(), 3, "{event}: {text:?}");
        assert_eq!(
            fields[0], "taw-hooks-test-topic",
            "the {event} hook did not get tmux's global environment"
        );
        // `TMUX` is what tmux gives a pane: this server's socket and pid, and the session the
        // event is about, `$0`.
        assert_eq!(
            fields[1],
            format!("{socket_path},{server_pid},0"),
            "the {event} hook is not told which server fired it"
        );
        assert_eq!(fields[2], event, "the event's own name wins");
    }

    drop(writer);
    assert!(poll_until(SETTLE, || watcher.is_finished()));
    watcher.join().unwrap().unwrap();
}

/// Sets `name` in this process while it lives, and takes it out again after. A watcher run by a
/// test is a thread of it, so this is what the watcher's process has, which is what the real
/// one has of the environment the tmux server was launched with. `SERIAL` is held by every test
/// that uses it.
struct ProcessVar(&'static str);

impl ProcessVar {
    fn set(name: &'static str, value: &str) -> ProcessVar {
        // SAFETY: `SERIAL` keeps the tests that read or change the environment one at a time.
        unsafe { std::env::set_var(name, value) };
        ProcessVar(name)
    }
}

impl Drop for ProcessVar {
    fn drop(&mut self) {
        // SAFETY: as in `set`.
        unsafe { std::env::remove_var(self.0) };
    }
}

/// What a hook a watcher starts is given beside tmux's global environment: the environment of the
/// session the event is about, over it, which is what tmux gives a new pane of that session. A
/// watcher is a `pipe-pane` child, so its own environment is the one the server was launched
/// with. The ssh agent of the terminal a session was attached from is in the session's
/// environment, and not in that one or in tmux's global one. A variable the session removes
/// (`update-environment` marks what a client did not have) is taken out of what the watcher has,
/// and so is one the global environment removes. The session's environment is read for each
/// event, since a client attaching changes it, where the global one is read once: the next hook
/// has the ssh agent of the client that attached in between.
#[test]
fn the_hooks_a_watcher_starts_see_the_environment_of_their_session_over_the_global_one() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let recorded = tmp.path().join("session-env.txt");
    // One line for each hook: the event, the ssh agent, and two variables the watcher's process
    // has, one that the session removes and one that the global environment does.
    let record = toml::Value::String(format!(
        "echo \"$TAW_EVENT $SSH_AUTH_SOCK ${{STALE_IN_SESSION-unset}} ${{STALE_IN_GLOBAL-unset}}\" >> {}",
        recorded.display()
    ));
    let mut cfg = test_config();
    cfg.ui.toast.delivery = ToastDelivery::Off;
    cfg.hooks = toml::from_str::<Config>(&format!(
        "[hooks]\n\"agent.working\" = {record}\n\"agent.blocked\" = {record}\n\"agent.done\" = {record}\n"
    ))
    .unwrap()
    .hooks;
    let _has_session_var = ProcessVar::set("STALE_IN_SESSION", "the-watchers-own");
    let _has_global_var = ProcessVar::set("STALE_IN_GLOBAL", "the-watchers-own");

    let server = Server::start(Some(fake_agent()));
    let pane = server.pane();
    let agent_pid = server.0.display(&pane, "#{pane_pid}").unwrap();
    let set = |args: &[&str]| {
        let mut command = vec!["set-environment"];
        command.extend_from_slice(args);
        server.0.run_ok(&command).unwrap();
    };
    set(&["-g", "SSH_AUTH_SOCK", "global-sock"]);
    set(&["-t", "one", "SSH_AUTH_SOCK", "sock-of-the-first-client"]);
    set(&["-t", "one", "-r", "STALE_IN_SESSION"]);
    set(&["-g", "-r", "STALE_IN_GLOBAL"]);
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("? for shortcuts")),
        "the fake agent never drew its idle screen"
    );
    seed_idle_claude(&server, &pane);

    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let (cfg, paths, pane) = (cfg.clone(), paths.clone(), pane.clone());
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };
    let lines = || -> Vec<String> {
        std::fs::read_to_string(&recorded)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    };
    let waits_for = |line: &str| {
        assert!(
            poll_until(SETTLE, || lines().iter().any(|got| got == line)),
            "no {line:?}: got {:?}, log {:?}",
            lines(),
            std::fs::read_to_string(paths.logs_dir.join("hooks.log")).unwrap_or_default()
        );
    };

    signal(&agent_pid, "-USR1");
    assert!(await_state(&server, &pane, &mut writer, "working"));
    waits_for("agent.working sock-of-the-first-client unset unset");

    // Another terminal attaches: tmux puts its ssh agent in the session's environment. The
    // watcher was not told, and the next hook has it all the same.
    set(&["-t", "one", "SSH_AUTH_SOCK", "sock-of-the-second-client"]);
    signal(&agent_pid, "-USR2");
    assert!(await_state(&server, &pane, &mut writer, "blocked"));
    waits_for("agent.blocked sock-of-the-second-client unset unset");

    // A session that no longer says anything of it leaves the global environment's.
    set(&["-t", "one", "-u", "SSH_AUTH_SOCK"]);
    signal(&agent_pid, "-USR1");
    assert!(await_state(&server, &pane, &mut writer, "done"));
    waits_for("agent.done global-sock unset unset");

    drop(writer);
    assert!(poll_until(SETTLE, || watcher.is_finished()));
    watcher.join().unwrap().unwrap();
}

/// A watcher reads tmux's global environment once, not for every event, and reads it again
/// when its configuration is reloaded (`taw config reload` bumps `@agent_wizard_reload`): so a
/// variable changed with `set-environment -g` reaches the hooks after the reload and not before.
#[test]
fn a_watcher_reads_tmuxs_environment_once_and_again_after_a_reload() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let recorded = tmp.path().join("topics.txt");
    let record = toml::Value::String(format!(
        "echo \"$TAW_EVENT $NTFY_TOPIC\" >> {}",
        recorded.display()
    ));
    // A reload reads the config file again, so the settings under test are in the file.
    let config_toml = format!(
        "[update]\nmanifest_check = false\n[ui.toast]\ndelivery = \"off\"\n\
         delay_seconds = 0.0\ndone_delay_seconds = 0.0\n[ui.sound]\nenabled = false\n\
         [hooks]\n\"agent.working\" = {record}\n\"agent.blocked\" = {record}\n\"agent.done\" = {record}\n"
    );
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(&paths.config_file, &config_toml).unwrap();
    let cfg: Config = toml::from_str(&config_toml).unwrap();

    let server = Server::start(Some(fake_agent()));
    let pane = server.pane();
    let agent_pid = server.0.display(&pane, "#{pane_pid}").unwrap();
    let set_topic = |topic: &str| {
        server
            .0
            .run_ok(&["set-environment", "-g", "NTFY_TOPIC", topic])
            .unwrap();
    };
    set_topic("first-topic");
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("? for shortcuts")),
        "the fake agent never drew its idle screen"
    );
    seed_idle_claude(&server, &pane);

    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let (cfg, paths, pane) = (cfg.clone(), paths.clone(), pane.clone());
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };
    let lines = || -> Vec<String> {
        std::fs::read_to_string(&recorded)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    };
    let waits_for = |line: &str| {
        assert!(
            poll_until(SETTLE, || lines().iter().any(|got| got == line)),
            "no {line:?}: got {:?}, log {:?}",
            lines(),
            std::fs::read_to_string(paths.logs_dir.join("hooks.log")).unwrap_or_default()
        );
    };

    signal(&agent_pid, "-USR1");
    assert!(await_state(&server, &pane, &mut writer, "working"));
    waits_for("agent.working first-topic");

    // Changed while the watcher is running, and not yet reloaded: the next hook still has what
    // the watcher read, since it does not ask tmux for every event.
    set_topic("second-topic");
    signal(&agent_pid, "-USR2");
    assert!(await_state(&server, &pane, &mut writer, "blocked"));
    waits_for("agent.blocked first-topic");

    // `taw config reload`: every watcher reads again, the environment included.
    server
        .0
        .set_global_option(watch::reload::RELOAD_OPTION, "2")
        .unwrap();
    signal(&agent_pid, "-USR1");
    assert!(await_state(&server, &pane, &mut writer, "done"));
    waits_for("agent.done second-topic");

    drop(writer);
    assert!(poll_until(SETTLE, || watcher.is_finished()));
    watcher.join().unwrap().unwrap();
}

/// A symptom, end to end: after a reply, the pane went on reading `idle`
/// although the agent was working. A turn never stops redrawing its spinner, and the
/// watcher must not wait for a 400 ms quiet gap that such a turn never leaves, or it reads
/// the turn only on its 2 s tick. Output only makes the pane eligible for a read, and
/// the next tick reads it, however much more output follows.
///
/// The bytes a spinner's redraws would pipe to the watcher are written by this test every
/// 50 ms for the whole run; the fake agent draws its working screen once, which is all a
/// redraw changes as far as the rules can tell. The identity recheck is pushed out to
/// 30 s, so only the byte path can explain a read in time.
#[test]
fn a_continuously_redrawn_turn_reads_working_within_two_ticks() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();

    let server = Server::start(Some(fake_agent()));
    let pane = server.pane();
    let agent_pid = server.0.display(&pane, "#{pane_pid}").unwrap();
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("? for shortcuts")),
        "the fake agent never drew its idle screen"
    );
    // A shell-script fixture is `bash` to tmux; the hint is what names it claude.
    server
        .0
        .set_pane_option(&pane, "@agent_kind_hint", "claude")
        .unwrap();

    let timing = Timing {
        identity_recheck: Duration::from_secs(30),
        ..Timing::default()
    };
    let (reader, mut writer) = std::io::pipe().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let redraws = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = writer.write_all(b".");
                let _ = writer.flush();
                std::thread::sleep(Duration::from_millis(50));
            }
            // Dropping the writer here is the EOF that ends the watcher.
        })
    };
    let watcher = {
        let t = server.0.clone();
        let cfg = test_config();
        let paths = paths.clone();
        let pane = pane.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(&t, &cfg, &paths, &taw_bin, &pane, timing, Box::new(reader))
        })
    };

    assert!(
        poll_until(SETTLE, || server.option(&pane, "@agent_state") == "idle"),
        "the pane was never classified idle (state {:?})",
        server.option(&pane, "@agent_state")
    );

    signal(&agent_pid, "-USR1");
    // The turn starts when its screen is on the pane, not when the signal was sent: the
    // fake handles a signal only between its 200 ms sleeps.
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("esc to interrupt")),
        "the fake agent never drew its working screen"
    );
    let drawn = Instant::now();
    assert!(
        poll_until(SETTLE, || server.option(&pane, "@agent_state") == "working"),
        "the redrawn turn was never read as working (state {:?})",
        server.option(&pane, "@agent_state")
    );
    let latency = drawn.elapsed();
    // Two ticks, plus the read's own tmux round trips, this poll's granularity and a
    // loaded machine's scheduling (see `SETTLE`): 1.8 s. Still inside the old 2 s tick,
    // and far inside the 30 s recheck a read that output pushed back would wait for.
    let ceiling = timing.identified * 2 + Duration::from_millis(1200);
    assert!(
        latency <= ceiling,
        "`working` was written {latency:?} after the working screen appeared, over the \
         {ceiling:?} two ticks allow"
    );

    stop.store(true, Ordering::Relaxed);
    redraws.join().unwrap();
    assert!(
        poll_until(SETTLE, || watcher.is_finished()),
        "run_with_reader did not return on EOF"
    );
    watcher.join().unwrap().unwrap();
}

/// herdr's name owner, against tmux: a pane recorded as Claude, named `reviewer`, whose
/// foreground process is now Codex. The watcher records Codex, and the name goes with the
/// agent it was given to — unless the pane was set up for Codex (its kind hint, what `taw
/// agent start --kind codex` records before it writes Codex's own name), in which case
/// the name is left for `agent start`.
///
/// `#{pane_current_command}` is the name of the executable the pane runs, so a process
/// whose executable is called `codex` is a pane tmux says is running Codex: here a copy
/// of this suite's own tmux, left waiting in `wait-for` on the test server. (A copy of a
/// system binary such as `sleep` will not do: macOS kills a relocated platform binary.)
#[test]
fn a_name_goes_with_its_agent_when_another_takes_the_pane() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    // Copied once, before anything runs it: an executable rewritten in place after it has
    // run is killed on its next exec by macOS's code-signing cache.
    let codex = tmp.path().join("codex");
    std::fs::copy(Tmux::with_socket_name("unused").bin, &codex).unwrap();
    crate::common::make_executable(&codex);
    let mut cfg = test_config();
    cfg.ui.toast.delivery = ToastDelivery::Off;

    for hint in [None, Some("codex")] {
        let server = Server::start(None);
        let pane = server.pane();
        let Some(taw::tmux::Socket::Name(socket)) = &server.0.socket else {
            panic!("the test server should use a named socket");
        };
        server
            .0
            .run_ok(&[
                "respawn-pane",
                "-k",
                "-t",
                &pane,
                &format!("{} -L {socket} wait-for taw-test-never", codex.display()),
            ])
            .unwrap();
        let command = || server.0.display(&pane, "#{pane_current_command}").unwrap();
        assert!(
            poll_until(SETTLE, || command() == "codex"),
            "the pane never ran `codex` (it runs {:?})",
            command()
        );
        // What the watcher leaves on a pane where Claude, named `reviewer`, sat idle.
        for (option, value) in [
            ("@agent_kind", "claude"),
            ("@agent_state", "idle"),
            ("@agent_rule", "live_prompt_box"),
            ("@agent_source", "manifest"),
            ("@agent_seen", "1"),
            ("@agent_name", "reviewer"),
        ] {
            server.0.set_pane_option(&pane, option, value).unwrap();
        }
        if let Some(hint) = hint {
            server
                .0
                .set_pane_option(&pane, "@agent_kind_hint", hint)
                .unwrap();
        }

        let (reader, writer) = std::io::pipe().unwrap();
        let watcher = {
            let t = server.0.clone();
            let cfg = cfg.clone();
            let paths = paths.clone();
            let pane = pane.clone();
            let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
            std::thread::spawn(move || {
                watch::run_with_reader(
                    &t,
                    &cfg,
                    &paths,
                    &taw_bin,
                    &pane,
                    test_timing(),
                    Box::new(reader),
                )
            })
        };

        if hint.is_none() {
            assert!(
                poll_until(SETTLE, || server.option(&pane, "@agent_name").is_empty()),
                "Claude's name stayed on Codex's pane (kind {:?}, name {:?})",
                server.option(&pane, "@agent_kind"),
                server.option(&pane, "@agent_name")
            );
            assert_eq!(server.option(&pane, "@agent_kind"), "codex");
        } else {
            assert!(
                poll_until(SETTLE, || server.option(&pane, "@agent_kind") == "codex"),
                "the watcher never recorded Codex (kind {:?})",
                server.option(&pane, "@agent_kind")
            );
            assert!(
                !poll_until(Duration::from_secs(1), || server
                    .option(&pane, "@agent_name")
                    .is_empty()),
                "the name was removed from a pane `agent start` set up for Codex"
            );
        }

        drop(writer);
        assert!(
            poll_until(SETTLE, || watcher.is_finished()),
            "run_with_reader did not return on EOF"
        );
        watcher.join().unwrap().unwrap();
    }
}

#[test]
fn attach_opens_a_pipe_detach_closes_it_and_the_watch_option_disables_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let server = Server::start(None);
    let pane = server.pane();
    let tmp = tempfile::tempdir().unwrap();
    // `attach` runs `<taw_bin> watch run <pane>`, and `#{pane_pipe}` follows that
    // process, so a binary that exits at once would make the assertions below depend on
    // when tmux next tries to write to a closed pipe. This stub records its argv and
    // then drains the pipe until tmux closes it — what the real `taw watch run` does,
    // without a `cargo test` run spawning real watchers.
    //
    // Recording the argv is the point: tmux expands the `pipe-pane` command (both
    // `#`-formats and `strftime`) before `/bin/sh` sees it, so "a pipe exists" is *not*
    // evidence that `taw` was handed a usable pane id.
    let argv_log = tmp.path().join("argv.txt");
    let taw_bin = tmp.path().join("taw-stub");
    std::fs::write(
        &taw_bin,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexec cat > /dev/null\n",
            argv_log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        crate::common::make_executable(&taw_bin);
    }

    let piped = |pane: &str| server.0.display(pane, "#{pane_pipe}").unwrap();
    assert_eq!(piped(&pane), "0");

    assert!(watch::attach(&server.0, &pane, &taw_bin).unwrap());
    assert!(
        poll_until(SETTLE, || piped(&pane) == "1"),
        "pipe-pane did not open a pipe"
    );
    assert!(
        poll_until(SETTLE, || std::fs::read_to_string(&argv_log)
            .is_ok_and(|argv| argv.trim() == format!("watch run {pane}"))),
        "the piped command did not reach the binary intact: {:?}",
        std::fs::read_to_string(&argv_log)
    );
    // `taw watch run` is what got started; every pane on the server is reported.
    let statuses = watch::status(&server.0).unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].pane.pane_id, pane);
    assert!(statuses[0].piped);
    // The stub records no `@agent_watch_pid`, which is exactly the "piped but no live
    // watcher" case `watch status` exists to surface.
    assert!(!statuses[0].watcher_alive);

    watch::detach(&server.0, &pane).unwrap();
    assert!(
        poll_until(SETTLE, || piped(&pane) == "0"),
        "pipe-pane did not close the pipe"
    );

    server
        .0
        .set_global_option("@agent-wizard-watch", "off")
        .unwrap();
    assert!(
        !watch::attach(&server.0, &pane, &taw_bin).unwrap(),
        "attach must refuse while @agent-wizard-watch is off"
    );
    assert_eq!(piped(&pane), "0");
    assert_eq!(watch::attach_all(&server.0, &taw_bin).unwrap(), 0);

    server
        .0
        .set_global_option("@agent-wizard-watch", "on")
        .unwrap();
    assert_eq!(watch::attach_all(&server.0, &taw_bin).unwrap(), 1);
    assert!(poll_until(SETTLE, || piped(&pane) == "1"));
}

/// Writes an executable stub at `path` that records its argv (one line per invocation) to
/// `argv_log` and then drains stdin until tmux closes the pipe — same technique
/// `attach_opens_a_pipe_detach_closes_it_and_the_watch_option_disables_it` uses, so
/// `#{pane_pipe}` follows a real (if fake) watcher process rather than one that exits at
/// once.
fn write_taw_stub(path: &Path, argv_log: &Path) {
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexec cat > /dev/null\n",
            argv_log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        crate::common::make_executable(path);
    }
}

/// tmux 3.6b's `pipe-pane -o <cmd>` toggles even when the command is unchanged: a second
/// `attach` on an already-watched pane must not use `-o`, or it would silently turn
/// watching back off while `attach` still reports success. This is the regression test for
/// that: two `attach` calls on the same pane must both leave `#{pane_pipe}` at `1`, and
/// the watcher process itself must not be restarted (which would happen even with `-o`
/// dropped, if `attach` still reissued `pipe-pane` unconditionally — tmux replaces the
/// pipe process on every `pipe-pane` call, restarting it and losing whatever state it was
/// mid-evaluation with).
#[test]
fn attach_twice_on_the_same_pane_does_not_restart_the_watcher() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let server = Server::start(None);
    let pane = server.pane();
    let tmp = tempfile::tempdir().unwrap();
    let argv_log = tmp.path().join("argv.txt");
    let taw_bin = tmp.path().join("taw-stub");
    write_taw_stub(&taw_bin, &argv_log);

    let piped = |pane: &str| server.0.display(pane, "#{pane_pipe}").unwrap();
    assert_eq!(piped(&pane), "0");

    assert!(watch::attach(&server.0, &pane, &taw_bin).unwrap());
    assert!(
        poll_until(SETTLE, || piped(&pane) == "1"),
        "the first attach did not open a pipe"
    );
    assert!(
        poll_until(SETTLE, || std::fs::read_to_string(&argv_log)
            .is_ok_and(|argv| argv.lines().count() == 1)),
        "the stub should have been started exactly once: {:?}",
        std::fs::read_to_string(&argv_log)
    );

    // The second attach: on tmux 3.6b, `pipe-pane -o` with the same command here would
    // toggle the pipe closed while `attach` still returned `Ok(true)`.
    assert!(watch::attach(&server.0, &pane, &taw_bin).unwrap());
    assert_eq!(
        piped(&pane),
        "1",
        "a second attach on an already-watched pane must leave the pipe open"
    );
    // Give tmux a moment to have acted on any (wrongly) reissued pipe-pane before checking
    // it settled.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        piped(&pane),
        "1",
        "the pipe should still be open after a short delay"
    );
    // The watcher (the stub) must not have been restarted: a wrongly reissued pipe-pane —
    // with or without `-o` — replaces the pipe process, which would append a second line
    // here.
    assert_eq!(
        std::fs::read_to_string(&argv_log).unwrap().lines().count(),
        1,
        "the watcher process must not be restarted by a second attach"
    );
}

/// `attach_all` at plugin startup, run after the per-pane hooks have already attached each
/// pane individually — exactly the sequence a plugin start runs. The second sweep
/// must report `0` newly attached, and every pane must still be piped afterwards.
#[test]
fn attach_all_twice_is_idempotent() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let server = Server::start(None);
    // A second pane, so the whole-server sweep covers more than one.
    server.0.run_ok(&["split-window", "-t", "one"]).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let argv_log = tmp.path().join("argv.txt");
    let taw_bin = tmp.path().join("taw-stub");
    write_taw_stub(&taw_bin, &argv_log);

    let panes: Vec<String> = server
        .0
        .list_panes()
        .unwrap()
        .into_iter()
        .map(|p| p.pane_id)
        .collect();
    assert_eq!(panes.len(), 2, "expected two panes after the split");
    let piped = |pane: &str| server.0.display(pane, "#{pane_pipe}").unwrap();

    assert_eq!(watch::attach_all(&server.0, &taw_bin).unwrap(), 2);
    for pane in &panes {
        assert!(
            poll_until(SETTLE, || piped(pane) == "1"),
            "pane {pane} was not piped by the first attach_all"
        );
    }

    assert_eq!(
        watch::attach_all(&server.0, &taw_bin).unwrap(),
        0,
        "a second attach_all must not count already-watched panes as newly attached"
    );
    for pane in &panes {
        assert_eq!(
            piped(pane),
            "1",
            "pane {pane} should still be piped after the second sweep"
        );
    }
}

/// `detach` then `attach` must reopen the pipe: closing it first takes the pane back to
/// "no pipe", so the following `attach` is the ordinary not-yet-watched path.
#[test]
fn detach_then_attach_reopens_the_pipe() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let server = Server::start(None);
    let pane = server.pane();
    let tmp = tempfile::tempdir().unwrap();
    let argv_log = tmp.path().join("argv.txt");
    let taw_bin = tmp.path().join("taw-stub");
    write_taw_stub(&taw_bin, &argv_log);
    let piped = |pane: &str| server.0.display(pane, "#{pane_pipe}").unwrap();

    assert!(watch::attach(&server.0, &pane, &taw_bin).unwrap());
    assert!(poll_until(SETTLE, || piped(&pane) == "1"));

    watch::detach(&server.0, &pane).unwrap();
    assert!(poll_until(SETTLE, || piped(&pane) == "0"));

    assert!(watch::attach(&server.0, &pane, &taw_bin).unwrap());
    assert!(
        poll_until(SETTLE, || piped(&pane) == "1"),
        "attach after detach should reopen the pipe"
    );
}

/// The watcher must not poll. A pane with no agent in it gets one evaluation and then
/// nothing at all until it produces output — no tick, no `capture-pane`, no CPU.
///
/// Proved by counting: the `Tmux` handed to `run_with_reader` is pointed at a wrapper
/// script that records every invocation and then execs the real tmux, so "no tmux calls"
/// is measured rather than asserted from the shape of the code.
#[test]
fn a_pane_with_no_agent_is_never_polled() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let server = Server::start(None);
    let pane = server.pane();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();

    let calls = tmp.path().join("tmux-calls.txt");
    let wrapper = tmp.path().join("tmux-counting");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\necho call >> {}\nexec {} \"$@\"\n",
            calls.display(),
            server.0.bin.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        crate::common::make_executable(&wrapper);
    }
    let counting = Tmux {
        bin: wrapper,
        socket: server.0.socket.clone(),
    };
    let count = || {
        std::fs::read_to_string(&calls)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    };

    let (reader, writer) = std::io::pipe().unwrap();
    let watcher = {
        let cfg = test_config();
        let paths = paths.clone();
        let pane = pane.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        // Every interval far shorter than the quiet window below: if the loop ticked or
        // re-checked a pane with no agent in it, this would be ~20 extra reads.
        let timing = Timing {
            identified: Duration::from_millis(50),
            unidentified: Duration::from_millis(50),
            hold_recheck: Duration::from_millis(50),
            identity_recheck: Duration::from_millis(50),
        };
        std::thread::spawn(move || {
            watch::run_with_reader(
                &counting,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                timing,
                Box::new(reader),
            )
        })
    };

    // Let the startup evaluation happen and settle. Its calls land one after another, and how long
    // each takes is the machine's business, so the first look is over when the count has stood still
    // for a second, and not after a stretch of the clock: a look still going would read as a poll.
    assert!(
        poll_until(SETTLE, || count() > 0),
        "the watcher never made its first tmux call"
    );
    let mut last_move = (count(), Instant::now());
    assert!(
        poll_until(SETTLE, || {
            if count() != last_move.0 {
                last_move = (count(), Instant::now());
            }
            last_move.1.elapsed() >= Duration::from_secs(1)
        }),
        "the watcher's tmux calls never stood still for a second ({} so far): it polls",
        count()
    );
    let settled = count();
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        count(),
        settled,
        "a quiet pane with no agent must cost nothing after the first look"
    );

    drop(writer);
    assert!(
        poll_until(SETTLE, || watcher.is_finished()),
        "run_with_reader did not return on EOF"
    );
    watcher.join().unwrap().unwrap();
}

/// A `Tmux` pointed at a wrapper that appends each invocation's arguments to a log, one
/// line per call — the watcher's display format holds real newlines and tabs, folded to
/// spaces here — and then runs the real tmux: what the watcher costs is counted rather
/// than read off the code.
fn counting_tmux(server: &Server, dir: &Path) -> (Tmux, PathBuf) {
    let calls = dir.join("tmux-calls.txt");
    let wrapper = dir.join("tmux-counting");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n{{ printf '%s' \"$*\" | tr '\\n\\r\\t' '   '; echo; }} >> {}\nexec {} \"$@\"\n",
            calls.display(),
            server.0.bin.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        crate::common::make_executable(&wrapper);
    }
    let counting = Tmux {
        bin: wrapper,
        socket: server.0.socket.clone(),
    };
    (counting, calls)
}

/// How many of the calls `counting_tmux` logged ran `subcommand`.
fn calls_of(log: &Path, subcommand: &str) -> usize {
    std::fs::read_to_string(log)
        .map(|text| {
            text.lines()
                .filter(|line| line.split_whitespace().any(|word| word == subcommand))
                .count()
        })
        .unwrap_or(0)
}

/// Writes a byte of pane output every 100 ms until the returned flag is set, then drops
/// the writer: the EOF that ends the watcher.
fn keep_printing(
    mut writer: std::io::PipeWriter,
) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let printing = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = writer.write_all(b".");
                let _ = writer.flush();
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };
    (stop, printing)
}

/// A quiet agent pane is re-read at every identity recheck, and a recheck of a pane that
/// printed nothing reuses the last capture: tmux changes a screen only when something in
/// it prints. So an idle, silent Claude costs one `display-message` per recheck and no
/// `capture-pane` after the first read.
#[test]
fn a_quiet_agent_pane_is_rechecked_without_being_captured_again() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let server = Server::start(Some(fake_agent()));
    let pane = server.pane();
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("? for shortcuts")),
        "the fake agent never drew its idle screen"
    );
    seed_idle_claude(&server, &pane);
    let (counting, calls) = counting_tmux(&server, tmp.path());

    let (reader, writer) = std::io::pipe().unwrap();
    let watcher = {
        let cfg = test_config();
        let paths = paths.clone();
        let pane = pane.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        let timing = Timing {
            identity_recheck: Duration::from_millis(300),
            ..test_timing()
        };
        std::thread::spawn(move || {
            watch::run_with_reader(
                &counting,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                timing,
                Box::new(reader),
            )
        })
    };
    // The first read is due at once, and captures.
    assert!(
        poll_until(SETTLE, || calls_of(&calls, "capture-pane") >= 1),
        "the watcher never read the pane"
    );
    let displays = calls_of(&calls, "display-message");
    // Three rechecks, with nothing printed: counted, so that a slow machine takes longer over them
    // and no more.
    assert!(
        poll_until(SETTLE, || calls_of(&calls, "display-message")
            >= displays + 3),
        "the quiet pane was not rechecked ({} display calls, {displays} before)",
        calls_of(&calls, "display-message")
    );
    assert_eq!(
        calls_of(&calls, "capture-pane"),
        1,
        "a recheck of a pane that printed nothing captured it again"
    );
    assert_eq!(server.option(&pane, "@agent_state"), "idle");

    drop(writer);
    assert!(
        poll_until(SETTLE, || watcher.is_finished()),
        "run_with_reader did not return on EOF"
    );
    watcher.join().unwrap().unwrap();
}

/// A busy agent pane is read every tick, and each read takes its display line and its
/// screen in one tmux invocation (`display-message … ; capture-pane …`) rather than two:
/// half the processes spawned per read. The screen still arrives whole — the idle rule it
/// matches stays the pane's evidence.
#[test]
fn a_busy_agent_pane_costs_one_tmux_call_per_read() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let server = Server::start(Some(fake_agent()));
    let pane = server.pane();
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("? for shortcuts")),
        "the fake agent never drew its idle screen"
    );
    seed_idle_claude(&server, &pane);
    let (counting, calls) = counting_tmux(&server, tmp.path());

    let (reader, writer) = std::io::pipe().unwrap();
    let (stop, printing) = keep_printing(writer);
    let watcher = {
        let cfg = test_config();
        let paths = paths.clone();
        let pane = pane.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &counting,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };
    assert!(
        poll_until(SETTLE, || calls_of(&calls, "capture-pane") >= 1),
        "the watcher never read the pane"
    );
    // Five reads of the printing pane, counted: how long the machine takes to make them is its own
    // business, so this waits for the fifth and does not measure a stretch of the clock.
    let log = || std::fs::read_to_string(&calls).unwrap_or_default();
    let reads_with_screen = || {
        log()
            .lines()
            .filter(|line| line.contains("display-message") && line.contains("capture-pane"))
            .count()
    };
    assert!(
        poll_until(SETTLE, || reads_with_screen() >= 5),
        "the reads did not take the screen with the display ({} did)",
        reads_with_screen()
    );
    let captures_alone = log()
        .lines()
        .filter(|line| line.contains("capture-pane") && !line.contains("display-message"))
        .count();
    assert!(
        captures_alone <= 1,
        "{captures_alone} captures took an invocation of their own; only the first read's may"
    );
    assert!(
        poll_until(SETTLE, || server.option(&pane, "@agent_state") == "idle"
            && server.option(&pane, "@agent_rule") == "live_prompt_box"),
        "the pane did not read idle by the prompt box ({:?} by {:?})",
        server.option(&pane, "@agent_state"),
        server.option(&pane, "@agent_rule")
    );

    stop.store(true, Ordering::Relaxed);
    printing.join().unwrap();
    assert!(
        poll_until(SETTLE, || watcher.is_finished()),
        "run_with_reader did not return on EOF"
    );
    watcher.join().unwrap().unwrap();
}

/// A new agent reads `unknown` for its 3 s startup grace whatever its screen shows —
/// herdr does not read the screen inside the window either — so the reads its output
/// makes due during the grace take the display alone, which still sees it exit. The
/// screen is captured again for the read that ends the grace.
///
/// What is asserted is the order of the watcher's calls, and no time. The grace is 3 s of
/// the wall clock, and a window of 2 s counted from when this test saw `unknown`, with a
/// count of the reads made in it, measures the machine as much as the watcher: on a busy
/// one the test's own polls and its sleep run long, the watcher's reads take too long to
/// make eight of them, or the grace ends inside the window, and the test fails with the
/// watcher right. The log of calls says in what order the watcher did things, which does
/// not depend on how long each took: between the write of `unknown` and the write of the
/// state that ends the grace the screen was captured once, for the read that ended it, and
/// the reads before it took the display alone.
#[test]
fn a_new_agent_is_not_captured_during_its_startup_grace() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let server = Server::start(Some(fake_agent()));
    let pane = server.pane();
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("? for shortcuts")),
        "the fake agent never drew its idle screen"
    );
    // What `taw agent start` leaves before the watcher has looked: the hint, no state.
    server
        .0
        .set_pane_option(&pane, "@agent_kind_hint", "claude")
        .unwrap();
    let (counting, calls) = counting_tmux(&server, tmp.path());

    let (reader, writer) = std::io::pipe().unwrap();
    let (stop, printing) = keep_printing(writer);
    let watcher = {
        let cfg = test_config();
        let paths = paths.clone();
        let pane = pane.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &counting,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };
    // The grace ends when the screen is believed: the state that is written then is the idle
    // the fake's prompt box is.
    assert!(
        poll_until(SETTLE, || server.option(&pane, "@agent_state") == "idle"),
        "the grace never ended (state {:?})",
        server.option(&pane, "@agent_state")
    );

    let log = std::fs::read_to_string(&calls).unwrap_or_default();
    let log: Vec<&str> = log.lines().collect();
    let written = state_writes(&log);
    let states: Vec<&str> = written.iter().map(|(_, state)| state.as_str()).collect();
    assert_eq!(
        states,
        ["unknown", "idle"],
        "the pane went through other states than the grace's `unknown` and what ended it"
    );
    let (began, ended) = (written[0].0, written[1].0);
    let inside = &log[began + 1..ended];
    let captured = inside
        .iter()
        .filter(|call| names_subcommand(call, "capture-pane"))
        .count();
    assert_eq!(
        captured, 1,
        "inside the startup grace the screen was captured {captured} times, and only the read \
         that ends it may: {inside:#?}"
    );
    // The reads the printing pane made due before that one took the display alone. How many the
    // machine had time for is its business, and two are what it takes to say that it was read
    // while the grace ran and not once, as it is when the pane is first looked at.
    let before_the_capture = inside
        .iter()
        .take_while(|call| !names_subcommand(call, "capture-pane"))
        .filter(|call| call.contains("pane_current_command"))
        .count();
    assert!(
        before_the_capture >= 2,
        "the printing pane was read {before_the_capture} times inside the startup grace, and two \
         are needed to say that its reads took the display alone: {inside:#?}"
    );

    stop.store(true, Ordering::Relaxed);
    printing.join().unwrap();
    assert!(
        poll_until(SETTLE, || watcher.is_finished()),
        "run_with_reader did not return on EOF"
    );
    watcher.join().unwrap().unwrap();
}

/// The states the watcher wrote to `@agent_state`, each with the position in the log of the call
/// that wrote it, in the order they were written.
fn state_writes(log: &[&str]) -> Vec<(usize, String)> {
    log.iter()
        .enumerate()
        .filter_map(|(at, call)| {
            let words: Vec<&str> = call.split_whitespace().collect();
            let option = words.iter().position(|word| *word == "@agent_state")?;
            words.contains(&"set-option").then_some(())?;
            Some((at, (*words.get(option + 1)?).to_string()))
        })
        .collect()
}

/// Whether a logged call ran `subcommand` (a word of its own: `capture-pane` is not a part of a
/// longer one).
fn names_subcommand(call: &str, subcommand: &str) -> bool {
    call.split_whitespace().any(|word| word == subcommand)
}

/// A pane `taw agent start` set up keeps its kind hint after its agent exits — by design,
/// herdr's too — so the shell the user goes on using in it is read at the agent cadence.
/// Its screen can say nothing about an agent the dead start pid already says is gone, so
/// once a read has found it gone, the reads the shell's output makes due take the
/// display (and the pid check) alone.
#[test]
fn an_exited_hinted_agents_shell_is_not_captured_on_every_read() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let server = Server::start(None);
    let pane = server.pane();
    // A pid that has exited: the agent `agent start` launched, gone.
    let mut gone = Command::new("true").spawn().unwrap();
    let gone_pid = gone.id().to_string();
    gone.wait().unwrap();
    // What the watcher leaves on such a pane: the hint, the pid, and `unknown`.
    for (option, value) in [
        ("@agent_kind_hint", "claude"),
        ("@agent_pid", gone_pid.as_str()),
        ("@agent_kind", "claude"),
        ("@agent_state", "unknown"),
        ("@agent_source", "manifest"),
    ] {
        server.0.set_pane_option(&pane, option, value).unwrap();
    }
    let (counting, calls) = counting_tmux(&server, tmp.path());

    let (reader, writer) = std::io::pipe().unwrap();
    let (stop, printing) = keep_printing(writer);
    let watcher = {
        let cfg = test_config();
        let paths = paths.clone();
        let pane = pane.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &counting,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };
    assert!(
        poll_until(SETTLE, || calls_of(&calls, "capture-pane") >= 1),
        "the watcher never read the pane"
    );
    let displays = calls_of(&calls, "display-message");
    // Five more reads, counted and not timed: a slow machine takes longer over them and no more.
    assert!(
        poll_until(SETTLE, || calls_of(&calls, "display-message")
            >= displays + 5),
        "the printing pane was not read every tick ({} display calls, {displays} before)",
        calls_of(&calls, "display-message")
    );
    assert_eq!(
        calls_of(&calls, "capture-pane"),
        1,
        "the shell of an agent already found gone was captured on every read"
    );
    assert_eq!(server.option(&pane, "@agent_state"), "unknown");
    assert_eq!(server.option(&pane, "@agent_kind_hint"), "claude");

    stop.store(true, Ordering::Relaxed);
    printing.join().unwrap();
    assert!(
        poll_until(SETTLE, || watcher.is_finished()),
        "run_with_reader did not return on EOF"
    );
    watcher.join().unwrap().unwrap();
}

/// A tab in a user-set option must not cost the pane its watcher.
///
/// `set-option -p @agent_display "a<tab>b"` round-trips the tab, and the evaluation reads
/// twelve tab-separated columns out of one `display-message` — five of which are free
/// text a user or a hook can set. The columns are de-tabbed by tmux itself; this proves
/// it end to end, on the field most likely to hold prose.
#[test]
fn a_tab_in_a_user_set_option_does_not_break_the_watcher() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();

    let server = Server::start(Some(fake_agent()));
    let pane = server.pane();
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("? for shortcuts")),
        "the fake agent never drew its idle screen"
    );
    server
        .0
        .set_pane_option(&pane, "@agent_kind_hint", "claude")
        .unwrap();
    // Tabs in free-text columns, one of them *before* the agent's own options (`@agent_source`, the
    // column before the state), so a positional misread could not go unnoticed. A session's name
    // is not one of them: 3.6 stores a tab in a name as the two characters `\t` and 3.8 refuses the
    // name ("invalid session name"), so no name puts a tab in the format.
    server
        .0
        .set_pane_option(&pane, "@agent_source", "a\tb")
        .unwrap();
    server
        .0
        .set_pane_option(&pane, "@agent_display", "code\treview")
        .unwrap();
    server
        .0
        .set_pane_option(&pane, "@agent_name", "rev\tiewer")
        .unwrap();

    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let cfg = test_config();
        let paths = paths.clone();
        let pane = pane.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };

    let deadline = Instant::now() + SETTLE;
    let mut reached = false;
    while Instant::now() < deadline && !reached {
        let _ = writer.write_all(b".");
        let _ = writer.flush();
        reached = poll_until(Duration::from_millis(400), || {
            server.option(&pane, "@agent_state") == "idle"
        });
    }
    assert!(
        reached,
        "the watcher should still classify the pane (state {:?})",
        server.option(&pane, "@agent_state")
    );
    // Polled, not read: `write_state` sets `@agent_state` before `@agent_kind`, in
    // separate `set-option` calls.
    assert!(
        poll_until(SETTLE, || server.option(&pane, "@agent_kind") == "claude"),
        "@agent_kind should be claude, got {:?}",
        server.option(&pane, "@agent_kind")
    );

    drop(writer);
    assert!(poll_until(SETTLE, || watcher.is_finished()));
    watcher.join().unwrap().unwrap();
}

/// A tmux call that fails is not a pane that vanished.
///
/// Proves both halves: several failed `display-message` calls in a row leave the watcher
/// alive and still owing an evaluation (which it runs, and gets right, once tmux answers
/// again), and a pane that really is gone retires it — *without* EOF on the pipe, which
/// this test holds open throughout so that EOF cannot be what ends the loop.
#[test]
fn survives_transient_tmux_failures_and_retires_only_when_its_pane_is_gone() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();

    let server = Server::start(None);
    // A second window, so killing the watched pane leaves the server (and this test's
    // own tmux calls) alive: what is under test is a gone *pane*, not a gone server.
    server.0.run_ok(&["new-window", "-t", "one"]).unwrap();
    let pane = server.0.display("one:1", "#{pane_id}").unwrap();

    // A stale agent record, so a *successful* evaluation of this plain shell pane is
    // observable: it clears the options.
    server
        .0
        .set_pane_option(&pane, "@agent_kind", "claude")
        .unwrap();
    server
        .0
        .set_pane_option(&pane, "@agent_state", "working")
        .unwrap();

    // While the marker exists, every `display-message` through this wrapper fails.
    let marker = tmp.path().join("fail-display");
    let attempts = tmp.path().join("display-attempts.txt");
    let wrapper = tmp.path().join("tmux-flaky");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = display-message ] && [ -f {marker} ]; then\n    echo attempt >> {attempts}\n    echo \"taw-test: injected tmux failure\" >&2\n    exit 1\n  fi\ndone\nexec {tmux} \"$@\"\n",
            marker = marker.display(),
            attempts = attempts.display(),
            tmux = server.0.bin.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        crate::common::make_executable(&wrapper);
    }
    std::fs::write(&marker, b"").unwrap();
    let flaky = Tmux {
        bin: wrapper,
        socket: server.0.socket.clone(),
    };
    let attempt_count = || {
        std::fs::read_to_string(&attempts)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    };

    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let cfg = test_config();
        let paths = paths.clone();
        let pane = pane.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        let timing = Timing {
            identified: Duration::from_millis(50),
            unidentified: Duration::from_millis(50),
            hold_recheck: Duration::from_millis(50),
            identity_recheck: Duration::from_secs(5),
        };
        std::thread::spawn(move || {
            watch::run_with_reader(
                &flaky,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                timing,
                Box::new(reader),
            )
        })
    };

    // Three attempts: `run_with_reader` resolves the pane id once at startup, then the
    // loop's own evaluations — so this can only be reached by retrying after a failure.
    assert!(
        poll_until(SETTLE, || attempt_count() >= 3),
        "the watcher gave up instead of retrying (attempts: {})",
        attempt_count()
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !watcher.is_finished(),
        "a failing tmux call must not retire the watcher"
    );
    assert_eq!(
        server.option(&pane, "@agent_state"),
        "working",
        "nothing should have been written while tmux was failing"
    );

    // tmux answers again: the evaluation the watcher still owes now runs, and clears the
    // stale record of a pane running a plain shell.
    std::fs::remove_file(&marker).unwrap();
    assert!(
        poll_until(SETTLE, || server.option(&pane, "@agent_kind").is_empty()),
        "the retried evaluation never happened (@agent_kind {:?})",
        server.option(&pane, "@agent_kind")
    );

    // Now the pane really is gone. The pipe is still open, so only the pane-gone path
    // can end the loop.
    server.0.run_ok(&["kill-pane", "-t", &pane]).unwrap();
    let _ = writer.write_all(b".");
    let _ = writer.flush();
    assert!(
        poll_until(SETTLE, || watcher.is_finished()),
        "a gone pane must retire the watcher even with the pipe still open"
    );
    watcher.join().unwrap().unwrap();
    drop(writer);
}

/// A missing pane must be recognised from the `display-message` expansion alone.
///
/// This is the branch a single-column probe gets wrong: `display -p -t %999 '#{pane_id}'`
/// answers with an empty string, but the real sixteen-column format answers with its
/// fifteen *literal* tabs and nothing else — success, not an error, and not empty. Today
/// `capture-pane` would catch the missing pane one call later with `can't find pane`, so
/// this test removes that safety net: the wrapper fails every `capture-pane` with a
/// message that is *not* tmux's, which the watcher classifies as transient and retries
/// forever. So the watcher can only return here if the display expansion is what told
/// it — and the attempt counter proves `capture-pane` was never even reached.
///
/// With `focus-events` on, which is when the terminal-focus column expands at all: its
/// client loop still runs for a missing pane, and would mark the client running the
/// command itself unless the column tests the pane's session first.
#[test]
fn a_missing_pane_is_recognised_from_the_display_expansion_alone() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();

    // A live server, so tmux itself answers every call — only the *pane* is missing.
    let server = Server::start(None);
    server
        .0
        .run_ok(&["set-option", "-g", "focus-events", "on"])
        .unwrap();
    let missing_pane = "%999";
    assert!(
        server
            .0
            .list_panes()
            .unwrap()
            .iter()
            .all(|pane| pane.pane_id != missing_pane),
        "%999 must not exist on this server"
    );

    let captures = tmp.path().join("capture-attempts.txt");
    let wrapper = tmp.path().join("tmux-no-capture");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = capture-pane ]; then\n    echo attempt >> {captures}\n    echo \"taw-test: capture-pane must not be reached\" >&2\n    exit 1\n  fi\ndone\nexec {tmux} \"$@\"\n",
            captures = captures.display(),
            tmux = server.0.bin.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        crate::common::make_executable(&wrapper);
    }
    let no_capture = Tmux {
        bin: wrapper,
        socket: server.0.socket.clone(),
    };

    // The writer is held for the whole test, so EOF cannot be what ends the loop.
    let (reader, _writer) = std::io::pipe().unwrap();
    let watcher = {
        let cfg = test_config();
        let paths = paths.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &no_capture,
                &cfg,
                &paths,
                &taw_bin,
                missing_pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };

    assert!(
        poll_until(SETTLE, || watcher.is_finished()),
        "the watcher kept retrying a pane that does not exist"
    );
    watcher.join().unwrap().unwrap();
    assert_eq!(
        std::fs::read_to_string(&captures)
            .map(|text| text.lines().count())
            .unwrap_or(0),
        0,
        "a missing pane must be settled by the display expansion, without capturing it"
    );
}

/// A rule id with a tab in it must reach `@agent_rule` folded, exactly as one written by
/// `store::write_state` on a transition does.
///
/// Rule ids come from manifest TOML, and a manifest is user-editable — an override in
/// `<config>/agent-detection/` shadows the bundled one, which is what this test installs.
/// The `RuleChanged` write path was added later than `write_state` and wrote the value
/// raw, so a tab there would have shifted every later column of the pane listings that
/// the fold exists to protect.
#[test]
fn a_rule_id_with_a_tab_reaches_the_pane_option_folded() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();

    // One rule, above every bundled priority, matching the fake agent's idle screen —
    // with a tab in its id. (`region` has to be one the engine knows, or the override is
    // rejected and the bundled manifest is used instead.)
    std::fs::write(
        paths.override_dir.join("claude.toml"),
        "id = \"claude\"\n\
         version = \"9999.01.01.1\"\n\
         min_engine_version = 2\n\
         updated_at = \"2026-01-01T00:00:00Z\"\n\
         \n\
         [[rules]]\n\
         id = \"tabbed\trule\"\n\
         state = \"idle\"\n\
         priority = 5000\n\
         region = \"whole_recent\"\n\
         regex = ['for shortcuts']\n",
    )
    .unwrap();

    let server = Server::start(Some(fake_agent()));
    let pane = server.pane();
    assert!(
        poll_until(SETTLE, || server
            .0
            .capture_pane(&pane, false)
            .unwrap()
            .contains("? for shortcuts")),
        "the fake agent never drew its idle screen"
    );
    // Seeded so the watcher's first pass is a `RuleChanged` and not a transition: the
    // state it will evaluate to is already recorded, the kind agrees, and only the rule
    // is stale. A transition would go through `store::write_state`, which has always
    // folded — this has to exercise the `RuleChanged` write specifically.
    for (name, value) in [
        ("@agent_kind_hint", "claude"),
        ("@agent_kind", "claude"),
        ("@agent_state", "idle"),
        ("@agent_seen", "1"),
        ("@agent_source", "manifest"),
        ("@agent_rule", "some_older_rule"),
    ] {
        server.0.set_pane_option(&pane, name, value).unwrap();
    }

    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let cfg = test_config();
        let paths = paths.clone();
        let pane = pane.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };

    let deadline = Instant::now() + SETTLE;
    let mut reached = false;
    while Instant::now() < deadline && !reached {
        let _ = writer.write_all(b".");
        let _ = writer.flush();
        reached = poll_until(Duration::from_millis(400), || {
            server.option(&pane, "@agent_rule") == "tabbed rule"
        });
    }
    assert!(
        reached,
        "@agent_rule should hold the folded rule id, got {:?}",
        server.option(&pane, "@agent_rule")
    );
    // The state itself did not move, which is what makes this the `RuleChanged` path.
    assert_eq!(server.option(&pane, "@agent_state"), "idle");
    // And the listing the fold protects still reads this pane as one row.
    assert_eq!(
        server
            .0
            .list_panes()
            .unwrap()
            .iter()
            .filter(|p| p.pane_id == pane)
            .count(),
        1
    );

    drop(writer);
    assert!(poll_until(SETTLE, || watcher.is_finished()));
    watcher.join().unwrap().unwrap();
}

/// The `pipe-pane` watcher must reach the server that owns the pipe, not tmux's default
/// socket. A `pipe-pane` child does not inherit a usable `$TMUX`, so the socket has to be
/// baked into the command `watch attach` opens; this drives the real product path through
/// a real tmux server and proves the spawned child is handed that server's socket.
///
/// `taw` is stood in for by a wrapper that records the socket env it was launched with —
/// a full watcher is unnecessary and the wrapper never connects anywhere, so nothing here
/// can touch any real server. Without the pin the recorded name is empty (and `$TMUX` is
/// empty for a pipe-pane child on Linux), which is exactly the failure the bug caused.
#[test]
fn the_watcher_pipe_command_carries_the_socket_to_the_spawned_child() {
    // Like every other test here: this one starts a tmux server of its own, and a
    // concurrent test's server, fake agent and winding-down watcher are exactly the load
    // that makes `pipe-pane`'s child slow to appear.
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let server = Server::start(None);
    let pane = server.pane();
    let socket_name = match &server.0.socket {
        Some(taw::tmux::Socket::Name(name)) => name.clone(),
        other => panic!("the test server should use a named socket, got {other:?}"),
    };

    let tmp = tempfile::tempdir().unwrap();
    let record = tmp.path().join("socket-env.txt");
    let wrapper = tmp.path().join("fake-taw");
    // Written beside its name and renamed over it, so a file that is there is all there: a plain
    // redirect creates the file empty before `printf` writes it, and a poll between the two reads "".
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf 'NAME=%s\\nPATH=%s\\nTMUX=%s\\n' \
             \"$TAW_TMUX_SOCKET_NAME\" \"$TAW_TMUX_SOCKET_PATH\" \"$TMUX\" > {0}.part && mv {0}.part {0}\n",
            record.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        crate::common::make_executable(&wrapper);
    }

    watch::attach(&server.0, &pane, &wrapper).unwrap();
    // `pipe-pane` starts its command as the pipe opens; a little pane output guarantees the
    // wrapper has run and written its record.
    server
        .0
        .run_ok(&["send-keys", "-t", &pane, "echo hi", "Enter"])
        .unwrap();

    assert!(
        poll_until(SETTLE, || record.exists()),
        "the pipe-pane child never ran"
    );
    let recorded = std::fs::read_to_string(&record).unwrap();
    assert!(
        recorded.contains(&format!("NAME={socket_name}")),
        "the watcher child was not pinned to the pipe's own server: {recorded}"
    );
}

/// A pane whose identity comes only from its kind hint (`taw agent start`) must report
/// `unknown` once the launched process has exited — even while the exited agent's last
/// screen still matches a rule, which is exactly when the pane would otherwise report a
/// live idle agent for ever. The hint is deliberately kept (herdr never clears it either;
/// spec: "the hint outlives its occupant, and that is why `unknown` exists"), so the pane
/// becomes `unknown`, not "no agent". The recorded `@agent_pid` is what lets the watcher
/// tell an exited agent from a still-running one whose process name it does not match.
#[test]
fn a_hint_only_pane_goes_unknown_keeping_its_hint_once_its_recorded_agent_pid_exits() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let server = Server::start(None); // a plain `/bin/sh` pane: its command names no agent
    let pane = server.pane();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();

    // A live process stands in for the running agent; an OSC title matching the bundled
    // claude `osc_title_idle` rule (`^✳ `) stands in for its last screen — evidence that
    // deliberately outlives the process. So the exit cannot be read from the screen (it
    // still says "idle"); only the dead pid gives it away. That is the exact shape of the
    // bug: without the pid, the pane reports a live idle agent for ever.
    let mut agent_proc = Command::new("sleep").arg("30").spawn().unwrap();
    let agent_pid = agent_proc.id().to_string();
    server
        .0
        .run_ok(&["select-pane", "-t", &pane, "-T", "\u{2733} idle"])
        .unwrap();
    server
        .0
        .set_pane_option(&pane, "@agent_kind_hint", "claude")
        .unwrap();
    server
        .0
        .set_pane_option(&pane, "@agent_pid", &agent_pid)
        .unwrap();

    let (reader, mut writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let cfg = test_config();
        let paths = paths.clone();
        let pane = pane.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                test_timing(),
                Box::new(reader),
            )
        })
    };
    let nudge = |w: &mut std::io::PipeWriter| {
        let _ = w.write_all(b".");
        let _ = w.flush();
    };

    // While the recorded pid is alive, the matching title is taken at face value: the pane
    // is a live `idle` claude, explained by that very rule.
    let idle = {
        let deadline = Instant::now() + SETTLE;
        loop {
            nudge(&mut writer);
            if poll_until(Duration::from_millis(400), || {
                server.option(&pane, "@agent_state") == "idle"
            }) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
        }
    };
    assert!(
        idle,
        "a live hint-only agent on an idle screen must read idle (@agent_state = {:?})",
        server.option(&pane, "@agent_state")
    );
    assert_eq!(
        server.option(&pane, "@agent_rule"),
        "osc_title_idle",
        "the live pane's idle is explained by the title rule"
    );

    // The agent exits: its recorded pid becomes dead, while `@agent_pid` and the "idle"
    // title are both unchanged — the screen still matches.
    agent_proc.kill().unwrap();
    agent_proc.wait().unwrap();

    // The same still-matching screen must now read `unknown`, the honest state for a pane
    // whose agent is gone, with no rule to explain it. Poll on @agent_state.
    let became_unknown = {
        let deadline = Instant::now() + SETTLE;
        loop {
            nudge(&mut writer);
            if poll_until(Duration::from_millis(400), || {
                server.option(&pane, "@agent_state") == "unknown"
            }) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
        }
    };
    assert!(
        became_unknown,
        "the exited hint-only agent never became unknown (@agent_state = {:?})",
        server.option(&pane, "@agent_state")
    );
    assert_eq!(
        server.option(&pane, "@agent_rule"),
        "",
        "no rule can explain a state a stale screen only seemed to show"
    );
    // The hint outlives its occupant by design, so the identity is kept — `unknown`, not
    // "no agent": the pane is still the place a claude belongs.
    assert_eq!(server.option(&pane, "@agent_kind_hint"), "claude");
    assert_eq!(server.option(&pane, "@agent_kind"), "claude");

    drop(writer);
    assert!(poll_until(SETTLE, || watcher.is_finished()));
    watcher.join().unwrap().unwrap();
}

/// A Claude whose own hook reported its session is still a Claude that taw names from its
/// process, so what follows it in the pane, `claude; npm run dev`, is a miss of it like any
/// other and the sixth clears the record, session id and all. herdr keeps an agent it cannot
/// identify by its process however long another program stays in front of it; it does not do
/// that for one it can.
///
/// The Claude is a copy of this suite's tmux named `claude`, and what follows it is `sleep`. The
/// recheck that counts the misses is 200 ms here, not the 5 s a real watcher waits, which is
/// what keeps the six of them under two seconds instead of half a minute.
#[test]
fn a_claude_that_reported_its_session_is_cleared_by_the_misses_when_another_program_follows_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let claude = tmp.path().join("claude");
    std::fs::copy(Tmux::with_socket_name("unused").bin, &claude).unwrap();
    crate::common::make_executable(&claude);

    let server = Server::start(None);
    let pane = server.pane();
    let Some(taw::tmux::Socket::Name(socket)) = &server.0.socket else {
        panic!("the test server should use a named socket");
    };
    // Claude, then a program that runs for as long as the test needs.
    server
        .0
        .run_ok(&[
            "send-keys",
            "-t",
            &pane,
            &format!(
                "{} -L {socket} wait-for taw-test-quit; sleep 300",
                claude.display()
            ),
            "Enter",
        ])
        .unwrap();
    let command = || server.0.display(&pane, "#{pane_current_command}").unwrap();
    assert!(
        poll_until(SETTLE, || command() == "claude"),
        "Claude never took the pane (it runs {:?})",
        command()
    );

    let timing = Timing {
        identified: Duration::from_millis(100),
        unidentified: Duration::from_millis(100),
        hold_recheck: Duration::from_millis(50),
        identity_recheck: Duration::from_millis(200),
    };
    let (reader, writer) = std::io::pipe().unwrap();
    let watcher = {
        let t = server.0.clone();
        let paths = paths.clone();
        let pane = pane.clone();
        let taw_bin = PathBuf::from(env!("CARGO_BIN_EXE_taw"));
        let cfg = test_config();
        std::thread::spawn(move || {
            watch::run_with_reader(&t, &cfg, &paths, &taw_bin, &pane, timing, Box::new(reader))
        })
    };
    assert!(
        poll_until(SETTLE, || server.option(&pane, "@agent_kind") == "claude"),
        "the watcher never named Claude from its process"
    );

    // Claude's own hook reports the session, with the call the real one makes.
    let reported = Command::new(env!("CARGO_BIN_EXE_taw"))
        .args([
            "pane",
            "report-agent-session",
            &pane,
            "--source",
            "integration:claude",
            "--agent",
            "claude",
            "--agent-session-id",
            "3f0c2a9e-7d41-4b6e-9c55-2b1e8a6d0f42",
        ])
        .env("TAW_TMUX_SOCKET_NAME", socket)
        .env("TAW_CONFIG_DIR", &paths.config_dir)
        .env("TAW_STATE_DIR", &paths.state_dir)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .unwrap();
    assert!(
        reported.status.success(),
        "the report failed: {}{}",
        String::from_utf8_lossy(&reported.stdout),
        String::from_utf8_lossy(&reported.stderr)
    );
    assert_eq!(server.option(&pane, "@agent_reported_kind"), "claude");

    // Claude quits and `sleep` takes the terminal: no shell, another program, for good.
    server
        .0
        .run_ok(&["wait-for", "-S", "taw-test-quit"])
        .unwrap();
    assert!(
        poll_until(SETTLE, || command() == "sleep"),
        "the program after Claude never started (the pane runs {:?})",
        command()
    );
    assert!(
        poll_until(SETTLE, || server.option(&pane, "@agent_kind").is_empty()
            && server.option(&pane, "@agent_session_id").is_empty()),
        "the record outlasted the misses: kind {:?}, session {:?}",
        server.option(&pane, "@agent_kind"),
        server.option(&pane, "@agent_session_id")
    );
    assert_eq!(
        command(),
        "sleep",
        "it went while the program was still running, which is what is asked"
    );

    drop(writer);
    assert!(poll_until(SETTLE, || watcher.is_finished()));
    watcher.join().unwrap().unwrap();
}

/// When another agent takes a pane the last one's own report goes: its session, and its word that
/// it was there. But only the report the replacing read saw: the agent that took the pane may
/// have reported its own session between that read and the write, and that must survive.
#[test]
fn the_last_agents_report_goes_unless_the_new_agent_has_reported_since() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let server = Server::start(None);
    let pane = server.pane();
    let report = |session: Option<&str>, path: Option<&str>, kind: &str| {
        for (option, value) in [
            ("@agent_session_id", session),
            ("@agent_session_path", path),
            ("@agent_reported_kind", Some(kind)),
        ] {
            match value {
                Some(value) => server.0.set_pane_option(&pane, option, value).unwrap(),
                None => server.0.unset_pane_option(&pane, option).unwrap(),
            }
        }
    };
    let held = || {
        [
            "@agent_session_id",
            "@agent_session_path",
            "@agent_reported_kind",
        ]
        .map(|option| server.option(&pane, option))
    };

    // The id Letta writes for a default conversation has a colon in it, which the comparison
    // has to read as part of the id.
    for saw in ["conv-1", "default:agent-1"] {
        report(Some(saw), Some("/tmp/one.jsonl"), "letta");
        watch::release_identity(&server.0, &pane, Some(saw), "").unwrap();
        assert_eq!(held(), ["", "", ""], "{saw}");
    }

    // The new agent's own report landed after the read that saw the old one's.
    report(Some("conv-2"), Some("/tmp/two.jsonl"), "claude");
    watch::release_identity(&server.0, &pane, Some("conv-1"), "").unwrap();
    assert_eq!(held(), ["conv-2", "/tmp/two.jsonl", "claude"]);

    // A pane that held no session: only its word that it was there goes, and a session
    // reported since is left alone.
    report(None, None, "letta");
    watch::release_identity(&server.0, &pane, None, "").unwrap();
    assert_eq!(held(), ["", "", ""]);
    report(Some("conv-3"), None, "claude");
    watch::release_identity(&server.0, &pane, None, "").unwrap();
    assert_eq!(held(), ["conv-3", "", "claude"]);

    // An id no report can have written is not compared with, and cannot be put into the command.
    report(Some("x;kill-server"), None, "letta");
    watch::release_identity(&server.0, &pane, Some("x;kill-server"), "").unwrap();
    assert_eq!(held(), ["", "", ""]);

    // A report since the read, which moved the report stamp, is the new agent's, whatever session it
    // carried: what it left stays. The stamp the read saw still there, the drop is made.
    report(None, None, "claude");
    server
        .0
        .set_pane_option(&pane, "@agent_report_stamp", "1790800000000000002")
        .unwrap();
    assert!(!watch::release_identity(&server.0, &pane, None, "1790800000000000001").unwrap());
    assert_eq!(held(), ["", "", "claude"]);
    assert!(watch::release_identity(&server.0, &pane, None, "1790800000000000002").unwrap());
    assert_eq!(held(), ["", "", ""]);
}
