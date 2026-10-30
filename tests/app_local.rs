//! `taw app` on this Mac, end to end: the built binary against private tmux servers.
//!
//! Inside tmux the app is the popup; outside it is a supervisor and a screen.
//! The tests that need a terminal give the app one the way a person has one: a pane of an outer
//! tmux server, whose own terminal is the test's to read and type into. The app reads and acts
//! on the inner server, which is "this Mac" and is named by `TAW_TMUX_SOCKET_NAME`.
//!
//! Nothing here reaches beyond the fixtures' own folders and `-L` sockets, and no ssh is run.

mod clients;
mod common;

use std::path::Path;
use std::time::Duration;

use clients::Client;
use common::machine::{FarSide, StandIn};
use common::owned::{
    Owned, alive, args_of, disown, is_the_recorded_process, own, own_as, record_names, signal,
    start_time_of, start_time_when_alive, status_of,
};
use common::{Server, ServerOptions, taw_bin, wait_for};
use taw::tmux::Tmux;

/// What a terminal shows, or what went wrong reading it.
fn screen(outer: &Tmux, target: &str) -> String {
    outer
        .run(&["capture-pane", "-p", "-t", target])
        .unwrap_or_default()
}

/// Waits (20 s) until `ready` holds of the screen of `target`, and fails with the screen when it does not.
fn expect_screen(outer: &Tmux, target: &str, what: &str, ready: impl Fn(&str) -> bool) {
    let seen = wait_for(|| ready(&screen(outer, target)), Duration::from_secs(20));
    assert!(
        seen,
        "{what} never showed on {target}:\n{}",
        screen(outer, target)
    );
}

/// Reads a file the test's pane wrote, or `None` until it is there.
fn read(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// A status counts once its line is whole.
#[test]
fn a_status_counts_once_its_line_is_whole() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("status");
    assert_eq!(status_of(&path), None, "no file");
    std::fs::write(&path, "").unwrap();
    assert_eq!(status_of(&path), None, "created and not yet written");
    std::fs::write(&path, "0").unwrap();
    assert_eq!(status_of(&path), None, "no line end yet");
    std::fs::write(&path, "0\n").unwrap();
    assert_eq!(status_of(&path).as_deref(), Some("0\n"));
}

/// A terminal that is already in tmux gets the popup, and is told why.
#[test]
fn inside_tmux_the_app_says_so_and_opens_the_popup_on_the_client() {
    let server = Server::start();
    let inner = Tmux::with_socket_name(server.socket());
    let client = Client::attach(&inner, "one");
    let said = server.work_dir().join("said");
    let status = server.work_dir().join("status");
    let line = format!(
        "'{}' app 2>'{}'; echo $? >'{}'",
        taw_bin(),
        said.display(),
        status.display()
    );
    server.tmux_ok(&["send-keys", "-t", &server.pane(), "-l", &line]);
    server.tmux_ok(&["send-keys", "-t", &server.pane(), "Enter"]);

    // The popup is the list: its own frame is on the client's terminal, over the pane's shell.
    expect_screen(client.outer(), "host", "the popup", |screen| {
        screen.contains("╭ sessions") && screen.contains("j/k move")
    });
    assert_eq!(
        read(&said).as_deref(),
        Some("taw app: this terminal is already in tmux; the popup is the list here\n")
    );
    // Closing the popup ends the verb, and it ends 0.
    client
        .outer()
        .run_ok(&["send-keys", "-t", "host", "q"])
        .unwrap();
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "taw app did not end after the popup closed"
    );
    assert_eq!(status_of(&status).as_deref(), Some("0\n"));
}

// ---------------------------------------------------------------- the app child's screen

/// A terminal as a person has one: a private server of its own whose pane is the terminal, `width`
/// columns wide and not in the tmux of "this Mac".
fn terminal(width: u32) -> Server {
    Server::start_with(ServerOptions {
        width,
        height: 30,
        ..ServerOptions::default()
    })
}

/// Types `taw <args>` into the terminal as run for "this Mac" `mac`: its server's name, folders and shell
/// home in the environment, `$TMUX` out of it (the terminal is outside tmux), the words it says on
/// standard error in `err`, and its status in `status`.
fn type_app(terminal: &Server, mac: &Server, args: &str, err: &Path, status: &Path) {
    type_app_env(terminal, mac, &[], args, err, status);
}

/// [`type_app`] with `extra` set after the fixture's variables, which therefore win.
fn type_app_env(
    terminal: &Server,
    mac: &Server,
    extra: &[(&str, &str)],
    args: &str,
    err: &Path,
    status: &Path,
) {
    type_line(terminal, mac, "", extra, args, err, status);
}

/// [`type_app`] with the app run by a parent that does not notice when it is stopped (`perl`'s `system`
/// waits for its child's end, where a shell, job control or not, takes a stop for an end and takes the
/// terminal back), so a test can stop the app with `SIGSTOP` while keys are typed that only it reads.
fn type_app_held(terminal: &Server, mac: &Server, args: &str, err: &Path, status: &Path) {
    let hold = "perl -e 'system(@ARGV); exit($? >> 8)' -- ";
    type_line(terminal, mac, hold, &[], args, err, status);
}

fn type_line(
    terminal: &Server,
    mac: &Server,
    hold: &str,
    extra: &[(&str, &str)],
    args: &str,
    err: &Path,
    status: &Path,
) {
    let env: String = mac
        .taw_env()
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .chain(extra.iter().copied())
        .map(|(name, value)| format!("'{name}={value}' "))
        .collect();
    let line = format!(
        "{hold}env -u TMUX -u TMUX_PANE {env}'{}' {args} 2>'{}'; echo $? >'{}'",
        taw_bin(),
        err.display(),
        status.display()
    );
    // Typed before the shell reads its terminal, the line would be cut at the terminal's line buffer.
    terminal.wait_for_prompt(&terminal.pane());
    terminal.tmux_ok(&["send-keys", "-t", &terminal.pane(), "-l", &line]);
    terminal.tmux_ok(&["send-keys", "-t", &terminal.pane(), "Enter"]);
}

/// The terminal's screen, or what went wrong reading it.
fn terminal_screen(terminal: &Server) -> String {
    terminal
        .tmux(&["capture-pane", "-p", "-t", &terminal.pane()])
        .unwrap_or_default()
}

/// Waits (20 s) until `ready` holds of the terminal's screen, and fails with the screen when it does not.
fn expect_terminal(terminal: &Server, what: &str, ready: impl Fn(&str) -> bool) {
    let seen = wait_for(
        || ready(&terminal_screen(terminal)),
        Duration::from_secs(20),
    );
    assert!(
        seen,
        "{what} never showed on the terminal:\n{}",
        terminal_screen(terminal)
    );
}

/// The records of the apps that run, by file name, under the "this Mac" fixture's state folder.
fn records(mac: &Server) -> Vec<String> {
    let mut names: Vec<String> = record_names(
        std::fs::read_dir(mac.state_dir().join("apps"))
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default(),
    );
    names.sort();
    names
}

/// Everything that is left in the apps folder, the temporary file a record is written through included: what an app
/// that has ended must have taken away. (`records` leaves the temporary names out, for a test that waits for a record.)
fn leftovers(mac: &Server) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(mac.state_dir().join("apps"))
        .map(|dir| {
            dir.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The absent-table proof: with no machines the app lists this Mac's sessions as the popup would,
/// in the whole terminal with its own top line, no machines part and no `this Mac` header row (that row
/// is drawn only when machines are configured), calls no ssh, keeps a record while it runs, and `q` ends it
/// 0 and takes the record away.
#[test]
fn the_app_lists_this_macs_sessions_under_its_own_line_and_q_ends_it() {
    let _own = Owned::new();
    let mac = Server::start();
    mac.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-c",
        &mac.work_dir().display().to_string(),
    ]);
    let term = terminal(120);
    let err = mac.work_dir().join("err");
    let status = mac.work_dir().join("status");
    type_app(&term, &mac, "app --child", &err, &status);

    expect_terminal(&term, "the app's list", |screen| {
        screen.contains(" taw app")
            && screen.contains("2 sessions")
            && screen.contains("alpha")
            && screen.contains("one")
    });
    // The guard holds the app's pids from here (its child ends by the watchdog when the terminal's server goes).
    let _ = the_app(&mac);
    let screen = terminal_screen(&term);
    assert!(!screen.contains("machine"), "no machines part:\n{screen}");
    assert!(
        !screen.contains("this Mac"),
        "no header row without machines (the header is for a list with machines):\n{screen}"
    );
    // The record of the running app, named by its pid, which is alive.
    assert!(
        wait_for(|| records(&mac).len() == 1, Duration::from_secs(20)),
        "no record of the app: {:?}",
        records(&mac)
    );
    let record = records(&mac).remove(0);
    let json = read(&mac.state_dir().join("apps").join(&record)).expect("the record reads");
    assert!(json.contains("\"holder\""), "{json}");
    assert!(json.contains(record.trim_end_matches(".json")), "{json}");

    term.tmux_ok(&["send-keys", "-t", &term.pane(), "q"]);
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "the app did not end on q:\n{}",
        terminal_screen(&term)
    );
    assert_eq!(status_of(&status).as_deref(), Some("0\n"));
    assert_eq!(
        leftovers(&mac),
        Vec::<String>::new(),
        "the record goes with the app"
    );
    assert_eq!(mac.ssh_calls(), Vec::<String>::new(), "no machines, no ssh");
}

/// Ctrl-C leaves at once, and that ends the child 0 as well.
#[test]
fn ctrl_c_ends_the_app() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    let status = mac.work_dir().join("status");
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &status,
    );
    expect_terminal(&term, "the app's list", |screen| {
        screen.contains(" taw app")
    });
    // The guard holds the app's pids from here (its child ends by the watchdog when the terminal's server goes).
    let _ = the_app(&mac);
    term.tmux_ok(&["send-keys", "-t", &term.pane(), "C-c"]);
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "the app did not end on Ctrl-C:\n{}",
        terminal_screen(&term)
    );
    assert_eq!(status_of(&status).as_deref(), Some("0\n"));
}

/// The cursor opens on the session attached most lately: the preview, which follows the cursor, shows
/// its pane and not the first session's.
#[test]
fn the_cursor_opens_on_the_session_a_client_was_last_attached_to() {
    let _own = Owned::new();
    let mac = Server::start();
    let inner = Tmux::with_socket_name(mac.socket());
    mac.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "zulu",
        "-c",
        &mac.work_dir().display().to_string(),
    ]);
    mac.tmux_ok(&["send-keys", "-t", "one", "echo ONE_MARK", "Enter"]);
    mac.tmux_ok(&["send-keys", "-t", "zulu", "echo ZULU_MARK", "Enter"]);
    // `one` is first in the list (names are sorted), and zulu is the session a client was last attached to.
    let _client = Client::attach(&inner, "zulu");
    let term = terminal(120);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "zulu's pane in the preview", |screen| {
        screen.contains("ZULU_MARK")
    });
    // The guard holds the app's pids from here (its child ends by the watchdog when the terminal's server goes).
    let _ = the_app(&mac);
    assert!(
        !terminal_screen(&term).contains("ONE_MARK"),
        "the first session's pane is not what the cursor is on"
    );
}

/// A `[machines]` table that does not load starts the app with no machines and says why, on its
/// top line, once the first frame is drawn; this Mac's rows are there as always. The words stay for a few
/// seconds, so the wait for them is a poll with a long deadline that fails with what it saw.
#[test]
fn a_machines_table_that_does_not_load_is_said_after_the_first_frame_and_this_macs_rows_work() {
    let _own = Owned::new();
    let mac = Server::start();
    mac.write_config("[machines]\nBad = \"x\"\n");
    let term = terminal(220);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    // One wait for both, so that the toast leaving (it does, after a few seconds) cannot fail the test.
    expect_terminal(
        &term,
        "the words of the doubt over this Mac's rows",
        |screen| screen.contains("taw drops") && screen.contains("one"),
    );
    // The guard holds the app's pids from here (its child ends by the watchdog when the terminal's server goes).
    let _ = the_app(&mac);
    assert_eq!(mac.ssh_calls(), Vec::<String>::new());
}

/// A client attached by hand to `alpha` is nobody's mark: no row of the app is the session of a client, so
/// alpha's header has no `*`, and Enter on it goes in like on any row instead of ending the app.
#[test]
fn a_hand_attached_session_is_no_own_row_and_enter_on_it_goes_in() {
    let _own = Owned::new();
    let mac = Server::start();
    mac.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-c",
        &mac.work_dir().display().to_string(),
    ]);
    let hand = Client::attach(&inner(&mac), "alpha");
    let term = terminal(120);
    let status = mac.work_dir().join("status");
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &status,
    );
    expect_terminal(&term, "alpha in the list", |screen| {
        screen.contains(" taw app") && screen.contains("alpha")
    });
    // Positive: alpha's row is drawn; and it carries no mark, though a client is in it.
    let screen = terminal_screen(&term);
    let alpha = screen
        .lines()
        .find(|line| line.contains("alpha"))
        .expect("alpha's row");
    assert!(
        !alpha.contains('*'),
        "alpha is marked as a client's:\n{screen}"
    );

    // Enter goes in: the record names the app's own client (a second one), and the app is still there.
    press(&term, &["Enter"]);
    let named = wait_for(
        || {
            the_record(&mac)["client_pid"]
                .as_u64()
                .is_some_and(|pid| pid != 0)
        },
        Duration::from_secs(20),
    );
    assert!(
        named,
        "Enter did not attach a client; the terminal shows:\n{}",
        terminal_screen(&term)
    );
    let client_pid = the_record(&mac)["client_pid"].as_u64().unwrap() as u32;
    // The record names the client as soon as it is started, and the server lists it once it has attached.
    assert!(
        wait_for(
            || clients_of(&mac).iter().any(|client| client.0 == client_pid),
            Duration::from_secs(20)
        ),
        "the app's client {client_pid} is not one of the server's: {:?}",
        clients_of(&mac)
    );
    let (_, name, _) = clients_of(&mac)
        .into_iter()
        .find(|client| client.0 == client_pid)
        .expect("the app's client is one of the server's");
    assert_eq!(status_of(&status), None, "the app ended on Enter");
    mac.tmux_ok(&["detach-client", "-t", &name]);
    expect_terminal(&term, "the app's list again", |screen| {
        screen.contains(" taw app") && screen.contains("alpha")
    });
    assert_eq!(status_of(&status), None, "the app is still running");
    drop(hand);
}

/// Ends the tmux server of "this Mac", so that it has none (waits until it is gone).
fn end_the_server(mac: &Server) {
    let _ = mac.tmux(&["kill-server"]);
    assert!(
        wait_for(
            || mac.tmux(&["list-sessions"]).is_err(),
            Duration::from_secs(20)
        ),
        "the server did not go"
    );
}

/// With no machines and no tmux server the app is the popup's own empty list under its top line: no header
/// row, the popup's words for an empty list, and `n` is reachable (this is the fresh terminal's first run).
#[test]
fn with_no_server_and_no_machines_the_app_shows_the_empty_list_and_n_opens_the_picker() {
    let _own = Owned::new();
    let mac = Server::start();
    end_the_server(&mac);
    let term = terminal(120);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the top line and the empty list", |screen| {
        screen.contains(" taw app") && screen.contains("no sessions")
    });
    // The guard holds the app's pids from here (its child ends by the watchdog when the terminal's server goes).
    let _ = the_app(&mac);
    let screen = terminal_screen(&term);
    assert!(
        !screen.contains("this Mac"),
        "a header row with no machines:\n{screen}"
    );

    press(&term, &["n"]);
    expect_terminal(&term, "the new-session picker", |screen| {
        screen.contains("new session") && screen.contains("type a path or project")
    });
    assert_eq!(mac.ssh_calls(), Vec::<String>::new(), "no machines, no ssh");
}

