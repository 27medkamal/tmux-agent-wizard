//! `taw machine feed`: the lines a machine sends the app, read off its standard output, and what it does
//! with the lines the app sends back. No ssh runs here: the feed runs on a far side's own environment
//! (`FarSide`, a second private tmux server with folders of its own), with its streams piped to the test.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use common::machine::FarSide;
use common::wait_for;
use serde_json::Value;
use taw::machine::files;
use taw::machine::wire::{Down, Hello, Left, State, Toast, parse_down};
use taw::paths::Paths;
use taw::store::{self, AgentState};
use taw::tmux::Tmux;

/// A feed that is running, with its lines arriving on a channel.
struct Feed {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Feed {
    fn start(far: &FarSide, args: &[&str]) -> Feed {
        let mut command = far.server.taw_command(&["machine", "feed"]);
        command.args(args);
        Feed::spawn(command)
    }

    fn spawn(mut command: std::process::Command) -> Feed {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        Feed {
            child,
            stdin,
            lines,
        }
    }

    /// The next line within `timeout`, parsed.
    fn next(&self, timeout: Duration) -> Option<Down> {
        match self.lines.recv_timeout(timeout) {
            Ok(line) => Some(parse_down(&line).unwrap_or_else(|e| panic!("{e:?}: {line}"))),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
        }
    }

    /// The next line that is a `State`, within `timeout`, skipping the others.
    fn next_state(&self, timeout: Duration) -> Option<State> {
        self.next_where(timeout, |line| match line {
            Down::State(state) => Some(state),
            _ => None,
        })
    }

    fn next_where<T>(
        &self,
        timeout: Duration,
        mut pick: impl FnMut(Down) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.next(left)?;
            if let Some(found) = pick(line) {
                return Some(found);
            }
        }
    }

    /// Reads until `quiet` passes with no line, and says how many lines it read. A feed that never goes
    /// quiet (a line every tick) makes this panic after 10 s with what it saw, so that the test fails
    /// instead of hanging the suite.
    fn settle(&self, quiet: Duration) -> usize {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen: Vec<String> = Vec::new();
        while let Ok(line) = self.lines.recv_timeout(quiet) {
            seen.push(line);
            assert!(
                Instant::now() < deadline,
                "the feed never went quiet: {} lines in 10 s, the last: {}",
                seen.len(),
                seen.last()
                    .map(|line| line.chars().take(120).collect::<String>())
                    .unwrap_or_default()
            );
        }
        seen.len()
    }

    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// The exit status once it has ended within `timeout`.
    fn exit_within(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        // Its own pid, started by this test.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const WAIT: Duration = Duration::from_secs(10);

fn far_paths(far: &FarSide) -> Paths {
    Paths::rooted_at(far.config_dir.clone(), far.state_dir.clone())
}

/// The far server as a tmux client, for the writes the watcher would make.
fn far_tmux(far: &FarSide) -> Tmux {
    Tmux::with_socket_name(far.server.socket())
}

/// The first pane of the far side, as tmux says it.
fn first_pane(far: &FarSide) -> String {
    far.server
        .tmux_ok(&["list-panes", "-a", "-F", "#{pane_id}"])
        .lines()
        .next()
        .unwrap()
        .to_string()
}

/// Marks the pane as an agent's, the way `tests/tmux_tui_data.rs` does.
fn mark_agent(far: &FarSide, pane: &str) {
    store::write_state_with_kind(
        &far_tmux(far),
        pane,
        Some("claude"),
        AgentState::Working,
        None,
        "manifest",
        100,
    )
    .unwrap();
}

fn hello_of(feed: &Feed) -> Hello {
    match feed.next(WAIT) {
        Some(Down::Hello(hello)) => hello,
        other => panic!("the first line is not a hello: {other:?}"),
    }
}

fn host_without_domain() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is writable for its whole length, which gethostname fills at most.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } == 0;
    assert!(ok);
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).into_owned();
    name.split('.').next().unwrap().to_string()
}

#[test]
fn the_first_line_is_a_hello_with_the_version_the_host_and_the_far_servers_pid_and_start() {
    let far = FarSide::new("studio");
    let feed = Feed::start(&far, &["--protocol", "1"]);
    let hello = hello_of(&feed);
    assert_eq!(hello.taw, env!("CARGO_PKG_VERSION"));
    assert_eq!(hello.protocol, 1);
    assert_eq!(hello.host, host_without_domain());
    let server = far
        .server
        .tmux_ok(&["display-message", "-p", "#{pid} #{start_time}"]);
    assert_eq!(hello.server, server);
}

