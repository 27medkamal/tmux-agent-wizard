//! Next up across machines, end to end: `prefix+O` pressed in a client the app started, against the
//! REAL far taw behind the stand-in for ssh, as `tests/app_remote.rs` does it.
//!
//! "This Mac" is a private server with a session `one`; the machine `studio` is a second one with a session
//! `garden` (windows `main` and `tools`); the terminal is a third. Every pane of the three that matters is
//! an agent that needs you, so the walk is this Mac's pane, studio's `main`, studio's `tools`, and round. The key
//! is bound as the plugin binds it (`F6` stands for the prefix key and `O`): `taw notification open-target`
//! run on that machine's own server. No test runs a real ssh. Every wait is positive, with a 20 s deadline.

mod common;

use std::time::Duration;

use common::machine::{FarSide, StandIn};
use common::owned::{Owned, own, record_names};
use common::{Server, ServerOptions, taw_bin, wait_for};
use taw::store::{self, AgentState};
use taw::tmux::Tmux;

struct Rig {
    term: Server,
    mac: Server,
    far: FarSide,
    stand_in: StandIn,
}

fn terminal() -> Server {
    Server::start_with(ServerOptions {
        width: 200,
        height: 30,
        ..ServerOptions::default()
    })
}

/// Marks `pane` of `server` as the agent of a kind that is blocked: it needs you.
fn needs_you(server: &Server, pane: &str) {
    store::write_state_with_kind(
        &Tmux::with_socket_name(server.socket()),
        pane,
        Some("claude"),
        AgentState::Blocked,
        None,
        "manifest",
        100,
    )
    .unwrap();
}

/// Where the key bound by [`bind_key`] on `server` leaves its exit status (and what it printed).
fn key_status_file(server: &Server) -> std::path::PathBuf {
    server.work_dir().join("key-status")
}

/// Binds `F6` on `server` to the notification key's command, with `server`'s own environment. The plugin's binding
/// (`tmux-agent-wizard.tmux`) reads the command's reply and its status in `sh`, and is a `bind` whose line needs the
/// plugin's own quoting; this is a `run-shell` of the same command, whose reply and exit status go to files in the
/// server's own scratch folder (the status is written last), so that a test can say the key did not fail.
fn bind_key(server: &Server) {
    let env: String = server
        .taw_env()
        .iter()
        .map(|(name, value)| format!("'{name}={value}' "))
        .collect();
    let status = key_status_file(server);
    server.tmux_ok(&[
        "bind-key",
        "-n",
        "F6",
        "run-shell",
        &format!(
            "out=$(env -u TMUX -u TMUX_PANE {env}'{}' notification open-target 2>&1); rc=$?; \
             printf '%s' \"$out\" > '{}.out'; echo $rc > '{}'",
            taw_bin(),
            status.display(),
            status.display()
        ),
    ]);
}

fn rig() -> Rig {
    let mac = Server::start();
    mac.write_config("[ui]\ndepth = \"all\"\n\n[machines]\nstudio = \"studio\"\n");
    let far = FarSide::with_session("studio", "garden");
    far.server
        .tmux_ok(&["rename-window", "-t", "garden:0", "main"]);
    far.server
        .tmux_ok(&["new-window", "-d", "-t", "garden:", "-n", "tools"]);
    // The two servers' first panes have the same id, and the mutations of the tests that name them (`you` ignored)
    // rely on that collision: nothing else would tell the two servers' `%0` apart.
    assert_eq!(
        mac.pane(),
        far.server.pane_of("garden:main"),
        "this Mac's first pane and studio's main must share an id"
    );
    needs_you(&mac, &mac.pane());
    needs_you(&far.server, &far.server.pane_of("garden:main"));
    needs_you(&far.server, &far.server.pane_of("garden:tools"));
    bind_key(&mac);
    bind_key(&far.server);
    let stand_in = StandIn::new(&[&far]);
    Rig {
        term: terminal(),
        mac,
        far,
        stand_in,
    }
}

fn terminal_screen(term: &Server) -> String {
    term.tmux(&["capture-pane", "-p", "-t", &term.pane()])
        .unwrap_or_default()
}