/// With a machine configured `this Mac`'s header shows even when this Mac has no server, with nothing under
/// it, next to the machine's own.
#[test]
fn with_no_server_and_a_machine_this_macs_header_shows_with_no_session_under_it() {
    let _own = Owned::new();
    let mac = Server::start();
    end_the_server(&mac);
    mac.write_config("[machines]\npi = \"pi\"\n");
    let term = terminal(120);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "both headers", |screen| {
        screen.contains(" taw app") && screen.contains("this Mac") && screen.contains("pi")
    });
    // The guard holds the app's pids from here (its child ends by the watchdog when the terminal's server goes).
    let _ = the_app(&mac);
    let screen = terminal_screen(&term);
    // Rows of the list: a session row begins with its number, and there is none.
    assert!(
        !screen
            .lines()
            .any(|line| line.starts_with("\u{2502}1") || line.starts_with("\u{2502}2")),
        "a session row on a Mac with no server:\n{screen}"
    );
    assert!(
        !screen.contains("no sessions"),
        "the empty-list words with a header drawn:\n{screen}"
    );
}

/// Whether the app that keeps the connections has written that machine `studio` is this machine's own tmux server
/// (`<state>/machines/studio.json`): the list draws no header over one machine, so the screen no longer says it.
fn studio_is_this_server(server: &Server) -> bool {
    std::fs::read_to_string(server.state_dir().join("machines").join("studio.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|file| file["this_server"] == true)
}

/// A machine of `[machines]` that is this Mac itself, listed while this Mac has no tmux server: the app
/// cannot tell it from another machine then, and when the server comes up it reads the word again and
/// starts its connections again with it, so the machine is said to be this tmux server and its session
/// is listed once, under this Mac. The stand-in ssh reaches the same server (its far side is the Mac).
#[test]
fn a_machine_that_is_this_mac_is_known_once_the_server_that_was_not_there_comes_up() {
    let _own = Owned::new();
    let mut far = FarSide::with_session("studio", "garden");
    end_the_server(&far.server);
    far.server.write_config("[machines]\nstudio = \"studio\"\n");
    let stand_in = StandIn::new(&[&far]);
    let bin = stand_in.bin.display().to_string();
    let term = terminal(120);
    type_app_env(
        &term,
        &far.server,
        &[("TAW_SSH_BIN", &bin)],
        "app --child",
        &far.server.work_dir().join("err"),
        &far.server.work_dir().join("status"),
    );
    expect_terminal(
        &term,
        "studio's header while there is no server",
        |screen| {
            screen.contains(" taw app") && screen.contains("this Mac") && screen.contains("studio")
        },
    );
    // The guard holds the app's pids from here (its child ends by the watchdog when the terminal's server goes).
    let _ = the_app(&far.server);
    assert!(
        wait_for(|| !stand_in.calls().is_empty(), Duration::from_secs(20)),
        "the app never reached for studio:\n{}",
        terminal_screen(&term)
    );
    // The server comes up, with the session `garden`, with the app already running.
    far.server.crash_and_start_at_once(&ServerOptions {
        session: "garden".to_string(),
        ..ServerOptions::default()
    });
    assert!(
        wait_for(
            || studio_is_this_server(&far.server),
            Duration::from_secs(20)
        ),
        "studio never said to be this tmux server:\n{}",
        terminal_screen(&term)
    );
    expect_terminal(&term, "no header over the one machine", |screen| {
        !screen.contains("studio") && screen.contains("garden")
    });
    let screen = terminal_screen(&term);
    assert_eq!(
        screen.matches("garden").count(),
        1,
        "garden listed once, under this Mac:\n{screen}"
    );
}

/// The app starts with no tmux server and no `[machines]` (its hub is idle), a server comes up while it runs,
/// and only then does the config gain a machine that is this Mac itself: the hub that takes the edit compares
/// the machines with the word of the server that is there now, so the machine is said to be this tmux server and
/// its session is listed once, under this Mac.
#[test]
fn a_machine_added_while_running_that_is_this_mac_is_known_after_a_server_came_up_with_none_listed()
{
    let _own = Owned::new();
    let mut far = FarSide::with_session("studio", "garden");
    end_the_server(&far.server);
    far.server.write_config("[ui]\ndepth = \"all\"\n");
    let stand_in = StandIn::new(&[&far]);
    let bin = stand_in.bin.display().to_string();
    let term = terminal(120);
    type_app_env(
        &term,
        &far.server,
        &[("TAW_SSH_BIN", &bin)],
        "app --child",
        &far.server.work_dir().join("err"),
        &far.server.work_dir().join("status"),
    );
    expect_terminal(&term, "the app up with no server", |screen| {
        screen.contains(" taw app")
    });
    // The guard holds the app's pids from here (its child ends by the watchdog when the terminal's server goes).
    let _ = the_app(&far.server);
    far.server.crash_and_start_at_once(&ServerOptions {
        session: "garden".to_string(),
        ..ServerOptions::default()
    });
    expect_terminal(&term, "garden listed from the new server", |screen| {
        screen.contains("garden")
    });
    // Let the app's ticks read the server's word (it asks once a tick) before the config changes.
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        stand_in.calls().is_empty(),
        "no machine yet, so no ssh:\n{}",
        terminal_screen(&term)
    );

    far.server
        .write_config("[ui]\ndepth = \"all\"\n\n[machines]\nstudio = \"studio\"\n");
    assert!(
        wait_for(|| !stand_in.calls().is_empty(), Duration::from_secs(20)),
        "the app never reached for studio after the edit:\n{}",
        terminal_screen(&term)
    );
    assert!(
        wait_for(
            || studio_is_this_server(&far.server),
            Duration::from_secs(20)
        ),
        "studio never said to be this tmux server after the edit:\n{}",
        terminal_screen(&term)
    );
    expect_terminal(&term, "no header over the one machine", |screen| {
        !screen.contains("studio") && screen.contains("garden")
    });
    let screen = terminal_screen(&term);
    assert_eq!(
        screen.matches("garden").count(),
        1,
        "garden listed once, under this Mac:\n{screen}"
    );
}

/// The same machine that is this Mac, with the server up from the start: when that server ends and a new one starts
/// while the app runs, the word the machines are compared with is read again (the app has seen none in between), so
/// the machine is still said to be this tmux server and its session is listed once, not twice.
#[test]
fn a_machine_that_is_this_mac_is_known_again_after_the_server_restarts() {
    let _own = Owned::new();
    let mut far = FarSide::with_session("studio", "garden");
    far.server.write_config("[machines]\nstudio = \"studio\"\n");
    let stand_in = StandIn::new(&[&far]);
    let bin = stand_in.bin.display().to_string();
    let term = terminal(120);
    type_app_env(
        &term,
        &far.server,
        &[("TAW_SSH_BIN", &bin)],
        "app --child",
        &far.server.work_dir().join("err"),
        &far.server.work_dir().join("status"),
    );
    assert!(
        wait_for(
            || studio_is_this_server(&far.server),
            Duration::from_secs(20)
        ),
        "studio never said to be this tmux server:\n{}",
        terminal_screen(&term)
    );
    expect_terminal(&term, "no header over the one machine", |screen| {
        screen.contains(" taw app") && !screen.contains("studio") && screen.contains("garden")
    });
    let _ = the_app(&far.server);
    let screen = terminal_screen(&term);
    assert_eq!(
        screen.matches("garden").count(),
        1,
        "garden listed once before the restart:\n{screen}"
    );
    let calls = stand_in.calls().len();

    // The server goes, is seen to be gone (the new one starts two seconds later), and comes back with `garden`.
    far.server.crash_and_start_afresh(&ServerOptions {
        session: "garden".to_string(),
        ..ServerOptions::default()
    });

    // The app started the connections again with the new word: the stand-in is reached again.
    assert!(
        wait_for(|| stand_in.calls().len() > calls, Duration::from_secs(20)),
        "the app never reached for studio again ({calls} calls):\n{}",
        terminal_screen(&term)
    );
    assert!(
        wait_for(
            || studio_is_this_server(&far.server),
            Duration::from_secs(20)
        ),
        "studio never said to be this tmux server again:\n{}",
        terminal_screen(&term)
    );
    expect_terminal(
        &term,
        "garden once, no header over the one machine",
        |screen| screen.matches("garden").count() == 1 && !screen.contains("studio"),
    );
}

/// A machine's file, written before the app starts, is on its first frame: the machine's header and its
/// session are drawn from what the file knew, with no wait on any connection (the fixture's ssh refuses, so
/// none ever comes up), next to this Mac's own.
#[test]
fn a_machines_file_is_on_the_first_frame_without_waiting_for_its_connection() {
    let _own = Owned::new();
    let mac = Server::start();
    mac.write_config("[machines]\npi = \"pi\"\n");
    let dir = mac.state_dir().join("machines");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pi.json"),
        r#"{"name":"pi","state":{"spaces":[{"session_id":"$7","session_name":"garden",
            "session_path":"/home/pi","windows":1,"active_window_id":"@1"}]},
            "received_at":1790000000100,"last_seen":1790000000500,"live":false}"#,
    )
    .unwrap();
    let term = terminal(120);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "pi's session from its file", |screen| {
        screen.contains(" taw app")
            && screen.contains("this Mac")
            && screen.contains("garden")
            && screen.contains("one")
    });
    // The guard holds the app's pids from here (its child ends by the watchdog when the terminal's server goes).
    let _ = the_app(&mac);
    let screen = terminal_screen(&term);
    let header = screen
        .lines()
        .position(|line| line.contains("\u{25cf} pi"))
        .unwrap_or_else(|| panic!("pi's header:\n{screen}"));
    let session = screen
        .lines()
        .position(|line| line.contains("garden"))
        .unwrap();
    assert!(header < session, "garden is under pi's header:\n{screen}");
}

// ---------------------------------------------------------------- going in

/// `this Mac`'s tmux, as the app and the test's own look at it read it.
fn inner(mac: &Server) -> Tmux {
    Tmux::with_socket_name(mac.socket())
}

/// Binds F5 on "this Mac" the way the plugin binds the popup key: `run-shell "taw ui open"`, which is run
/// by the server for the client whose key it was.
fn bind_the_popup_key(mac: &Server) {
    let env: String = mac
        .taw_env()
        .iter()
        .map(|(name, value)| format!("'{name}={value}' "))
        .collect();
    mac.tmux_ok(&[
        "bind-key",
        "-n",
        "F5",
        "run-shell",
        &format!(
            "env -u TMUX -u TMUX_PANE {env}'{}' ui open >/dev/null 2>&1",
            taw_bin()
        ),
    ]);
}

/// A client of "this Mac" as it lists it: (process id, name, session name).
fn clients_of(mac: &Server) -> Vec<(u32, String, String)> {
    inner(mac)
        .run(&[
            "list-clients",
            "-F",
            "#{client_pid} #{client_name} #{session_name}",
        ])
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut words = line.split(' ');
            Some((
                words.next()?.parse().ok()?,
                words.next()?.to_string(),
                words.next()?.to_string(),
            ))
        })
        .collect()
}

/// Waits (20 s) for exactly one client of "this Mac", and returns it.
fn the_client(mac: &Server, what: &str) -> (u32, String, String) {
    let seen = wait_for(|| clients_of(mac).len() == 1, Duration::from_secs(20));
    assert!(
        seen,
        "{what}: no one client; clients are {:?}",
        clients_of(mac)
    );
    clients_of(mac).remove(0)
}

/// The parent of `pid`, from the real `ps`.
fn parent_of(pid: u32) -> u32 {
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("ps said no parent of {pid}"))
}

/// The record of the one app that runs, read as JSON.
fn the_record(mac: &Server) -> serde_json::Value {
    let (child, _) = the_app(mac);
    let text = std::fs::read_to_string(mac.state_dir().join("apps").join(format!("{child}.json")))
        .expect("the record reads");
    serde_json::from_str(&text).expect("the record is JSON")
}

/// Two sessions of "this Mac" with a word each on their pane, for the preview to show.
fn two_sessions(mac: &Server) {
    mac.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "beta",
        "-c",
        &mac.work_dir().display().to_string(),
    ]);
    mac.tmux_ok(&["send-keys", "-t", "one", "echo ONE_MARK", "Enter"]);
    mac.tmux_ok(&["send-keys", "-t", "beta", "echo BETA_MARK", "Enter"]);
}

/// Types `keys` (tmux key names) into the terminal.
fn press(term: &Server, keys: &[&str]) {
    let mut args = vec!["send-keys", "-t"];
    let pane = term.pane();
    args.push(&pane);
    args.extend_from_slice(keys);
    term.tmux_ok(&args);
}

/// Enter on a session row puts that session on the terminal: the inner status line is on the outer capture,
/// and the client of "this Mac" is a child of the app, which its record names while it lives.
#[test]
fn enter_on_a_row_puts_the_session_on_the_terminal_and_the_client_is_the_apps_child() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    let term = terminal(120);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| screen.contains("2 sessions"));
    // The first session in the list, whichever it is.
    press(&term, &["Enter"]);
    let (client_pid, _, session) = the_client(&mac, "after Enter");
    expect_terminal(&term, "the session's status line", |screen| {
        screen.contains(&format!("[{session}]"))
    });
    let (child, _) = the_app(&mac);
    assert_eq!(
        parent_of(client_pid),
        child,
        "the client is the app child's own child"
    );
    assert!(
        wait_for(
            || the_record(&mac)["client_pid"].as_u64() == Some(u64::from(client_pid)),
            Duration::from_secs(20)
        ),
        "the record never named the client {client_pid}: {}",
        the_record(&mac)
    );
    let record = the_record(&mac);
    assert!(
        record["client_tty"]
            .as_str()
            .is_some_and(|tty| tty.starts_with("/dev/")),
        "{record}"
    );
}

/// The app on a terminal over `mac` (two sessions, the popup key bound), after Enter on the first row, with the
/// client then moved by tmux's own `switch-client` to the other session: so "the pane you were in" and "the row you
/// went into" are different panes. Returns (the session gone into, the session the client is in now, the
/// client's name).
fn went_in_and_moved(mac: &Server, term: &Server, args: &str) -> (String, String, String) {
    went_in_and_moved_by(mac, term, args, type_app)
}

/// [`went_in_and_moved`] with the app typed by `typing` ([`type_app`] or [`type_app_held`]).
fn went_in_and_moved_by(
    mac: &Server,
    term: &Server,
    args: &str,
    typing: fn(&Server, &Server, &str, &Path, &Path),
) -> (String, String, String) {
    two_sessions(mac);
    bind_the_popup_key(mac);
    typing(
        term,
        mac,
        args,
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(term, "the list", |screen| screen.contains("2 sessions"));
    press(term, &["Enter"]);
    let (_, name, first) = the_client(mac, "after Enter");
    expect_terminal(term, "the session's status line", |screen| {
        screen.contains(&format!("[{first}]"))
    });
    // The guard holds the app's pids from here.
    let _ = the_app(mac);
    let other = if first == "one" { "beta" } else { "one" };
    mac.tmux_ok(&["switch-client", "-c", &name, "-t", other]);
    expect_terminal(term, "the other session's status line", |screen| {
        screen.contains(&format!("[{other}]"))
    });
    (first, other.to_string(), name)
}

/// The mark the preview shows for a session of [`two_sessions`].
fn mark_of(session: &str) -> &'static str {
    if session == "one" {
        "ONE_MARK"
    } else {
        "BETA_MARK"
    }
}

/// The popup key, pressed in a client the app started, returns the terminal to the app's list (no popup, the client
/// gone) with the cursor on the pane the client was in when the key was pressed, not on the row Enter went into.
#[test]
fn the_popup_key_returns_to_the_list_on_the_pane_you_were_in() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    let (first, other, _) = went_in_and_moved(&mac, &term, "app --child");
    let (child, _) = the_app(&mac);

    press(&term, &["F5"]);

    // The list is back, and its preview (which follows the cursor) is the other session's pane.
    expect_terminal(&term, "the list on the pane you were in", |screen| {
        screen.contains(" taw app")
            && screen.contains(mark_of(&other))
            && !screen.contains(mark_of(&first))
    });
    // tmux's exit line (`[detached (from session ...)]`) is not on the shell's screen behind the list: it showed
    // there until the list's first frame. That screen is read where the typed command is, so an empty read fails.
    let behind = term
        .tmux(&["capture-pane", "-a", "-p", "-J", "-t", &term.pane()])
        .unwrap_or_default();
    assert!(
        behind.contains("app --child"),
        "the shell's screen behind the list was not read:\n{behind}"
    );
    assert!(
        !behind.contains("[detached"),
        "tmux's exit line is on the shell's screen behind the list:\n{behind}"
    );
    assert!(
        wait_for(|| clients_of(&mac).is_empty(), Duration::from_secs(20)),
        "the client was not detached: {:?}",
        clients_of(&mac)
    );
    assert_eq!(the_app(&mac).0, child, "the same app, not a new one");
}

