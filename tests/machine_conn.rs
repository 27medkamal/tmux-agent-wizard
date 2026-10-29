//! The app's connection to a machine, headless: `conn::spawn` and `Machines` against the stand-in ssh
//! (`tests/common/machine.rs`), which runs the far side's own `taw machine feed`, and against scripts that
//! say what a feed might. No real ssh runs.
//!
//! No test waits a fixed time for something to happen: each waits for an event, a recorded call or a file,
//! with a deadline that panics with what was seen.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use common::machine::{Failure, FarSide, StandIn};
use common::write_script;
use taw::machine::Machine;
use taw::machine::conn::{ConnCtx, ConnEvent, ConnHandle, Timing, spawn};
use taw::machine::feed::server_word;
use taw::machine::files::{self, Machines};
use taw::machine::ssh::Ssh;
use taw::machine::wire::{self, Down, Hello, Left, Toast, Up};
use taw::machine::words;
use taw::paths::Paths;
use taw::tmux::Tmux;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const WAIT: Duration = Duration::from_secs(10);

fn machine(name: &str) -> Machine {
    Machine {
        name: name.to_string(),
        ssh: name.to_string(),
        taw: None,
    }
}

/// The paths of an app: its own state folder, as the app of one machine has.
fn app_paths(dir: &Path) -> Paths {
    Paths::rooted_at(dir.join("config"), dir.join("state"))
}

fn far_paths(far: &FarSide) -> Paths {
    Paths::rooted_at(far.config_dir.clone(), far.state_dir.clone())
}

fn quick() -> Timing {
    // Only for a feed that never says a word: a real feed pings every 5 seconds.
    Timing {
        quiet: Duration::from_millis(1500),
        ..Timing::default()
    }
}

fn ctx(ssh: &Path, paths: &Paths, timing: Timing) -> ConnCtx {
    ConnCtx {
        ssh: Ssh {
            bin: ssh.to_path_buf(),
        },
        paths: paths.clone(),
        holder: "mac-1".to_string(),
        timing,
        local_server: None,
    }
}

/// The events of connections, as they arrive.
struct Events {
    rx: Receiver<ConnEvent>,
    seen: Vec<ConnEvent>,
}

impl Events {
    fn new() -> (Sender<ConnEvent>, Events) {
        let (tx, rx) = channel();
        (
            tx,
            Events {
                rx,
                seen: Vec::new(),
            },
        )
    }

    /// The next event `pick` takes, within `within`; the ones before it are passed over. Panics with
    /// everything seen so far.
    fn wait<T>(
        &mut self,
        what: &str,
        within: Duration,
        pick: impl Fn(&ConnEvent) -> Option<T>,
    ) -> T {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(event) => {
                    let found = pick(&event);
                    self.seen.push(event);
                    if let Some(found) = found {
                        return found;
                    }
                }
                Err(_) => panic!("no {what} within {within:?}; seen: {:#?}", self.seen),
            }
        }
    }

    fn words(&mut self, live: bool) -> String {
        self.wait("words", WAIT, |event| match event {
            ConnEvent::Words {
                live: is, words, ..
            } if *is == live => Some(words.clone()),
            _ => None,
        })
    }

    /// The words and their short form of the next not-live words event.
    fn words_and_short(&mut self) -> (String, String) {
        self.wait("words", WAIT, |event| match event {
            ConnEvent::Words {
                live: false,
                words,
                short,
                ..
            } => Some((words.clone(), short.clone())),
            _ => None,
        })
    }

    fn state(&mut self) -> wire::State {
        self.wait("state", WAIT, |event| match event {
            ConnEvent::State { state, .. } => Some(state.clone()),
            _ => None,
        })
    }

    fn hello(&mut self) -> Hello {
        self.wait("hello", WAIT, |event| match event {
            ConnEvent::Hello { hello, .. } => Some(hello.clone()),
            _ => None,
        })
    }

    /// Everything that arrives within `within` (after something else that should have arrived did).
    fn quiet_for(&mut self, within: Duration) -> Vec<ConnEvent> {
        let mut got = Vec::new();
        let deadline = Instant::now() + within;
        while let Ok(event) = self
            .rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            got.push(event);
        }
        got
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only looks whether the process is there.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// A script standing in for ssh that says `lines` (one JSON line each) and then `tail` (a shell
/// command): the destination and options are ignored. `exec sleep` for a feed that goes on.
fn scripted(dir: &Path, lines: &[String], tail: &str) -> PathBuf {
    let said = dir.join("said.txt");
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(&said, text).unwrap();
    let bin = dir.join("scripted-ssh");
    write_script(&bin, &format!("cat '{}'\n{tail}\n", said.display()));
    bin
}

fn hello_line(taw: &str, protocol: u32, server: &str) -> String {
    serde_json::json!({"type": "hello", "taw": taw, "protocol": protocol, "host": "studio", "server": server})
        .to_string()
}

fn state_line(state: &serde_json::Value) -> String {
    let mut line = state.clone();
    line["type"] = "state".into();
    line.to_string()
}

const GOES_ON: &str = "exec sleep 30";

#[test]
fn a_connection_says_hello_then_the_far_states_and_its_words() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut events) = Events::new();
    let _conn = spawn(
        ctx(&stand_in.bin, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    let hello = events.hello();
    assert_eq!(hello.taw, VERSION);
    assert_eq!(hello.protocol, 1);
    assert_eq!(
        events.words(true),
        words::connected(VERSION),
        "the words of a connected machine"
    );
    let state = events.state();
    assert_eq!(state.spaces.len(), 1);
    assert_eq!(state.spaces[0].session_name, "garden");
    assert!(state.spaces[0].session_id.starts_with('$'));
    assert_eq!(state.panes.len(), 1);
    // The feed was run with the protocol, in the feed's own mode.
    let calls = stand_in.calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0]
            .last()
            .is_some_and(|word| word.ends_with("'machine' 'feed' '--protocol' '1'")),
        "{calls:?}"
    );
}

