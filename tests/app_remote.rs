//! Going into a machine from `taw app`, end to end: the built binary against private tmux servers.
//!
//! The app runs on a terminal made the way `tests/app_local.rs` makes one (a pane of an outer server whose own
//! screen is the test's to read). "This Mac" is a second private server; the machine `studio` is a third, a
//! [`FarSide`], reached through the stand-in for ssh, which records every call and runs the far side's own
//! `taw` where a real ssh would log in. No test runs a real ssh.
//!
//! Every wait is positive, with a 20 s deadline, and panics with what was seen: the screen, the stand-in's
//! calls and the `ps` lines of the apps of this build.

mod common;

use std::time::Duration;

use common::machine::{Failure, FarSide, StandIn};
use common::owned::{Owned, alive, args_of, own, own_as, record_names, signal, start_time_of};
use common::{Server, ServerOptions, taw_bin, wait_for};
use taw::machine::words;
use taw::tui::CONFIG_UNCHANGED_LOG;

/// Everything a test of going into studio has: the terminal, this Mac, studio and the stand-in.
struct Rig {
    term: Server,
    mac: Server,
    far: FarSide,
    stand_in: StandIn,
    /// The level of `TAW_LOG` the app is started with, when a test reads what it logs.
    app_log: Option<&'static str>,
}

/// A terminal as a person has one: a private server of its own whose pane is the terminal. Wide, so that a toast
/// is not cut.
fn terminal() -> Server {
    Server::start_with(ServerOptions {
        width: 200,
        height: 30,
        ..ServerOptions::default()
    })
}

/// This Mac with `studio` in `[machines]`, and studio: a session `garden` with the windows `main` and `tools`,
/// and the popup key bound on studio's own server as the plugin binds it.
fn rig() -> Rig {
    rig_with(&[])
}

/// [`rig`], with more far sides the stand-in also reaches. They are not in `[machines]` until a test writes them
/// there.
fn rig_with(more: &[&FarSide]) -> Rig {
    let mac = Server::start();
    // Every pane is a row (the depth `all`), so that a pane can be gone into.
    mac.write_config("[ui]\ndepth = \"all\"\n\n[machines]\nstudio = \"studio\"\n");
    let far = FarSide::with_session("studio", "garden");
    far.server
        .tmux_ok(&["rename-window", "-t", "garden:0", "main"]);
    far.server
        .tmux_ok(&["new-window", "-d", "-t", "garden:", "-n", "tools"]);
    let env: String = far
        .server
        .taw_env()
        .iter()
        .map(|(name, value)| format!("'{name}={value}' "))
        .collect();
    far.server.tmux_ok(&[
        "bind-key",
        "-n",
        "F5",
        "run-shell",
        &format!(
            "env -u TMUX -u TMUX_PANE {env}'{}' ui open >/dev/null 2>&1",
            taw_bin()
        ),
    ]);
    let mut reached = vec![&far];
    reached.extend(more.iter().copied());
    let stand_in = StandIn::new(&reached);
    Rig {
        term: terminal(),
        mac,
        far,
        stand_in,
        app_log: None,
    }
}

// ---------------------------------------------------------------- reading and waiting

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

// ---------------------------------------------------------------- the app and its record

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

/// The shell's line after the app also writes the terminal's modes (`modes`) before the status: readline's own
/// modes at the prompt are not what the app left, which is what a test of the end reads.
///
/// Types the app into the terminal as run for this Mac, with the stand-in as its ssh, and waits for its record (the
/// guard holds the pid from then on) and for studio's rows. Returns the app's pid.
fn start_app(rig: &Rig) -> u32 {
    let pid = type_app(rig, "app --child");
    expect_screen(rig, "studio's rows on the list", |screen| {
        screen.contains(" taw app") && screen.contains("0:main") && screen.contains("1:tools")
    });
    pid
}

/// Types `taw <args>` into the terminal as [`start_app`] does and waits for the app's record, the guard
/// holding the pid from then on. Returns the app's pid; what the screen shows is the caller's to wait for.
fn type_app(rig: &Rig, args: &str) -> u32 {
    type_app_with(rig, args, true)
}

/// [`type_app`], with the app's standard error in a file (`true`) or on the terminal as a person's is (`false`):
/// the far side's own words of a failed attach are the ssh's standard error, which is the app's.
fn type_app_with(rig: &Rig, args: &str, stderr_in_file: bool) -> u32 {
    let after = format!(
        "stty -a >'{}' 2>&1; echo $rc >'{}'",
        rig.mac.work_dir().join("modes").display(),
        rig.mac.work_dir().join("status").display()
    );
    type_app_then(rig, args, stderr_in_file, &after)
}

/// [`type_app_with`], with what the shell runs once the app has ended (`after`, where `$rc` is the app's status).
fn type_app_then(rig: &Rig, args: &str, stderr_in_file: bool, after: &str) -> u32 {
    let stderr = if stderr_in_file {
        format!("2>'{}'", rig.mac.work_dir().join("err").display())
    } else {
        String::new()
    };
    let bin = rig.stand_in.bin.display().to_string();
    let env: String = rig
        .mac
        .taw_env()
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .chain([("TAW_SSH_BIN", bin.as_str())])
        .chain(rig.app_log.map(|level| ("TAW_LOG", level)))
        .map(|(name, value)| format!("'{name}={value}' "))
        .collect();
    let line = format!(
        "env -u TMUX -u TMUX_PANE {env}'{}' {args} {stderr}; rc=$?; {after}",
        taw_bin()
    );
    // Every caller types into the terminal's fresh shell, once per rig.
    rig.term.wait_for_prompt(&rig.term.pane());
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "-l", &line]);
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "Enter"]);
    // The record first and the guard at once: a failed wait for the rows must not leave the app outside it.
    expect(rig, "one record of an app", || records(&rig.mac).len() == 1);
    let name = records(&rig.mac).remove(0);
    let pid: u32 = name.trim_end_matches(".json").parse().expect("a pid");
    own(pid);
    pid
}

/// The app's record, read as JSON (null while it cannot be read).
fn record(rig: &Rig, pid: u32) -> serde_json::Value {
    std::fs::read_to_string(rig.mac.state_dir().join("apps").join(format!("{pid}.json")))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null)
}

// ---------------------------------------------------------------- the stand-in's attach calls

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

/// Goes into studio at `main`, moves studio's own client to the window `tools` with tmux's own `select-window`,
/// and presses the popup key there: the app is back on its list. Returns the pane id of `tools`.
fn went_in_moved_and_back(rig: &Rig, pid: u32) -> String {
    go_in_at(rig, "0:main");
    let tools = rig.far.server.pane_of("garden:tools");
    rig.far
        .server
        .tmux_ok(&["select-window", "-t", "garden:tools"]);
    expect_screen(rig, "studio's client on the window tools", |screen| {
        screen.contains("1:tools*")
    });
    press(&rig.term, &["F5"]);
    expect_screen(rig, "the list again", |screen| screen.contains(" taw app"));
    expect(rig, "the record without a client", || {
        record(rig, pid)["client_pid"] == 0
    });
    tools
}