/// `taw app` started by a process with no controlling terminal, whose standard input is a terminal opened read only
/// and whose standard output is a file (`setsid taw app </dev/ttys004 >log`): the app draws on that terminal, so the
/// output it is given must be one that can be written. It used to be a duplicate of the read only standard input,
/// which `isatty` accepts and every write to fails, so the screen failed to start three times.
#[test]
fn a_terminal_that_is_only_read_on_standard_input_still_gets_the_app_drawn() {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    let _own = Owned::new();
    let mac = Server::start();
    let (mut master, mut slave) = (-1, -1);
    let size = libc::winsize {
        ws_row: 30,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `openpty` writes the two descriptors; the name and termios are its defaults.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &size as *const libc::winsize as *mut libc::winsize,
        )
    };
    assert_eq!(opened, 0, "no pseudo terminal");
    for fd in [master, slave] {
        // SAFETY: `fcntl` of descriptors made here: kept from the processes other tests start meanwhile.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    // SAFETY: both are open descriptors that only this test owns.
    let (master, slave) = unsafe {
        (
            std::fs::File::from_raw_fd(master),
            std::fs::File::from_raw_fd(slave),
        )
    };
    // SAFETY: `ttyname` of the slave just made, copied at once.
    let name = unsafe {
        std::ffi::CStr::from_ptr(libc::ttyname(slave.as_raw_fd()))
            .to_string_lossy()
            .into_owned()
    };
    let read_only = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOCTTY)
        .open(&name)
        .expect("the terminal opens read only");
    drop(slave);
    let err = mac.work_dir().join("err");
    let mut command = std::process::Command::new(taw_bin());
    command
        .arg("app")
        .envs(mac.taw_env())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("TERM", "xterm-256color")
        .stdin(std::process::Stdio::from(read_only))
        .stdout(std::process::Stdio::from(
            std::fs::File::create(mac.work_dir().join("out")).unwrap(),
        ))
        .stderr(std::process::Stdio::from(
            std::fs::File::create(&err).unwrap(),
        ));
    // SAFETY: between fork and exec only `setsid`, which is async-signal-safe: no controlling terminal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut app = command.spawn().expect("taw starts");
    let app_pid = app.id();
    own(app_pid);
    let mut drawn = Vec::new();
    let read_some = |drawn: &mut Vec<u8>| {
        let mut fds = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd of a descriptor this test holds, then a read of it into a buffer that is alive.
        unsafe {
            if libc::poll(&mut fds, 1, 100) > 0 {
                let mut chunk = [0u8; 4096];
                let n = libc::read(master.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len());
                if n > 0 {
                    drawn.extend_from_slice(&chunk[..n as usize]);
                }
            }
        }
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !String::from_utf8_lossy(&drawn).contains("taw app")
        && app.try_wait().ok().flatten().is_none()
        && std::time::Instant::now() < deadline
    {
        read_some(&mut drawn);
    }
    let said = read(&err).unwrap_or_default();
    assert!(
        String::from_utf8_lossy(&drawn).contains("taw app"),
        "the list was never drawn on the terminal; the app said {said:?}, the terminal got {:?}",
        String::from_utf8_lossy(&drawn)
    );
    // `q` ends the app, and the supervisor with it, 0.
    use std::io::Write;
    (&master).write_all(b"q").unwrap();
    let ended = wait_for(
        || {
            read_some(&mut drawn);
            app.try_wait().ok().flatten().is_some()
        },
        Duration::from_secs(20),
    );
    if !ended {
        let _ = app.kill();
    }
    let status = app.wait().expect("the app's end");
    assert!(ended, "the app did not end after q");
    assert_eq!(status.code(), Some(0), "the app said {:?}", read(&err));
}

/// Writes its file when dropped: a program that waits for the file is let go when a test fails
/// before it lets the program go itself.
struct Release(std::path::PathBuf);

impl Drop for Release {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "go");
    }
}

/// A person's own `detach-client -E '<program>'` binding, pressed in a client the app started, runs the program in
/// that client's process: its output shows on the terminal while it runs (it went into a pipe the app only reads
/// when the client has ended, so nothing showed), and more than a pipe's worth of it does not stop the program (it
/// blocked once the pipe was full). When the program ends the list is back.
#[test]
fn a_detach_client_e_binding_in_a_client_the_app_started_shows_its_programs_output_live() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    let term = terminal(120);
    let release = mac.work_dir().join("release");
    let finished = mac.work_dir().join("finished");
    let _let_go = Release(release.clone());
    let program = mac.work_dir().join("program.sh");
    // The marker first, then a wait for the file (bounded, 30 s), then 200000 bytes in lines of 99, then the
    // file that says the program got to its end.
    std::fs::write(
        &program,
        format!(
            "echo LIVE_MARKER\ni=0\nwhile [ ! -e '{}' ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done\n\
             head -c 200000 /dev/zero | tr '\\0' 'x' | fold -w 99\necho >'{}'\n",
            release.display(),
            finished.display()
        )
    )
    .unwrap();
    mac.tmux_ok(&[
        "bind-key",
        "-n",
        "F6",
        "detach-client",
        "-E",
        &format!("sh '{}'", program.display()),
    ]);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| screen.contains("2 sessions"));
    press(&term, &["Enter"]);
    let (_, _, first) = the_client(&mac, "after Enter");
    expect_terminal(&term, "the session's status line", |screen| {
        screen.contains(&format!("[{first}]"))
    });
    // The guard holds the app's pids from here.
    let (child, _) = the_app(&mac);

    press(&term, &["F6"]);

    expect_terminal(&term, "the program's first line", |screen| {
        screen.contains("LIVE_MARKER")
    });
    std::fs::write(&release, "go").unwrap();
    assert!(
        wait_for(|| finished.exists(), Duration::from_secs(20)),
        "the program never got to its end (it blocks once its output has nowhere to go):\n{}",
        terminal_screen(&term)
    );
    expect_terminal(&term, "the list, back", |screen| {
        screen.contains(" taw app") && screen.contains("2 sessions")
    });
    assert!(
        wait_for(|| clients_of(&mac).is_empty(), Duration::from_secs(20)),
        "the client did not end: {:?}",
        clients_of(&mac)
    );
    assert_eq!(the_app(&mac).0, child, "the same app, not a new one");
}

/// The `tmux` taw runs may be a version manager's shim (mise, asdf), which is one program when it starts and execs
/// the real tmux a moment later. That is no `detach-client -E` replacing the client: a plain detach prints tmux's
/// exit line, which is held back from the terminal as it is without a shim, and not copied to it like a program's
/// output.
#[test]
fn a_tmux_that_is_a_wrapper_at_first_still_shows_no_exit_line_on_a_plain_detach() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    let term = terminal(120);
    let real = Tmux::with_socket_name("unused").bin;
    let wrapper = mac.work_dir().join("tmux-wrapper");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n/bin/sleep 0.3\nexec '{}' \"$@\"\n",
            real.display()
        ),
    )
    .unwrap();
    crate::common::make_executable(&wrapper);
    type_app_env(
        &term,
        &mac,
        &[("TAW_TMUX_BIN", &wrapper.display().to_string())],
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| screen.contains("2 sessions"));
    press(&term, &["Enter"]);
    let (_, _, first) = the_client(&mac, "after Enter");
    expect_terminal(&term, "the session's status line", |screen| {
        screen.contains(&format!("[{first}]"))
    });
    // The guard holds the app's pids from here.
    let (child, _) = the_app(&mac);
    // Past a few looks of the app's, so that a program taken for the client's at the spawn has changed.
    std::thread::sleep(Duration::from_millis(1000));

    mac.tmux_ok(&["detach-client", "-s", &first]);

    // Positive wait first: the list is back, and the client is gone.
    expect_terminal(&term, "the list, back", |screen| {
        screen.contains(" taw app") && screen.contains("2 sessions")
    });
    assert!(
        wait_for(|| clients_of(&mac).is_empty(), Duration::from_secs(20)),
        "the client did not end: {:?}",
        clients_of(&mac)
    );
    let behind = term
        .tmux(&["capture-pane", "-a", "-p", "-J", "-t", &term.pane()])
        .unwrap_or_default();
    assert!(
        behind.contains("app --child"),
        "the shell's screen behind the list was not read:\n{behind}"
    );
    assert!(
        !behind.contains("[detached"),
        "tmux's exit line was copied to the terminal:\n{behind}"
    );
    assert_eq!(the_app(&mac).0, child, "the same app, not a new one");
}

/// A client that a `detach-client -E` replaces at once (a `client-attached` hook of the app's own server) is replaced
/// before the app's first look at it. The program's output still shows on the terminal while it runs, and the list is
/// back when it ends.
#[test]
fn a_program_that_replaces_the_client_before_the_apps_first_look_shows_its_output_live() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    let term = terminal(120);
    let release = mac.work_dir().join("release");
    let finished = mac.work_dir().join("finished");
    let _let_go = Release(release.clone());
    let program = mac.work_dir().join("program.sh");
    // The marker, then a wait for the file (bounded, 30 s), then the file that says the program got to its end.
    std::fs::write(
        &program,
        format!(
            "echo HOOK_MARKER\ni=0\nwhile [ ! -e '{}' ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done\n\
             echo >'{}'\n",
            release.display(),
            finished.display()
        ),
    )
    .unwrap();
    mac.tmux_ok(&[
        "set-hook",
        "-g",
        "client-attached",
        &format!("detach-client -E \"sh '{}'\"", program.display()),
    ]);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| screen.contains("2 sessions"));
    // The guard holds the app's pids from here.
    let (child, _) = the_app(&mac);

    press(&term, &["Enter"]);

    expect_terminal(&term, "the program's first line", |screen| {
        screen.contains("HOOK_MARKER")
    });
    std::fs::write(&release, "go").unwrap();
    assert!(
        wait_for(|| finished.exists(), Duration::from_secs(20)),
        "the program never got to its end:\n{}",
        terminal_screen(&term)
    );
    expect_terminal(&term, "the list, back", |screen| {
        screen.contains(" taw app") && screen.contains("2 sessions")
    });
    assert!(
        wait_for(|| clients_of(&mac).is_empty(), Duration::from_secs(20)),
        "the client did not end: {:?}",
        clients_of(&mac)
    );
    assert_eq!(the_app(&mac).0, child, "the same app, not a new one");
}

/// The terminal's main screen behind the app's own (`capture-pane -a`), joined into lines: where a program's output
/// lands when the app has its list on the alternate screen.
fn terminal_behind(term: &Server) -> String {
    term.tmux(&["capture-pane", "-a", "-p", "-J", "-t", &term.pane()])
        .unwrap_or_default()
}

/// What the terminal shows on its screen and on the one behind it.
fn terminal_everywhere(term: &Server) -> String {
    format!("{}\n{}", terminal_screen(term), terminal_behind(term))
}

/// A `lock-command` of the app's own server runs in the client's process, with the pipe for its standard output: what
/// it prints shows on the terminal while it runs. Once the lock is over a plain detach is tmux's exit line again and
/// not a program's: it is not shown (the line was copied for good by a rule that counted the client as replaced).
#[test]
fn a_lock_commands_output_shows_and_a_plain_detach_after_the_lock_shows_no_exit_line() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    let term = terminal(120);
    let done = mac.work_dir().join("lock-done");
    mac.tmux_ok(&[
        "set-option",
        "-g",
        "lock-command",
        &format!("printf LOCK_MARKER; sleep 2; touch '{}'", done.display()),
    ]);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| screen.contains("2 sessions"));
    press(&term, &["Enter"]);
    let (_, name, first) = the_client(&mac, "after Enter");
    expect_terminal(&term, "the session's status line", |screen| {
        screen.contains(&format!("[{first}]"))
    });
    // The guard holds the app's pids from here.
    let (child, _) = the_app(&mac);

    mac.tmux_ok(&["lock-client", "-t", &name]);

    assert!(
        wait_for(
            || terminal_everywhere(&term).contains("LOCK_MARKER"),
            Duration::from_secs(20)
        ),
        "the lock command's output never showed on the terminal:\n{}",
        terminal_everywhere(&term)
    );
    assert!(
        wait_for(|| done.exists(), Duration::from_secs(20)),
        "the lock command never ended"
    );
    // A little for the client to take the unlock and draw its session again.
    std::thread::sleep(Duration::from_millis(1000));
    mac.tmux_ok(&["detach-client", "-t", &name]);

    // Positive wait first: the list is back, and the client is gone.
    expect_terminal(&term, "the list, back", |screen| {
        screen.contains(" taw app") && screen.contains("2 sessions")
    });
    assert!(
        wait_for(|| clients_of(&mac).is_empty(), Duration::from_secs(20)),
        "the client did not end: {:?}",
        clients_of(&mac)
    );
    let everywhere = terminal_everywhere(&term);
    assert!(
        terminal_behind(&term).contains("app --child"),
        "the shell's screen behind the list was not read:\n{everywhere}"
    );
    assert!(
        !everywhere.contains("[detached"),
        "tmux's exit line reached the terminal:\n{everywhere}"
    );
    assert_eq!(the_app(&mac).0, child, "the same app, not a new one");
}

/// A `detach-client -E` program that runs for a while and writes its summary line as it ends (a program writing to a
/// pipe buffers it until then) has the line on the terminal. The app has its list on the alternate screen, so the line
/// is on the main screen behind it, which is read with `capture-pane -a`.
#[test]
fn a_programs_summary_written_as_it_ends_reaches_the_terminal() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    let term = terminal(120);
    mac.tmux_ok(&[
        "bind-key",
        "-n",
        "F6",
        "detach-client",
        "-E",
        "sleep 1; exec printf 'SUMMARY_MARKER 3 files\\n'",
    ]);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| screen.contains("2 sessions"));
    press(&term, &["Enter"]);
    let (_, _, first) = the_client(&mac, "after Enter");
    expect_terminal(&term, "the session's status line", |screen| {
        screen.contains(&format!("[{first}]"))
    });
    // The guard holds the app's pids from here.
    let (child, _) = the_app(&mac);

    press(&term, &["F6"]);

    // Positive waits first: the client has ended and the list is back.
    expect_terminal(&term, "the list, back", |screen| {
        screen.contains(" taw app") && screen.contains("2 sessions")
    });
    assert!(
        wait_for(|| clients_of(&mac).is_empty(), Duration::from_secs(20)),
        "the client did not end: {:?}",
        clients_of(&mac)
    );
    let everywhere = terminal_everywhere(&term);
    assert!(
        terminal_behind(&term).contains("app --child"),
        "the shell's screen behind the list was not read:\n{everywhere}"
    );
    assert!(
        terminal_behind(&term).contains("SUMMARY_MARKER 3 files"),
        "the program's summary never reached the terminal:\n{everywhere}"
    );
    assert_eq!(the_app(&mac).0, child, "the same app, not a new one");
}

/// A program of a `detach-client -E` that writes without a pause does not keep the app from seeing a signal: the
/// copy of its output to the terminal is one look's worth at a time, so the app ends the client and itself at once
/// (and not by its watchdog, two seconds later).
#[test]
fn a_program_that_never_stops_writing_does_not_keep_the_app_from_ending_on_a_signal() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    let term = terminal(120);
    let status = mac.work_dir().join("status");
    mac.tmux_ok(&["bind-key", "-n", "F6", "detach-client", "-E", "exec yes"]);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &status,
    );
    expect_terminal(&term, "the list", |screen| screen.contains("2 sessions"));
    press(&term, &["Enter"]);
    let (_, _, first) = the_client(&mac, "after Enter");
    expect_terminal(&term, "the session's status line", |screen| {
        screen.contains(&format!("[{first}]"))
    });
    let (child, _) = the_app(&mac);

    press(&term, &["F6"]);

    // Positive wait: the program's lines fill the terminal.
    expect_terminal(&term, "the program's output", |screen| {
        screen.lines().filter(|line| line.trim() == "y").count() > 10
    });
    signal(child, libc::SIGTERM);
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "the app did not end on the signal"
    );
    // Its own loop ended it: the watchdog, which ends a loop that is stuck two seconds after the signal, ends the
    // process with 1. A status, not a time, so a slow machine cannot fail this.
    assert_ne!(
        status_of(&status).as_deref(),
        Some("1\n"),
        "the watchdog ended the app: its loop was held by the copy of the program's output"
    );
}