/// What a failure says besides the screen: the stand-in's calls and the apps of this build that run.
fn seen(rig: &Rig) -> String {
    let ps = std::process::Command::new("/bin/ps")
        .args(["-Ao", "pid=,ppid=,args="])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).to_string())
        .unwrap_or_default();
    let apps: Vec<&str> = ps
        .lines()
        .filter(|line| line.contains(&format!("{} app", taw_bin())))
        .collect();
    format!(
        "the terminal:\n{}\nthe last words of the stand-in's calls: {:#?}\nthe app's standard error: {:?}\nthe apps that run: {apps:#?}",
        terminal_screen(&rig.term)
            .lines()
            .filter(|line| !line
                .trim_matches(|c: char| c == ' ' || c == '\u{2502}')
                .is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        rig.stand_in
            .calls()
            .iter()
            .map(|call| call.last().cloned().unwrap_or_default())
            .collect::<Vec<_>>(),
        std::fs::read_to_string(rig.mac.work_dir().join("err")).unwrap_or_default()
    )
}

/// Waits (20 s) until `ready` holds, and fails with `what` and everything [`seen`] when it does not.
fn expect(rig: &Rig, what: &str, mut ready: impl FnMut() -> bool) {
    assert!(
        wait_for(&mut ready, Duration::from_secs(20)),
        "{what} never happened.\n{}",
        seen(rig)
    );
}

/// Waits (20 s) until `ready` holds of the terminal's screen.
fn expect_screen(rig: &Rig, what: &str, ready: impl Fn(&str) -> bool) {
    expect(rig, what, || ready(&terminal_screen(&rig.term)));
}

/// Types `keys` (tmux key names) into the terminal.
fn press(term: &Server, keys: &[&str]) {
    let pane = term.pane();
    let mut args = vec!["send-keys", "-t", pane.as_str()];
    args.extend_from_slice(keys);
    term.tmux_ok(&args);
}

/// The row of the list the cursor is on, as plain text: the line of the screen drawn with the selection's
/// background (the theme's own, fixed by the default), or empty.
fn selected_row(term: &Server) -> String {
    let styled = term
        .tmux(&["capture-pane", "-p", "-e", "-t", &term.pane()])
        .unwrap_or_default();
    for line in styled.lines() {
        if line.contains("48;2;69;71;90m") {
            let mut plain = String::new();
            let mut chars = line.chars();
            while let Some(c) = chars.next() {
                if c == '\u{1b}' {
                    for end in chars.by_ref() {
                        if end.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    plain.push(c);
                }
            }
            return plain;
        }
    }
    String::new()
}

/// Puts the cursor on the row that shows `needle`: `2` is studio's session, then `j` a row at a time, each step
/// waited for.
fn move_to(rig: &Rig, needle: &str) {
    move_to_in(rig, "2", "garden", needle);
}

/// The same from the session that key `key` selects, whose row says `session`.
fn move_to_in(rig: &Rig, key: &str, session: &str, needle: &str) {
    press(&rig.term, &[key]);
    expect(rig, "the cursor on studio's session", || {
        selected_row(&rig.term).contains(session)
    });
    for _ in 0..12 {
        let row = selected_row(&rig.term);
        if row.contains(needle) {
            return;
        }
        press(&rig.term, &["j"]);
        expect(rig, "the cursor moving down a row", || {
            selected_row(&rig.term) != row
        });
    }
    panic!(
        "no row with {needle} was reached; the cursor is on {:?}\n{}",
        selected_row(&rig.term),
        seen(rig)
    );
}

/// The records of the apps that run, by file name, under this Mac's state folder.
fn records(mac: &Server) -> Vec<String> {
    record_names(
        std::fs::read_dir(mac.state_dir().join("apps"))
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default(),
    )
}

/// Types the app into the terminal as run for this Mac, with the stand-in as its ssh, and waits for its record (the
/// guard holds the pid from then on) and for studio's rows. Returns the app's pid.
fn start_app(rig: &Rig) -> u32 {
    let bin = rig.stand_in.bin.display().to_string();
    let env: String = rig
        .mac
        .taw_env()
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .chain([("TAW_SSH_BIN", bin.as_str())])
        .map(|(name, value)| format!("'{name}={value}' "))
        .collect();
    let line = format!(
        "env -u TMUX -u TMUX_PANE {env}'{}' app --child 2>'{}'; rc=$?; stty -a >'{}' 2>&1; echo $rc >'{}'",
        taw_bin(),
        rig.mac.work_dir().join("err").display(),
        rig.mac.work_dir().join("modes").display(),
        rig.mac.work_dir().join("status").display()
    );
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "-l", &line]);
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "Enter"]);
    // The record first and the guard at once: a failed wait for the rows must not leave the app outside it.
    expect(rig, "one record of an app", || records(&rig.mac).len() == 1);
    let name = records(&rig.mac).remove(0);
    let pid: u32 = name.trim_end_matches(".json").parse().expect("a pid");
    own(pid);
    expect_screen(rig, "studio's rows on the list", |screen| {
        screen.contains(" taw app") && screen.contains("0:main") && screen.contains("1:tools")
    });
    pid
}

