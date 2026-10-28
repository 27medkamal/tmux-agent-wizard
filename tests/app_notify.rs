//! Notifications across machines, end to end: the built binary against private tmux servers, the app
//! of this Mac and the REAL far `taw` of the machine `studio` behind the stand-in for ssh, as
//! `tests/app_remote.rs` and `tests/app_nextup.rs` do it.
//!
//! "This Mac" is a private server with a session `one`; `studio` is a second one with a session `garden`
//! (windows `main` and `tools`); the terminal is a third. A real watcher is on the pane of the agent that
//! turns blocked, told of the state by a real `taw pane report-agent`, as `tests/e2e_hook_reports.rs` does it.
//! Both machines have a `[hooks] notification` that is a script of the test's own which only writes
//! `TAW_MACHINE|TAW_MESSAGE` to a file, and a low `[ui.toast] delay_seconds`. No test runs a real ssh.
//!
//! Every wait is positive, with a 20 s deadline, and a check that something did NOT happen comes after a
//! wait for the effect of the same event that did.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::machine::{Failure, FarSide, StandIn};
use common::owned::{
    Owned, alive, args_of, disown, own, own_as, record_names, signal, start_time_of,
};
use common::{Server, ServerOptions, taw_bin, wait_for, write_script};
use taw::machine::{self, words};

/// The agent's name on the toast of a plain event.
const AGENT: &str = "claude";

/// What the list's `session` rows for each machine are, and the rest the tests of a toast share.
struct Rig {
    term: Server,
    mac: Server,
    far: FarSide,
    stand_in: StandIn,
    /// This test's own folder: the hooks' files and the markers.
    scratch: tempfile::TempDir,
}

impl Rig {
    /// The file this Mac's notification hook writes to.
    fn mac_hook(&self) -> PathBuf {
        self.scratch.path().join("mac-hook.txt")
    }

    /// The file studio's notification hook writes to.
    fn far_hook(&self) -> PathBuf {
        self.scratch.path().join("studio-hook.txt")
    }
}

/// A terminal as a person has one: a private server of its own whose pane is the terminal. Wide, so that a toast
/// is not cut.
fn terminal() -> Server {
    Server::start_with(ServerOptions {
        width: 240,
        height: 30,
        ..ServerOptions::default()
    })
}

/// `text` as a TOML basic string.
fn toml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A config with a low toast delay, no sound, and a `[hooks] notification` that is `script`; `machines` adds
/// `[machines]` with studio, and the depth `all` so that every pane is a row.
fn config(script: &Path, machines: bool) -> String {
    let mut text = format!(
        "[update]\nmanifest_check = false\n\n[ui]\ndepth = \"all\"\n\n[ui.toast]\ndelay_seconds = 0.3\n\
         done_delay_seconds = 3600\n\n[ui.sound]\nenabled = false\n\n[hooks]\nnotification = {}\n",
        toml_string(&script.display().to_string())
    );
    if machines {
        text.push_str("\n[machines]\nstudio = \"studio\"\n");
    }
    text
}

/// A hook script that appends `<TAW_MACHINE>|<TAW_MESSAGE>` to `log` and runs nothing else.
fn recorder(dir: &Path, name: &str, log: &Path) -> PathBuf {
    let script = dir.join(name);
    write_script(
        &script,
        &format!(
            "printf '%s|%s\\n' \"$TAW_MACHINE\" \"$TAW_MESSAGE\" >> '{}'\n",
            log.display()
        ),
    );
    script
}

fn rig() -> Rig {
    let scratch = tempfile::tempdir().unwrap();
    let mac = Server::start();
    let far = FarSide::with_session("studio", "garden");
    far.server
        .tmux_ok(&["rename-window", "-t", "garden:0", "main"]);
    far.server
        .tmux_ok(&["new-window", "-d", "-t", "garden:", "-n", "tools"]);
    // A message stays on a status line for a long time, so that a screen read finds it.
    for server in [&mac, &far.server] {
        server.tmux_ok(&["set-option", "-g", "display-time", "30000"]);
    }
    let stand_in = StandIn::new(&[&far]);
    let (mac_hook, far_hook) = (
        scratch.path().join("mac-hook.txt"),
        scratch.path().join("studio-hook.txt"),
    );
    let mac_script = recorder(scratch.path(), "mac-hook.sh", &mac_hook);
    let far_script = recorder(scratch.path(), "studio-hook.sh", &far_hook);
    mac.write_config(&config(&mac_script, true));
    far.server.write_config(&config(&far_script, false));
    Rig {
        term: terminal(),
        mac,
        far,
        stand_in,
        scratch,
    }
}