/// After the last pane of the session you went into exits, the session and its client are gone: the pane you
/// came from is nowhere, so Back has nowhere to go in and one `q` ends the app (it used to say "that pane is
/// gone" and want a second press).
#[test]
fn after_the_last_pane_exits_one_q_ends_the_app() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    two_sessions(&mac);
    let status = mac.work_dir().join("status");
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &status,
    );
    expect_terminal(&term, "the list", |screen| screen.contains("2 sessions"));
    // The guard holds the app's pids from here (its child ends by the watchdog when the terminal's server goes).
    let _ = the_app(&mac);
    press(&term, &["Enter"]);
    let (_, _, session) = the_client(&mac, "after Enter");
    expect_terminal(&term, "the session's status line", |screen| {
        screen.contains(&format!("[{session}]"))
    });

    mac.tmux_ok(&["send-keys", "-t", &session, "exit", "Enter"]);

    expect_terminal(&term, "the list again", |screen| {
        screen.contains(" taw app") && screen.contains("1 session")
    });
    press(&term, &["q"]);
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "one q did not end the app:\n{}",
        terminal_screen(&term)
    );
    assert_eq!(status_of(&status).as_deref(), Some("0\n"));
}

/// Enter on a pane row (a window of a session that is not the one open there) puts the client on that window and
/// that pane, not on the window the session had open.
#[test]
fn enter_on_a_pane_row_puts_the_client_on_that_window_and_pane() {
    let _own = Owned::new();
    let mac = Server::start();
    let dir = mac.work_dir().display().to_string();
    mac.tmux_ok(&["new-session", "-d", "-s", "gamma", "-n", "wone", "-c", &dir]);
    mac.tmux_ok(&["new-window", "-t", "gamma", "-n", "wtwo"]);
    let wone_pane = mac
        .tmux_ok(&["display-message", "-p", "-t", "gamma:wone", "#{pane_id}"])
        .trim()
        .to_string();
    let term = terminal(120);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| screen.contains("2 sessions"));
    // The guard holds the app's pids from here (its child ends by the watchdog when the terminal's server goes).
    let _ = the_app(&mac);
    // `l` twice reaches the depth that lists every pane; gamma's header is the first row, and its first
    // pane the second.
    press(&term, &["l", "l"]);
    expect_terminal(&term, "the list with the pane rows", |screen| {
        screen.contains("wone") && screen.contains("wtwo")
    });

    press(&term, &["j", "Enter"]);

    let (_, name, session) = the_client(&mac, "after Enter");
    assert_eq!(session, "gamma");
    let at = |what: &str| {
        mac.tmux_ok(&["display-message", "-p", "-c", &name, what])
            .trim()
            .to_string()
    };
    assert_eq!(at("#{window_name}"), "wone", "the window of the row");
    assert_eq!(at("#{pane_id}"), wone_pane, "the pane of the row");
}

/// A session that is gone when Enter reaches it is a toast and nothing attached: the cursor stays and the app
/// lives. The app is stopped while the session goes and Enter is typed, so the key is read before the refresh
/// that would drop the row.
#[test]
fn enter_on_a_session_that_is_gone_says_so_and_the_app_lives() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    let term = terminal(120);
    type_app_held(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| screen.contains("2 sessions"));
    let (child, _) = the_app(&mac);
    // The first row is `beta`: it is the one that goes.
    signal(child, libc::SIGSTOP);
    mac.tmux_ok(&["kill-session", "-t", "beta"]);
    press(&term, &["Enter"]);
    signal(child, libc::SIGCONT);

    expect_terminal(&term, "the toast of a failed attach", |screen| {
        screen.contains("that session is gone")
    });
    assert!(
        !terminal_screen(&term).contains("ended the attach"),
        "tmux's own words are not what the person is told"
    );
    assert!(alive(child), "the app lives");
    assert!(clients_of(&mac).is_empty(), "nothing is attached");
    // Positive half: the app still answers, and `q` ends it.
    let status = mac.work_dir().join("status");
    press(&term, &["q"]);
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "the app did not end on q:\n{}",
        terminal_screen(&term)
    );
    assert_eq!(status_of(&status).as_deref(), Some("0\n"));
}

/// Back (Esc) on a pane that is gone says so, keeps the app, and forgets the pane: the next Esc leaves.
#[test]
fn back_on_a_pane_that_is_gone_says_so_and_the_next_back_leaves() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    let (_, other, _) = went_in_and_moved_by(&mac, &term, "app --child", type_app_held);
    press(&term, &["F5"]);
    expect_terminal(&term, "the list", |screen| screen.contains(" taw app"));
    assert!(
        wait_for(|| clients_of(&mac).is_empty(), Duration::from_secs(20)),
        "the client was not detached: {:?}",
        clients_of(&mac)
    );
    let (child, _) = the_app(&mac);

    // The pane the client was in goes while the app is stopped, and Esc is typed meanwhile.
    signal(child, libc::SIGSTOP);
    mac.tmux_ok(&["kill-session", "-t", &other]);
    press(&term, &["Escape"]);
    signal(child, libc::SIGCONT);

    expect_terminal(&term, "the gone pane's toast", |screen| {
        screen.contains("that pane is gone")
    });
    assert!(alive(child), "the app lives");
    assert!(clients_of(&mac).is_empty(), "nothing is attached");
    let status = mac.work_dir().join("status");
    press(&term, &["Escape"]);
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "the second Esc did not leave:\n{}",
        terminal_screen(&term)
    );
    assert_eq!(status_of(&status).as_deref(), Some("0\n"));
}

/// Esc, on the list the key returned to, goes back in at the pane the client was in.
#[test]
fn esc_goes_back_in_at_the_pane_you_were_in() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    let (_, other, _) = went_in_and_moved_by(&mac, &term, "app --child", type_app_held);
    press(&term, &["F5"]);
    expect_terminal(&term, "the list", |screen| screen.contains(" taw app"));
    assert!(
        wait_for(|| clients_of(&mac).is_empty(), Duration::from_secs(20)),
        "the client was not detached: {:?}",
        clients_of(&mac)
    );

    press(&term, &["Escape"]);

    let (_, _, session) = the_client(&mac, "after Esc");
    assert_eq!(session, other, "back in at the pane you were in");
    expect_terminal(&term, "the session's status line", |screen| {
        screen.contains(&format!("[{other}]"))
    });
}

/// A capital Q is Leave: on the list the key returned to (where Esc would go back in), it ends the app at once, with
/// no client put back on the terminal.
#[test]
fn capital_q_ends_the_app_at_once_where_esc_would_go_back_in() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    let (_, _, _) = went_in_and_moved(&mac, &term, "app --child");
    press(&term, &["F5"]);
    expect_terminal(&term, "the list", |screen| screen.contains(" taw app"));
    assert!(
        wait_for(|| clients_of(&mac).is_empty(), Duration::from_secs(20)),
        "the client was not detached: {:?}",
        clients_of(&mac)
    );

    press(&term, &["Q"]);

    let status = mac.work_dir().join("status");
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "the app did not end on Q:\n{}",
        terminal_screen(&term)
    );
    assert_eq!(status_of(&status).as_deref(), Some("0\n"));
    assert_eq!(clients_of(&mac), vec![], "no client was put back");
    assert_eq!(leftovers(&mac), Vec::<String>::new());
}

/// In the filter Q is a letter of the query, and Ctrl-C from the filter ends the app.
#[test]
fn q_is_text_in_the_filter_and_ctrl_c_from_the_filter_ends_the_app() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    let term = terminal(120);
    let status = mac.work_dir().join("status");
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &status,
    );
    expect_terminal(&term, "the list", |screen| screen.contains("2 sessions"));
    // The guard holds the app's pids from here (its child ends by the watchdog when the terminal's server goes).
    let _ = the_app(&mac);
    press(&term, &["/", "Q"]);
    expect_terminal(&term, "Q in the query", |screen| {
        screen
            .lines()
            .any(|line| line.contains("/Q") || line.contains("/ Q"))
    });
    assert_eq!(
        status_of(&status),
        None,
        "Q in the filter did not end the app"
    );
    press(&term, &["C-c"]);
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "the app did not end on Ctrl-C from the filter:\n{}",
        terminal_screen(&term)
    );
    assert_eq!(status_of(&status).as_deref(), Some("0\n"));
}

// ---------------------------------------------------------------- the supervisor

/// The pid in a record's file name, for the records of `mac`.
fn pids(mac: &Server) -> Vec<u32> {
    records(mac)
        .iter()
        .filter_map(|name| name.trim_end_matches(".json").parse().ok())
        .collect()
}

/// A read that fails for a while is read again; a process that is gone has no start time; a live one whose
/// read never gives one is a panic and not an empty record.
#[test]
fn the_guard_reads_the_start_time_again_and_is_loud_when_it_stays_empty() {
    let mut reads = vec!["", "", "Tue Oct 20 12:00:01 2026"].into_iter();
    let got = start_time_when_alive(
        7,
        |_| reads.next().expect("no more reads").to_string(),
        |_| true,
        Duration::from_millis(1),
        Duration::from_secs(5),
    );
    assert_eq!(got, "Tue Oct 20 12:00:01 2026", "read again until it says");
    let gone = start_time_when_alive(
        7,
        |_| String::new(),
        |_| false,
        Duration::from_millis(1),
        Duration::from_secs(5),
    );
    assert_eq!(gone, "", "a process that is gone needs no record");
    let loud = std::panic::catch_unwind(|| {
        start_time_when_alive(
            std::process::id(),
            |_| String::new(),
            |_| true,
            Duration::from_millis(1),
            Duration::from_millis(30),
        )
    });
    assert!(loud.is_err(), "a live process that reads empty is a panic");
}

/// The guard ends a process only when it is the one that was recorded: the same start time, and this
/// build's `taw`. A pid given to another process since, an empty record and another program are left alone.
#[test]
fn the_guard_ends_only_the_process_that_was_recorded() {
    let start = "Tue Oct 20 12:00:01 2026";
    let taw = "/w/target/debug/taw";
    let args = "/w/target/debug/taw app --child";
    assert!(is_the_recorded_process(start, start, args, taw));
    assert!(
        !is_the_recorded_process(start, "Tue Oct 20 12:07:44 2026", args, taw),
        "a pid that another process has now"
    );
    assert!(
        !is_the_recorded_process("", "", args, taw),
        "a process that was already gone when it was recorded"
    );
    assert!(
        !is_the_recorded_process(start, start, "-bash", taw),
        "the pane's shell is not taw"
    );
    // A process that is not taw, owned with the word its command line holds: the same start time and the word
    // end it, the same start time with other args does not.
    let sleeper = "/bin/sleep 600";
    assert!(is_the_recorded_process(start, start, sleeper, "sleep 600"));
    assert!(
        !is_the_recorded_process(start, start, args, "sleep 600"),
        "a taw is not the sleep that was recorded"
    );
    assert!(
        !is_the_recorded_process(start, start, sleeper, taw),
        "a sleep is not the taw that was recorded"
    );
    // The reads of a live process agree with themselves, and a pid that is not one reads as empty.
    let me = std::process::id();
    assert!(!start_time_of(me).is_empty());
    assert_eq!(start_time_of(me), start_time_of(me));
    assert!(args_of(me).contains("app_local"), "{}", args_of(me));
    assert_eq!(start_time_of(4_000_000_000), "");
}

/// A records folder read while the app is between creating its record's temporary file and renaming it holds
/// the temporary name, which is no record: only the final `.json` name is listed. The positive half is the
/// final name, which is kept.
#[test]
fn a_records_folder_lists_only_whole_json_files() {
    let names = |list: &[&str]| list.iter().map(|name| name.to_string()).collect::<Vec<_>>();
    assert_eq!(
        record_names(names(&["4242.json"])),
        names(&["4242.json"]),
        "a final name is a record"
    );
    assert_eq!(
        record_names(names(&[".4242.json.4242.0.tmp"])),
        Vec::<String>::new(),
        "a temporary name alone is not"
    );
    assert_eq!(
        record_names(names(&[
            ".4242.json.4242.0.tmp",
            "4242.json",
            "notes.txt",
            ".7.json"
        ])),
        names(&["4242.json"]),
        "a temporary name beside a final one, another file and a dot file named .json"
    );
}

/// The guard ends a process that is not `taw` when it was owned with the word its command line holds, and
/// leaves it alone when it was owned as a `taw`: the positive half is the owned sleep, which is ended.
#[test]
fn the_guard_ends_a_process_that_is_not_taw_when_owned_by_its_command_line() {
    let mut sleeper = Reaped(
        std::process::Command::new("/bin/sleep")
            .arg("600")
            .spawn()
            .expect("sleep runs"),
    );
    let mut bystander = Reaped(
        std::process::Command::new("/bin/sleep")
            .arg("601")
            .spawn()
            .expect("sleep runs"),
    );
    {
        let _own = Owned::new();
        own_as(sleeper.id(), "sleep 600");
        // Owned as a taw, which it is not: the guard must leave it alone.
        own(bystander.id());
    }
    let until = std::time::Instant::now() + Duration::from_secs(20);
    let ended = loop {
        if let Some(status) = sleeper.try_wait().expect("wait") {
            break Some(status);
        }
        if std::time::Instant::now() > until {
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        ended.is_some(),
        "the sleep owned by its command line still ran after the guard"
    );
    // The negative half comes after the positive one and a moment more: a guard that ended every process it owned,
    // whatever its command line, would have ended the bystander in the same pass as the sleep.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        bystander.try_wait().expect("wait").is_none(),
        "the guard ended a process that is not a taw"
    );
}

/// A child the test started, ended and waited for when it goes out of scope, so a failing assert leaves nothing running.
struct Reaped(std::process::Child);

impl std::ops::Deref for Reaped {
    type Target = std::process::Child;
    fn deref(&self) -> &std::process::Child {
        &self.0
    }
}

impl std::ops::DerefMut for Reaped {
    fn deref_mut(&mut self) -> &mut std::process::Child {
        &mut self.0
    }
}

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The `ps` line of `pid` (pid, parent, state, command), or what went wrong, for a failure to say.
fn ps_line(pid: u32) -> String {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "pid=,ppid=,stat=,args="])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The state letters of `pid` as `ps` says them (`T` is stopped), or empty when there is no such process.
fn stat_of(pid: u32) -> String {
    std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The one record of the app that runs, as (child pid, supervisor pid), once there is one.
fn the_app(mac: &Server) -> (u32, u32) {
    assert!(
        wait_for(|| pids(mac).len() == 1, Duration::from_secs(20)),
        "no record of one app: {:?}",
        records(mac)
    );
    let child = pids(mac)[0];
    let json = std::fs::read_to_string(mac.state_dir().join("apps").join(format!("{child}.json")))
        .expect("the record reads");
    let value: serde_json::Value = serde_json::from_str(&json).expect("the record is JSON");
    let supervisor = value["supervisor"].as_u64().expect("a supervisor") as u32;
    own(child);
    own(supervisor);
    (child, supervisor)
}

/// The supervisor starts the screen and `q` on it ends both, 0.
#[test]
fn the_supervisor_starts_the_screen_and_ends_with_it() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    let status = mac.work_dir().join("status");
    type_app(&term, &mac, "app", &mac.work_dir().join("err"), &status);
    expect_terminal(&term, "the app's list", |screen| {
        screen.contains(" taw app")
    });
    let (child, supervisor) = the_app(&mac);
    assert_ne!(child, supervisor, "two processes");
    assert!(alive(supervisor));
    term.tmux_ok(&["send-keys", "-t", &term.pane(), "q"]);
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "the supervisor did not end:\n{}",
        terminal_screen(&term)
    );
    assert_eq!(status_of(&status).as_deref(), Some("0\n"));
}

/// The child killed by its pid: the supervisor starts another on the same terminal, and the list is back.
#[test]
fn a_child_that_is_killed_is_started_again() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    let status = mac.work_dir().join("status");
    type_app(&term, &mac, "app", &mac.work_dir().join("err"), &status);
    let (child, supervisor) = the_app(&mac);
    signal(child, libc::SIGKILL);
    assert!(
        wait_for(
            || pids(&mac).iter().any(|pid| *pid != child),
            Duration::from_secs(20)
        ),
        "no new child after {child} was killed: {:?}",
        records(&mac)
    );
    let (again, same) = the_app(&mac);
    assert_ne!(again, child);
    assert_eq!(same, supervisor, "the same supervisor started it");
    // The killed child's frame stays on the screen, so the screen proves nothing about the new child; `q`
    // does: only a child that reads the keyboard ends the supervisor 0.
    term.tmux_ok(&["send-keys", "-t", &term.pane(), "q"]);
    assert!(wait_for(
        || status_of(&status).is_some(),
        Duration::from_secs(20)
    ));
    assert_eq!(status_of(&status).as_deref(), Some("0\n"));
}