/// The calls of the stand-in that are an attach: ssh with a terminal (`-t`) running `machine attach`, with
/// their place among all the calls.
fn attaches(stand_in: &StandIn) -> Vec<(usize, Vec<String>)> {
    stand_in
        .calls()
        .into_iter()
        .enumerate()
        .filter(|(_, call)| {
            call.iter().any(|word| word == "-t")
                && call
                    .last()
                    .is_some_and(|line| line.contains("'machine' 'attach'"))
        })
        .collect()
}

/// The target of an attach call: the word after `'attach'` in the command line it runs.
fn target_of(call: &[String]) -> String {
    let line = call.last().expect("a command line");
    let after = line
        .split("'attach' '")
        .nth(1)
        .unwrap_or_else(|| panic!("no attach in {line}"));
    after.split('\'').next().unwrap().to_string()
}

/// How many clients studio's server has: the people (and the app's attach) looking at it.
fn far_clients(rig: &Rig) -> usize {
    rig.far
        .server
        .tmux(&["list-clients", "-F", "#{client_pid}"])
        .map(|out| out.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0)
}

/// Enter on the row of `needle`, and the wait until studio's session is on the terminal: studio's server has
/// the client and its status line is drawn.
fn go_in_at(rig: &Rig, needle: &str) {
    move_to(rig, needle);
    press(&rig.term, &["Enter"]);
    expect_screen(rig, "studio's status line on the terminal", |screen| {
        screen.contains("[garden]")
    });
    expect(rig, "studio's server having the client", || {
        far_clients(rig) == 1
    });
}