// ---------------------------------------------------------------- reading and waiting

fn terminal_screen(term: &Server) -> String {
    term.tmux(&["capture-pane", "-p", "-t", &term.pane()])
        .unwrap_or_default()
}

/// What a failure says besides the screen: the stand-in's calls, the hooks' files and the apps that run.
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
        "the terminal:\n{}\nthe last words of the stand-in's calls: {:#?}\nthis Mac's hook: {:?}\nstudio's hook: {:?}\n\
         studio's lease: {}\nstudio's agent: {:?}\nwhat this Mac shows: {:#?}\nwhat studio shows: {:#?}\nthe app's standard error: {:?}\nthe apps that run: {apps:#?}",
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
        hook_lines(&rig.mac_hook()),
        hook_lines(&rig.far_hook()),
        lease(&rig.far.state_dir),
        rig.far
            .server
            .pane_opt(&rig.far.server.pane_of("garden:tools"), "@agent_state"),
        displayed(&rig.mac, "\u{23f8}"),
        displayed(&rig.far.server, "\u{23f8}"),
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

/// How long a check that something did not happen watches for it, after the effect of its event was seen.
const MARGIN: Duration = Duration::from_millis(3000);

/// The lines a notification hook wrote so far.
fn hook_lines(file: &Path) -> Vec<String> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// The `display-message -c` commands (a message for a client, which is what a toast is) `server` was sent whose
/// text holds `needle`, from its log of messages.
fn displayed(server: &Server, needle: &str) -> Vec<String> {
    server
        .tmux(&["show-messages"])
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("command: display-message -c ") && line.contains(needle))
        .map(str::to_string)
        .collect()
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