/// Whether the terminal's pane shows the alternate screen (`#{alternate_on}`), or what tmux said.
fn alternate_on(term: &Server) -> String {
    term.tmux(&[
        "display-message",
        "-p",
        "-t",
        &term.pane(),
        "#{alternate_on}",
    ])
    .map(|said| said.trim().to_string())
    .unwrap_or_default()
}

/// Whether the terminal's pane shows its cursor (`#{cursor_flag}`: 1 when it is visible), or what tmux said.
fn cursor_flag(term: &Server) -> String {
    term.tmux(&[
        "display-message",
        "-p",
        "-t",
        &term.pane(),
        "#{cursor_flag}",
    ])
    .map(|said| said.trim().to_string())
    .unwrap_or_default()
}

/// The supervisor sent `sig` by its pid: the child goes too, the terminal's program ends with 128 and the
/// signal, the child's record goes with it, the terminal is put away (not on the alternate screen), and the
/// sessions of this Mac are all still there.
fn a_supervisor_is_signalled(sig: libc::c_int, ends: &str) {
    let mac = Server::start();
    mac.tmux_ok(&["new-session", "-d", "-s", "alpha"]);
    let term = terminal(120);
    let status = mac.work_dir().join("status");
    type_app(&term, &mac, "app", &mac.work_dir().join("err"), &status);
    expect_terminal(&term, "the app's list", |screen| {
        screen.contains(" taw app")
    });
    assert_eq!(
        alternate_on(&term),
        "1",
        "the app is on the alternate screen"
    );
    let (child, supervisor) = the_app(&mac);
    signal(supervisor, sig);
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "the supervisor did not end:\n{}",
        terminal_screen(&term)
    );
    assert_eq!(status_of(&status).as_deref(), Some(ends));
    assert!(!alive(child), "the child is gone with it");
    disown(child);
    assert_eq!(
        leftovers(&mac),
        Vec::<String>::new(),
        "no record of the child is left"
    );
    assert!(
        wait_for(|| alternate_on(&term) == "0", Duration::from_secs(20)),
        "the terminal was left on the alternate screen"
    );
    let sessions = mac.tmux_ok(&["list-sessions", "-F", "#{session_name}"]);
    assert!(
        sessions.contains("alpha") && sessions.contains("one"),
        "{sessions}"
    );
}

#[test]
fn a_supervisor_that_is_terminated_takes_its_child_and_leaves_the_sessions() {
    let _own = Owned::new();
    a_supervisor_is_signalled(libc::SIGTERM, "143\n");
}

/// The twin for the hang-up a closing terminal sends: 128 and 1.
#[test]
fn a_supervisor_that_is_hung_up_takes_its_child_and_leaves_the_sessions() {
    let _own = Owned::new();
    a_supervisor_is_signalled(libc::SIGHUP, "129\n");
}

/// A child whose supervisor was killed by its pid (no signal reaches the child) ends by itself, though
/// nothing tells it: it sees that it has no supervisor now.
#[test]
fn a_child_whose_supervisor_was_killed_ends_by_itself() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    type_app(
        &term,
        &mac,
        "app",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the app's list", |screen| {
        screen.contains(" taw app")
    });
    let (child, supervisor) = the_app(&mac);
    assert!(alive(child) && alive(supervisor), "both run");

    signal(supervisor, libc::SIGKILL);

    assert!(
        wait_for(|| !alive(child), Duration::from_secs(20)),
        "the child of the killed supervisor {supervisor} still runs: {}",
        ps_line(child)
    );
    disown(child);
}

/// A child whose supervisor was killed by its pid while a client of the app is attached ends too. The
/// loop is inside the wait for the client then and looks at no parent, so the wait itself looks at it (it is not
/// the one the child started under), ends the client and the app, and the screen is not taken back over it.
#[test]
fn a_child_whose_supervisor_was_killed_while_a_client_is_attached_ends_by_itself() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    went_in_and_moved(&mac, &term, "app");
    let (child, supervisor) = the_app(&mac);
    assert!(alive(child) && alive(supervisor), "both run");
    // Positive half: a client is attached, so the child is inside its wait for it.
    assert_eq!(
        clients_of(&mac).len(),
        1,
        "the app's client is attached: {:?}",
        clients_of(&mac)
    );
    let client = clients_of(&mac)[0].0;
    own(client);

    signal(supervisor, libc::SIGKILL);

    assert!(
        wait_for(|| !alive(child), Duration::from_secs(20)),
        "the child of the killed supervisor {supervisor} still runs with a client attached: {}",
        ps_line(child)
    );
    disown(child);
    // The app's wait told the client to end, so the shell and a live client do not share the
    // terminal.
    assert!(
        wait_for(
            || !alive(client) && clients_of(&mac).is_empty(),
            Duration::from_secs(20)
        ),
        "the client {client} of the app that ended is still there: {}; clients {:?}",
        ps_line(client),
        clients_of(&mac)
    );
    disown(client);
    // The client put the terminal back as it went, and the app did not take the screen again after it:
    // the pane is not on the alternate screen (an app that came back to its screen reads 1 here).
    assert!(
        wait_for(|| alternate_on(&term) == "0", Duration::from_secs(20)),
        "the outer pane is still on the alternate screen after the client went: alternate_on {:?}",
        alternate_on(&term)
    );
    // This sleep follows the positive wait above: the client's own exit puts the pane off the alternate screen
    // for a moment, and a screen taken back over it 150 ms later would read 1 again, which a read taken at once
    // could miss.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        alternate_on(&term),
        "0",
        "the app took its screen back after the client went"
    );
}

/// The twin of the test above, going in through Esc (the Back arm) and not through Enter. After
/// the client ended for the gone supervisor, the loop's next turn must not draw a frame on the shell's screen
/// (a frame also writes the sequence that hides the cursor). The cursor itself reads shown at the end either
/// way, since ratatui's terminal shows it again as it is dropped; the frame's cells are what a draw leaves.
#[test]
fn a_child_whose_supervisor_was_killed_after_esc_went_back_in_leaves_the_shells_screen_alone() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    went_in_and_moved(&mac, &term, "app");
    let (child, supervisor) = the_app(&mac);
    press(&term, &["F5"]);
    expect_terminal(&term, "the list", |screen| screen.contains(" taw app"));
    assert!(
        wait_for(|| clients_of(&mac).is_empty(), Duration::from_secs(20)),
        "the client was not detached: {:?}",
        clients_of(&mac)
    );

    // The Back arm goes in again, where you were.
    press(&term, &["Escape"]);
    let client = the_client(&mac, "after Esc").0;
    own(client);
    // The pane's size changes while the client has the terminal: a frame drawn after the end then redraws
    // everything (ratatui clears and draws again when the size is not the one it drew at), so a frame drawn
    // on the shell's screen is on the pane's screen to see.
    term.tmux_ok(&["resize-window", "-t", &term.pane(), "-x", "100", "-y", "24"]);

    signal(supervisor, libc::SIGKILL);

    assert!(
        wait_for(
            || !alive(child) && !alive(client) && clients_of(&mac).is_empty(),
            Duration::from_secs(20)
        ),
        "the child {child} and its client {client} did not end: {} / {}; clients {:?}\n{}",
        ps_line(child),
        ps_line(client),
        clients_of(&mac),
        terminal_screen(&term)
    );
    disown(child);
    disown(client);
    assert!(
        wait_for(
            || cursor_flag(&term) == "1" && alternate_on(&term) == "0",
            Duration::from_secs(20)
        ),
        "the outer pane's cursor is hidden or it is on the alternate screen: cursor_flag {:?}, alternate_on {:?}\n{}",
        cursor_flag(&term),
        alternate_on(&term),
        terminal_screen(&term)
    );
    // Positive half of the check below: the shell has the terminal again and wrote the status.
    let status = mac.work_dir().join("status");
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "the supervisor's end was not seen by the shell:\n{}",
        terminal_screen(&term)
    );
    // This sleep follows the positive waits above: a frame drawn after the end (or the app taking its screen
    // again) would show up within a loop turn, which a read taken at once could miss.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(cursor_flag(&term), "1", "the cursor was hidden afterwards");
    assert_eq!(alternate_on(&term), "0", "the alternate screen came back");
    assert!(
        !terminal_screen(&term).contains(" taw app  \u{b7}"),
        "the app drew a frame on the shell's screen after its client ended:\n{}",
        terminal_screen(&term)
    );
}

/// A child whose supervisor was killed by its pid ends through its own exit, which puts the screen back:
/// the terminal shows the shell and no longer the app's frame (an end by the watchdog alone leaves the
/// alternate screen as it was).
#[test]
fn a_child_whose_supervisor_was_killed_puts_the_terminal_back() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    type_app(
        &term,
        &mac,
        "app",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the app's list", |screen| {
        screen.contains(" taw app  \u{b7}")
    });
    // The row the check below looks for is on the screen before the supervisor goes, so the check cannot
    // pass with nothing to check.
    expect_terminal(&term, "the app's first row", |screen| {
        screen.contains("\u{2502}1   one")
    });
    let (child, supervisor) = the_app(&mac);

    signal(supervisor, libc::SIGKILL);

    assert!(
        wait_for(|| !alive(child), Duration::from_secs(20)),
        "the child of the killed supervisor {supervisor} still runs: {}",
        ps_line(child)
    );
    disown(child);
    // The positive half is the child's end; its screen is put back before the process goes, so the frame is
    // gone from the terminal by now (a short wait for tmux to show it). The shell's prompt may be drawn over
    // the top line, so the frame is told by its tree row.
    assert!(
        wait_for(
            || !terminal_screen(&term).contains("\u{2502}1   one"),
            Duration::from_secs(20)
        ),
        "the app's frame is still on the terminal after the child ended:\n{}",
        terminal_screen(&term)
    );
}

/// A loop that cannot reach its look in time (every tmux call takes 3 s, and a tick makes several) is
/// ended by the watchdog after its grace, which puts the terminal away itself: the alternate screen is left and
/// the frame is gone, though the loop never got to do it.
#[test]
fn a_loop_too_slow_to_look_has_its_terminal_put_back_by_the_watchdog() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    let flag = mac.work_dir().join("stall-now");
    let stalled = mac.work_dir().join("stalled");
    let slow = mac.work_dir().join("slow-tmux");
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    common::write_script(
        &slow,
        &format!(
            "if [ -e '{}' ]; then echo stall >> '{}'; sleep 3; fi\nexec '{}' \"$@\"\n",
            flag.display(),
            stalled.display(),
            real.display()
        ),
    );
    type_app_env(
        &term,
        &mac,
        &[("TAW_TMUX_BIN", &slow.display().to_string())],
        "app",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the app's first row", |screen| {
        screen.contains("\u{2502}1   one")
    });
    assert_eq!(
        alternate_on(&term),
        "1",
        "the app is on the alternate screen"
    );
    let (child, supervisor) = the_app(&mac);

    // From now on every tmux call of the app takes 3 s; the supervisor goes while one is under way.
    std::fs::write(&flag, "").unwrap();
    assert!(
        wait_for(|| read(&stalled).is_some(), Duration::from_secs(20)),
        "the app never made a stalled call: {}",
        ps_line(child)
    );
    signal(supervisor, libc::SIGKILL);

    assert!(
        wait_for(|| !alive(child), Duration::from_secs(20)),
        "the child of the killed supervisor {supervisor} still runs: {}",
        ps_line(child)
    );
    disown(child);
    assert!(
        wait_for(|| alternate_on(&term) == "0", Duration::from_secs(20)),
        "the terminal was left on the alternate screen:\n{}",
        terminal_screen(&term)
    );
    assert!(
        wait_for(
            || !terminal_screen(&term).contains("\u{2502}1   one"),
            Duration::from_secs(20)
        ),
        "the app's frame is still on the terminal:\n{}",
        terminal_screen(&term)
    );
}

/// The terminal going away (its server ends, the pane's tty is hung up) while the supervisor lives ends
/// the child and the supervisor, though the child's read of the tty never comes back.
#[test]
fn a_child_whose_terminal_is_gone_ends_and_so_does_its_supervisor() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    type_app(
        &term,
        &mac,
        "app",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the app's list", |screen| {
        screen.contains(" taw app")
    });
    let (child, supervisor) = the_app(&mac);
    assert!(alive(child) && alive(supervisor), "both run");

    let _ = term.tmux(&["kill-server"]);

    assert!(
        wait_for(
            || !alive(child) && !alive(supervisor),
            Duration::from_secs(20)
        ),
        "after the terminal went: child {}, supervisor {}",
        ps_line(child),
        ps_line(supervisor)
    );
}

// `revoke(2)`, which only macOS and the BSDs have: every open descriptor of the device at `path` is hung up, as
// a closed window does to a terminal.
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
unsafe extern "C" {
    fn revoke(path: *const libc::c_char) -> libc::c_int;
}

/// A terminal that was revoked, with no signal sent to anything, ends the child by the watchdog's own look
/// at it (the shell is blocked waiting for the supervisor and does not notice, and the child's read of the tty
/// never comes back), and the supervisor gives up after its three starts with no terminal. Only the probe of the
/// terminal can end the child here: with it off the child stays.
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
#[test]
fn a_child_whose_terminal_was_revoked_ends_and_so_does_its_supervisor() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    type_app(
        &term,
        &mac,
        "app",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the app's list", |screen| {
        screen.contains(" taw app")
    });
    let (child, supervisor) = the_app(&mac);
    assert!(alive(child) && alive(supervisor), "both run");
    // The pty is the test's own: the user that runs it owns it.
    let tty = term.tmux_ok(&["display-message", "-p", "-t", &term.pane(), "#{pane_tty}"]);
    let tty = std::ffi::CString::new(tty.trim()).expect("a tty path");
    // SAFETY: `revoke` of the device path tmux named for the pane of this test's own terminal server.
    assert_eq!(
        unsafe { revoke(tty.as_ptr()) },
        0,
        "revoke of {tty:?} failed"
    );

    assert!(
        wait_for(
            || !alive(child) && !alive(supervisor),
            Duration::from_secs(20)
        ),
        "after the terminal was revoked: child {}, supervisor {}",
        ps_line(child),
        ps_line(supervisor)
    );
}

/// A child that cannot start (here the debug build's test seam makes it say so and end 1 at once) is tried
/// three times and then given up on, with the log named; the log holds what the child said, and a log past
/// 256 KiB starts again from empty. The app runs on the terminal, so only the child fails.
#[test]
fn a_child_that_keeps_failing_is_given_up_on_and_its_words_are_in_app_log() {
    let mac = Server::start();
    let term = terminal(120);
    let logs = mac.state_dir().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join("app.log"), vec![b'x'; 300 * 1024]).unwrap();
    let err = mac.work_dir().join("err");
    let status = mac.work_dir().join("status");
    // The child alone ends 1 at once, every time; the supervisor runs on this terminal.
    type_app_env(
        &term,
        &mac,
        &[("TAW_TEST_APP_CHILD_FAILS", "1")],
        "app",
        &err,
        &status,
    );
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "the supervisor did not give up:\n{}",
        terminal_screen(&term)
    );
    assert_eq!(status_of(&status).as_deref(), Some("1\n"));
    let said = read(&err).unwrap_or_default();
    let log = logs.join("app.log");
    assert_eq!(
        said,
        format!(
            "taw app: the app keeps failing; its log is {}\n",
            log.display()
        )
    );
    let written = std::fs::read_to_string(&log).expect("app.log exists");
    assert_eq!(
        written.matches("taw app: ").count(),
        3,
        "three children said their words, no more and no fewer:\n{written}"
    );
    assert!(
        written.len() < 10_000,
        "the old 300 KiB went: {} bytes",
        written.len()
    );
}