#[test]
fn the_state_that_follows_has_the_far_sessions_and_a_pane_marked_as_an_agent() {
    let far = FarSide::new("studio");
    let pane = first_pane(&far);
    mark_agent(&far, &pane);
    let feed = Feed::start(&far, &["--protocol", "1"]);
    let _ = hello_of(&feed);
    let state = feed.next_state(WAIT).expect("a state");
    assert_eq!(state.spaces.len(), 1);
    assert_eq!(state.spaces[0].session_name, "garden");
    assert!(state.spaces[0].session_id.starts_with('$'));
    assert_eq!(state.panes.len(), 1);
    assert_eq!(state.panes[0].pane_id, pane);
    assert_eq!(state.agents.len(), 1);
    assert_eq!(state.agents[0].pane_id, pane);
    assert_eq!(state.agents[0].kind.as_deref(), Some("claude"));
    assert_eq!(state.agents[0].state.as_deref(), Some("working"));
    assert_eq!(state.spaces[0].state.as_deref(), Some("working"));
}

/// A record of `worktrees.json` the way `taw workspace new` writes one.
fn record_json(id: &str, name: &str, path: &std::path::Path, repo_root: &str) -> Value {
    serde_json::json!({
        "workspace_id": id,
        "session_name": name,
        "path": path,
        "branch": name,
        "base": null,
        "repo_root": repo_root,
        "source_workspace_id": "w0",
        "created_unix_ms": 1,
    })
}

#[test]
fn a_worktree_a_session_runs_is_sent_as_open_and_a_closed_one_as_closed() {
    let far = FarSide::new("studio");
    let root = far
        .server
        .tmux_ok(&["display-message", "-p", "-t", "garden", "#{session_path}"])
        .trim()
        .to_string();
    let open = far.home.join("garden--open");
    let closed = far.home.join("garden--closed");
    std::fs::create_dir_all(&open).unwrap();
    std::fs::create_dir_all(&closed).unwrap();
    far.server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "garden--open",
        "-c",
        open.to_str().unwrap(),
    ]);
    std::fs::create_dir_all(&far.state_dir).unwrap();
    let records = serde_json::json!({ "worktrees": [
        record_json("w5", "garden--open", &open, &root),
        record_json("w6", "garden--closed", &closed, &root),
    ]});
    std::fs::write(far.state_dir.join("worktrees.json"), records.to_string()).unwrap();
    let feed = Feed::start(&far, &["--protocol", "1"]);
    let _ = hello_of(&feed);
    let state = feed.next_state(WAIT).expect("a state");
    let names = |rows: &[taw::machine::wire::WireWorktree]| {
        rows.iter()
            .map(|r| r.session_name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&state.worktrees), vec!["garden--closed"]);
    assert_eq!(names(&state.open_worktrees), vec!["garden--open"]);
}