/// Stops `pid` (the stand-in's feed, a process of this test's own fixture) and sets it going again when
/// dropped, so that it can end with its connection.
struct Resume(u32, String);

impl Resume {
    fn stop(pid: u32) -> Resume {
        let started = start_time_of(pid);
        assert!(!started.is_empty(), "no process {pid}");
        // SAFETY: a pid the stand-in recorded for its own call, just checked to be there.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGSTOP) };
        Resume(pid, started)
    }
}

impl Drop for Resume {
    fn drop(&mut self) {
        if alive(self.0) && start_time_of(self.0) == self.1 {
            // SAFETY: the same process, seen by its start time.
            unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGCONT) };
        }
    }
}

/// Ends the process `pid` that the stand-in recorded, after checking it is the one the call started (its command
/// line has `expect`).
fn kill_recorded(pid: u32, expect: &str) {
    let args = args_of(pid);
    assert!(args.contains(expect), "pid {pid} is {args:?}, not {expect}");
    // SAFETY: a pid the stand-in recorded for its own call, checked above.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
}

// ---------------------------------------------------------------- an edit of [machines] while the app runs

/// The config with `studio` and, when `pi` is given, `pi`, as a person edits it, under the depth `all`.
fn machines_config(extra: &str, pi: bool) -> String {
    format!(
        "[ui]\ndepth = \"all\"\n\n[machines]\nstudio = \"studio\"\n{}{extra}",
        if pi { "pi = \"pi\"\n" } else { "" }
    )
}

/// The calls of the stand-in made for `destination`, with the pid of each: one connection (or one command) each.
fn calls_for(rig: &Rig, destination: &str) -> Vec<u32> {
    let calls = rig.stand_in.calls();
    let pids = rig.stand_in.pids();
    calls
        .iter()
        .zip(pids)
        .filter(|(call, _)| call.iter().any(|word| word == destination))
        .map(|(_, pid)| pid)
        .collect()
}

/// A machine added to the config shows its rows without a restart, and studio's connection is not started
/// again; the machine removed again takes its rows away and its ssh ends.
#[test]
fn a_machine_added_to_the_config_shows_while_the_app_runs_and_one_removed_goes() {
    let _own = Owned::new();
    let pi = FarSide::with_session("pi", "orchard");
    let rig = rig_with(&[&pi]);
    start_app(&rig);
    let studio = calls_for(&rig, "studio");
    assert!(!studio.is_empty(), "studio is connected\n{}", seen(&rig));
    assert!(calls_for(&rig, "pi").is_empty(), "pi is not listed yet");

    rig.mac.write_config(&machines_config("", true));
    expect_screen(&rig, "pi's session on the list", |screen| {
        screen.contains("orchard")
    });
    assert_eq!(
        calls_for(&rig, "studio"),
        studio,
        "studio's connection was not started again\n{}",
        seen(&rig)
    );
    let started = calls_for(&rig, "pi");
    assert_eq!(started.len(), 1, "one connection to pi\n{}", seen(&rig));
    assert!(alive(started[0]), "pi's ssh runs");
    assert!(
        terminal_screen(&rig.term).contains("0:main"),
        "studio's rows stay"
    );

    rig.mac.write_config(&machines_config("", false));
    expect_screen(&rig, "pi's rows gone", |screen| {
        !screen.contains("orchard") && screen.contains("0:main")
    });
    expect(&rig, "pi's ssh ended", || !alive(started[0]));
    assert_eq!(
        calls_for(&rig, "studio"),
        studio,
        "studio's was kept\n{}",
        seen(&rig)
    );
}

/// A `[machines]` that cannot be told is said once, as at start, and studio goes on as it was; the file
/// fixed is applied.
#[test]
fn a_bad_edit_of_machines_is_said_and_changes_nothing_and_the_fixed_file_applies() {
    let _own = Owned::new();
    let pi = FarSide::with_session("pi", "orchard");
    let rig = rig_with(&[&pi]);
    start_app(&rig);
    let studio = calls_for(&rig, "studio");

    rig.mac
        .write_config(&machines_config("pi = \"pi\"\n[broken\n", false));
    expect_screen(&rig, "the doubt's words", |screen| {
        screen.contains("could not be read") && screen.contains("config.toml")
    });
    assert!(
        terminal_screen(&rig.term).contains("0:main"),
        "studio's rows stay"
    );
    assert_eq!(
        calls_for(&rig, "studio"),
        studio,
        "not started again\n{}",
        seen(&rig)
    );
    assert!(
        calls_for(&rig, "pi").is_empty(),
        "pi was not started\n{}",
        seen(&rig)
    );

    rig.mac.write_config(&machines_config("", true));
    expect_screen(&rig, "pi's session once the file is fixed", |screen| {
        screen.contains("orchard")
    });
    assert_eq!(
        calls_for(&rig, "studio"),
        studio,
        "studio's was kept\n{}",
        seen(&rig)
    );
}

/// An edit of the config outside `[machines]` starts and ends no connection. The app says in its log when it has
/// read the file and found the machines as they were: that is the positive sign that the edit was seen.
#[test]
fn an_edit_outside_machines_touches_no_connection() {
    let _own = Owned::new();
    let mut rig = rig();
    rig.app_log = Some("info");
    start_app(&rig);
    let studio = calls_for(&rig, "studio");
    let before = rig.stand_in.calls().len();
    let log = || std::fs::read_to_string(rig.mac.work_dir().join("err")).unwrap_or_default();
    assert!(
        !log().contains(CONFIG_UNCHANGED_LOG),
        "not read yet\n{}",
        seen(&rig)
    );

    rig.mac
        .write_config(&machines_config("\n# a note\n", false));
    expect(&rig, "the app reading the edited file", || {
        log().contains(CONFIG_UNCHANGED_LOG)
    });

    assert!(
        terminal_screen(&rig.term).contains("0:main"),
        "studio's rows stay"
    );
    assert_eq!(
        rig.stand_in.calls().len(),
        before,
        "no call of any kind\n{}",
        seen(&rig)
    );
    assert!(
        studio.iter().all(|pid| alive(*pid)),
        "studio's ssh runs\n{}",
        seen(&rig)
    );
}

/// The line of `screen` that is a machine's header: the first whose words include `name`.
fn header_line(screen: &str, name: &str) -> Option<usize> {
    screen.lines().position(|line| {
        line.split(|c: char| !c.is_alphanumeric())
            .any(|word| word == name)
    })
}

/// Whether `session` is listed under the header of `machine` and above the header of `next` (or to the end).
fn listed_under(screen: &str, machine: &str, session: &str, next: Option<&str>) -> bool {
    let (Some(header), Some(at)) = (
        header_line(screen, machine),
        screen.lines().position(|line| line.contains(session)),
    ) else {
        return false;
    };
    let end = next
        .and_then(|name| header_line(screen, name))
        .unwrap_or(usize::MAX);
    header < at && at < end
}