/// The app child with no terminal says so at once, as one plain line, and draws nothing: crossterm would
/// open `/dev/tty` and go on, so the child itself looks at its standard input and output first.
#[test]
fn the_app_child_without_a_terminal_says_so_and_ends_1_without_drawing() {
    let mac = Server::start();
    let mut child = std::process::Command::new(taw_bin())
        .args(["app", "--child"])
        .envs(mac.taw_env())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("taw starts");
    let ended = wait_for(
        || child.try_wait().ok().flatten().is_some(),
        Duration::from_secs(5),
    );
    if !ended {
        let _ = child.kill();
    }
    let out = child.wait_with_output().expect("the child's words");
    assert!(ended, "the child did not end within 5 s");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "taw app: the app needs a terminal\n"
    );
    let drawn = String::from_utf8_lossy(&out.stdout);
    assert!(
        !drawn.contains("\u{1b}[?1049h"),
        "it entered the alternate screen: {drawn:?}"
    );
    assert!(drawn.is_empty(), "it wrote to its stdout: {drawn:?}");
}

/// `taw app` with no terminal (a script, `ssh` with no `-t`) says it needs one at once, in one line, and
/// starts no child: it used to start the app three times and report that it "keeps failing".
#[test]
fn taw_app_without_a_terminal_says_it_needs_one_at_once() {
    let mac = Server::start();
    let mut app = std::process::Command::new(taw_bin())
        .arg("app")
        .envs(mac.taw_env())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("taw starts");
    let ended = wait_for(
        || app.try_wait().ok().flatten().is_some(),
        Duration::from_secs(5),
    );
    if !ended {
        let _ = app.kill();
    }
    let out = app.wait_with_output().expect("the app's words");
    assert!(ended, "taw app did not end within 5 s");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "taw app: the app needs a terminal\n"
    );
    assert!(out.stdout.is_empty(), "it wrote to its stdout");
}

/// After the child ends badly the supervisor waits for the client the record names before it starts another:
/// a process the test owns stands in for the client, and the new child starts only after it ends.
#[test]
fn the_supervisor_waits_for_the_client_to_end_before_it_starts_the_app_again() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    let status = mac.work_dir().join("status");
    type_app(&term, &mac, "app", &mac.work_dir().join("err"), &status);
    let (child, _) = the_app(&mac);
    let mut client = std::process::Command::new("/bin/sleep")
        .arg("120")
        .spawn()
        .unwrap();
    let record = mac.state_dir().join("apps").join(format!("{child}.json"));
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record).unwrap()).unwrap();
    value["client_pid"] = serde_json::json!(client.id());
    std::fs::write(&record, serde_json::to_string(&value).unwrap()).unwrap();

    signal(child, libc::SIGKILL);
    assert!(
        wait_for(|| !alive(child), Duration::from_secs(20)),
        "the child did not end"
    );
    disown(child);
    // Long enough for a supervisor that does not wait to have started the next one.
    std::thread::sleep(Duration::from_millis(1500));
    let ended = taw::machine::files::now_ms();
    client.kill().unwrap();
    client.wait().unwrap();

    assert!(
        wait_for(
            || pids(&mac).iter().any(|pid| *pid != child),
            Duration::from_secs(20)
        ),
        "no new child after the client ended: {:?}",
        records(&mac)
    );
    let (again, _) = the_app(&mac);
    let json = std::fs::read_to_string(mac.state_dir().join("apps").join(format!("{again}.json")))
        .unwrap();
    let started: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        started["started_at"].as_u64().unwrap() >= ended,
        "the new child started before the client ended: {json}"
    );
    term.tmux_ok(&["send-keys", "-t", &term.pane(), "q"]);
    assert!(wait_for(
        || status_of(&status).is_some(),
        Duration::from_secs(20)
    ));
}

/// A client pid that is no pid (above `i32::MAX`) is not waited for: the process check takes such a number
/// for a process that is there, and the supervisor would wait for ever.
#[test]
fn a_client_pid_above_any_pid_does_not_make_the_supervisor_wait() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    let status = mac.work_dir().join("status");
    type_app(&term, &mac, "app", &mac.work_dir().join("err"), &status);
    let (child, _) = the_app(&mac);
    let record = mac.state_dir().join("apps").join(format!("{child}.json"));
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record).unwrap()).unwrap();
    value["client_pid"] = serde_json::json!(4_294_967_295u64);
    std::fs::write(&record, serde_json::to_string(&value).unwrap()).unwrap();

    signal(child, libc::SIGKILL);

    assert!(
        wait_for(
            || pids(&mac).iter().any(|pid| *pid != child),
            Duration::from_secs(20)
        ),
        "no new child after {child} was killed: {:?}",
        records(&mac)
    );
    term.tmux_ok(&["send-keys", "-t", &term.pane(), "q"]);
    assert!(wait_for(
        || status_of(&status).is_some(),
        Duration::from_secs(20)
    ));
    assert_eq!(status_of(&status).as_deref(), Some("0\n"));
}

/// A stray `SIGINT`, `SIGQUIT` or `SIGTSTP` never ends or stops the terminal's program: after all three the
/// supervisor is still the one that ends 0 when the app is quit.
#[test]
fn the_supervisor_ignores_interrupt_quit_and_stop() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    let status = mac.work_dir().join("status");
    type_app(&term, &mac, "app", &mac.work_dir().join("err"), &status);
    let (child, supervisor) = the_app(&mac);
    for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP] {
        signal(supervisor, sig);
    }
    term.tmux_ok(&["send-keys", "-t", &term.pane(), "q"]);
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "the supervisor (or the app {child}) did not end on q:\n{}",
        terminal_screen(&term)
    );
    assert_eq!(status_of(&status).as_deref(), Some("0\n"));
}

/// The child gets `SIGINT` back as any program does: a `SIGINT` sent to it by its pid ends it, and the
/// supervisor, which ignores it, starts the list again. (A child that inherited the supervisor's ignoring
/// would go on, and Ctrl-C typed at it would never reach it.)
#[test]
fn a_child_dies_of_sigint_and_the_supervisor_starts_another() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    let status = mac.work_dir().join("status");
    type_app(&term, &mac, "app", &mac.work_dir().join("err"), &status);
    let (child, supervisor) = the_app(&mac);

    signal(child, libc::SIGINT);

    assert!(
        wait_for(
            || pids(&mac).iter().any(|pid| *pid != child),
            Duration::from_secs(20)
        ),
        "no new child after {child} was sent SIGINT: {:?}",
        records(&mac)
    );
    let (again, same) = the_app(&mac);
    assert_ne!(again, child);
    assert_eq!(same, supervisor, "the same supervisor started it");
    assert!(!alive(child), "the first child is gone");
    disown(child);
    term.tmux_ok(&["send-keys", "-t", &term.pane(), "q"]);
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "the new child did not end on q:\n{}",
        terminal_screen(&term)
    );
    assert_eq!(status_of(&status).as_deref(), Some("0\n"));
}

/// A child stopped by `SIGTSTP` (Ctrl-Z typed before its screen is raw stops it) is set going again by the
/// supervisor: the same child still draws, and `q` ends the terminal's program 0.
#[test]
fn a_child_that_is_stopped_is_set_going_again() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    let status = mac.work_dir().join("status");
    type_app(&term, &mac, "app", &mac.work_dir().join("err"), &status);
    expect_terminal(&term, "the app's list", |screen| {
        screen.contains(" taw app")
    });
    let (child, _) = the_app(&mac);

    signal(child, libc::SIGTSTP);
    // The supervisor looks every 100 ms and sets a stopped child going: for 1.5 s (fifteen looks) the child must
    // still be the process that was stopped, and then it must have been set going, not ended. A supervisor that
    // ended it instead would have a new child by then.
    let looking = std::time::Instant::now();
    while looking.elapsed() < Duration::from_millis(1500) {
        assert!(
            alive(child),
            "the stopped child {child} was ended, not set going: {:?}",
            records(&mac)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        wait_for(
            || !stat_of(child).starts_with('T') && !stat_of(child).is_empty(),
            Duration::from_secs(20)
        ),
        "the stopped child was never set going: {}",
        ps_line(child)
    );
    assert_eq!(the_app(&mac).0, child, "the same child, not a new one");
    // Typed while the child is stopped or just set going: the terminal holds the key for it.
    term.tmux_ok(&["send-keys", "-t", &term.pane(), "q"]);

    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "the stopped child {child} was never set going; the terminal shows:\n{}",
        terminal_screen(&term)
    );
    assert_eq!(status_of(&status).as_deref(), Some("0\n"));
}

// ---------------------------------------------------------------- when the app and its client part

/// The app child killed while its client is attached leaves the client where it is: the popup key then opens the
/// popup (nobody's app is that client's parent now, so it is not sent back), and once the client is detached the
/// supervisor, which waited for it, starts the list again.
#[test]
fn a_child_killed_while_attached_leaves_the_client_and_the_key_opens_the_popup() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    two_sessions(&mac);
    bind_the_popup_key(&mac);
    type_app(
        &term,
        &mac,
        "app",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| screen.contains("2 sessions"));
    press(&term, &["Enter"]);
    let (client, name, session) = the_client(&mac, "after Enter");
    expect_terminal(&term, "the session's status line", |screen| {
        screen.contains(&format!("[{session}]"))
    });
    let (child, supervisor) = the_app(&mac);

    signal(child, libc::SIGKILL);
    assert!(
        wait_for(|| !alive(child), Duration::from_secs(20)),
        "the child did not end"
    );
    disown(child);
    assert!(
        alive(supervisor) && alive(client),
        "the client and the supervisor stay"
    );

    press(&term, &["F5"]);
    expect_terminal(&term, "the popup over the client", |screen| {
        screen.contains("\u{256d} sessions") && screen.contains("j/k move")
    });
    assert_eq!(
        clients_of(&mac).iter().map(|c| c.0).collect::<Vec<_>>(),
        vec![client],
        "the key opened a popup and did not detach the client"
    );

    // Closing the popup and detaching: the supervisor starts the app again, and its list is back.
    press(&term, &["q"]);
    // The `q` is read by the popup before the client goes: a key still unread would be the next child's.
    expect_terminal(&term, "the popup closed over the client", |screen| {
        screen.contains(&format!("[{session}]")) && !screen.contains("\u{256d} sessions")
    });
    mac.tmux_ok(&["detach-client", "-t", &name]);
    // The list is back within 10 s of the detach (the test's own margin for a loaded machine): a supervisor
    // that looked only now and then would be told apart from one that starts the app at once.
    let detached = std::time::Instant::now();
    let back = wait_for(
        || {
            let screen = terminal_screen(&term);
            screen.contains(" taw app") && screen.contains("2 sessions")
        },
        Duration::from_secs(10),
    );
    assert!(
        back,
        "the app's list was not back {:?} after the detach:\n{}",
        detached.elapsed(),
        terminal_screen(&term)
    );
    assert_ne!(the_app(&mac).0, child, "a new child");
    assert!(alive(supervisor), "the same supervisor");
}

/// The row of the terminal's cursor (`#{cursor_y}`), or what tmux said.
fn cursor_y(term: &Server) -> String {
    term.tmux(&["display-message", "-p", "-t", &term.pane(), "#{cursor_y}"])
        .map(|said| said.trim().to_string())
        .unwrap_or_default()
}

/// Whether the screen `text` has the line `exit_line` whole on a row of its own, a line with something on it
/// below that row (the shell's prompt), and the cursor on a row below it too.
fn exit_line_above_the_prompt(text: &str, exit_line: &str, cursor_y: usize) -> bool {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let Some(row) = lines.iter().position(|line| *line == exit_line) else {
        return false;
    };
    lines[row + 1..].iter().any(|line| !line.is_empty()) && cursor_y > row
}

/// [`exit_line_above_the_prompt`] on the terminal's pane as it is now.
fn exit_line_is_above_the_prompt(term: &Server, exit_line: &str) -> bool {
    exit_line_above_the_prompt(
        &terminal_screen(term),
        exit_line,
        cursor_y(term).parse().unwrap_or(0),
    )
}

/// The check reads the three shapes: the line above the prompt (true), the prompt drawn over the line (false), and
/// a cursor that is not below the line (false).
#[test]
fn the_exit_line_check_tells_the_prompt_below_from_the_prompt_over_it() {
    let below = "sh-3.2$ taw app\n[terminated]\nsh-3.2$ \n\n";
    assert!(exit_line_above_the_prompt(below, "[terminated]", 2));
    assert!(
        !exit_line_above_the_prompt(below, "[terminated]", 1),
        "cursor on the line"
    );
    let over = "sh-3.2$ taw app\nsh-3.2$ \n\n";
    assert!(
        !exit_line_above_the_prompt(over, "[terminated]", 1),
        "line gone"
    );
    let nothing_below = "sh-3.2$ taw app\n[terminated]\n\n";
    assert!(
        !exit_line_above_the_prompt(nothing_below, "[terminated]", 2),
        "no prompt yet"
    );
}

/// The supervisor sent `sig` while its client is attached ends the terminal's program with 128 and the signal, with
/// its child already gone when the status is written (the supervisor reaps it before it returns), the attach client
/// told the same signal and gone from the terminal (a signal to the supervisor hangs nothing up, so only the child's
/// passing it on ends the client), and what "this Mac" runs untouched.
fn a_supervisor_is_signalled_while_attached(sig: libc::c_int, ends: &str, exit_line: &str) {
    let mac = Server::start();
    let term = terminal(120);
    let (first, other, _) = went_in_and_moved(&mac, &term, "app");
    let (child, supervisor) = the_app(&mac);
    let (client, _, _) = the_client(&mac, "before the signal");

    signal(supervisor, sig);

    let status = mac.work_dir().join("status");
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "the supervisor did not end on the signal:\n{}",
        terminal_screen(&term)
    );
    assert_eq!(status_of(&status).as_deref(), Some(ends));
    // Straight away, with no wait: the supervisor passed the signal on and reaped the child before it returned,
    // so a supervisor that passed nothing on (and died of it at once) leaves the child here.
    assert!(
        !alive(child),
        "the child outlived the supervisor: {}",
        ps_line(child)
    );
    disown(child);
    assert!(
        wait_for(
            || !alive(client) && clients_of(&mac).is_empty(),
            Duration::from_secs(20)
        ),
        "the attach client {client} was left on the terminal: {} / {:?}",
        ps_line(client),
        clients_of(&mac)
    );
    // The app was lent the terminal and ended while it was lent, so its screen's `Drop` wrote nothing: the
    // line the attach client printed as it ended is whole on the outer pane, with the shell's prompt on a
    // line below it and the cursor below it, not over it (a second leave of the alternate screen would send
    // the cursor back to the place the first one saved, and the prompt would be drawn over the text above).
    assert!(
        wait_for(
            || exit_line_is_above_the_prompt(&term, exit_line),
            Duration::from_secs(20)
        ),
        "the client's line {exit_line:?} is not whole above the shell's prompt:\n{}\ncursor_y {}",
        terminal_screen(&term),
        cursor_y(&term)
    );
    // This sleep follows the positive wait above: a cursor that jumped late would show within a moment.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        exit_line_is_above_the_prompt(&term, exit_line),
        "the client's line {exit_line:?} was written over afterwards:\n{}\ncursor_y {}",
        terminal_screen(&term),
        cursor_y(&term)
    );
    let sessions = inner(&mac)
        .run(&["list-sessions", "-F", "#{session_name}"])
        .unwrap();
    for name in [first.as_str(), other.as_str()] {
        assert!(
            sessions.lines().any(|s| s == name),
            "{name} gone: {sessions}"
        );
    }
}

/// The supervisor stopped by `SIGTERM` while its client is attached ends the terminal's program with 143.
#[test]
fn a_supervisor_terminated_while_attached_ends_143_and_the_sessions_stay() {
    let _own = Owned::new();
    a_supervisor_is_signalled_while_attached(libc::SIGTERM, "143\n", "[terminated]");
}

/// The twin for the hang-up of a closing terminal: 129.
#[test]
fn a_supervisor_hung_up_while_attached_ends_129_and_the_sessions_stay() {
    let _own = Owned::new();
    a_supervisor_is_signalled_while_attached(libc::SIGHUP, "129\n", "[lost tty]");
}