/// Puts the cursor on the row of session `session` (the key `key` selects), then on the row that shows `needle`.
fn move_to_in(rig: &Rig, key: &str, session: &str, needle: &str) {
    press(&rig.term, &[key]);
    expect(rig, "the cursor on the session", || {
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

// ---------------------------------------------------------------- the app, its record and the leases

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

/// The app's record, read as JSON (null while it cannot be read).
fn record(rig: &Rig, pid: u32) -> serde_json::Value {
    std::fs::read_to_string(rig.mac.state_dir().join("apps").join(format!("{pid}.json")))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// The lease file of the state folder `state`, as JSON (null while it is not there).
fn lease(state: &Path) -> serde_json::Value {
    std::fs::read_to_string(state.join("machines").join("lease.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Whether the lease of `state` was refreshed in the last 10 s.
fn lease_is_fresh(state: &Path) -> bool {
    lease(state)["refreshed_at"]
        .as_i64()
        .is_some_and(|at| (now_ms() - at).abs() < 10_000)
}

/// Waits for the first call the app made through the stand-in (its feed) and has the guard end it by its pid
/// when the test is over, as it does the app: a failing assert leaves no feed behind. Returns its pid.
fn own_feed(rig: &Rig) -> u32 {
    expect(rig, "the feed's call", || !rig.stand_in.pids().is_empty());
    let feed = rig.stand_in.pids()[0];
    own_as(feed, "feed");
    feed
}

/// Types the app into the terminal as run for this Mac, with the stand-in as its ssh, and waits for its record (the
/// guard holds the pid from then on), for studio's rows and for the leases of both machines. Returns the app's pid.
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
        "env -u TMUX -u TMUX_PANE {env}'{}' app --child 2>'{}'; rc=$?; echo $rc >'{}'",
        taw_bin(),
        rig.mac.work_dir().join("err").display(),
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
    // R11-6: the far watcher and the app read the lease from the state folder their own server and app run with.
    assert_eq!(
        rig.far
            .server
            .taw_env()
            .into_iter()
            .find(|(name, _)| name == "TAW_STATE_DIR")
            .map(|(_, dir)| dir),
        Some(rig.far.state_dir.display().to_string()),
        "studio's server and the stand-in's far side must share one state folder"
    );
    expect(rig, "a fresh lease on studio", || {
        lease_is_fresh(&rig.far.state_dir)
    });
    expect(rig, "a fresh lease on this Mac", || {
        lease_is_fresh(rig.mac.state_dir())
    });
    pid
}

/// How many clients `server` has: the people (and the app's attach) looking at it.
fn clients(server: &Server) -> usize {
    server
        .tmux(&["list-clients", "-F", "#{client_pid}"])
        .map(|out| out.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0)
}

/// Enter on the row of `needle` of studio, and the wait until studio's session is on the terminal.
fn go_in_studio(rig: &Rig, needle: &str) {
    move_to_in(rig, "2", "garden", needle);
    press(&rig.term, &["Enter"]);
    expect_screen(rig, "studio's status line on the terminal", |screen| {
        screen.contains("[garden]")
    });
    expect(rig, "studio's server having the client", || {
        clients(&rig.far.server) == 1
    });
}

/// Enter on this Mac's session, and the wait until this Mac's session is on the terminal and the app's record
/// says where the person is.
fn go_in_mac(rig: &Rig, pid: u32) {
    let session = rig
        .mac
        .tmux_ok(&["display-message", "-p", "#{session_name}"]);
    move_to_in(rig, "1", session.trim(), "");
    press(&rig.term, &["Enter"]);
    expect(rig, "this Mac's server having the client", || {
        clients(&rig.mac) == 1
    });
    expect(rig, "the record naming the client", || {
        record(rig, pid)["client_pid"] != 0
    });
}

// ---------------------------------------------------------------- an agent that turns blocked

/// `sleep 300` in `pane` of `server` (the program a quiet agent stands in as), and a real watcher on it, waited
/// for until it has recorded its pid.
fn watched(server: &Server, pane: &str) {
    server.tmux_ok(&["send-keys", "-t", pane, "sleep 300", "Enter"]);
    assert!(
        wait_for(
            || server.display(pane, "#{pane_current_command}") == "sleep",
            Duration::from_secs(20)
        ),
        "`sleep` never took the pane"
    );
    server.ok(&["watch", "attach", pane], "cli:watch:attach", "ok");
    assert!(
        wait_for(
            || server
                .pane_opt(pane, "@agent_watch_pid")
                .parse::<u32>()
                .is_ok(),
            Duration::from_secs(20)
        ),
        "the watcher never recorded its pid"
    );
}

/// What an integration does: `taw pane report-agent` of `agent` in `pane`, from outside it.
fn report(server: &Server, pane: &str, agent: &str, state: &str) {
    server.ok(
        &[
            "pane",
            "report-agent",
            pane,
            "--source",
            "integration:test",
            // One word, so that a name that starts with a dash is the flag's value.
            &format!("--agent={agent}"),
            "--state",
            state,
        ],
        "cli:pane:report-agent",
        "ok",
    );
}

/// The agent of `pane` is reported working and then blocked: its watcher announces a toast after the delay.
fn turns_blocked(server: &Server, pane: &str, agent: &str) {
    report(server, pane, agent, "working");
    assert!(
        wait_for(
            || server.pane_opt(pane, "@agent_state") == "working",
            Duration::from_secs(20)
        ),
        "the watcher never read the agent as working\n{}",
        server.watcher_logs()
    );
    report(server, pane, agent, "blocked");
}

// ---------------------------------------------------------------- the tests

/// (1) The toast line through the real feed. The app is inside this Mac; studio's agent turns blocked. Studio's
/// watcher, under the app's fresh lease, shows the toast on studio and appends a `toast` line to studio's events
/// file; studio's real feed sends it with the lease set; the app draws it on this Mac's client once, with studio
/// named (the toast's own words, with `studio` in the place), and runs THIS Mac's hook once with
/// `TAW_MACHINE=studio`. Studio's own hook does not run. Mutations: the lease gate of `announce_with` dropped
/// (studio's hook runs too), `Some(name)` replaced by `Some("")` or `None` for the hook's machine.
#[test]
fn a_toast_of_studio_is_drawn_on_this_macs_client_and_pushed_from_this_mac_once() {
    let _own = Owned::new();
    let rig = rig();
    let pid = start_app(&rig);
    own_feed(&rig);
    let pane = rig.far.server.pane_of("garden:tools");
    watched(&rig.far.server, &pane);
    go_in_mac(&rig, pid);

    turns_blocked(&rig.far.server, &pane, AGENT);

    let place = "blocked claude \u{b7} studio \u{b7} garden:1";
    expect_screen(&rig, "the forwarded toast on this Mac's status line", |s| {
        s.contains(place)
    });
    expect(&rig, "this Mac's hook", || {
        hook_lines(&rig.mac_hook()).len() == 1
    });
    // Positive halves seen: the app drew the toast and pushed; now what must not have happened.
    std::thread::sleep(MARGIN);
    let lines = hook_lines(&rig.mac_hook());
    assert_eq!(lines.len(), 1, "one push: {lines:?}\n{}", seen(&rig));
    assert!(
        lines[0].starts_with("studio|") && lines[0].contains("claude blocked"),
        "TAW_MACHINE and the toast's words: {lines:?}"
    );
    assert_eq!(
        displayed(&rig.mac, "claude").len(),
        1,
        "this Mac's client was shown the toast once\n{}",
        seen(&rig)
    );
    assert_eq!(
        hook_lines(&rig.far_hook()),
        Vec::<String>::new(),
        "studio's own hook did not run: the app delivers while its lease is fresh\n{}",
        seen(&rig)
    );
}

/// (2) Inside the machine the toast is about. The app is inside studio; studio's agent (on a window that is not
/// the client's) turns blocked. Asserted: studio shows its own toast on its own
/// status line once (it has a client), and the app forwards nothing through studio for studio's own toast (a
/// second toast on studio's status line is the mutation this test kills); nothing is drawn on this Mac; the lease
/// holder still pushes: this Mac's hook runs once with `TAW_MACHINE=studio` (the push is the Mac's way of telling
/// its person, wherever the person's client is), and studio's own hook does not run.
#[test]
fn a_toast_of_the_machine_you_are_inside_is_shown_once_by_that_machine_and_not_forwarded() {
    let _own = Owned::new();
    let rig = rig();
    let _pid = start_app(&rig);
    own_feed(&rig);
    let pane = rig.far.server.pane_of("garden:tools");
    watched(&rig.far.server, &pane);
    go_in_studio(&rig, "0:main");

    turns_blocked(&rig.far.server, &pane, AGENT);

    expect(&rig, "studio's own toast on its client", || {
        displayed(&rig.far.server, "claude").len() == 1
    });
    expect(&rig, "this Mac's hook", || {
        hook_lines(&rig.mac_hook()).len() == 1
    });
    std::thread::sleep(MARGIN);
    let lines = hook_lines(&rig.mac_hook());
    assert_eq!(lines.len(), 1, "one push: {lines:?}\n{}", seen(&rig));
    assert!(lines[0].starts_with("studio|"), "{lines:?}");
    assert_eq!(
        displayed(&rig.far.server, "claude").len(),
        1,
        "studio showed the toast once and was sent no second one\n{}",
        seen(&rig)
    );
    let forwards: Vec<String> = rig
        .stand_in
        .calls()
        .iter()
        .filter_map(|call| call.last().cloned())
        .filter(|line| line.contains("'notification' 'show'"))
        .collect();
    assert!(forwards.is_empty(), "nothing was forwarded: {forwards:?}");
    assert!(
        displayed(&rig.mac, "claude").is_empty(),
        "nothing was drawn on this Mac\n{}",
        seen(&rig)
    );
    assert_eq!(hook_lines(&rig.far_hook()), Vec::<String>::new());
}

/// (3) The forward through the real far taw. The app is inside studio; an agent of THIS Mac turns blocked, with a
/// name that starts with a dash and holds shell and tmux metacharacters. This Mac's watcher shows the toast here, appends it to this Mac's
/// events file under the app's lease; the app forwards it through studio: studio's real `taw` takes `notification
/// show --forwarded=...` with the flags the app wrote (its own clap definition is the judge), and draws it on
/// studio's status line with its words intact and this Mac named by its short host name; the words were data all
/// the way: no marker file was made. This
/// Mac's hook ran once, for this Mac (`TAW_MACHINE` empty). Mutations: the `--` before the title dropped, or
/// `--forwarded` renamed in the forward's words: the far taw refuses and the toast never arrives.
#[test]
fn a_toast_of_this_mac_is_drawn_by_the_real_far_taw_with_its_words_as_data() {
    let _own = Owned::new();
    let rig = rig();
    let _pid = start_app(&rig);
    own_feed(&rig);
    go_in_studio(&rig, "0:main");
    let marks = [
        rig.scratch.path().join("made-by-tmux"),
        rig.scratch.path().join("made-by-shell"),
        rig.scratch.path().join("made-by-backticks"),
    ];
    let name = format!(
        "-x a#(touch {}) #{{pane_id}} \"q\" $(touch {}) `touch {}`",
        marks[0].display(),
        marks[1].display(),
        marks[2].display()
    );
    let pane = rig.mac.pane();
    watched(&rig.mac, &pane);

    turns_blocked(&rig.mac, &pane, &name);

    // On studio's status line this Mac is named by its short host name (what tmux calls #{host_short}), which
    // there does not read as studio; this Mac's word only when the name cannot be read.
    let host = machine::feed::host_name();
    let word = if host.is_empty() {
        machine::this_machine_word().to_string()
    } else {
        host
    };
    expect_screen(&rig, "the forwarded toast on studio's status line", |s| {
        // A far client in an environment with no locale draws the middle dot and the icon as other characters.
        s.contains("blocked -x a#(touch ") && s.contains("#{pane_id}") && s.contains("$(touch ")
    });
    let screen = terminal_screen(&rig.term);
    assert!(
        screen.contains("a#(touch ") && screen.contains("\"q\" $(touch "),
        "the words are drawn literally: {screen}"
    );
    // What studio's tmux was sent, whole: the place line is cut off the screen by the long name.
    let sent = displayed(&rig.far.server, "blocked#[default] -x a##(touch ");
    assert_eq!(sent.len(), 1, "studio drew it once\n{}", seen(&rig));
    assert!(
        sent[0].contains(&format!("{word} \u{b7} one:0")) && sent[0].contains(" ##{pane_id} "),
        "the place is this Mac's and the substitution is escaped: {sent:?}"
    );
    expect(&rig, "this Mac's hook", || {
        hook_lines(&rig.mac_hook()).len() == 1
    });
    std::thread::sleep(MARGIN);
    let lines = hook_lines(&rig.mac_hook());
    assert_eq!(lines.len(), 1, "one push: {lines:?}\n{}", seen(&rig));
    assert!(
        lines[0].starts_with("|"),
        "this Mac's own toast has no machine: {lines:?}"
    );
    for mark in &marks {
        assert!(!mark.exists(), "{} was made", mark.display());
    }
    let forwards: Vec<String> = rig
        .stand_in
        .calls()
        .iter()
        .filter_map(|call| call.last().cloned())
        .filter(|line| line.contains("'notification' 'show'"))
        .collect();
    assert_eq!(forwards.len(), 1, "one forward: {forwards:?}");
    assert!(
        forwards[0].contains(&format!("'--forwarded={word}'")) && forwards[0].contains("'--'"),
        "{forwards:?}"
    );
    assert_eq!(
        displayed(&rig.far.server, "blocked#[default] -x a##(touch ").len(),
        1,
        "studio drew it once\n{}",
        seen(&rig)
    );
}

/// (4) The lease lapses. The app and its feed are stopped by their pids (a feed whose app ended would give the
/// lease up at once) and its lease on studio is made a minute old, in the file (`refreshed_at`, Unix
/// milliseconds): a new blocked event on studio runs studio's own hook, once, with `TAW_MACHINE` empty, and this
/// Mac's hook does not run again. Before it, with the app alive, the first event was this Mac's push and not
/// studio's. After it, with the lease made fresh again, the app is killed and the feed set going: the feed ends and
/// the next app takes the lease at once. Mutations: `lease_fresh` always true in `announce_with`; a holder lock
/// that keeps the lease when dropped and a holder never seen as gone.
#[test]
fn when_the_lease_lapses_studios_own_hook_runs_for_the_next_event() {
    let _own = Owned::new();
    let rig = rig();
    let pid = start_app(&rig);
    let feed = own_feed(&rig);
    let pane = rig.far.server.pane_of("garden:tools");
    watched(&rig.far.server, &pane);

    turns_blocked(&rig.far.server, &pane, AGENT);
    expect(&rig, "this Mac's hook for the first event", || {
        hook_lines(&rig.mac_hook()).len() == 1
    });
    assert_eq!(hook_lines(&rig.far_hook()), Vec::<String>::new());

    // The app and the feed it started are stopped, not ended: a feed whose app ended gives the lease up at once,
    // and here the lease is to stay on file and lapse.
    let app_stopped = Resume::stop(pid);
    let feed_stopped = Resume::stop(feed);
    let state = rig.far.state_dir.clone();
    let file = state.join("machines").join("lease.json");
    let mut old = lease(&state);
    assert!(old["refreshed_at"].is_i64(), "the lease file: {old}");
    old["refreshed_at"] = serde_json::json!(now_ms() - 60_000);
    std::fs::write(&file, old.to_string()).unwrap();
    assert!(!lease_is_fresh(&state));

    // A new event, on an agent of a window of its own, reported as the first one was.
    rig.far
        .server
        .tmux_ok(&["new-window", "-d", "-t", "garden:", "-n", "extra"]);
    let other = rig.far.server.pane_of("garden:extra");
    watched(&rig.far.server, &other);
    turns_blocked(&rig.far.server, &other, AGENT);
    expect(&rig, "studio's own hook for the new event", || {
        hook_lines(&rig.far_hook()).len() == 1
    });
    std::thread::sleep(MARGIN);
    let lines = hook_lines(&rig.far_hook());
    assert_eq!(lines.len(), 1, "one run of studio's hook: {lines:?}");
    assert!(
        lines[0].starts_with("|") && lines[0].contains("claude blocked"),
        "TAW_MACHINE is empty on studio's own run: {lines:?}"
    );
    assert_eq!(
        hook_lines(&rig.mac_hook()).len(),
        1,
        "this Mac's hook did not run for the new event\n{}",
        seen(&rig)
    );

    // The app is ended by its pid and the feed set going: the feed hears its app's end and goes, and the lease is
    // no one's: the next app takes it at once, without waiting out its 15 s. The lease is made fresh first, so that
    // only its holder's end lets the next app have it.
    let mut fresh = lease(&state);
    fresh["refreshed_at"] = serde_json::json!(now_ms());
    std::fs::write(&file, fresh.to_string()).unwrap();
    assert!(lease_is_fresh(&state));
    signal(pid, libc::SIGKILL);
    expect(&rig, "the app gone", || !alive(pid));
    disown(pid);
    drop(app_stopped);
    drop(feed_stopped);
    expect(&rig, "the feed gone", || !alive(feed));
    let paths = taw::paths::Paths::rooted_at(rig.far.config_dir.clone(), state.clone());
    assert!(
        machine::files::offer_lease(&paths, "next-app", machine::files::now_ms()),
        "the ended app's lease was not given up: {}",
        lease(&state)
    );
    assert_eq!(lease(&state)["holder"], "next-app");
}

/// A process that is stopped and set going again when dropped, so that it can end with its connection.
struct Resume(u32, String);

impl Resume {
    fn stop(pid: u32) -> Resume {
        let started = start_time_of(pid);
        assert!(!started.is_empty(), "no process {pid}");
        signal(pid, libc::SIGSTOP);
        Resume(pid, started)
    }
}

impl Drop for Resume {
    fn drop(&mut self) {
        if alive(self.0) && start_time_of(self.0) == self.1 {
            signal(self.0, libc::SIGCONT);
        }
    }
}

/// (5) The drop toast. With the app on its list, studio's feed stops and connections to it are refused: it goes
/// from live to dropped, and `studio dropped off (last seen just now)` is on the app's top line once. Seen once:
/// the top line goes by (its time to live is four seconds) and the words do not come back. Mutation: the
/// `if !live` of `notice_drops` firing on every look (the toast comes again at each look, so the line is
/// never gone and the check that it went by fails).
#[test]
fn a_machine_that_drops_off_is_said_once_on_the_apps_top_line() {
    let _own = Owned::new();
    let rig = rig();
    let _pid = start_app(&rig);
    expect(&rig, "the feed's call", || !rig.stand_in.pids().is_empty());
    let feed = rig.stand_in.pids()[0];
    assert!(args_of(feed).contains("feed"), "{}", args_of(feed));
    own_as(feed, "feed");
    let _resume = Resume::stop(feed);
    rig.stand_in.fail_as("studio", Failure::Refused);

    let said = words::dropped_toast("studio");
    // Nothing is heard for 15 s before the machine is dropped: a longer wait than the others.
    let until = Instant::now() + Duration::from_secs(60);
    while !terminal_screen(&rig.term).contains(&said) {
        assert!(Instant::now() < until, "no drop toast\n{}", seen(&rig));
        std::thread::sleep(Duration::from_millis(100));
    }
    // Positive seen. It goes by: a toast lives four seconds. Then it does not come back.
    expect_screen(&rig, "the drop toast going by", |s| !s.contains(&said));
    let until = Instant::now() + MARGIN;
    while Instant::now() < until {
        assert!(
            !terminal_screen(&rig.term).contains(&said),
            "the drop toast came a second time\n{}",
            seen(&rig)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