/// Three machines kept by one app, `studio` (as the rig has it), `pi` and `mini`, each with a session of its
/// own: `garden`, `orchard`, `meadow`.
fn three_machines() -> (Rig, FarSide, FarSide) {
    let pi = FarSide::with_session("pi", "orchard");
    let mini = FarSide::with_session("mini", "meadow");
    let rig = rig_with(&[&pi, &mini]);
    rig.mac
        .write_config(&three_config(&["studio", "pi", "mini"]));
    (rig, pi, mini)
}

/// The config with the machines `names` in this order, each reached as its own name.
fn three_config(names: &[&str]) -> String {
    let lines: String = names
        .iter()
        .map(|name| format!("{name} = \"{name}\"\n"))
        .collect();
    format!("[ui]\ndepth = \"all\"\n\n[machines]\n{lines}")
}

/// A machine before the others removed while the app runs: every machine left keeps its sessions under its own
/// header, and a session made on the last one afterwards is listed under it (what it says still lands).
#[test]
fn removing_the_first_of_three_machines_keeps_the_others_sessions_under_their_own_headers() {
    let _own = Owned::new();
    let (rig, _pi, mini) = three_machines();
    start_app(&rig);
    expect_screen(&rig, "pi's and mini's sessions", |screen| {
        listed_under(screen, "pi", "orchard", Some("mini"))
            && listed_under(screen, "mini", "meadow", None)
    });

    rig.mac.write_config(&three_config(&["pi", "mini"]));
    expect_screen(&rig, "studio's rows gone", |screen| {
        !screen.contains("garden") && screen.contains("orchard") && screen.contains("meadow")
    });
    let screen = terminal_screen(&rig.term);
    assert!(
        listed_under(&screen, "pi", "orchard", Some("mini"))
            && listed_under(&screen, "mini", "meadow", None),
        "each machine's session is under its own header\n{}",
        seen(&rig)
    );

    mini.server.tmux_ok(&["new-session", "-d", "-s", "grove"]);
    expect_screen(&rig, "the session made on mini after the edit", |screen| {
        listed_under(screen, "mini", "grove", None)
    });
    assert!(
        listed_under(&terminal_screen(&rig.term), "pi", "orchard", Some("mini")),
        "pi's stays under pi\n{}",
        seen(&rig)
    );
}

/// The order of the machines changed while the app runs: each keeps its sessions under its own header, and a
/// session made on the machine now last is listed under it.
#[test]
fn reordering_three_machines_keeps_each_ones_sessions_under_its_own_header() {
    let _own = Owned::new();
    let (rig, _pi, _mini) = three_machines();
    start_app(&rig);
    expect_screen(&rig, "pi's and mini's sessions", |screen| {
        listed_under(screen, "pi", "orchard", Some("mini"))
            && listed_under(screen, "mini", "meadow", None)
    });

    rig.mac
        .write_config(&three_config(&["mini", "pi", "studio"]));
    expect_screen(&rig, "the new order", |screen| {
        listed_under(screen, "mini", "meadow", Some("pi"))
            && listed_under(screen, "pi", "orchard", Some("studio"))
            && listed_under(screen, "studio", "garden", None)
    });

    rig.far
        .server
        .tmux_ok(&["new-session", "-d", "-s", "terrace"]);
    expect_screen(&rig, "the session made on studio, now last", |screen| {
        listed_under(screen, "studio", "terrace", None)
    });
    assert!(
        listed_under(&terminal_screen(&rig.term), "mini", "meadow", Some("pi")),
        "mini's stays under mini\n{}",
        seen(&rig)
    );
}

// ---------------------------------------------------------------- the tests

/// (a) Enter on studio's pane row puts studio's session on the terminal, through ssh with a terminal, running
/// `machine attach <the pane> --holder <the app's holder>`; while it runs the app's record names the client, its
/// terminal and the machine, and after it none.
#[test]
fn enter_on_a_pane_row_of_studio_puts_studio_on_the_terminal() {
    let _own = Owned::new();
    let rig = rig();
    let pid = start_app(&rig);
    let main = rig.far.server.pane_of("garden:main");

    go_in_at(&rig, "0:main");

    let holder = record(&rig, pid)["holder"].as_str().unwrap().to_string();
    let found = attaches(&rig.stand_in);
    assert_eq!(found.len(), 1, "one attach\n{}", seen(&rig));
    let call = &found[0].1;
    assert_eq!(target_of(call), main, "the pane of the row\n{}", seen(&rig));
    assert!(
        call.last()
            .unwrap()
            .ends_with(&format!("'attach' '{main}' '--holder' '{holder}'")),
        "{call:?}"
    );
    expect(&rig, "the record naming the client and the machine", || {
        let record = record(&rig, pid);
        record["client_pid"].as_u64().is_some_and(|pid| pid > 1)
            && record["client_tty"]
                .as_str()
                .is_some_and(|tty| tty.starts_with("/dev/"))
            && record["inside"] == "studio"
    });

    // The popup key, on studio's own server, ends it: the record has no client and no machine.
    press(&rig.term, &["F5"]);
    expect_screen(&rig, "the list again", |screen| screen.contains(" taw app"));
    expect(&rig, "the record without the attach", || {
        let record = record(&rig, pid);
        record["client_pid"] == 0 && record["client_tty"] == "" && record["inside"] == ""
    });
}

/// `--go studio:%99`, a pane studio does not have: the app says `studio:%99 is gone` and shows the list with
/// studio's rows, and nothing is attached (no attach call reaches the stand-in): the machine is live and its
/// state, which the list shows, has no such pane, so the far side is not asked to be the one to refuse.
#[test]
fn go_to_an_id_studio_does_not_have_says_it_is_gone_and_shows_the_list() {
    let _own = Owned::new();
    let rig = rig();
    let _pid = type_app(&rig, "app --child --go studio:%99");
    let gone = words::gone("studio:%99");
    expect_screen(&rig, "the gone words and studio's rows", |screen| {
        screen.contains(&gone) && screen.contains("0:main") && screen.contains("1:tools")
    });
    assert!(
        attaches(&rig.stand_in).is_empty(),
        "nothing was attached\n{}",
        seen(&rig)
    );
    press(&rig.term, &["q"]);
}

/// `--go studio:<id>` with an id studio's state holds attaches that id as it is, with studio's session on the
/// terminal: `status_has` is what its status line then shows, `id` is read off studio's own server.
fn go_attaches_the_id_the_state_holds(id: impl Fn(&Rig) -> String, status_has: &str) {
    let _own = Owned::new();
    let rig = rig();
    let id = id(&rig);
    let _pid = type_app(&rig, &format!("app --child --go 'studio:{id}'"));
    expect(&rig, "the attach of the id", || {
        attaches(&rig.stand_in).len() == 1
    });
    let found = attaches(&rig.stand_in);
    assert_eq!(target_of(&found[0].1), id, "{}", seen(&rig));
    expect_screen(&rig, "studio's session on the terminal", |screen| {
        screen.contains(status_has)
    });
    assert!(
        !terminal_screen(&rig.term).contains(&words::gone(&format!("studio:{id}"))),
        "{}",
        seen(&rig)
    );
    press(&rig.term, &["F5"]);
    expect_screen(&rig, "the list again", |screen| screen.contains(" taw app"));
    press(&rig.term, &["q"]);
}