/// A client somebody attached by hand is nobody's app child: the popup key opens the popup on it, the client stays,
/// and no `left` line is written, even with an app running elsewhere.
#[test]
fn a_hand_attached_clients_key_opens_the_popup_and_writes_no_left_line() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    bind_the_popup_key(&mac);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| screen.contains(" taw app"));
    let _ = the_app(&mac);
    let hand = Client::attach(&inner(&mac), "one");

    hand.outer()
        .run_ok(&["send-keys", "-t", "host", "F5"])
        .unwrap();

    expect_screen(
        hand.outer(),
        "host",
        "the popup on the hand-attached client",
        |screen| screen.contains("\u{256d} sessions") && screen.contains("j/k move"),
    );
    assert_eq!(clients_of(&mac).len(), 1, "the client is still attached");
    let events = read(&mac.state_dir().join("machines").join("events.jsonl")).unwrap_or_default();
    assert!(
        !events.contains("left"),
        "a left line was written: {events}"
    );
}

/// What a process runs as, from the real `ps`: its program and arguments on one line.
fn command_line_of(pid: u32) -> String {
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The id of the first pane of `session` of "this Mac".
fn pane_of(mac: &Server, session: &str) -> String {
    inner(mac)
        .run(&["display-message", "-p", "-t", session, "#{pane_id}"])
        .expect("the session has a pane")
        .trim()
        .to_string()
}

/// `--go %N` goes into the session of that pane of this Mac before the first frame: its client is the app's,
/// and the list is what comes back when it is detached; a pane that is gone is said, and nothing is attached.
#[test]
fn go_on_a_pane_of_this_mac_attaches_its_session_and_says_a_gone_pane() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    let pane = pane_of(&mac, "beta");
    let term = terminal(120);
    let status = mac.work_dir().join("status");
    type_app(
        &term,
        &mac,
        &format!("app --child --go {pane}"),
        &mac.work_dir().join("err"),
        &status,
    );
    let (_, name, session) = the_client(&mac, "--go on a pane");
    assert_eq!(session, "beta");
    assert_eq!(status_of(&status), None, "the app ended on the go");
    mac.tmux_ok(&["detach-client", "-t", &name]);
    expect_terminal(&term, "the app's list again", |screen| {
        screen.contains(" taw app") && screen.contains("beta")
    });
    press(&term, &["q"]);
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "q did not end the app"
    );

    // A pane that is not there: the words, the list, no client (positive: the list is drawn first).
    let _ = std::fs::remove_file(&status);
    type_app(
        &term,
        &mac,
        "app --child --go %9999",
        &mac.work_dir().join("err"),
        &status,
    );
    expect_terminal(&term, "the gone pane said", |screen| {
        screen.contains("%9999 is gone") && screen.contains("beta")
    });
    assert!(clients_of(&mac).is_empty(), "{:?}", clients_of(&mac));
    press(&term, &["q"]);
}

/// `--go last` goes into the session that was attached most lately; with no server it is the session
/// `taw open .` makes, and the client is in it.
#[test]
fn go_last_attaches_the_session_attached_most_lately_or_the_one_it_makes() {
    let _own = Owned::new();
    let mac = Server::start();
    // The post-create hook of `taw open` writes what it is run with, one line a run, so that the session
    // `--go last` makes is shown to have been made by the open, and once.
    let ran = mac.work_dir().join("hook-ran");
    let hook = mac.work_dir().join("hook.sh");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\necho \"$1 $2\" >> '{}'\n", ran.display()),
    )
    .unwrap();
    crate::common::make_executable(&hook);
    mac.write_config(&format!(
        "[sessions]\npost_create_hook = \"{}\"\n",
        hook.display()
    ));
    two_sessions(&mac);
    // `one` is attached, then left: its time is the greatest, though `beta` sorts before it and is listed
    // first (and `one` is the one `Server::start` made, never attached before).
    drop(Client::attach(&inner(&mac), "one"));
    assert!(
        wait_for(|| clients_of(&mac).is_empty(), Duration::from_secs(20)),
        "the hand client stays"
    );
    let term = terminal(120);
    let status = mac.work_dir().join("status");
    type_app(
        &term,
        &mac,
        "app --child --go last",
        &mac.work_dir().join("err"),
        &status,
    );
    let (_, name, session) = the_client(&mac, "--go last");
    assert_eq!(
        session, "one",
        "the session attached most lately, not the first listed"
    );
    assert!(!ran.exists(), "a session was found: the open made none");
    mac.tmux_ok(&["detach-client", "-t", &name]);
    expect_terminal(&term, "the app's list again", |screen| {
        screen.contains(" taw app")
    });
    press(&term, &["q"]);
    assert!(
        wait_for(|| status_of(&status).is_some(), Duration::from_secs(20)),
        "q did not end the app"
    );

    // No server: the session `taw open .` makes.
    end_the_server(&mac);
    let _ = std::fs::remove_file(&status);
    type_app(
        &term,
        &mac,
        "app --child --go last",
        &mac.work_dir().join("err"),
        &status,
    );
    let (_, name, session) = the_client(&mac, "--go last with no server");
    // The session is the one `taw open .` makes for the terminal's directory: one session, there, and its
    // post-create hook ran once, with that session and that directory.
    let mut lines = Vec::new();
    let seen = wait_for(
        || {
            lines = std::fs::read_to_string(&ran)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect();
            !lines.is_empty()
        },
        Duration::from_secs(20),
    );
    assert!(seen, "the post-create hook never ran");
    let dir = term.work_dir().canonicalize().unwrap();
    let listed = inner(&mac)
        .run(&["list-sessions", "-F", "#{session_name}"])
        .unwrap();
    assert_eq!(
        listed.lines().collect::<Vec<_>>(),
        [session.as_str()],
        "{listed}"
    );
    let path = inner(&mac)
        .run(&["display-message", "-p", "-t", &session, "#{session_path}"])
        .unwrap();
    assert_eq!(
        Path::new(path.trim()).canonicalize().unwrap(),
        dir,
        "{path}"
    );
    assert_eq!(lines.len(), 1, "the hook ran once: {lines:?}");
    let (hooked_session, hooked_dir) = lines[0].split_once(' ').expect("session and directory");
    assert_eq!(hooked_session, session, "{lines:?}");
    assert_eq!(
        Path::new(hooked_dir).canonicalize().unwrap(),
        dir,
        "{lines:?}"
    );
    mac.tmux_ok(&["detach-client", "-t", &name]);
    expect_terminal(&term, "the app's list again", |screen| {
        screen.contains(" taw app")
    });
    press(&term, &["q"]);
}

/// `--go` goes to the first child only: a child started again after a failure has none.
#[test]
fn the_supervisor_passes_go_to_the_first_child_only() {
    let _own = Owned::new();
    let mac = Server::start();
    let term = terminal(120);
    type_app(
        &term,
        &mac,
        "app --go somewhere:%7",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| screen.contains(" taw app"));
    let (first, _) = the_app(&mac);
    let line = command_line_of(first);
    assert!(line.contains("--child --go somewhere:%7"), "{line}");

    signal(first, libc::SIGKILL);
    assert!(
        wait_for(
            || pids(&mac).iter().any(|pid| *pid != first),
            Duration::from_secs(20)
        ),
        "no new child: {:?}",
        records(&mac)
    );
    let (second, _) = the_app(&mac);
    let line = command_line_of(second);
    assert!(line.contains("--child"), "{line}");
    assert!(!line.contains("--go"), "the second child got --go: {line}");
}

/// The refreshed_at of the lease on file, or 0 while there is none.
fn lease_refreshed_at(mac: &Server) -> u64 {
    read(&mac.state_dir().join("machines").join("lease.json"))
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|lease| lease["refreshed_at"].as_u64())
        .unwrap_or(0)
}

/// While a client is attached the app keeps its machines moving: the keeper's lease is offered again every few
/// seconds, so the followers do not take the machines for gone after 15 s and the key rule finds a fresh lease.
#[test]
fn the_machines_keep_moving_while_a_client_is_attached() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    mac.write_config("[machines]\nstudio = \"studio.local\"\n");
    let ssh = mac.work_dir().join("login-ssh");
    common::write_script(
        &ssh,
        "echo 'studio.local: Permission denied (publickey).' >&2\nexit 255\n",
    );
    let term = terminal(120);
    type_app_env(
        &term,
        &mac,
        &[("TAW_SSH_BIN", &ssh.display().to_string())],
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| screen.contains("one"));
    // The guard holds the app's pids from here (its child ends by the watchdog when the terminal's server goes).
    let _ = the_app(&mac);
    press(&term, &["/", "o", "n", "e", "Enter"]);
    let (_, _, session) = the_client(&mac, "after Enter");
    expect_terminal(&term, "the session's status line", |screen| {
        screen.contains(&format!("[{session}]"))
    });
    assert!(
        wait_for(|| lease_refreshed_at(&mac) != 0, Duration::from_secs(20)),
        "the keeper never offered a lease"
    );

    // Read after the client is there: a later value was written while it is attached.
    let first = lease_refreshed_at(&mac);
    assert!(
        wait_for(|| lease_refreshed_at(&mac) > first, Duration::from_secs(20)),
        "the lease stayed at {first} while the client was attached"
    );
    let second = lease_refreshed_at(&mac);
    assert!(
        wait_for(
            || lease_refreshed_at(&mac) > second,
            Duration::from_secs(20)
        ),
        "the lease stayed at {second} while the client was attached"
    );
}

/// Waits until `count` has not moved for 1.5 s (20 s at most) and returns it: the calls of a connection that
/// is still starting are not what a later wake is told apart from.
fn settled_count(count: impl Fn() -> usize) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut last = count();
    let mut since = std::time::Instant::now();
    while since.elapsed() < Duration::from_millis(1500) {
        assert!(
            std::time::Instant::now() < deadline,
            "the count never settled, at {last}"
        );
        std::thread::sleep(Duration::from_millis(100));
        let now = count();
        if now != last {
            last = now;
            since = std::time::Instant::now();
        }
    }
    last
}

/// A machine that needs you (a refused login) is tried once when the app starts, not twice in the first second:
/// the wake is for coming back to the list, not for the start.
#[test]
fn a_machine_that_refuses_is_tried_once_when_the_app_starts() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    bind_the_popup_key(&mac);
    mac.write_config("[machines]\nstudio = \"studio.local\"\n");
    let ssh = mac.work_dir().join("login-ssh");
    let calls = mac.work_dir().join("login-calls");
    common::write_script(
        &ssh,
        &format!(
            "echo call >> '{}'\necho 'studio.local: Permission denied (publickey).' >&2\nexit 255\n",
            calls.display()
        ),
    );
    let count = || read(&calls).map_or(0, |text| text.lines().count());
    // A slow `zoxide` holds the start between the connections beginning and the first frame, so the
    // stand-in has refused (the machine waits for its 30 s) before a wake at start would reach it: with a
    // wake there the machine would be tried again at once, and without one it is not.
    let slow = mac.work_dir().join("slow-bin");
    std::fs::create_dir_all(&slow).unwrap();
    common::write_script(&slow.join("zoxide"), "sleep 1\nexit 0\n");
    let path = format!(
        "{}:{}",
        slow.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let term = terminal(120);
    type_app_env(
        &term,
        &mac,
        &[("TAW_SSH_BIN", &ssh.display().to_string()), ("PATH", &path)],
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    // The positive halves: the list is up and ssh was tried.
    expect_terminal(&term, "the list", |screen| screen.contains("one"));
    // The guard holds the app's pids from here (its child ends by the watchdog when the terminal's server goes).
    let _ = the_app(&mac);
    assert!(
        wait_for(|| count() >= 1, Duration::from_secs(20)),
        "ssh was never tried"
    );
    // Once the calls settle (no new one for 1.5 s) there is exactly the start's own.
    let settled = settled_count(count);
    assert_eq!(
        settled, 1,
        "the first second of the app made {settled} tries of a machine that refuses"
    );
}

/// A machine whose login is refused is tried again in 30 s; the list shown again after a client ended wakes it, so
/// the next try comes then, well before the 30 s.
#[test]
fn the_list_shown_again_after_an_attach_wakes_a_machine_that_waits() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    bind_the_popup_key(&mac);
    mac.write_config("[machines]\nstudio = \"studio.local\"\n");
    let ssh = mac.work_dir().join("login-ssh");
    let calls = mac.work_dir().join("login-calls");
    common::write_script(
        &ssh,
        &format!(
            "echo call >> '{}'\necho 'studio.local: Permission denied (publickey).' >&2\nexit 255\n",
            calls.display()
        ),
    );
    let count = || read(&calls).map_or(0, |text| text.lines().count());
    let term = terminal(120);
    type_app_env(
        &term,
        &mac,
        &[("TAW_SSH_BIN", &ssh.display().to_string())],
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    assert!(
        wait_for(|| count() >= 1, Duration::from_secs(20)),
        "ssh was never tried"
    );
    expect_terminal(&term, "the list", |screen| screen.contains("one"));
    // The guard holds the app's pids from here (its child ends by the watchdog when the terminal's server goes).
    let _ = the_app(&mac);
    // Whatever the start made of the connection is over: this is the count a wake is told apart by.
    let before = settled_count(count);
    let settled_at = std::time::Instant::now();
    // Into the session `one` through the filter, and back by the popup key.
    press(&term, &["/", "o", "n", "e", "Enter"]);
    let _ = the_client(&mac, "after Enter");
    press(&term, &["F5"]);
    expect_terminal(&term, "the list again", |screen| {
        screen.contains(" taw app")
    });

    // The next try on its own is 30 s after the last call, which was before the 1.5 s of the settling: a call
    // within 20 s of the list coming back is the wake's.
    assert!(
        wait_for(|| count() > before, Duration::from_secs(20)),
        "the machine was not tried again after the list came back; calls: {} (before: {before})",
        count()
    );
    // The 30 s of the machine's own wait run from its last call, 1.5 s before the settling began to count.
    assert!(
        settled_at.elapsed() < Duration::from_millis(26_500),
        "the second try came only with the machine's own 30 s wait: {:?} after the count settled",
        settled_at.elapsed()
    );
}

/// The session and the last session of the one client of "this Mac" that is not `old`, once it is there.
fn new_client_sessions(mac: &Server, old: u32) -> (u32, String, String, String) {
    let mut seen = None;
    let found = wait_for(
        || {
            seen = inner(mac)
                .run(&[
                    "list-clients",
                    "-F",
                    "#{client_pid} #{client_name} #{session_name} #{client_last_session}",
                ])
                .unwrap_or_default()
                .lines()
                .filter_map(|line| {
                    let words: Vec<&str> = line.split(' ').collect();
                    let pid: u32 = words.first()?.parse().ok()?;
                    (pid != old && words.len() >= 3).then(|| {
                        (
                            pid,
                            words[1].to_string(),
                            words[2].to_string(),
                            words.get(3).copied().unwrap_or("").to_string(),
                        )
                    })
                })
                .next();
            seen.is_some()
        },
        Duration::from_secs(20),
    );
    assert!(found, "no new client of the app showed on the server");
    seen.unwrap()
}

/// Every attach the app makes is a new client, and a new client has no last session, so prefix + L said "Can't
/// find last session". The second session the app goes into in one run is attached through the first, so the client
/// lands on the second with the first as its last session, and `switch-client -l` goes back to it.
#[test]
fn the_second_session_the_app_goes_into_has_the_first_as_its_last_session() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    let term = terminal(120);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| {
        screen.contains(" taw app") && screen.contains("beta")
    });
    press(&term, &["Enter"]);
    let (pid, name, first, last) = new_client_sessions(&mac, 0);
    assert_eq!(last, "", "the first attach has no session to come from");
    mac.tmux_ok(&["detach-client", "-t", &name]);
    expect_terminal(&term, "the list again", |screen| {
        screen.contains(" taw app") && screen.contains("beta")
    });
    // The cursor is on the row just left: the other session is one row away (beta sorts before one).
    press(&term, &[if first == "one" { "k" } else { "j" }, "Enter"]);
    let (_, name, second, last) = new_client_sessions(&mac, pid);
    assert_ne!(first, second, "the second Enter went into the same session");
    assert_eq!(last, first, "the client's last session");
    mac.tmux_ok(&["switch-client", "-c", &name, "-l"]);
    let now = mac.tmux_ok(&["display-message", "-p", "-c", &name, "#{session_name}"]);
    assert_eq!(now.trim(), first, "switch-client -l goes back to the first");
}