/// The stops of the newest walk file in the walks folder of `state` (one file for each holder; here one app
/// writes each), or none while there is none yet.
fn walk_stops(state: &std::path::Path) -> Vec<String> {
    let walks: Vec<serde_json::Value> = std::fs::read_dir(state.join("machines").join("walks"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|text| serde_json::from_str(&text).ok())
        .collect();
    walks
        .iter()
        .max_by_key(|walk| walk["at"].as_u64().unwrap_or(0))
        .and_then(|walk| {
            walk["stops"].as_array().map(|stops| {
                stops
                    .iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// Waits until this Mac's and studio's walk files both hold the walk of the three agents, and returns the
/// stops: this Mac's pane, then studio's `main`, then studio's `tools`.
fn walk_known(rig: &Rig) -> Vec<String> {
    let want = vec![
        rig.mac.pane(),
        format!("studio:{}", rig.far.server.pane_of("garden:main")),
        format!("studio:{}", rig.far.server.pane_of("garden:tools")),
    ];
    expect(rig, "the walk in this Mac's file", || {
        walk_stops(rig.mac.state_dir()) == want
    });
    expect(rig, "the walk in studio's file", || {
        walk_stops(&rig.far.state_dir) == want
    });
    want
}

/// Presses the key on the server of `server`, as the client the app started would.
fn press_key(rig: &Rig) {
    press(&rig.term, &["F6"]);
}

/// From this Mac's session the key goes to the first of studio's agents: studio's session is on the terminal at
/// that pane, by the app's own attach. Mutation: `go` ignored in the feed's `left` handling.
#[test]
fn the_key_in_this_macs_session_goes_into_studio_at_the_next_agent() {
    let _own = Owned::new();
    let rig = rig();
    let pid = start_app(&rig);
    let stops = walk_known(&rig);
    let session = rig
        .mac
        .tmux_ok(&["display-message", "-p", "#{session_name}"]);
    move_to_in(&rig, "1", session.trim(), "");
    press(&rig.term, &["Enter"]);
    expect(&rig, "this Mac's server having the client", || {
        rig.mac
            .tmux(&["list-clients", "-F", "#{client_pid}"])
            .is_ok_and(|out| !out.trim().is_empty())
    });
    press_key(&rig);
    expect(&rig, "studio's server having the client", || {
        far_clients(&rig) == 1
    });
    // The key itself did not fail: its exit status is 0 (the file is written when the command is done).
    let status = key_status_file(&rig.mac);
    expect(&rig, "the key's exit status", || {
        std::fs::read_to_string(&status).is_ok_and(|text| !text.trim().is_empty())
    });
    assert_eq!(
        std::fs::read_to_string(&status).unwrap().trim(),
        "0",
        "the key said {:?}\n{}",
        std::fs::read_to_string(format!("{}.out", status.display())).unwrap_or_default(),
        seen(&rig)
    );
    let found = attaches(&rig.stand_in);
    assert_eq!(found.len(), 1, "one attach\n{}", seen(&rig));
    assert_eq!(
        target_of(&found[0].1),
        stops[1][7..],
        "studio's main\n{}",
        seen(&rig)
    );
    let _ = pid;
}

/// Inside studio the key between two of studio's agents is a tmux switch and nothing else: the window `tools`
/// becomes the current one and the app made no second attach. Mutation: `you` ignored (the pane ids of the
/// two servers collide, so the next stop is looked for among this Mac's).
#[test]
fn the_key_inside_studio_moves_to_studios_next_agent_with_no_second_attach() {
    let _own = Owned::new();
    let rig = rig();
    let _pid = start_app(&rig);
    walk_known(&rig);
    go_in_at(&rig, "0:main");
    press_key(&rig);
    expect_screen(&rig, "studio's client on the window tools", |screen| {
        screen.contains("1:tools*")
    });
    assert_eq!(
        attaches(&rig.stand_in).len(),
        1,
        "no second attach\n{}",
        seen(&rig)
    );
    assert_eq!(far_clients(&rig), 1);
}

/// From studio's last agent the walk wraps to this Mac's first: this Mac's server gets the client at that pane
/// at once, with no list between. Mutation: `go` ignored.
#[test]
fn the_key_at_studios_last_agent_goes_to_this_macs_agent_at_once() {
    let _own = Owned::new();
    let rig = rig();
    let _pid = start_app(&rig);
    let stops = walk_known(&rig);
    // This Mac gets a second window and so a second pane, `spare`, which is not a stop and is the one its session
    // shows: only a go that selects the stop can put the client at it.
    let spare = rig
        .mac
        .tmux_ok(&[
            "new-window",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "one:",
            "-n",
            "spare",
        ])
        .trim()
        .to_string();
    assert_ne!(spare, stops[0], "the spare pane is not the stop");
    go_in_at(&rig, "1:tools");
    press_key(&rig);
    expect(&rig, "this Mac's server having the client", || {
        rig.mac
            .tmux(&["list-clients", "-F", "#{client_pid}"])
            .is_ok_and(|out| !out.trim().is_empty())
    });
    expect(&rig, "studio's client gone", || far_clients(&rig) == 0);
    let client_pane = rig
        .mac
        .tmux_ok(&["list-clients", "-F", "#{pane_id}"])
        .trim()
        .to_string();
    assert_eq!(
        client_pane,
        stops[0],
        "the client is at the stop, not at the spare pane {spare}\n{}",
        seen(&rig)
    );
}