/// `--go studio:$N`, the id of studio's session: attached by that id, and studio's session is on the terminal.
#[test]
fn go_to_the_session_id_studio_has_attaches_it() {
    go_attaches_the_id_the_state_holds(
        |rig| {
            rig.far
                .server
                .tmux_ok(&["display-message", "-p", "-t", "garden", "#{session_id}"])
        },
        "[garden]",
    );
}

/// `--go studio:@N`, the id of a window of studio's (`tools`, which is not the one its client would show): attached
/// by that id, and studio's client is on that window.
#[test]
fn go_to_the_window_id_studio_has_attaches_that_window() {
    go_attaches_the_id_the_state_holds(
        |rig| {
            rig.far.server.tmux_ok(&[
                "display-message",
                "-p",
                "-t",
                "garden:tools",
                "#{window_id}",
            ])
        },
        "1:tools*",
    );
}

/// (b) Studio's popup key returns to the list with the cursor on the pane you were in: studio's own client is
/// moved to another window first, so the cursor is not simply where it was.
#[test]
fn studios_popup_key_returns_to_the_list_with_the_cursor_on_the_pane_you_were_in() {
    let _own = Owned::new();
    let rig = rig();
    let pid = start_app(&rig);

    went_in_moved_and_back(&rig, pid);

    expect(&rig, "the cursor on the pane of tools", || {
        selected_row(&rig.term).contains("1:tools")
    });
    assert_eq!(far_clients(&rig), 0, "studio's client is gone");
    // Studio's tmux client said its exit line (`[detached (from session garden)]`) through ssh onto the shell's
    // screen behind the list. That screen is read where the typed command is, so an empty read fails.
    let behind = rig
        .term
        .tmux(&["capture-pane", "-a", "-p", "-J", "-t", &rig.term.pane()])
        .unwrap_or_default();
    assert!(
        behind.contains("app --child"),
        "the shell's screen behind the list was not read:\n{behind}"
    );
    assert!(
        !behind.contains("[detached"),
        "studio's exit line is on the shell's screen behind the list:\n{behind}"
    );
}

/// The lease on studio's side, as studio's taw keeps it: its holder and when it was last refreshed.
fn far_lease(rig: &Rig) -> Option<(String, u64)> {
    let text = std::fs::read_to_string(
        rig.far
            .server
            .state_dir()
            .join("machines")
            .join("lease.json"),
    )
    .ok()?;
    let lease: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some((
        lease["holder"].as_str()?.to_string(),
        lease["refreshed_at"].as_u64()?,
    ))
}

/// An app reopened a moment after the last one quit takes studio's lease at once, not 15 s after the quit app
/// last refreshed it, and studio's popup key brings its list back.
#[test]
fn an_app_reopened_at_once_takes_the_lease_and_its_key_back_works() {
    let _own = Owned::new();
    let rig = rig();
    // Two apps one after the other in the terminal's shell: the second starts as soon as `q` ends the first, and
    // goes into studio's session by its id.
    let session =
        rig.far
            .server
            .tmux_ok(&["display-message", "-p", "-t", "garden", "#{session_id}"]);
    let env: String = rig
        .mac
        .taw_env()
        .iter()
        .map(|(name, value)| format!("'{name}={value}' "))
        .collect();
    let first = type_app(
        &rig,
        &format!(
            "app --child 2>/dev/null; env -u TMUX -u TMUX_PANE {env}'TAW_SSH_BIN={}' '{}' app --child --go 'studio:{}'",
            rig.stand_in.bin.display(),
            taw_bin(),
            session.trim()
        ),
    );
    expect_screen(&rig, "studio's rows on the first app's list", |screen| {
        screen.contains("0:main")
    });
    let holder_of = |pid: u32| {
        record(&rig, pid)["holder"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    let first_holder = holder_of(first);
    // The last refresh of the first app's lease that studio's file showed, read while the app runs: an
    // app that quits gives its lease up, and the file goes with it.
    let mut first_refreshed = 0;
    expect(
        &rig,
        "studio's lease held by the first app",
        || match far_lease(&rig) {
            Some((holder, at)) if holder == first_holder => {
                first_refreshed = at;
                true
            }
            _ => false,
        },
    );

    press(&rig.term, &["q"]);
    let mut second = 0;
    expect(&rig, "the second app's record", || {
        second = records(&rig.mac)
            .iter()
            .filter_map(|name| name.trim_end_matches(".json").parse().ok())
            .find(|pid| *pid != first)
            .unwrap_or(0);
        second != 0
    });
    own(second);
    let second_holder = holder_of(second);
    assert_ne!(second_holder, first_holder);
    // A later refresh of the first app's lease, if the file still showed one, and when the second app took it.
    let mut taken_at = 0;
    expect(&rig, "studio's lease held by the second app", || {
        match far_lease(&rig) {
            Some((holder, at)) if holder == first_holder => {
                first_refreshed = first_refreshed.max(at)
            }
            Some((holder, at)) if holder == second_holder => taken_at = at,
            _ => {}
        }
        taken_at != 0
    });
    assert!(
        first_refreshed != 0 && taken_at < first_refreshed + 15_000,
        "the lease was taken {} ms after the quit app last refreshed it\n{}",
        taken_at.saturating_sub(first_refreshed),
        seen(&rig)
    );

    expect_screen(&rig, "studio's session on the terminal", |screen| {
        screen.contains("[garden]")
    });
    expect(&rig, "studio's server having the client", || {
        far_clients(&rig) == 1
    });
    press(&rig.term, &["F5"]);
    expect(&rig, "studio's client sent back by the key", || {
        far_clients(&rig) == 0
    });
    expect_screen(&rig, "the list again", |screen| screen.contains(" taw app"));
    press(&rig.term, &["q"]);
}

/// (c) `Esc` then goes back into studio, at the pane the key came from.
#[test]
fn esc_goes_back_into_studio_at_the_pane_you_came_from() {
    let _own = Owned::new();
    let rig = rig();
    let pid = start_app(&rig);
    let tools = went_in_moved_and_back(&rig, pid);
    expect(&rig, "the cursor on the pane of tools", || {
        selected_row(&rig.term).contains("1:tools")
    });

    press(&rig.term, &["Escape"]);

    expect(&rig, "a second attach", || {
        attaches(&rig.stand_in).len() == 2
    });
    expect(&rig, "studio's client again", || far_clients(&rig) == 1);
    assert_eq!(
        target_of(&attaches(&rig.stand_in)[1].1),
        tools,
        "Back goes in at the pane of tools\n{}",
        seen(&rig)
    );
    expect_screen(&rig, "studio's status line", |screen| {
        screen.contains("[garden]")
    });
}

/// (d) A session header attaches the session.
#[test]
fn a_session_row_of_studio_attaches_the_session() {
    let _own = Owned::new();
    let rig = rig();
    let _pid = start_app(&rig);
    let session =
        rig.far
            .server
            .tmux_ok(&["display-message", "-p", "-t", "garden", "#{session_id}"]);

    go_in_at(&rig, "garden");

    let found = attaches(&rig.stand_in);
    assert_eq!(found.len(), 1, "one attach\n{}", seen(&rig));
    assert_eq!(
        target_of(&found[0].1),
        session,
        "the session\n{}",
        seen(&rig)
    );
}

/// (d) A closed worktree of studio, a real one made by studio's own `taw`, is opened (the stand-in's calls show
/// `worktree open ... --no-focus` before the attach) and the session it made is attached: that session's
/// own id, which is not the id the worktree's session had when it was created.
#[test]
fn a_closed_worktree_of_studio_is_opened_and_the_session_it_made_is_attached() {
    let _own = Owned::new();
    let rig = rig();
    // A repository in a session of studio of its own, started in the folder as git spells it
    // (a temp folder is a link here, and the record keeps the resolved path).
    let work = rig.far.server.work_dir().canonicalize().unwrap();
    let git = |args: &[&str]| {
        // Nothing of the person running the tests reaches git: no global or system config (which may sign a
        // commit with their key), a name of its own, and no signing.
        let status = taw::git::command()
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
                "-c",
                "user.name=taw-test",
                "-c",
                "user.email=taw-test@example.invalid",
            ])
            .arg("-C")
            .arg(&work)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "first"]);
    rig.far.server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "orchard",
        "-c",
        &work.display().to_string(),
    ]);
    let checkouts = tempfile::tempdir().unwrap();
    rig.far.server.write_config(&format!(
        "[worktrees]\ndirectory = \"{}\"\n",
        checkouts.path().display()
    ));
    let (code, created) = rig.far.server.taw(&[
        "worktree",
        "create",
        "--workspace",
        "orchard",
        "--branch",
        "feat/x",
        "--no-focus",
    ]);
    assert_eq!(code, 0, "{created}");
    let linked = created["result"]["worktree"]["session_name"]
        .as_str()
        .unwrap()
        .to_string();
    let first_id = rig.far.server.tmux_ok(&[
        "display-message",
        "-p",
        "-t",
        &format!("={linked}:"),
        "#{session_id}",
    ]);
    // Closed: the session goes, the record stays, and studio's list shows the parked worktree.
    rig.far
        .server
        .tmux_ok(&["kill-session", "-t", &format!("={linked}")]);
    let _pid = start_app(&rig);
    expect_screen(&rig, "the closed worktree on the list", |screen| {
        screen.contains("feat/x")
    });
    move_to_in(&rig, "3", "orchard", "feat/x");

    press(&rig.term, &["Enter"]);

    expect(&rig, "the attach", || !attaches(&rig.stand_in).is_empty());
    let sessions =
        rig.far
            .server
            .tmux_ok(&["list-sessions", "-F", "#{session_name} #{session_id}"]);
    let running = sessions
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{linked} ")))
        .unwrap_or_else(|| panic!("{linked} is not running:\n{sessions}"))
        .to_string();
    let calls = rig.stand_in.calls();
    let found = attaches(&rig.stand_in);
    let opened = calls
        .iter()
        .position(|call| {
            call.last()
                .is_some_and(|line| line.contains("'worktree' 'open' '--workspace'"))
        })
        .unwrap_or_else(|| panic!("no worktree open\n{}", seen(&rig)));
    assert!(
        calls[opened].last().unwrap().ends_with("'--no-focus'"),
        "{:?}",
        calls[opened]
    );
    assert!(opened < found[0].0, "the open comes before the attach");
    assert_eq!(
        target_of(&found[0].1),
        running,
        "the session the open made\n{}",
        seen(&rig)
    );
    // A session made by the open takes the next id, which no session had before.
    assert_ne!(running, first_id, "not the id it had when it was created");
}