/// What the plugin's seen hook is told to do on a session change, with the plugin's own script: it keeps the startup
/// one-shots off the network and the host's notifier.
const PLUGIN_CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// A `done` agent on the active pane of `session`, unseen, as the watcher leaves one that finished out of sight.
fn leave_done_agent(mac: &Server, session: &str) {
    let target = format!("={session}:");
    for (name, value) in [
        ("@agent_kind", "claude"),
        ("@agent_state", "done"),
        ("@agent_seen", "0"),
    ] {
        mac.tmux_ok(&["set-option", "-p", "-t", &target, name, value]);
    }
}

/// What a pane of `session` says its agent's state is.
fn agent_state_of(mac: &Server, session: &str) -> String {
    let target = format!("={session}:");
    mac.tmux_ok(&["display-message", "-p", "-t", &target, "#{@agent_state}"])
        .trim()
        .to_string()
}

/// The attach that goes in through the session the person came from passes through it: tmux fires
/// `client-session-changed` for the came-from session, which ran `taw seen` on its active pane and turned the
/// `done` of a finish nobody saw into `idle`. Only a pane someone is looking at is marked seen: the client that
/// lands on the target marks the target's, and a switch onto a session still marks it.
#[test]
fn going_in_through_the_session_you_came_from_does_not_mark_its_agent_seen() {
    let _own = Owned::new();
    let mac = Server::start();
    mac.write_config(PLUGIN_CONFIG);
    mac.tmux_ok(&["set-option", "-g", "@agent-wizard-watch", "off"]);
    two_sessions(&mac);
    mac.source_plugin();
    let term = terminal(120);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| {
        screen.contains(" taw app") && screen.contains("beta")
    });
    press(&term, &["Enter"]);
    let (pid, name, first, _) = new_client_sessions(&mac, 0);
    mac.tmux_ok(&["detach-client", "-t", &name]);
    expect_terminal(&term, "the list again", |screen| {
        screen.contains(" taw app") && screen.contains("beta")
    });
    let second = if first == "one" { "beta" } else { "one" };
    // An agent finished in each while the person was on the list.
    leave_done_agent(&mac, &first);
    leave_done_agent(&mac, second);
    press(&term, &[if first == "one" { "k" } else { "j" }, "Enter"]);
    let (_, name, landed, last) = new_client_sessions(&mac, pid);
    assert_eq!((landed.as_str(), last.as_str()), (second, first.as_str()));
    // The landing is looked at: its mark is the proof that the hooks ran.
    assert!(
        wait_for(
            || agent_state_of(&mac, second) == "idle",
            Duration::from_secs(20)
        ),
        "the session the client landed on was not marked seen: {:?}",
        agent_state_of(&mac, second)
    );
    // The pass-through's hook was queued before that one: let any job of it finish before the negative check.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        agent_state_of(&mac, &first),
        "done",
        "the session passed through was marked seen, and nobody looked at it"
    );
    // A plain switch onto a session with a `done` agent still marks it seen.
    mac.tmux_ok(&["switch-client", "-c", &name, "-t", &format!("={first}:")]);
    assert!(
        wait_for(
            || agent_state_of(&mac, &first) == "idle",
            Duration::from_secs(20)
        ),
        "a switch onto the session did not mark its agent seen: {:?}",
        agent_state_of(&mac, &first)
    );
}

/// A tmux client on `session` of "this Mac" `mac`, in a pane of a terminal of its own, and the terminal that keeps
/// it. Waits until the client is on the server.
fn attach_client(mac: &Server, session: &str) -> Server {
    let term = terminal(120);
    let before = clients_of(mac).len();
    let env: String = mac
        .taw_env()
        .iter()
        .map(|(name, value)| format!("'{name}={value}' "))
        .collect();
    let line = format!(
        "env -u TMUX -u TMUX_PANE {env}'{}' -L '{}' attach-session -t '={session}'",
        Tmux::with_socket_name("unused").bin.display(),
        mac.socket()
    );
    term.wait_for_prompt(&term.pane());
    term.tmux_ok(&["send-keys", "-t", &term.pane(), "-l", &line]);
    term.tmux_ok(&["send-keys", "-t", &term.pane(), "Enter"]);
    assert!(
        wait_for(|| clients_of(mac).len() > before, Duration::from_secs(20)),
        "no client came up on {session}"
    );
    term
}

/// A pane counts as looked at when any client shows it. A session grouped with another (`new-session -t`) shares its
/// windows, and `display-message -t <pane>` answers for the session of the group attached most lately, which can
/// be one nobody is on: the person on the other session was looking at the pane and its `done` stayed.
#[test]
fn a_pane_a_client_on_a_grouped_session_shows_is_marked_seen() {
    let mac = Server::start();
    mac.write_config(PLUGIN_CONFIG);
    mac.tmux_ok(&["set-option", "-g", "@agent-wizard-watch", "off"]);
    mac.tmux_ok(&["new-session", "-d", "-s", "twin", "-t", "one"]);
    mac.source_plugin();
    // The client that stays is on `twin`; a second client was on `one` after it and went, so `one` is the session of
    // the group attached most lately, with no client on it.
    let _stays = attach_client(&mac, "twin");
    let goes = attach_client(&mac, "one");
    let name = clients_of(&mac)
        .into_iter()
        .find(|(_, _, session)| session == "one")
        .expect("the second client is on `one`")
        .1;
    mac.tmux_ok(&["detach-client", "-t", &name]);
    drop(goes);
    assert!(
        wait_for(|| clients_of(&mac).len() == 1, Duration::from_secs(20)),
        "the second client did not go"
    );
    // The hooks of the attaches and the detach have run before the negative start: let their jobs finish.
    std::thread::sleep(Duration::from_millis(1500));
    let pane = mac.pane_of("one");
    for (name, value) in [
        ("@agent_kind", "claude"),
        ("@agent_state", "done"),
        ("@agent_seen", "0"),
    ] {
        mac.tmux_ok(&["set-option", "-p", "-t", &pane, name, value]);
    }
    // Selecting the window the client is on again fires the hooks, with the pane that is the active one.
    mac.tmux_ok(&["select-window", "-t", "=twin:0"]);
    assert!(
        wait_for(
            || agent_state_of(&mac, "one") == "idle",
            Duration::from_secs(20)
        ),
        "a pane the client on the grouped session shows was not marked seen: {:?}",
        agent_state_of(&mac, "one")
    );
}

/// The session the app came from is kept by its id. A session row lands by its id, but a directory the filter lists
/// lands by the name of the session it opens (`=alpha`). Renamed while the person was on the list, and its old name
/// given to another session, the session the app came from is still the one prefix + L goes back to. Kept by the
/// landing's name, the client's last session would be the new session of that name.
#[test]
fn the_session_the_app_came_from_is_kept_by_its_id_through_a_rename() {
    let _own = Owned::new();
    let mac = Server::start();
    let projects = mac.work_dir().join("projects");
    std::fs::create_dir_all(projects.join("alpha")).unwrap();
    mac.write_config(&format!(
        "[ui]\nprojects_in_filter = true\n\n[sessions]\nzoxide = false\nproject_dirs = [\"{}\"]\n",
        projects.display()
    ));
    two_sessions(&mac);
    let term = terminal(120);
    type_app(
        &term,
        &mac,
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| {
        screen.contains(" taw app") && screen.contains("beta")
    });
    // Into the session of the directory, which the app creates and lands on by its name.
    press(&term, &["/"]);
    press(&term, &["a", "l", "p", "h", "a"]);
    expect_terminal(&term, "the directory listed", |screen| {
        screen.contains("alpha")
    });
    press(&term, &["Enter"]);
    let (pid, name, first, _) = new_client_sessions(&mac, 0);
    assert_eq!(first, "alpha", "the directory's session");
    mac.tmux_ok(&["detach-client", "-t", &name]);
    expect_terminal(&term, "the list again", |screen| {
        screen.contains(" taw app") && screen.contains("beta")
    });
    mac.tmux_ok(&["rename-session", "-t", "=alpha", "uno"]);
    mac.tmux_ok(&["new-session", "-d", "-s", "alpha"]);
    expect_terminal(&term, "the renamed session listed", |screen| {
        screen.contains("uno")
    });
    // Into beta by the filter (a session row lands by its id): the client's last session is the one it came from.
    press(&term, &["/", "b", "e", "t", "a", "Enter"]);
    let (_, _, second, last) = new_client_sessions(&mac, pid);
    assert_eq!(second, "beta", "the second landing");
    assert_eq!(
        last, "uno",
        "the client's last session is the one it came from, which has another name now"
    );
}

/// A tmux that stands in for the client the app lends the terminal to, and logs the terminal's modes at every call
/// it sees: `<dir>/log` gets one line per call (`<first word> <mode words>`, the words `stty` says for `icanon` and
/// `echo` and their negations, read from the controlling terminal). An `attach-session` is the stand-in client: it
/// writes `<dir>/up`, reads one line from the terminal (what a person types while it has it), writes it to
/// `<dir>/typed` and waits for `<dir>/go` before it ends, so a test can look at the plain screen while it has the
/// terminal. Every other call is run by the real tmux.
fn lending_tmux(dir: &Path) -> std::path::PathBuf {
    let stand_in = dir.join("tmux-lends");
    let script = format!(
        "#!/bin/sh\n\
         d='{dir}'\n\
         modes() {{ stty -a </dev/tty 2>/dev/null | tr ' ;' '\\n\\n' | grep -x -e echo -e -echo -e icanon -e -icanon | tr '\\n' ' '; }}\n\
         case \" $* \" in\n\
         *\" attach-session \"*)\n\
           echo \"attach $(modes)\" >>\"$d/log\"\n\
           echo up >\"$d/up\"\n\
           IFS= read -r line </dev/tty\n\
           echo \"$line\" >\"$d/typed\"\n\
           while [ ! -e \"$d/go\" ]; do sleep 0.05; done\n\
           exit 0;;\n\
         esac\n\
         echo \"$1 $(modes)\" >>\"$d/log\"\n\
         exec '{real}' \"$@\"\n",
        dir = dir.display(),
        real = Tmux::with_socket_name("unused").bin.display(),
    );
    std::fs::write(&stand_in, script).expect("the stand-in tmux is written");
    common::make_executable(&stand_in);
    stand_in
}

/// The words of a mode line of [`lending_tmux`]'s log for the call whose first word is `call`, the first of them.
fn modes_at(dir: &Path, call: &str) -> Option<Vec<String>> {
    let log = std::fs::read_to_string(dir.join("log")).ok()?;
    log.lines()
        .find_map(|line| line.strip_prefix(&format!("{call} ")))
        .map(|words| words.split_whitespace().map(str::to_string).collect())
}

/// Going into a session and coming back shows no typed key: the client finds the terminal line by line with echo off,
/// a line typed while it has the terminal reaches it and shows nowhere on the plain screen, and after the app ends
/// the shell gets a terminal that shows what is typed.
#[test]
fn a_key_typed_while_the_terminal_is_lent_is_read_by_the_client_and_shown_nowhere() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    let term = terminal(120);
    let dir = mac.work_dir().join("lends");
    std::fs::create_dir_all(&dir).unwrap();
    let tmux = lending_tmux(&dir);
    let modes = dir.join("modes");
    type_app_env(
        &term,
        &mac,
        &[("TAW_TMUX_BIN", tmux.to_str().unwrap())],
        &format!("app --child; stty -a >'{}'", modes.display()),
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| screen.contains("2 sessions"));
    let _ = the_app(&mac);
    press(&term, &["Enter"]);
    assert!(
        wait_for(|| dir.join("up").exists(), Duration::from_secs(20)),
        "the stand-in client never got the terminal:\n{}",
        terminal_screen(&term)
    );
    let found = modes_at(&dir, "attach").expect("the client logged no modes");
    assert!(
        found.iter().any(|word| word == "icanon") && found.iter().any(|word| word == "-echo"),
        "the client found {found:?}, not the cooked modes with echo off"
    );
    assert!(
        !found.iter().any(|word| word == "echo"),
        "the client found echo on: {found:?}"
    );
    press(&term, &["-l", "ZQZQ"]);
    press(&term, &["Enter"]);
    // The positive half: the client read what was typed.
    assert!(
        wait_for(
            || std::fs::read_to_string(dir.join("typed")).is_ok_and(|typed| typed == "ZQZQ\n"),
            Duration::from_secs(20)
        ),
        "the client never read the typed line:\n{}",
        terminal_screen(&term)
    );
    let screen = terminal_screen(&term);
    assert!(
        !screen.contains("ZQZQ"),
        "a key typed while the terminal is lent was shown:\n{screen}"
    );
    std::fs::write(dir.join("go"), "").unwrap();
    expect_terminal(&term, "the list again", |screen| {
        screen.contains("2 sessions")
    });
    press(&term, &["C-c"]);
    assert!(
        wait_for(
            || std::fs::read_to_string(&modes)
                .is_ok_and(|text| text.contains("lflags") || text.contains("echo")),
            Duration::from_secs(20)
        ),
        "the shell never ran stty:\n{}",
        terminal_screen(&term)
    );
    let words = std::fs::read_to_string(&modes).unwrap();
    let words: Vec<&str> = words.split([' ', ';', '\n']).collect();
    assert!(
        words.contains(&"echo") && !words.contains(&"-echo"),
        "the shell got a terminal that shows nothing typed: {words:?}"
    );
}

/// A child killed while a client has the terminal leaves it without echo, and the supervisor, which ends on a signal
/// after a new child started, puts back the modes it found: the shell gets a terminal that shows what is typed.
#[test]
fn the_supervisor_gives_the_shell_back_a_terminal_that_shows_what_is_typed() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    let term = terminal(120);
    let dir = mac.work_dir().join("lends");
    std::fs::create_dir_all(&dir).unwrap();
    let tmux = lending_tmux(&dir);
    let modes = dir.join("modes");
    type_app_env(
        &term,
        &mac,
        &[("TAW_TMUX_BIN", tmux.to_str().unwrap())],
        &format!("app; stty -a >'{}'", modes.display()),
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| screen.contains("2 sessions"));
    let (child, supervisor) = the_app(&mac);
    press(&term, &["Enter"]);
    assert!(
        wait_for(|| dir.join("up").exists(), Duration::from_secs(20)),
        "the stand-in client never got the terminal:\n{}",
        terminal_screen(&term)
    );
    // The child dies with the client attached; the client is let end, and a new child starts on the terminal.
    signal(child, libc::SIGKILL);
    // The stand-in client reads a line before it ends.
    press(&term, &["Enter"]);
    std::fs::write(dir.join("go"), "").unwrap();
    assert!(
        wait_for(
            || pids(&mac).iter().any(|pid| *pid != child),
            Duration::from_secs(20)
        ),
        "no new child started:\n{}",
        terminal_screen(&term)
    );
    expect_terminal(&term, "the new child's list", |screen| {
        screen.contains("2 sessions")
    });
    let _ = the_app(&mac);
    signal(supervisor, libc::SIGTERM);
    assert!(
        wait_for(
            || std::fs::read_to_string(&modes).is_ok(),
            Duration::from_secs(20)
        ),
        "the shell never ran stty:\n{}",
        terminal_screen(&term)
    );
    let words = std::fs::read_to_string(&modes).unwrap();
    let words: Vec<&str> = words.split([' ', ';', '\n']).collect();
    assert!(
        words.contains(&"echo") && !words.contains(&"-echo"),
        "the shell got a terminal that shows nothing typed: {words:?}"
    );
}

/// Raw mode is taken before the app's first call to tmux, so a key typed while the app starts is not echoed: every
/// call the app makes finds the terminal not line by line.
#[test]
fn raw_mode_is_on_before_the_apps_first_call_to_tmux() {
    let _own = Owned::new();
    let mac = Server::start();
    two_sessions(&mac);
    let term = terminal(120);
    let dir = mac.work_dir().join("lends");
    std::fs::create_dir_all(&dir).unwrap();
    let tmux = lending_tmux(&dir);
    type_app_env(
        &term,
        &mac,
        &[("TAW_TMUX_BIN", tmux.to_str().unwrap())],
        "app --child",
        &mac.work_dir().join("err"),
        &mac.work_dir().join("status"),
    );
    expect_terminal(&term, "the list", |screen| screen.contains("2 sessions"));
    let _ = the_app(&mac);
    let log = std::fs::read_to_string(dir.join("log")).unwrap_or_default();
    let first = log.lines().next().unwrap_or_default().to_string();
    assert!(
        first.split_whitespace().any(|word| word == "-icanon"),
        "the first call to tmux found the terminal line by line: {first:?}\nlog:\n{log}"
    );
    press(&term, &["q"]);
}