#[test]
fn a_window_renamed_on_the_far_side_reaches_the_next_state_within_a_second_and_a_half() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut events) = Events::new();
    let _conn = spawn(
        ctx(&stand_in.bin, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    let first = events.state();
    assert_ne!(first.panes[0].window_name, "renamed");
    far.server
        .tmux_ok(&["rename-window", "-t", "=garden:0", "renamed"]);
    // The first state in which the window is renamed, within 1.5 s of the rename. States before it are
    // passed over: a far shell that is still changing its title makes some, and they are not the point.
    let state = events.wait(
        "a state with the window renamed",
        Duration::from_millis(1500),
        |event| match event {
            ConnEvent::State { state, .. }
                if state
                    .panes
                    .first()
                    .is_some_and(|pane| pane.window_name == "renamed") =>
            {
                Some(state.clone())
            }
            _ => None,
        },
    );
    assert_eq!(state.panes[0].window_name, "renamed");
}

#[test]
fn a_feed_that_says_nothing_is_dropped_after_the_quiet_time_and_tried_again() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    stand_in.fail_as("studio", Failure::Hang);
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut events) = Events::new();
    let started = Instant::now();
    let _conn = spawn(
        ctx(&stand_in.bin, &app_paths(dir.path()), quick()),
        0,
        machine("studio"),
        tx,
    );
    let said = events.words(false);
    assert!(
        started.elapsed() >= Duration::from_millis(1400),
        "dropped before the quiet time: {:?}",
        started.elapsed()
    );
    assert_eq!(said, "not answering, trying again in 1s");
    // The ssh that said nothing was ended by its pid.
    let first = stand_in.pids()[0];
    assert!(
        common::wait_for(|| !alive(first), Duration::from_secs(2)),
        "the quiet ssh {first} is still there"
    );
    // And the next try is made, since this wait is the backoff's first (none).
    assert!(
        common::wait_for(|| stand_in.calls().len() >= 2, Duration::from_secs(5)),
        "no second try: {:?}",
        stand_in.calls()
    );
}