/// No git that the closed-worktree test runs reaches the repository that git's variables of whoever runs it point at
/// (git runs the command of a `rebase -x` with `GIT_DIR` set): the test runs again with each of the variables pointing
/// into a decoy, passes and leaves it byte for byte as it was (tests/common/git_decoy.rs). The far taw that the
/// stand-in starts (`worktree open`) runs under `env -i` and does not see the decoy's variables; the one the test
/// runs itself (`worktree create`) does see them, and must leave the decoy alone (`taw::git::command()` takes
/// them out of the git it starts).
#[test]
fn the_test_that_makes_a_repository_leaves_the_one_gits_variables_point_at_as_it_was() {
    common::git_decoy::rerun(
        &["a_closed_worktree_of_studio_is_opened_and_the_session_it_made_is_attached"],
        "leaves_the_one_gits_variables_point_at",
    );
}

/// (e) A target that is no pane (its window killed on studio after the list was drawn: studio's feed is stopped
/// so that the list stays as it was) says the words of a refused attach and leaves the list drawn.
#[test]
fn a_target_that_is_no_pane_says_so_and_leaves_the_list_drawn() {
    let _own = Owned::new();
    let rig = rig();
    let _pid = start_app(&rig);
    move_to(&rig, "1:tools");
    expect(&rig, "the feed's call", || !rig.stand_in.pids().is_empty());
    let feed = rig.stand_in.pids()[0];
    assert!(args_of(feed).contains("feed"), "{}", args_of(feed));
    let _resume = Resume::stop(feed);
    rig.far
        .server
        .tmux_ok(&["kill-window", "-t", "garden:tools"]);

    press(&rig.term, &["Enter"]);

    let said = words::attach_ended("studio", Some(2), None).unwrap();
    expect_screen(&rig, "the words of a refused attach", |screen| {
        screen.contains(&said)
    });
    // The words are held on the plain terminal until a key (the held line says so); Enter brings the list back.
    expect_screen(&rig, "the held line", |screen| {
        screen.contains(&words::attach_held(&said))
    });
    press(&rig.term, &["Enter"]);
    expect_screen(&rig, "the list", |screen| screen.contains("Enter go in"));
    // The positive half is the attach that was run and refused; the list is drawn.
    assert_eq!(attaches(&rig.stand_in).len(), 1, "{}", seen(&rig));
    let screen = terminal_screen(&rig.term);
    assert!(
        screen.contains("Enter go in") && screen.contains("0:main"),
        "{screen}"
    );
    assert_eq!(far_clients(&rig), 0);
}