#[test]
fn a_window_rename_gives_a_new_state_within_a_second_and_a_half() {
    let far = FarSide::new("studio");
    let feed = Feed::start(&far, &["--protocol", "1"]);
    let _ = hello_of(&feed);
    let first = feed.next_state(WAIT).expect("a first state");
    assert_ne!(first.panes[0].window_name, "renamed");
    feed.settle(Duration::from_millis(1200));
    far.server
        .tmux_ok(&["rename-window", "-t", "=garden:0", "renamed"]);
    // A state read just before the rename can still come first, and it is no news: the one that names the new
    // name is, and it is due within the time the test is named for.
    let mut before = Vec::new();
    let state = feed
        .next_where(Duration::from_millis(1500), |line| match line {
            Down::State(state) => {
                let name = state.panes.first().map(|pane| pane.window_name.clone());
                if name.as_deref() == Some("renamed") {
                    Some(state)
                } else {
                    before.push(name);
                    None
                }
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!("no state with the new name in 1.5 s of the rename; the first window of those that came: {before:?}")
        });
    assert_eq!(state.panes[0].window_name, "renamed");
}

#[test]
fn a_spinner_glyph_in_a_title_is_not_a_change_but_a_real_change_still_is() {
    let far = FarSide::new("studio");
    let pane = first_pane(&far);
    mark_agent(&far, &pane);
    far.server
        .tmux_ok(&["select-pane", "-t", &pane, "-T", "\u{280b} thinking"]);
    let feed = Feed::start(&far, &["--protocol", "1"]);
    let _ = hello_of(&feed);
    // Positive wait: the state with the first title is out.
    let first = feed.next_state(WAIT).expect("a first state");
    assert_eq!(first.agents[0].pane_title, "\u{280b} thinking");
    feed.settle(Duration::from_millis(1200));

    far.server
        .tmux_ok(&["select-pane", "-t", &pane, "-T", "\u{2819} thinking"]);
    let none = feed.next_state(Duration::from_millis(1800));
    assert!(
        none.is_none(),
        "a glyph that spins was sent as a change: {none:?}"
    );

    // The feed is alive and does send a real change right after.
    far.server
        .tmux_ok(&["select-pane", "-t", &pane, "-T", "\u{2819} writing"]);
    let state = feed
        .next_state(Duration::from_millis(1500))
        .expect("a real change");
    assert_eq!(state.agents[0].pane_title, "\u{2819} writing");
}

#[test]
fn a_ping_comes_after_five_quiet_seconds() {
    let far = FarSide::new("studio");
    let feed = Feed::start(&far, &["--protocol", "1"]);
    let _ = hello_of(&feed);
    let _ = feed.next_state(WAIT).expect("a state");
    // Whatever else settles first (git's answer, a late state) is read before the quiet is timed.
    feed.settle(Duration::from_millis(1200));
    let quiet_since = Instant::now() - Duration::from_millis(1200);
    let seen = feed.next_where(Duration::from_secs(7), |line| {
        (line == Down::Ping).then_some(())
    });
    assert!(seen.is_some(), "no ping within 7 s of quiet");
    let waited = quiet_since.elapsed();
    assert!(
        waited >= Duration::from_millis(3500),
        "a ping after only {waited:?}"
    );
}

fn lease_holder(far: &FarSide) -> Option<String> {
    files::read_lease(&far_paths(far)).map(|lease| lease.holder)
}

/// The lease's safety rule: a second app's feed does not take a fresh lease from a holder whose feed is still
/// there. Each app speaks through a feed of its own, and the first holder's feed stays alive while the second
/// offers. Once the first holder's app ends (its feed's input closes), its lease is given up, and the second
/// holder's next lease line takes it at once, without waiting out the 15 s.
#[test]
fn a_lease_line_writes_the_lease_and_a_second_holder_does_not_replace_a_fresh_one() {
    let far = FarSide::new("studio");
    let mut first = Feed::start(&far, &["--protocol", "1"]);
    let _ = hello_of(&first);
    first.send(r#"{"type":"lease","holder":"laptop-1"}"#);
    assert!(
        wait_for(|| lease_holder(&far).as_deref() == Some("laptop-1"), WAIT),
        "the lease file was not written"
    );
    let mut second = Feed::start(&far, &["--protocol", "1"]);
    let _ = hello_of(&second);
    // Lines are read in order: the walk is written after the second holder's offer was looked at.
    second.send(r#"{"type":"lease","holder":"desktop-2"}"#);
    second.send(r#"{"type":"walk","you":"studio","stops":["%1"]}"#);
    assert!(wait_for(
        || files::read_walk(&far_paths(&far), "desktop-2").is_some(),
        WAIT
    ));
    assert_eq!(
        lease_holder(&far).as_deref(),
        Some("laptop-1"),
        "a second holder replaced a fresh one"
    );

    // The first holder's app ends: its feed gives the lease up as it goes.
    first.close_stdin();
    assert!(
        first.exit_within(WAIT).is_some(),
        "the first feed did not end"
    );
    assert!(
        wait_for(|| lease_holder(&far).is_none(), WAIT),
        "the ended holder's lease was not given up: {:?}",
        lease_holder(&far)
    );
    second.send(r#"{"type":"lease","holder":"desktop-2"}"#);
    assert!(
        wait_for(|| lease_holder(&far).as_deref() == Some("desktop-2"), WAIT),
        "the second holder did not take the given up lease: {:?}",
        lease_holder(&far)
    );
}

#[test]
fn a_walk_line_writes_the_walk_file() {
    let far = FarSide::new("studio");
    let mut feed = Feed::start(&far, &["--protocol", "1"]);
    let _ = hello_of(&feed);
    feed.send(r#"{"type":"lease","holder":"laptop-1"}"#);
    feed.send(r#"{"type":"walk","you":"studio","stops":["%7","studio:%16","pi:%3"]}"#);
    assert!(wait_for(
        || files::read_walk(&far_paths(&far), "laptop-1").is_some(),
        WAIT
    ));
    let walk = files::read_walk(&far_paths(&far), "laptop-1").unwrap();
    assert_eq!(walk.you, "studio");
    assert_eq!(walk.stops, ["%7", "studio:%16", "pi:%3"]);
    assert!(walk.at > 0);
}

/// The names of the files in the folder of the walks.
fn walk_files(far: &FarSide) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(far_paths(far).walks_dir())
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The walk lands in the file of the holder of the lease the feed last heard: a walk heard before any lease is
/// written nowhere (lines are read in order, so the lease file shows the walk before it was looked at), the same
/// walk after a lease is that holder's, and after a second holder's lease it is the second one's.
#[test]
fn the_walk_lands_in_the_file_of_the_holder_the_feed_heard() {
    let far = FarSide::new("studio");
    let mut feed = Feed::start(&far, &["--protocol", "1"]);
    let _ = hello_of(&feed);
    feed.send(r#"{"type":"walk","you":"studio","stops":["%1"]}"#);
    feed.send(r#"{"type":"lease","holder":"laptop-1"}"#);
    assert!(
        wait_for(|| lease_holder(&far).as_deref() == Some("laptop-1"), WAIT),
        "the lease file was not written"
    );
    assert_eq!(
        walk_files(&far),
        Vec::<String>::new(),
        "a walk was written before a lease was heard"
    );
    feed.send(r#"{"type":"walk","you":"studio","stops":["%2"]}"#);
    assert!(
        wait_for(
            || files::read_walk(&far_paths(&far), "laptop-1")
                .is_some_and(|walk| walk.stops == ["%2"]),
            WAIT
        ),
        "the walk was not written: {:?}",
        walk_files(&far)
    );
    assert_eq!(walk_files(&far), ["laptop-1.json"]);
    feed.send(r#"{"type":"lease","holder":"desktop-2"}"#);
    feed.send(r#"{"type":"walk","you":"studio","stops":["%3"]}"#);
    assert!(
        wait_for(
            || files::read_walk(&far_paths(&far), "desktop-2")
                .is_some_and(|walk| walk.stops == ["%3"]),
            WAIT
        ),
        "the second holder's walk was not written: {:?}",
        walk_files(&far)
    );
    // The first holder's walk is still its own.
    assert_eq!(
        files::read_walk(&far_paths(&far), "laptop-1")
            .unwrap()
            .stops,
        ["%2"]
    );
}

#[test]
fn lines_that_are_not_the_apps_are_ignored_and_the_feed_goes_on() {
    let far = FarSide::new("studio");
    let mut feed = Feed::start(&far, &["--protocol", "1"]);
    let _ = hello_of(&feed);
    feed.send("this is not json");
    feed.send(r#"{"type":"weather"}"#);
    feed.send(&format!(
        r#"{{"type":"lease","holder":"{}"}}"#,
        "h".repeat(70_000)
    ));
    feed.send(r#"{"type":"lease","holder":"laptop-1"}"#);
    feed.send(r#"{"type":"walk","you":"studio","stops":["%2"]}"#);
    assert!(wait_for(
        || files::read_walk(&far_paths(&far), "laptop-1").is_some(),
        WAIT
    ));
    // The oversize line was dropped whole: it is no holder the walk went under, and the feed did not end.
    assert_eq!(lease_holder(&far).as_deref(), Some("laptop-1"));
    assert_eq!(walk_files(&far), ["laptop-1.json"]);
    assert_eq!(feed.exit_within(Duration::from_millis(100)), None);
}

/// A lease line for `holder` whose text is exactly `length` bytes, not counting its newline.
fn lease_line_of(holder: &str, length: usize) -> String {
    let head = format!(r#"{{"type":"lease","holder":"{holder}","pad":""#);
    let tail = r#""}"#;
    let line = format!(
        "{head}{}{tail}",
        "p".repeat(length - head.len() - tail.len())
    );
    assert_eq!(line.len(), length);
    line
}

#[test]
fn a_line_of_exactly_64_kib_is_read_and_one_byte_more_is_ignored() {
    let far = FarSide::new("studio");
    let mut feed = Feed::start(&far, &["--protocol", "1"]);
    let _ = hello_of(&feed);
    // One over first: had it been read it would hold the lease, and the line after it could not take it.
    feed.send(&lease_line_of("over", 64 * 1024 + 1));
    feed.send(&lease_line_of("exact", 64 * 1024));
    assert!(
        wait_for(|| lease_holder(&far).as_deref() == Some("exact"), WAIT),
        "the line of exactly 64 KiB was not read: {:?}",
        lease_holder(&far)
    );
    assert_eq!(feed.exit_within(Duration::from_millis(100)), None);
}

fn toast(text: &str) -> Toast {
    Toast {
        kind: "blocked".into(),
        pane: "%1".into(),
        text: text.into(),
        agent: "claude".into(),
        ..Toast::default()
    }
}

#[test]
fn a_toast_event_comes_out_as_a_toast_line_and_the_lease_is_true_for_the_holder_only() {
    let far = FarSide::new("studio");
    let paths = far_paths(&far);
    // Written before the feed started: not news.
    files::append_event(&paths, &Down::Toast(toast("old")));
    let mut feed = Feed::start(&far, &["--protocol", "1"]);
    let _ = hello_of(&feed);
    let _ = feed.next_state(WAIT).expect("a state");

    // No lease yet: the toast says so.
    files::append_event(&paths, &Down::Toast(toast("first")));
    let first = feed
        .next_where(WAIT, |line| match line {
            Down::Toast(t) => Some(t),
            _ => None,
        })
        .expect("a toast");
    assert_eq!(first.text, "first", "the old toast came out");
    assert!(!first.lease);
    assert_eq!(first.kind, "blocked");

    feed.send(r#"{"type":"lease","holder":"laptop-1"}"#);
    assert!(wait_for(
        || lease_holder(&far).as_deref() == Some("laptop-1"),
        WAIT
    ));
    files::append_event(&paths, &Down::Toast(toast("second")));
    files::append_event(
        &paths,
        &Down::Left(Left {
            holder: "laptop-1".into(),
            pane: "%1".into(),
            go: Some("pi:%3".into()),
        }),
    );
    let second = feed
        .next_where(WAIT, |line| match line {
            Down::Toast(t) => Some(t),
            _ => None,
        })
        .expect("a second toast");
    assert_eq!(second.text, "second");
    assert!(second.lease);
    let left = feed
        .next_where(WAIT, |line| match line {
            Down::Left(left) => Some(left),
            _ => None,
        })
        .expect("a left line");
    assert_eq!(left.go.as_deref(), Some("pi:%3"));

    // Another app took the lease: this feed's holder no longer holds it.
    std::fs::write(
        paths.lease_file(),
        format!(
            r#"{{"holder":"desktop-2","refreshed_at":{}}}"#,
            files::now_ms()
        ),
    )
    .unwrap();
    files::append_event(&paths, &Down::Toast(toast("third")));
    let third = feed
        .next_where(WAIT, |line| match line {
            Down::Toast(t) => Some(t),
            _ => None,
        })
        .expect("a third toast");
    assert_eq!(third.text, "third");
    assert!(!third.lease);
}

#[test]
fn closing_standard_input_ends_the_feed_within_a_second() {
    let far = FarSide::new("studio");
    let mut feed = Feed::start(&far, &["--protocol", "1"]);
    let _ = hello_of(&feed);
    let _ = feed.next_state(WAIT).expect("a state");
    feed.close_stdin();
    assert_eq!(feed.exit_within(Duration::from_secs(1)), Some(0));
}

#[test]
fn once_says_hello_and_ends() {
    let far = FarSide::new("studio");
    let out = far
        .server
        .taw_command(&["machine", "feed", "--protocol", "1", "--once"])
        .stdin(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.ends_with('\n'));
    let Down::Hello(hello) = parse_down(text.trim_end()).unwrap() else {
        panic!("{text}")
    };
    assert_eq!(
        (hello.protocol, hello.taw.as_str()),
        (1, env!("CARGO_PKG_VERSION"))
    );
    assert!(!hello.server.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).is_empty());
}

#[test]
fn another_protocol_is_answered_with_a_hello_of_protocol_one_and_the_feed_ends() {
    let far = FarSide::new("studio");
    let mut feed = Feed::start(&far, &["--protocol", "2"]);
    let hello = hello_of(&feed);
    assert_eq!(hello.protocol, 1);
    assert_eq!(feed.exit_within(Duration::from_secs(3)), Some(0));
    assert!(
        feed.next(Duration::from_millis(200)).is_none(),
        "more than a hello"
    );
}

#[test]
fn a_feed_with_no_tmux_server_says_so_and_keeps_looking() {
    let mut far = FarSide::new("studio");
    far.server.tmux_ok(&["kill-server"]);
    assert!(wait_for(
        || far.server.tmux(&["list-sessions"]).is_err(),
        WAIT
    ));
    let feed = Feed::start(&far, &["--protocol", "1"]);
    let hello = hello_of(&feed);
    assert_eq!(hello.server, "");
    let state = feed.next_state(WAIT).expect("an empty state");
    assert_eq!(state, State::default());

    // A server comes up on the same socket: the feed finds it.
    far.server.crash_and_start_afresh(&common::ServerOptions {
        session: "orchard".to_string(),
        ..common::ServerOptions::default()
    });
    let again = feed
        .next_where(Duration::from_secs(10), |line| match line {
            Down::Hello(hello) if !hello.server.is_empty() => Some(hello),
            _ => None,
        })
        .expect("a hello that names the new server");
    let server = far
        .server
        .tmux_ok(&["display-message", "-p", "#{pid} #{start_time}"]);
    assert_eq!(again.server, server);
    let state = feed
        .next_where(Duration::from_secs(10), |line| match line {
            Down::State(state) if !state.spaces.is_empty() => Some(state),
            _ => None,
        })
        .expect("a state with the new server's session");
    assert_eq!(state.spaces[0].session_name, "orchard");
}

#[test]
fn a_tmux_server_that_restarts_is_named_in_a_new_hello() {
    let mut far = FarSide::new("studio");
    let feed = Feed::start(&far, &["--protocol", "1"]);
    let first = hello_of(&feed);
    assert!(!first.server.is_empty());
    // Positive wait: the first state is out before the server goes.
    assert!(feed.next_state(WAIT).is_some());
    // No wait and no pause between the old server going and the new one: a tick that looks in between
    // sees a server both times, so only the check of who the server is, every tick, can say it changed.
    far.server.crash_and_start_at_once(&common::ServerOptions {
        session: "orchard".to_string(),
        ..common::ServerOptions::default()
    });
    let now = far
        .server
        .tmux_ok(&["display-message", "-p", "#{pid} #{start_time}"]);
    assert_ne!(now, first.server);
    let again = feed
        .next_where(WAIT, |line| match line {
            Down::Hello(hello) if hello.server == now => Some(hello),
            _ => None,
        })
        .expect("a hello that names the restarted server");
    assert_eq!(again.host, first.host);
}

#[test]
fn the_feed_prints_nothing_but_lines_of_json_on_standard_output() {
    let far = FarSide::new("studio");
    let mut feed = Feed::start(&far, &["--protocol", "1"]);
    let _ = hello_of(&feed);
    let _ = feed.next_state(WAIT);
    feed.settle(Duration::from_millis(500));
    feed.close_stdin();
    assert_eq!(feed.exit_within(Duration::from_secs(2)), Some(0));
    while let Ok(line) = feed.lines.try_recv() {
        let value: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("{line}"));
        assert!(value.get("type").is_some(), "{line}");
    }
}

#[test]
fn a_closed_standard_output_ends_the_feed_with_status_zero_and_no_message() {
    let far = FarSide::new("studio");
    let mut child = far
        .server
        .taw_command(&["machine", "feed", "--protocol", "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    assert!(first.contains("\"type\":\"hello\""), "{first}");
    // The app has gone: nothing reads the feed any more. The next thing it says finds that out.
    drop(reader);
    far.server
        .tmux_ok(&["rename-window", "-t", "=garden:0", "gone"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    let _ = child.wait();
    drop(stdin);
    let status = status.expect("the feed did not end");
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert_eq!(stderr, "");
}