#[test]
fn something_that_is_not_a_feed_gives_the_words_for_it() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    stand_in.fail_as("studio", Failure::Garbage);
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut events) = Events::new();
    let _conn = spawn(
        ctx(&stand_in.bin, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    assert_eq!(events.words(false), words::garbage_header());
    assert_eq!(
        words::garbage("studio"),
        "studio sent something taw does not read"
    );
}

/// A scripted feed's first words, as the one connection says them.
fn words_of(lines: &[String], tail: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let script = scripted(dir.path(), lines, tail);
    let (tx, mut events) = Events::new();
    let _conn = spawn(
        ctx(&script, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    events.words(false)
}

/// A valid state line of exactly `bytes` bytes before its newline: an unknown field pads it, which a
/// feed from a later taw may well carry.
fn state_line_of(bytes: usize) -> String {
    let bare = state_line(&serde_json::json!({"spaces": [], "pad": ""}));
    assert!(bare.len() < bytes);
    let line =
        state_line(&serde_json::json!({"spaces": [], "pad": "a".repeat(bytes - bare.len())}));
    assert_eq!(line.len(), bytes);
    line
}

#[test]
fn a_valid_line_of_more_than_a_mebibyte_gives_the_words_for_something_not_read() {
    // Valid JSON, so that only the cap on the length can make it not read.
    let said = words_of(
        &[hello_line(VERSION, 1, ""), state_line_of(1_153_434)],
        GOES_ON,
    );
    assert_eq!(said, words::garbage_header());
    // One byte more than the cap is too long too.
    let said = words_of(
        &[hello_line(VERSION, 1, ""), state_line_of(1_048_577)],
        GOES_ON,
    );
    assert_eq!(said, words::garbage_header());
}

#[test]
fn a_valid_line_of_exactly_a_mebibyte_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let script = scripted(
        dir.path(),
        &[hello_line(VERSION, 1, ""), state_line_of(1_048_576)],
        GOES_ON,
    );
    let (tx, mut events) = Events::new();
    let _conn = spawn(
        ctx(&script, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    // A state arrives (and not the words for something not read).
    assert!(events.state().spaces.is_empty());
    assert!(
        !events
            .seen
            .iter()
            .any(|event| matches!(event, ConnEvent::Words { live: false, .. })),
        "{:#?}",
        events.seen
    );
}

#[test]
fn an_output_that_ends_in_the_middle_of_a_line_of_exactly_a_mebibyte_is_an_end_and_not_garbage() {
    // The cap is on what is more than a mebibyte: a partial line of exactly that many bytes, at the
    // end of the output, is only the end.
    let dir = tempfile::tempdir().unwrap();
    let script = scripted(
        dir.path(),
        &[hello_line(VERSION, 1, "")],
        "head -c 1048576 /dev/zero | tr '\\0' a",
    );
    let (tx, mut events) = Events::new();
    let _conn = spawn(
        ctx(&script, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    let said = events.words(false);
    assert!(said.starts_with("not answering"), "{said}");
}

#[test]
fn a_line_with_no_newline_that_goes_past_a_mebibyte_ends_the_connection_and_does_not_hang() {
    // More than the cap and no newline at all, then a feed that goes on: the cap ends it, not the end of
    // the line (which never comes) or the quiet time (the default 15 s).
    let dir = tempfile::tempdir().unwrap();
    let said = dir.path().join("said.txt");
    std::fs::write(&said, format!("{}\n", hello_line(VERSION, 1, ""))).unwrap();
    let bin = dir.path().join("scripted-ssh");
    write_script(
        &bin,
        &format!(
            "cat '{}'\nhead -c 1200000 /dev/zero | tr '\\0' a\nexec sleep 30\n",
            said.display()
        ),
    );
    let (tx, mut events) = Events::new();
    let started = Instant::now();
    let _conn = spawn(
        ctx(&bin, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    assert_eq!(events.words(false), words::garbage_header());
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_line_that_is_not_json_after_the_hello_ends_the_connection_as_not_read() {
    let said = words_of(
        &[hello_line(VERSION, 1, ""), "this is not json".to_string()],
        GOES_ON,
    );
    assert_eq!(said, words::garbage_header());
}

#[test]
fn a_first_line_that_is_not_a_hello_ends_the_connection_as_not_read() {
    let said = words_of(&[serde_json::json!({"type": "ping"}).to_string()], GOES_ON);
    assert_eq!(said, words::garbage_header());
    let said = words_of(&[serde_json::json!({"type": "later"}).to_string()], GOES_ON);
    assert_eq!(said, words::garbage_header());
}

#[test]
fn a_state_of_more_than_5000_panes_ends_the_connection_as_not_read() {
    let panes = |n: usize| -> Vec<serde_json::Value> {
        (0..n)
            .map(|i| serde_json::json!({"pane_id": format!("%{i}"), "session_id": "$1", "window_id": "@1"}))
            .collect()
    };
    let state = |n| state_line(&serde_json::json!({"panes": panes(n)}));
    let said = words_of(&[hello_line(VERSION, 1, ""), state(5001)], GOES_ON);
    assert_eq!(said, words::garbage_header());
    // 5000 is still a state: the connection says connected and goes on.
    let dir = tempfile::tempdir().unwrap();
    let script = scripted(
        dir.path(),
        &[hello_line(VERSION, 1, ""), state(5000)],
        GOES_ON,
    );
    let (tx, mut events) = Events::new();
    let _conn = spawn(
        ctx(&script, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    assert_eq!(events.state().panes.len(), 5000);
}

#[test]
fn a_kind_of_line_or_a_field_taw_does_not_know_is_skipped_and_the_connection_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let lines = [
        hello_line(VERSION, 1, ""),
        serde_json::json!({"type": "from-a-later-taw", "x": 1}).to_string(),
        state_line(
            &serde_json::json!({"spaces": [{"session_id": "$1", "session_name": "garden", "colour": "blue"}], "later": [1]}),
        ),
    ];
    let script = scripted(dir.path(), &lines, GOES_ON);
    let (tx, mut events) = Events::new();
    let _conn = spawn(
        ctx(&script, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    assert_eq!(events.state().spaces[0].session_name, "garden");
}

#[test]
fn each_failure_that_needs_you_gives_its_words() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    for (how, want, short) in [
        (Failure::NoTaw, words::no_taw(), words::no_taw_short()),
        (
            Failure::OldTaw("0.0.9".to_string()),
            words::no_machines(""),
            words::no_machines_short(),
        ),
        (
            Failure::PermissionDenied,
            words::login(),
            words::login_short(),
        ),
        (
            Failure::HostKey,
            words::host_key("studio"),
            words::host_key_short("studio"),
        ),
    ] {
        stand_in.fail_as("studio", how.clone());
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut events) = Events::new();
        let _conn = spawn(
            ctx(&stand_in.bin, &app_paths(dir.path()), Timing::default()),
            0,
            machine("studio"),
            tx,
        );
        // The short form is made with the words: the header never matches strings to find it.
        assert_eq!(events.words_and_short(), (want, short), "{how:?}");
    }
}

#[test]
fn a_taw_of_another_protocol_is_newer_in_words_and_one_with_no_protocol_is_not_a_feed() {
    // Only the words are checked here; that the wait after them is 30 seconds is pinned by the unit test
    // in `conn.rs` (each outcome that needs you waits 30 seconds).
    let dir = tempfile::tempdir().unwrap();
    let script = scripted(dir.path(), &[hello_line("9.0.0", 2, "")], GOES_ON);
    let (tx, mut events) = Events::new();
    let _conn = spawn(
        ctx(&script, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    assert_eq!(
        events.words_and_short(),
        (words::newer("9.0.0", VERSION), words::newer_short())
    );
    // A hello with no protocol is not a newer taw: it is not a feed.
    let line = serde_json::json!({"type": "hello", "taw": "9.0.0"}).to_string();
    assert_eq!(words_of(&[line], GOES_ON), words::garbage_header());
}

#[test]
fn a_needs_you_failure_is_not_tried_again_at_once() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    stand_in.fail_as("studio", Failure::NoTaw);
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut events) = Events::new();
    let _conn = spawn(
        ctx(&stand_in.bin, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    // Positive: the words came, so the try happened and ended.
    assert_eq!(events.words(false), words::no_taw());
    assert!(events.quiet_for(Duration::from_millis(700)).is_empty());
    assert_eq!(stand_in.calls().len(), 1, "{:?}", stand_in.calls());
}

#[test]
fn waking_a_waiting_connection_tries_it_now() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    stand_in.fail_as("studio", Failure::NoTaw);
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut events) = Events::new();
    let conn = spawn(
        ctx(&stand_in.bin, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    assert_eq!(events.words(false), words::no_taw());
    assert_eq!(stand_in.calls().len(), 1);
    // The user fixes it, and shows the list: the machine connects without waiting out the 30 seconds.
    stand_in.fail_as("studio", Failure::Garbage);
    conn.wake();
    assert_eq!(events.words(false), words::garbage_header());
    assert_eq!(stand_in.calls().len(), 2);
}

#[test]
fn a_hello_that_names_this_machines_own_server_is_this_tmux_server_with_no_rows() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    let dir = tempfile::tempdir().unwrap();
    let mut context = ctx(&stand_in.bin, &app_paths(dir.path()), Timing::default());
    context.local_server = Some(server_word(&Tmux::with_socket_name(far.server.socket())));
    let (tx, mut events) = Events::new();
    let _conn = spawn(context, 0, machine("studio"), tx);
    assert_eq!(events.words(false), words::this_server("studio"));
    // Positive done (the words came); nothing is listed and nothing connected came before them.
    assert!(
        events.seen.iter().all(|event| matches!(
            event,
            ConnEvent::Hello { .. } | ConnEvent::Words { live: false, .. }
        )),
        "{:?}",
        events.seen
    );
    assert!(
        events
            .quiet_for(Duration::from_millis(700))
            .iter()
            .all(|event| !matches!(event, ConnEvent::State { .. }))
    );
    // Retried at the slow pace.
    assert_eq!(stand_in.calls().len(), 1);
}

#[test]
fn a_later_hello_that_names_this_machines_own_server_ends_the_feed_as_this_tmux_server() {
    let dir = tempfile::tempdir().unwrap();
    let said = dir.path().join("said.txt");
    let lines = [
        hello_line("0.1.0", 1, "10 100"),
        state_line(&serde_json::json!({"spaces": [{"session_id": "$1", "session_name": "one"}]})),
        hello_line("0.1.0", 1, "20 200"),
        state_line(&serde_json::json!({"spaces": [{"session_id": "$1", "session_name": "two"}]})),
    ];
    std::fs::write(&said, format!("{}\n", lines.join("\n"))).unwrap();
    // Counts its runs, so that a retry shows.
    let runs = dir.path().join("runs.txt");
    let script = dir.path().join("scripted-ssh");
    write_script(
        &script,
        &format!(
            "echo run >> '{}'\ncat '{}'\n{GOES_ON}\n",
            runs.display(),
            said.display()
        ),
    );
    let mut context = ctx(&script, &app_paths(dir.path()), Timing::default());
    context.local_server = Some("20 200".to_string());
    let (tx, mut events) = Events::new();
    let _conn = spawn(context, 0, machine("studio"), tx);
    // The first hello is another server's: connected, and its state is listed.
    assert_eq!(events.words(true), words::connected("0.1.0"));
    assert_eq!(events.state().spaces[0].session_name, "one");
    // The later one is this machine's own: the words for it, and nothing after them.
    assert_eq!(events.words(false), words::this_server("studio"));
    let after = events.quiet_for(Duration::from_millis(700));
    assert!(
        after
            .iter()
            .all(|event| !matches!(event, ConnEvent::State { .. } | ConnEvent::Hello { .. })),
        "{after:?}"
    );
    // Retried at the needs-you pace (30 s), not at once: one run so far.
    let count = std::fs::read_to_string(&runs).unwrap().lines().count();
    assert_eq!(count, 1);
}

#[test]
fn a_later_hello_on_an_open_feed_replaces_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let lines = [
        hello_line("0.1.0", 1, "10 100"),
        state_line(&serde_json::json!({"spaces": [{"session_id": "$1", "session_name": "one"}]})),
        hello_line("0.1.0", 1, "20 200"),
        state_line(&serde_json::json!({"spaces": [{"session_id": "$1", "session_name": "two"}]})),
    ];
    let script = scripted(dir.path(), &lines, GOES_ON);
    let (tx, mut events) = Events::new();
    let _conn = spawn(
        ctx(&script, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    assert_eq!(events.hello().server, "10 100");
    assert_eq!(events.state().spaces[0].session_name, "one");
    assert_eq!(events.hello().server, "20 200");
    assert_eq!(events.state().spaces[0].session_name, "two");
    // The connection never dropped.
    assert!(
        events
            .seen
            .iter()
            .all(|event| !matches!(event, ConnEvent::Words { live: false, .. }))
    );
}

#[test]
fn the_text_of_a_state_is_cleaned_before_the_app_sees_it() {
    let dir = tempfile::tempdir().unwrap();
    let state = serde_json::json!({
        "spaces": [{"session_id": "$1", "session_name": "gar\x1b[31mden\x1b[0m"}],
        "agents": [{"pane_id": "%1", "session_id": "$1", "window_id": "@1",
                    "pane_title": "\x1b]0;x\x07 title\x1b]52;c;SGVsbG8=\x1b\\", "kind": "claude\u{202e}"}],
    });
    let script = scripted(
        dir.path(),
        &[hello_line("0.1.0\x1b[2J", 1, ""), state_line(&state)],
        GOES_ON,
    );
    let (tx, mut events) = Events::new();
    let _conn = spawn(
        ctx(&script, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    assert_eq!(events.hello().taw, "0.1.0");
    let state = events.state();
    assert_eq!(state.spaces[0].session_name, "garden");
    assert_eq!(state.agents[0].pane_title, " title");
    assert_eq!(state.agents[0].kind.as_deref(), Some("claude"));
}

#[test]
fn the_text_of_a_toast_and_a_left_is_cleaned_too() {
    let dir = tempfile::tempdir().unwrap();
    let toast = serde_json::json!({
        "type": "toast", "kind": "blocked", "pane": "%1",
        "text": "gar\u{1b}[2Jden needs you\u{202e}", "title": "\u{1b}]0;x\u{7}t", "lease": true,
    })
    .to_string();
    let left = serde_json::json!({
        "type": "left", "holder": "mac\u{1b}[31m-1", "pane": "%1", "go": "pi:%3\u{1b}[0m",
    })
    .to_string();
    let script = scripted(
        dir.path(),
        &[hello_line(VERSION, 1, ""), toast, left],
        GOES_ON,
    );
    let (tx, mut events) = Events::new();
    let _conn = spawn(
        ctx(&script, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    let toast = events.wait("a toast", WAIT, |event| match event {
        ConnEvent::Toast { toast, .. } => Some(toast.clone()),
        _ => None,
    });
    assert_eq!(toast.text, "garden needs you");
    assert_eq!(toast.title, "t");
    assert!(toast.lease);
    let left = events.wait("a left", WAIT, |event| match event {
        ConnEvent::Left { left, .. } => Some(left.clone()),
        _ => None,
    });
    assert_eq!(left.holder, "mac-1");
    assert_eq!(left.go.as_deref(), Some("pi:%3"));
}

#[test]
fn rows_whose_ids_are_not_tmuxs_are_dropped_from_a_state() {
    let dir = tempfile::tempdir().unwrap();
    let state = serde_json::json!({
        "spaces": [{"session_id": "$1"}, {"session_id": "studio:$2"}, {"session_id": "$3;x"}],
        "panes": [{"pane_id": "%1", "session_id": "$1", "window_id": "@1"},
                  {"pane_id": "%2; reboot", "session_id": "$1", "window_id": "@1"}],
        "agents": [{"pane_id": "%1", "session_id": "$1", "window_id": "@1"},
                   {"pane_id": "w1:p3", "session_id": "$1", "window_id": "@1"}],
    });
    let script = scripted(
        dir.path(),
        &[hello_line("0.1.0", 1, ""), state_line(&state)],
        GOES_ON,
    );
    let (tx, mut events) = Events::new();
    let _conn = spawn(
        ctx(&script, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    let state = events.state();
    assert_eq!(
        state
            .spaces
            .iter()
            .map(|s| s.session_id.as_str())
            .collect::<Vec<_>>(),
        ["$1"]
    );
    assert_eq!(state.panes.len(), 1);
    assert_eq!(state.agents.len(), 1);
}

#[test]
fn a_lease_line_reaches_the_far_sides_lease_file_soon_after_the_hello() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut events) = Events::new();
    let _conn = spawn(
        ctx(&stand_in.bin, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    let _ = events.hello();
    let paths = far_paths(&far);
    // The target is within 1 s of the hello. The test allows 3 s so that a loaded
    // gate does not make it flake.
    assert!(
        common::wait_for(
            || files::read_lease(&paths).is_some_and(|lease| lease.holder == "mac-1"),
            Duration::from_secs(3)
        ),
        "no lease on the far side: {:?}",
        files::read_lease(&paths)
    );
    assert!(files::lease_fresh(&paths, files::now_ms()));
}

/// A recorder in place of ssh: says `hello`, records every line it is sent, and goes on (its output stays
/// open, so the connection stays up: one that closed its output would end and be tried again).
fn recorder(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("received.txt");
    std::fs::write(&log, "").unwrap();
    let script = scripted(
        dir,
        &[hello_line("0.1.0", 1, "")],
        &format!("cat <&0 >> '{}' &\nexec sleep 30", log.display()),
    );
    (script, log)
}

fn received(log: &Path) -> Vec<Up> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| wire::parse_up(line).ok())
        .collect()
}

#[test]
fn a_walk_is_said_at_once_and_again_with_each_lease_line() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = recorder(dir.path());
    // The exact order of the log below needs the test thread to read it before the first tick (a lease
    // before the walk) and before the third (a lease after the second walk): 3 s each way, so a thread
    // that stalls for a second under load does not break it. The test waits for one tick, so it takes
    // about 3 s.
    let timing = Timing {
        lease_every: Duration::from_secs(3),
        ..Timing::default()
    };
    let (tx, mut events) = Events::new();
    let conn = spawn(
        ctx(&script, &app_paths(dir.path()), timing),
        0,
        machine("studio"),
        tx,
    );
    assert_eq!(events.words(true), words::connected("0.1.0"));
    // The lease line goes out at the hello.
    assert!(common::wait_for(|| !received(&log).is_empty(), WAIT));
    assert!(matches!(received(&log)[0], Up::Lease(ref lease) if lease.holder == "mac-1"));
    conn.send(Up::Walk(wire::WalkLine {
        you: "studio".to_string(),
        stops: vec!["%7".to_string(), "pi:%3".to_string()],
    }));
    let walks = |log: &Path| {
        received(log)
            .iter()
            .filter(|line| matches!(line, Up::Walk(_)))
            .count()
    };
    assert!(common::wait_for(|| walks(&log) >= 1, WAIT));
    // The next lease tick says it again, with the walk after it: two walks in all.
    assert!(common::wait_for(|| walks(&log) >= 2, WAIT));
    let lines = received(&log);
    assert_eq!(walks(&log), 2, "{lines:?}");
    let kinds: Vec<&str> = lines
        .iter()
        .map(|line| match line {
            Up::Lease(_) => "lease",
            Up::Walk(_) => "walk",
        })
        .collect();
    // The first walk goes out at once, right after the first lease and well before the tick; the tick
    // then says a lease and the walk again. Nothing else, and in this order.
    assert_eq!(kinds, ["lease", "walk", "lease", "walk"], "{lines:?}");
    let Up::Walk(last) = lines.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(last.stops, ["%7", "pi:%3"]);
}

/// The keeper app and the one connection to studio, ready: waits until the machine is live.
fn poll_until(machines: &mut Machines, what: &str, mut done: impl FnMut(&Machines) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        machines.poll(Instant::now());
        if done(machines) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: {:#?}", machines.views());
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_keeper_writes_the_machines_file_live_with_the_state_and_its_words() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    let dir = tempfile::tempdir().unwrap();
    let paths = app_paths(dir.path());
    let mut machines = Machines::start(
        ctx(&stand_in.bin, &paths, Timing::default()),
        vec![machine("studio")],
    );
    poll_until(&mut machines, "the machine never had a state", |m| {
        m.views()[0].state.is_some()
    });
    let view = machines.views().remove(0);
    assert!(view.live);
    assert_eq!(view.name, "studio");
    assert_eq!(view.words, words::connected(VERSION));
    assert_eq!(view.state.unwrap().spaces[0].session_name, "garden");
    // The file was written as it changed.
    poll_until(&mut machines, "no file", |_| {
        files::read(&paths, "studio").is_some_and(|file| file.state.is_some())
    });
    let file = files::read(&paths, "studio").unwrap();
    assert!(file.live);
    assert_eq!(file.name, "studio");
    assert_eq!(file.words, words::connected(VERSION));
    assert_eq!(file.taw_version.as_deref(), Some(VERSION));
    assert_eq!(file.protocol, Some(1));
    assert!(file.last_seen.is_some() && file.received_at.is_some());
    assert!(files::is_live(
        &file,
        files::keeper_alive(&paths),
        files::now_ms()
    ));
    assert_eq!(file.state.unwrap().spaces[0].session_name, "garden");
}

#[test]
fn the_keeper_writes_a_connected_machines_file_again_with_each_beat() {
    let dir = tempfile::tempdir().unwrap();
    let paths = app_paths(dir.path());
    // A feed that says its hello and one state, and then only pings: no state changes after the first.
    let script = scripted(
        dir.path(),
        &[
            hello_line("0.1.0", 1, ""),
            state_line(
                &serde_json::json!({"spaces": [{"session_id": "$1", "session_name": "one"}]}),
            ),
        ],
        "while :; do echo '{\"type\":\"ping\"}'; sleep 0.2; done",
    );
    let timing = Timing {
        lease_every: Duration::from_millis(400),
        ..Timing::default()
    };
    let mut machines = Machines::start(ctx(&script, &paths, timing), vec![machine("studio")]);
    poll_until(&mut machines, "no file", |_| {
        files::read(&paths, "studio").is_some_and(|file| file.live && file.state.is_some())
    });
    let first = files::read(&paths, "studio").unwrap();
    let (received_at, seen) = (first.received_at.unwrap(), first.last_seen.unwrap());
    // A beat with no new state: the file is written again with `last_seen` moved on (the pings) and
    // `received_at` where it was. A rewrite for another reason (a state) would have moved `received_at`.
    poll_until(&mut machines, "the file was not written again", |_| {
        files::read(&paths, "studio").is_some_and(|file| {
            file.last_seen.is_some_and(|at| at > seen) && file.received_at == Some(received_at)
        })
    });
}

/// The keeper takes this machine's lease at its start and keeps it with each offer; while it runs, another
/// holder's offer is refused. When it ends (dropped, as on the app's clean end) it gives the lease up, so the
/// next app takes it at once instead of waiting out the 15 s.
#[test]
fn the_keeper_offers_this_machines_lease_and_gives_it_up_when_it_ends() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    let dir = tempfile::tempdir().unwrap();
    let paths = app_paths(dir.path());
    let mut machines = Machines::start(
        ctx(&stand_in.bin, &paths, Timing::default()),
        vec![machine("studio")],
    );
    assert_eq!(files::read_lease(&paths).unwrap().holder, "mac-1");
    assert!(machines.offer_lease());
    assert!(
        !files::offer_lease(&paths, "mac-2", files::now_ms()),
        "another holder took the running keeper's lease"
    );
    assert_eq!(files::read_lease(&paths).unwrap().holder, "mac-1");
    drop(machines);
    assert!(
        files::read_lease(&paths).is_none(),
        "the ended keeper's lease was not given up"
    );
    assert!(files::offer_lease(&paths, "mac-2", files::now_ms()));
    assert_eq!(files::read_lease(&paths).unwrap().holder, "mac-2");
}

#[test]
fn a_machine_that_is_this_machines_server_is_listed_with_no_state_and_says_so() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    let dir = tempfile::tempdir().unwrap();
    let paths = app_paths(dir.path());
    let mut context = ctx(&stand_in.bin, &paths, Timing::default());
    context.local_server = Some(server_word(&Tmux::with_socket_name(far.server.socket())));
    let mut machines = Machines::start(context, vec![machine("studio")]);
    poll_until(&mut machines, "never said it is this server", |m| {
        m.views()[0].words == words::this_server("studio")
    });
    let view = machines.views().remove(0);
    assert!(view.this_server && !view.live && view.state.is_none());
    poll_until(&mut machines, "no file", |_| {
        files::read(&paths, "studio").is_some()
    });
    let file = files::read(&paths, "studio").unwrap();
    assert!(file.this_server && !file.live && file.state.is_none());
}

#[test]
fn toasts_and_lefts_the_far_side_announces_reach_the_app_once() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    let dir = tempfile::tempdir().unwrap();
    let paths = app_paths(dir.path());
    let mut machines = Machines::start(
        ctx(&stand_in.bin, &paths, Timing::default()),
        vec![machine("studio")],
    );
    poll_until(&mut machines, "no state", |m| m.views()[0].state.is_some());
    // The feed tails the far side's events from where its file ends now.
    files::append_event(
        &far_paths(&far),
        &Down::Toast(Toast {
            kind: "needs_input".to_string(),
            pane: "%1".to_string(),
            text: "garden needs you".to_string(),
            ..Toast::default()
        }),
    );
    files::append_event(
        &far_paths(&far),
        &Down::Left(Left {
            holder: "mac-1".to_string(),
            pane: "%1".to_string(),
            go: None,
        }),
    );
    let mut toasts = Vec::new();
    let mut lefts = Vec::new();
    let deadline = Instant::now() + WAIT;
    while toasts.is_empty() || lefts.is_empty() {
        machines.poll(Instant::now());
        toasts.extend(machines.toasts());
        lefts.extend(machines.lefts());
        assert!(
            Instant::now() < deadline,
            "toasts {toasts:?} lefts {lefts:?} after the announcements"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(toasts.len(), 1, "{toasts:?}");
    assert_eq!(toasts[0].0, 0, "the machine's index");
    assert_eq!(toasts[0].1.text, "garden needs you");
    assert_eq!(lefts.len(), 1, "{lefts:?}");
    assert_eq!(lefts[0].1.holder, "mac-1");
    // Taken once: a later poll has nothing more.
    machines.poll(Instant::now());
    assert!(machines.toasts().is_empty() && machines.lefts().is_empty());
}

#[test]
fn the_pane_a_toast_or_a_left_names_is_a_pane_id_or_empty_when_the_app_reads_it() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    let dir = tempfile::tempdir().unwrap();
    let paths = app_paths(dir.path());
    let mut machines = Machines::start(
        ctx(&stand_in.bin, &paths, Timing::default()),
        vec![machine("studio")],
    );
    poll_until(&mut machines, "no state", |m| m.views()[0].state.is_some());
    // A window id where a pane id goes, and then a pane id, for a toast and for a left.
    for pane in ["@9", "%4"] {
        files::append_event(
            &far_paths(&far),
            &Down::Toast(Toast {
                kind: "needs_input".to_string(),
                pane: pane.to_string(),
                text: format!("garden needs you {pane}"),
                ..Toast::default()
            }),
        );
        files::append_event(
            &far_paths(&far),
            &Down::Left(Left {
                holder: format!("mac-{pane}"),
                pane: pane.to_string(),
                go: None,
            }),
        );
    }
    let mut toasts = Vec::new();
    let mut lefts = Vec::new();
    let deadline = Instant::now() + WAIT;
    while toasts.len() < 2 || lefts.len() < 2 {
        machines.poll(Instant::now());
        toasts.extend(machines.toasts());
        lefts.extend(machines.lefts());
        assert!(
            Instant::now() < deadline,
            "toasts {toasts:?} lefts {lefts:?} after the announcements"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let toasts: Vec<(&str, &str)> = toasts
        .iter()
        .map(|(_, toast)| (toast.pane.as_str(), toast.text.as_str()))
        .collect();
    assert_eq!(
        toasts,
        [("", "garden needs you @9"), ("%4", "garden needs you %4")]
    );
    let lefts: Vec<(&str, &str)> = lefts
        .iter()
        .map(|(_, left)| (left.pane.as_str(), left.holder.as_str()))
        .collect();
    assert_eq!(lefts, [("", "mac-@9"), ("%4", "mac-%4")]);
}

#[test]
fn dropping_the_app_ends_every_thread_and_the_ssh_and_what_it_started() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    stand_in.fail_as("studio", Failure::HangWithProxy);
    let dir = tempfile::tempdir().unwrap();
    // The quiet time is not under test, so it is the default 15 s: a loaded gate cannot make the quiet
    // kill try again before the drop.
    let machines = Machines::start(
        ctx(&stand_in.bin, &app_paths(dir.path()), Timing::default()),
        vec![machine("studio")],
    );
    // Positive: the ssh and its helper are up.
    assert!(common::wait_for(
        || stand_in.pids().len() == 1 && stand_in.helper_pids().len() == 1,
        WAIT
    ));
    let (ssh, helper) = (stand_in.pids()[0], stand_in.helper_pids()[0]);
    let dropping = Instant::now();
    drop(machines);
    assert!(
        dropping.elapsed() < Duration::from_secs(4),
        "dropping took {:?}",
        dropping.elapsed()
    );
    for pid in [ssh, helper] {
        assert!(
            common::wait_for(|| !alive(pid), Duration::from_secs(2)),
            "process {pid} is still there after the drop"
        );
    }
    // No second try was made on the way out.
    assert_eq!(stand_in.calls().len(), 1);
}

#[test]
fn a_connection_that_is_stopped_ends_its_ssh_by_its_pid() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut events) = Events::new();
    let conn: ConnHandle = spawn(
        ctx(&stand_in.bin, &app_paths(dir.path()), Timing::default()),
        0,
        machine("studio"),
        tx,
    );
    let _ = events.state();
    let ssh = stand_in.pids()[0];
    conn.stop();
    assert!(
        common::wait_for(|| !alive(ssh), Duration::from_secs(2)),
        "ssh {ssh} is still there"
    );
}

#[test]
fn a_follower_reads_what_the_keeper_wrote_and_takes_over_when_the_keeper_ends() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    let dir = tempfile::tempdir().unwrap();
    let paths = app_paths(dir.path());
    let context = ctx(&stand_in.bin, &paths, Timing::default());
    let mut keeper = Machines::start(context.clone(), vec![machine("studio")]);
    poll_until(&mut keeper, "the keeper never connected", |m| {
        m.views()[0].state.is_some()
    });
    poll_until(&mut keeper, "no file", |_| {
        files::read(&paths, "studio").is_some_and(|file| file.state.is_some())
    });
    let mut follower = Machines::follow(context, vec![machine("studio")]);
    poll_until(&mut follower, "the follower never read the file", |m| {
        m.views()[0].live && m.views()[0].state.is_some()
    });
    let view = follower.views().remove(0);
    assert_eq!(view.words, words::connected(VERSION));
    assert_eq!(view.state.unwrap().spaces[0].session_name, "garden");
    // The follower holds no connection: the one ssh is the keeper's. Its first try at the lock comes a
    // second after it started, so it is polled well past that (and stays live: the positive that the
    // keeper is still keeping) before the calls are counted.
    let until = Instant::now() + Duration::from_millis(1600);
    while Instant::now() < until {
        follower.poll(Instant::now());
        assert!(
            follower.views()[0].live,
            "the follower stopped seeing the keeper: {:#?}",
            follower.views()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(stand_in.calls().len(), 1, "{:?}", stand_in.calls());
    drop(keeper);
    // The keeper has gone: within a few seconds the follower takes the lock and connects by itself.
    poll_until(&mut follower, "the follower never took over", |_| {
        stand_in.calls().len() >= 2
    });
    assert!(files::keeper_alive(&paths));
    poll_until(&mut follower, "no state after taking over", |m| {
        m.views()[0].live && m.views()[0].state.is_some()
    });
}

#[test]
fn with_no_machines_nothing_is_started_written_or_locked() {
    let dir = tempfile::tempdir().unwrap();
    let paths = app_paths(dir.path());
    let stand_in = taw_standin_for_nothing();
    let mut machines = Machines::start(ctx(&stand_in, &paths, Timing::default()), Vec::new());
    assert!(!machines.poll(Instant::now()));
    assert!(machines.views().is_empty());
    assert!(machines.toasts().is_empty() && machines.lefts().is_empty());
    machines.wake();
    machines.send_walk(0, "studio", &[]);
    assert!(!machines.offer_lease());
    assert!(
        !paths.state_dir.exists(),
        "something was made under the state folder"
    );
    assert!(!files::keeper_alive(&paths));
    drop(machines);
    // No lock is held after the drop.
    assert!(files::take_keeper(&paths).is_some());
    // The same for an app that follows.
    let dir = tempfile::tempdir().unwrap();
    let paths = app_paths(dir.path());
    let mut follower = Machines::follow(ctx(&stand_in, &paths, Timing::default()), Vec::new());
    assert!(!follower.poll(Instant::now()));
    assert!(!paths.state_dir.exists());
}

/// A path that is no ssh at all: a run of it would fail loudly.
fn taw_standin_for_nothing() -> PathBuf {
    PathBuf::from("/nonexistent/ssh-that-must-not-run")
}

/// What a second process does in [`two_processes_cannot_both_keep_the_connections`]: takes the keeper
/// lock of the state folder it is given, says so, and holds it until its standard input ends.
#[test]
fn keeper_holder_process() {
    let Some(state) = std::env::var_os("TAW_TEST_KEEPER_HOLDER") else {
        return;
    };
    let paths = Paths::rooted_at(
        PathBuf::from(&state).join("config"),
        PathBuf::from(&state).join("state"),
    );
    let keeper = files::take_keeper(&paths);
    println!("{}", if keeper.is_some() { "held" } else { "refused" });
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    drop(keeper);
}

#[test]
fn two_processes_cannot_both_keep_the_connections() {
    let dir = tempfile::tempdir().unwrap();
    let paths = app_paths(dir.path());
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "keeper_holder_process",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("TAW_TEST_KEEPER_HOLDER", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, held) = channel();
    std::thread::spawn(move || {
        // Reads to the end: a closed pipe would fail the other process's own writes.
        let mut said = false;
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            // The test harness writes "test <name> ... " before what the test prints.
            for word in ["held", "refused"] {
                if !said && line.ends_with(word) {
                    said = true;
                    let _ = tx.send(word.to_string());
                }
            }
        }
    });
    assert_eq!(
        held.recv_timeout(WAIT)
            .expect("the other process said nothing"),
        "held"
    );
    // The second process to ask gets nothing, and sees the first one alive.
    assert!(files::take_keeper(&paths).is_none());
    assert!(files::keeper_alive(&paths));
    // The first ends (its input closes), and the lock is free for the second.
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    assert!(!files::keeper_alive(&paths));
    assert!(files::take_keeper(&paths).is_some());
}

/// A file an earlier keeper left: connected, heard from a moment ago, with words that say so.
fn write_stale_live_file(paths: &Paths, name: &str) {
    files::write_machine_file(
        paths,
        &files::MachineFile {
            name: name.to_string(),
            live: true,
            last_seen: Some(files::now_ms()),
            words: words::connected(VERSION),
            taw_version: Some(VERSION.to_string()),
            protocol: Some(1),
            ..files::MachineFile::default()
        },
    );
}

/// Polls until the file of `name` says it is not live: the new keeper has no connection yet, and the
/// file must not go on saying there is one. Well within the quiet time of a feed that says nothing (15
/// s), so that the drop of that feed cannot be what rewrote it.
fn poll_until_file_not_live(machines: &mut Machines, paths: &Paths, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        machines.poll(Instant::now());
        if files::read(paths, name).is_some_and(|file| !file.live) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the file still says live: {:?}; views {:#?}",
            files::read(paths, name),
            machines.views()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_new_keeper_rewrites_the_files_it_inherits_as_not_live_until_a_hello() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    stand_in.fail_as("studio", Failure::Hang);
    let dir = tempfile::tempdir().unwrap();
    let paths = app_paths(dir.path());
    write_stale_live_file(&paths, "studio");
    let mut machines = Machines::start(
        ctx(&stand_in.bin, &paths, Timing::default()),
        vec![machine("studio")],
    );
    poll_until_file_not_live(&mut machines, &paths, "studio");
    let file = files::read(&paths, "studio").unwrap();
    assert!(file.words.starts_with("connecting"), "{}", file.words);
    // What it last said is kept.
    assert!(file.last_seen.is_some());
    assert!(!machines.views()[0].live);
}

#[test]
fn a_follower_that_takes_over_rewrites_the_files_as_not_live_until_a_hello() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    stand_in.fail_as("studio", Failure::Hang);
    let dir = tempfile::tempdir().unwrap();
    let paths = app_paths(dir.path());
    // Another app keeps the connections for now, and has left a file that says live.
    let held = files::take_keeper(&paths).unwrap();
    write_stale_live_file(&paths, "studio");
    let mut follower = Machines::follow(
        ctx(&stand_in.bin, &paths, Timing::default()),
        vec![machine("studio")],
    );
    // Positive: it follows, and sees the file as live while the other keeper lives.
    poll_until(&mut follower, "the follower never read the file", |m| {
        m.views()[0].live
    });
    assert!(files::read(&paths, "studio").unwrap().live);
    drop(held);
    poll_until_file_not_live(&mut follower, &paths, "studio");
    assert!(files::keeper_alive(&paths), "it took the lock");
}

fn walk_in(log: &Path) -> Option<wire::WalkLine> {
    received(log).into_iter().find_map(|line| match line {
        Up::Walk(walk) => Some(walk),
        Up::Lease(_) => None,
    })
}

#[test]
fn the_walk_a_keeper_sends_through_machines_reaches_the_far_side() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = recorder(dir.path());
    let paths = app_paths(dir.path());
    let mut machines = Machines::start(
        ctx(&script, &paths, Timing::default()),
        vec![machine("studio")],
    );
    machines.send_walk(0, "studio", &["%7".to_string(), "pi:%3".to_string()]);
    assert!(
        common::wait_for(|| walk_in(&log).is_some(), WAIT),
        "no walk reached the far side: {:?}",
        received(&log)
    );
    let walk = walk_in(&log).unwrap();
    assert_eq!(walk.you, "studio");
    assert_eq!(walk.stops, ["%7", "pi:%3"]);
}

#[test]
fn the_walk_a_follower_was_given_is_said_to_the_far_side_once_it_takes_over() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = recorder(dir.path());
    let paths = app_paths(dir.path());
    let held = files::take_keeper(&paths).unwrap();
    let mut follower = Machines::follow(
        ctx(&script, &paths, Timing::default()),
        vec![machine("studio")],
    );
    follower.send_walk(0, "studio", &["%7".to_string()]);
    // A follower has no connection: nothing has been said to anyone.
    follower.poll(Instant::now());
    assert!(received(&log).is_empty());
    drop(held);
    // It takes the lock within a few seconds, connects, and the walk arrives with no further call.
    let deadline = Instant::now() + WAIT;
    while walk_in(&log).is_none() {
        follower.poll(Instant::now());
        assert!(
            Instant::now() < deadline,
            "no walk after the takeover: {:?}",
            received(&log)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(walk_in(&log).unwrap().stops, ["%7"]);
    assert!(files::keeper_alive(&paths));
}

#[test]
fn a_keeper_does_not_hold_this_machines_lease_when_another_app_holds_a_fresh_one() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    stand_in.fail_as("studio", Failure::Hang);
    let dir = tempfile::tempdir().unwrap();
    let paths = app_paths(dir.path());
    let now = files::now_ms();
    assert!(files::offer_lease(&paths, "mac-2", now));
    let before = files::read_lease(&paths).unwrap();
    assert_eq!(before.holder, "mac-2");
    let mut machines = Machines::start(
        ctx(&stand_in.bin, &paths, Timing::default()),
        vec![machine("studio")],
    );
    // It is the keeper (positive), and it does not hold the lease.
    assert!(files::keeper_alive(&paths));
    assert!(!machines.offer_lease());
    // The other app's lease is left as it was.
    assert_eq!(files::read_lease(&paths).unwrap(), before);
}

#[test]
fn an_app_that_follows_never_offers_the_lease_and_holds_none_it_does_not_have() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    stand_in.fail_as("studio", Failure::Hang);
    let dir = tempfile::tempdir().unwrap();
    let paths = app_paths(dir.path());
    let held = files::take_keeper(&paths).unwrap();
    // A follower of another holder's name, and no lease at all: it does not make one.
    let mut context = ctx(&stand_in.bin, &paths, Timing::default());
    context.holder = "mac-2".to_string();
    let mut follower = Machines::follow(context, vec![machine("studio")]);
    assert!(files::keeper_alive(&paths));
    assert!(!follower.offer_lease());
    assert!(
        files::read_lease(&paths).is_none(),
        "a follower wrote a lease"
    );
    // Another holder's fresh lease is not this follower's.
    assert!(files::offer_lease(&paths, "mac-1", files::now_ms()));
    assert!(!follower.offer_lease());
    assert_eq!(files::read_lease(&paths).unwrap().holder, "mac-1");
    drop(held);
}

#[test]
fn the_keeper_refreshes_the_lease_at_each_beat() {
    let far = FarSide::new("studio");
    let stand_in = StandIn::new(&[&far]);
    stand_in.fail_as("studio", Failure::Hang);
    let dir = tempfile::tempdir().unwrap();
    let paths = app_paths(dir.path());
    let timing = Timing {
        lease_every: Duration::from_millis(400),
        ..Timing::default()
    };
    let mut machines = Machines::start(ctx(&stand_in.bin, &paths, timing), vec![machine("studio")]);
    let first = files::read_lease(&paths).unwrap().refreshed_at;
    poll_until(&mut machines, "the lease was not refreshed", |_| {
        files::read_lease(&paths).is_some_and(|lease| lease.refreshed_at > first)
    });
    assert_eq!(files::read_lease(&paths).unwrap().holder, "mac-1");
}

/// How many lines of `log` have `needle` in them: a count that does not parse every line, for a log of
/// hundreds of thousands.
fn lines_with(log: &Path, needle: &str) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(needle))
        .count()
}

#[test]
fn the_lease_goes_out_on_time_while_the_input_has_a_long_queue() {
    // The app has said 2 000 walks of 400 stops each at once: each costs the connection more to write
    // than it cost the app to say, so the input stays full for as long as it takes to write them all (many
    // times the 10 ms between two lease lines). A lease line that waited for the input to be empty would
    // come once, at the end; one that is said after every item comes at each tick of the burst.
    const WALKS: usize = 2_000;
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = recorder(dir.path());
    let timing = Timing {
        lease_every: Duration::from_millis(10),
        ..Timing::default()
    };
    let (tx, mut events) = Events::new();
    let conn = spawn(
        ctx(&script, &app_paths(dir.path()), timing),
        0,
        machine("studio"),
        tx,
    );
    assert_eq!(events.words(true), words::connected("0.1.0"));
    // Positive: the connection is up and says its leases, before the burst.
    assert!(common::wait_for(
        || lines_with(&log, "\"lease\"") >= 2,
        WAIT
    ));
    let stops: Vec<String> = (0..400).map(|n| format!("pi:%{n}")).collect();
    // Each walk has its own name, so the log says where in the burst a lease line was said. They are all
    // made before the first is sent, so that the loop below only hands them over: if the test thread
    // stalls early (a loaded machine), the queue is still long when it goes on, and a lease line said
    // only when the input is empty cannot fall between the first walk and the last.
    let walks: Vec<wire::WalkLine> = (0..WALKS)
        .map(|i| wire::WalkLine {
            you: format!("burst-{i}"),
            stops: stops.clone(),
        })
        .collect();
    for walk in walks {
        conn.send(Up::Walk(walk));
    }
    // Positive half: the last walk of the burst is in the log.
    let last = format!("\"burst-{}\"", WALKS - 1);
    let first = "\"burst-0\"";
    let deadline = Instant::now() + Duration::from_secs(20);
    let during = loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let mut from_first = false;
        let mut leases = 0;
        let mut done = false;
        for line in text.lines() {
            if line.contains(&last) {
                done = true;
                break;
            }
            if line.contains(first) {
                from_first = true;
            } else if from_first && line.contains("\"lease\"") {
                leases += 1;
            }
        }
        if done {
            break leases;
        }
        assert!(
            Instant::now() < deadline,
            "the last walk of the burst was not written in 20 s ({leases} lease lines so far between the first and the end of the log)"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    // A lease line said only on a timeout of the input would come after the last walk of the burst, so it
    // never lies between the first walk and the last one, however slow the machine is.
    assert!(
        during >= 1,
        "{during} lease lines between the walk burst-0 and the walk burst-{}",
        WALKS - 1
    );
}