/// (f) Studio made unreachable while attached (the attach's process killed by its pid, connections refused from
/// then on) brings the list back with words that say it did not answer, and studio is offline.
#[test]
fn studio_made_unreachable_while_attached_brings_the_list_back() {
    let _own = Owned::new();
    let rig = rig();
    let _pid = start_app(&rig);
    go_in_at(&rig, "0:main");
    let (at, _) = attaches(&rig.stand_in).remove(0);
    let pids = rig.stand_in.pids();
    let (feed, attach) = (pids[0], pids[at]);
    rig.stand_in.fail_as("studio", Failure::Refused);

    kill_recorded(feed, "feed");
    kill_recorded(attach, "attach");

    let said = words::attach_ended("studio", None, Some(libc::SIGKILL)).unwrap();
    expect_screen(
        &rig,
        "the words of an attach that lost its machine, held",
        |screen| screen.contains(&words::attach_held(&said)),
    );
    press(&rig.term, &["Enter"]);
    expect_screen(&rig, "the list", |screen| screen.contains(" taw app"));
    expect_screen(
        &rig,
        "studio's header says it is away",
        studio_header_is_away,
    );
}

/// Whether the list's header of studio says it is away: its dot and name with the words of a machine
/// that is not live (the top line names no machine).
fn studio_header_is_away(screen: &str) -> bool {
    screen.lines().any(|line| {
        line.contains("\u{25cf} studio")
            && (line.contains("not answering") || line.contains("connecting"))
    })
}

/// (g) A machine shown away (its feed stopped, so the connection stays and its rows stay, but nothing was heard
/// from it for 15 s): Enter says the not-connected words and runs nothing. Two checks stand in the way (the row's
/// refusal in the list, and `machine_to_enter` at the attach, whose own unit tests pin it; the window between a
/// tick and the key, where only the second answers, cannot be hit from outside): this test fails when both are
/// taken out, and each alone leaves the same words. The feed's own reconnect calls are allowed meanwhile.
#[test]
fn enter_on_a_machine_that_is_away_says_so_and_attaches_nothing() {
    let _own = Owned::new();
    let rig = rig();
    let _pid = start_app(&rig);
    expect(&rig, "the feed's call", || !rig.stand_in.pids().is_empty());
    let feed = rig.stand_in.pids()[0];
    assert!(args_of(feed).contains("feed"), "{}", args_of(feed));
    move_to(&rig, "1:tools");
    let _resume = Resume::stop(feed);
    rig.stand_in.fail_as("studio", Failure::Refused);
    // Nothing is heard for 15 s, so the header says studio is offline, and the row is still there.
    assert!(
        wait_for(
            &mut || studio_header_is_away(&terminal_screen(&rig.term)),
            Duration::from_secs(40)
        ),
        "studio never went offline\n{}",
        seen(&rig)
    );
    assert!(
        terminal_screen(&rig.term).contains("1:tools"),
        "{}",
        seen(&rig)
    );

    press(&rig.term, &["Enter"]);

    let away = words::not_connected("studio", 0);
    let prefix = away.split(" (last seen").next().unwrap().to_string();
    // Only the toast has "(last seen": the preview beside the cursor row says `studio is not connected` before
    // the key is read, so waiting for the prefix would not tell that Enter was answered.
    expect_screen(&rig, "the toast with the not-connected words", |screen| {
        screen.contains(&format!("{prefix} (last seen"))
    });
    let ran: Vec<String> = rig
        .stand_in
        .calls()
        .iter()
        .filter_map(|call| call.last().cloned())
        .filter(|line| line.contains("'attach'") || line.contains("'worktree' 'open'"))
        .collect();
    assert!(ran.is_empty(), "Enter ran {ran:?}\n{}", seen(&rig));
    assert_eq!(far_clients(&rig), 0);
}

// ---------------------------------------------------------------- a failed attach keeps its words

/// What the far tmux client says when it cannot start, as the stand-in prints it on standard error.
const FAR_WORDS: &str = "missing or unsuitable terminal: xterm-ghostty";

/// The held line of an attach of studio that ended with `status`.
fn held_line(status: i32) -> String {
    words::attach_held(&words::attach_ended("studio", Some(status), None).unwrap())
}

/// A failed attach (the far tmux client says `missing or unsuitable terminal` and ends 1) leaves the far words
/// and one line under them on the plain terminal, and the app waits there: the list is not drawn over them.
/// Enter takes the terminal back and draws the list, with the toast of the failure.
#[test]
fn a_failed_attach_keeps_the_far_words_on_the_screen_until_enter() {
    let _own = Owned::new();
    let rig = rig();
    // The app's standard error is the terminal, as a person's is: the ssh's words land on the screen.
    let _pid = type_app_with(&rig, "app --child", false);
    expect_screen(&rig, "studio's rows on the list", |screen| {
        screen.contains(" taw app") && screen.contains("0:main") && screen.contains("1:tools")
    });
    rig.stand_in.fail_attach(1, FAR_WORDS);
    move_to(&rig, "0:main");

    press(&rig.term, &["Enter"]);

    let held = held_line(1);
    expect_screen(&rig, "the far words and the held line", |screen| {
        screen.contains(FAR_WORDS) && screen.contains(&held)
    });
    // The positive half is above. Held: a beat later the screen is still the words, and not the list.
    std::thread::sleep(Duration::from_millis(600));
    let screen = terminal_screen(&rig.term);
    assert!(
        screen.contains(FAR_WORDS) && screen.contains(&held) && !screen.contains(" taw app"),
        "{}",
        seen(&rig)
    );
    assert_eq!(attaches(&rig.stand_in).len(), 1, "{}", seen(&rig));

    press(&rig.term, &["Enter"]);

    let said = words::attach_ended("studio", Some(1), None).unwrap();
    expect_screen(&rig, "the list again with the toast", |screen| {
        // The toast takes the place of the title on the top line.
        screen.contains("Enter go in") && screen.contains(&said) && !screen.contains(FAR_WORDS)
    });
}

/// An attach that ends well holds nothing: the list is back without a key.
#[test]
fn a_clean_attach_end_holds_nothing() {
    let _own = Owned::new();
    let rig = rig();
    let _pid = start_app(&rig);
    // The far words on standard error, and status 0: nothing failed.
    rig.stand_in.fail_attach(0, "bye");
    move_to(&rig, "0:main");

    press(&rig.term, &["Enter"]);

    expect(&rig, "the attach that was run", || {
        attaches(&rig.stand_in).len() == 1
    });
    expect_screen(&rig, "the list, with no key pressed", |screen| {
        screen.contains(" taw app") && screen.contains("0:main")
    });
    assert!(
        !terminal_screen(&rig.term).contains(words::ATTACH_HELD),
        "{}",
        seen(&rig)
    );
}

/// The wait for Enter ends when the app is told to end: SIGTERM to the app that holds ends it, and no list
/// is drawn.
#[test]
fn a_held_app_told_to_end_ends() {
    let _own = Owned::new();
    let rig = rig();
    let pid = start_app(&rig);
    rig.stand_in.fail_attach(1, FAR_WORDS);
    move_to(&rig, "0:main");
    press(&rig.term, &["Enter"]);
    let held = held_line(1);
    expect_screen(&rig, "the held line", |screen| screen.contains(&held));

    signal(pid, libc::SIGTERM);

    expect(&rig, "the held app ended", || !alive(pid));
    assert!(
        !terminal_screen(&rig.term).contains("Enter go in"),
        "{}",
        seen(&rig)
    );
}

// ---------------------------------------------------------------- the end of an attach

/// Whether the terminal's pane is on the alternate screen (`1` or `0`), or what tmux said.
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

/// Whether the terminal's pane shows its cursor (`1`), or what tmux said.
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

/// The modes of the pane's own tty as `stty -a` says them, split into words (`-icanon` is a word, `icanon` is
/// another).
fn tty_modes(term: &Server) -> Vec<String> {
    let tty = term.tmux_ok(&["display-message", "-p", "-t", &term.pane(), "#{pane_tty}"]);
    // The tty is the shell's input (a redirect), which both BSD and GNU stty read: `-f` is BSD's, `-F` GNU's.
    let out = std::process::Command::new("/bin/sh")
        .args(["-c", "exec stty -a < \"$1\"", "sh", tty.trim()])
        .output()
        .expect("stty runs");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(|word| word.trim_end_matches(';').to_string())
        .collect()
}

/// (h) The app is told to end (`SIGTERM`, as its supervisor passes one on) while an ssh has the terminal that
/// sets it raw, enters the far tmux's alternate screen and mouse mode and ignores `SIGHUP` and `SIGTERM`: the
/// app kills the ssh, and the shell gets the terminal as it would have found it, cooked, off the alternate
/// screen, with the cursor shown. Three things must hold, each pinned by a mutation of the code (the remote flag
/// of `attach` passed as `false`, the put-back of the terminal's modes, the write of the restore bytes).
#[test]
fn an_ssh_that_ignores_the_hangup_is_killed_and_the_terminal_is_given_back_cooked() {
    let _own = Owned::new();
    let rig = rig();
    rig.stand_in.stubborn_attach();
    let pid = start_app(&rig);
    move_to(&rig, "0:main");
    press(&rig.term, &["Enter"]);
    let mut stubborn = None;
    expect(&rig, "the stubborn stand-in's own pid", || {
        stubborn = rig.stand_in.stubborn_pid();
        stubborn.is_some()
    });
    let ssh = stubborn.expect("the wait saw it");
    // The stand-in ends as `sleep 600`, which ignores the hangup: the guard looks for that, not for `taw`.
    own_as(ssh, "sleep 600");
    // The positive half: before the signal the terminal is raw and on the far tmux's alternate screen, with the
    // cursor hidden, and the stand-in runs.
    expect(&rig, "the terminal raw, with the cursor hidden", || {
        tty_modes(&rig.term).contains(&"-icanon".to_string()) && cursor_flag(&rig.term) == "0"
    });
    assert_eq!(alternate_on(&rig.term), "1", "{}", seen(&rig));
    assert!(alive(ssh), "the stand-in's attach is gone\n{}", seen(&rig));

    signal(pid, libc::SIGTERM);

    let status = rig.mac.work_dir().join("status");
    expect(&rig, "the shell having the terminal again", || {
        common::owned::status_of(&status).is_some()
    });
    expect(&rig, "the ssh that ignored the hangup killed", || {
        !alive(ssh)
    });
    expect(&rig, "the terminal off the alternate screen", || {
        alternate_on(&rig.term) == "0" && cursor_flag(&rig.term) == "1"
    });
    let cooked = |modes: &[String]| {
        ["icanon", "echo", "opost", "icrnl"]
            .iter()
            .all(|word| modes.iter().any(|m| m == word))
            && !modes
                .iter()
                .any(|m| m == "-icanon" || m == "-echo" || m == "-opost")
    };
    // What the shell found, written by its own line before the status (readline's own modes at the prompt are
    // not cooked, so the pane's tty is not read now).
    let found: Vec<String> = std::fs::read_to_string(rig.mac.work_dir().join("modes"))
        .unwrap_or_default()
        .split_whitespace()
        .map(|word| word.trim_end_matches(';').to_string())
        .collect();
    assert!(
        found.iter().any(|word| word == "isig") && cooked(&found),
        "the shell found the terminal in modes {found:?}\n{}",
        seen(&rig)
    );
    // This sleep follows the positive waits above: something that changed the terminal late would show now.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(alternate_on(&rig.term), "0", "{}", seen(&rig));
    assert_eq!(cursor_flag(&rig.term), "1", "{}", seen(&rig));
}

/// The pids of the processes whose parent is `parent`, with their command lines, as `ps` lists them.
fn children_of(parent: u32) -> Vec<(u32, String)> {
    let out = std::process::Command::new("/bin/ps")
        .args(["-Ao", "pid=,ppid=,args="])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid: u32 = words.next()?.parse().ok()?;
            let ppid: u32 = words.next()?.parse().ok()?;
            (ppid == parent).then(|| (pid, words.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

/// Types the supervised app (`taw app`) into the terminal with the stand-in's stubborn attach, goes into studio
/// and waits for the ssh. Returns the child's pid, the supervisor's, and the ssh's.
fn supervised_attached(rig: &Rig) -> (u32, u32, u32) {
    rig.stand_in.stubborn_attach();
    let child = type_app(rig, "app");
    attached_after_typing(rig, child)
}

/// The rest of [`supervised_attached`], once the app `child` has been typed.
fn attached_after_typing(rig: &Rig, child: u32) -> (u32, u32, u32) {
    expect_screen(rig, "studio's rows on the list", |screen| {
        screen.contains(" taw app") && screen.contains("0:main") && screen.contains("1:tools")
    });
    let supervisor = record(rig, child)["supervisor"]
        .as_u64()
        .expect("the record names the supervisor") as u32;
    own(supervisor);
    move_to(rig, "0:main");
    press(&rig.term, &["Enter"]);
    let mut stubborn = None;
    expect(rig, "the stubborn stand-in's own pid", || {
        stubborn = rig.stand_in.stubborn_pid();
        stubborn.is_some()
    });
    let ssh = stubborn.expect("the wait saw it");
    own_as(ssh, "sleep 600");
    expect(rig, "the terminal raw, the stand-in running", || {
        tty_modes(&rig.term).contains(&"-icanon".to_string()) && alive(ssh)
    });
    (child, supervisor, ssh)
}

/// The app is killed with `SIGKILL` (nothing of its own runs), the supervisor that has the terminal first, while an
/// ssh has the terminal that ignores the hangup too: the attach ends with the app and the shell has the terminal
/// to itself. Before, the ssh stayed, a client of the far tmux beside the shell. Only what the app started goes,
/// by pid: the test reads the app's children before the kill and looks at those.
#[test]
fn an_app_killed_with_sigkill_while_attached_leaves_no_attach_behind() {
    let _own = Owned::new();
    let rig = rig();
    let (child, supervisor, ssh) = supervised_attached(&rig);
    // What the app started besides the ssh: copies of itself (its helper for this attach) must be gone too.
    let helpers: Vec<u32> = children_of(child)
        .into_iter()
        .filter(|(helper, args)| *helper != ssh && args.contains("app --child"))
        .map(|(helper, _)| helper)
        .collect();
    for helper in &helpers {
        own(*helper);
    }

    signal(supervisor, libc::SIGKILL);

    let status = rig.mac.work_dir().join("status");
    expect(&rig, "the shell having the terminal again", || {
        common::owned::status_of(&status).is_some()
    });
    expect(&rig, "the attach ssh ended with the app", || !alive(ssh));
    expect(&rig, "the app child gone", || !alive(child));
    for helper in &helpers {
        expect(&rig, "the app's helper gone", || !alive(*helper));
    }
    // The far tmux's modes the ssh set are put away too.
    expect(&rig, "the terminal off the alternate screen", || {
        alternate_on(&rig.term) == "0" && cursor_flag(&rig.term) == "1"
    });
}

/// Both processes of the app killed with `SIGKILL` at once (the out-of-memory killer, a `kill -9` of the
/// group's leader and the child): nothing of the app runs and the attach still ends.
#[test]
fn an_app_and_its_supervisor_both_killed_leave_no_attach_behind() {
    let _own = Owned::new();
    let rig = rig();
    let (child, supervisor, ssh) = supervised_attached(&rig);

    signal(child, libc::SIGKILL);
    signal(supervisor, libc::SIGKILL);

    let status = rig.mac.work_dir().join("status");
    expect(&rig, "the shell having the terminal again", || {
        common::owned::status_of(&status).is_some()
    });
    expect(&rig, "the attach ssh ended with the app", || !alive(ssh));
}

/// Both processes of the app killed while the stubborn ssh has the terminal, and the shell, which
/// has the terminal back at once, sets modes of its own (as a line editor does) and waits: the guard that kills the
/// ssh a second later leaves those modes alone. Before, it put the modes of the attach back (cooked, echo on) under
/// the shell.
///
/// The order is made firm, so that the old guard fails every time, however slow the shell: the guard is stopped
/// (`SIGSTOP`) once the supervisor is gone, while the ssh still runs, and only goes on (`SIGCONT`) after the shell
/// has set its modes (the status file is written after its `stty`). It is not stopped before the kills: when the
/// supervisor ends, the app's process group is left orphaned, and the kernel then sends a stopped member `SIGCONT`.
#[test]
fn the_guard_leaves_the_modes_of_a_shell_that_has_the_terminal_back_alone() {
    let _own = Owned::new();
    let rig = rig();
    rig.stand_in.stubborn_attach();
    let status = rig.mac.work_dir().join("status");
    let after = format!(
        "stty icanon -echo; echo $rc >'{}'; read -r line",
        status.display()
    );
    let child = type_app_then(&rig, "app", true, &after);
    let (child, supervisor, ssh) = attached_after_typing(&rig, child);
    // The guard is a copy of the app child (its fork), so it has the child's command line.
    let helpers: Vec<u32> = children_of(child)
        .into_iter()
        .filter(|(helper, args)| *helper != ssh && args.contains("app --child"))
        .map(|(helper, _)| helper)
        .collect();
    assert!(!helpers.is_empty(), "the guard among the app's children");
    for helper in &helpers {
        own(*helper);
    }

    signal(child, libc::SIGKILL);
    signal(supervisor, libc::SIGKILL);

    // The guard waits a look for the supervisor to be gone and then eight more before it kills the ssh (0.9 s): the
    // test looks every millisecond, so it holds the guard well before then.
    let until = std::time::Instant::now() + Duration::from_secs(20);
    while alive(supervisor) {
        assert!(
            std::time::Instant::now() < until,
            "the supervisor never went.\n{}",
            seen(&rig)
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    for helper in &helpers {
        signal(*helper, libc::SIGSTOP);
    }
    assert!(
        alive(ssh),
        "the guard killed the ssh before the test could hold it, so the order this test needs was not reached.\n{}",
        seen(&rig)
    );
    expect(&rig, "the shell having set its modes", || {
        common::owned::status_of(&status).is_some()
    });
    assert!(alive(ssh), "the ssh still there while the guard is held");
    for helper in &helpers {
        signal(*helper, libc::SIGCONT);
    }
    expect(&rig, "the attach ssh killed by the guard", || !alive(ssh));
    for helper in &helpers {
        expect(&rig, "the guard ended", || !alive(*helper));
    }
    let modes = tty_modes(&rig.term);
    assert!(
        modes.iter().any(|mode| mode == "-echo") && modes.iter().any(|mode| mode == "icanon"),
        "the shell's modes (icanon -echo) changed under it: {modes:?}\n{}",
        seen(&rig)
    );
}

// ---------------------------------------------------------------- a machine with no tmux

/// studio with its tmux ended: connected, and nothing to press Enter on but the row that starts a session.
fn studio_without_tmux() -> Rig {
    let rig = rig();
    rig.far.server.tmux_ok(&["kill-server"]);
    expect(&rig, "studio's tmux to be gone", || {
        rig.far.server.tmux(&["list-sessions"]).is_err()
    });
    rig
}

/// What the far side's one session is called, `0` for one that tmux numbered.
fn far_sessions(rig: &Rig) -> String {
    rig.far
        .server
        .tmux(&["list-sessions", "-F", "#{session_name}"])
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// A connected machine with no tmux shows the row that starts one, the cursor is on it, and Enter puts a new
/// session of studio's own tmux on the terminal (tmux numbers it: no name was given).
#[test]
fn enter_on_the_row_of_a_machine_with_no_tmux_starts_a_session_there() {
    let _own = Owned::new();
    let rig = studio_without_tmux();
    let _pid = type_app(&rig, "app --child");
    expect_screen(&rig, "the row that starts a session", |screen| {
        screen.contains(words::NO_SESSION_ROW)
    });

    // This Mac's own session is listed above studio's header: the cursor walks down to the row.
    for _ in 0..12 {
        if selected_row(&rig.term).contains(words::NO_SESSION_ROW) {
            break;
        }
        press(&rig.term, &["j"]);
        std::thread::sleep(Duration::from_millis(250));
    }
    expect(&rig, "the cursor on the row", || {
        selected_row(&rig.term).contains(words::NO_SESSION_ROW)
    });
    press(&rig.term, &["Enter"]);

    expect_screen(&rig, "the new session's status line", |screen| {
        screen.contains("[0]")
    });
    expect(&rig, "studio's server having the client", || {
        far_clients(&rig) == 1
    });
    assert_eq!(far_sessions(&rig), "0");
}

/// `--go studio:new` does the same before the first frame: the app starts on that attach.
#[test]
fn go_studio_new_starts_a_session_on_a_machine_with_no_tmux() {
    let _own = Owned::new();
    let rig = studio_without_tmux();
    let _pid = type_app(&rig, "app --child --go studio:new");

    expect_screen(&rig, "the new session's status line", |screen| {
        screen.contains("[0]")
    });
    expect(&rig, "studio's server having the client", || {
        far_clients(&rig) == 1
    });
    assert_eq!(far_sessions(&rig), "0");
}
