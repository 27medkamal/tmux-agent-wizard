//! `taw machine attach <target> --holder <id>`: what the app runs on a machine, over ssh with a terminal, to
//! put that machine's tmux on its terminal.
//!
//! The tests that need a terminal give the verb one the way a person has one: a pane of an outer private
//! server, whose own screen is the test's to read. The machine is a second private server, named to the verb by
//! `TAW_TMUX_SOCKET_NAME`. Nothing here reaches beyond the fixtures' own folders and `-L` sockets, and no ssh
//! is run.

mod common;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use common::{Server, ServerOptions, taw_bin, wait_for};

/// The machine: a server with the session `garden` (windows `main` and `tools`).
fn machine() -> Server {
    let machine = Server::start_with(ServerOptions {
        session: "garden".into(),
        ..ServerOptions::default()
    });
    // Named, so that the shell's own name does not decide what the status line says.
    machine.tmux_ok(&["rename-window", "-t", "garden:0", "main"]);
    machine.tmux_ok(&["new-window", "-d", "-t", "garden:", "-n", "tools"]);
    machine
}

/// A terminal as a person has one: a private server of its own whose pane is the terminal.
fn terminal() -> Server {
    Server::start_with(ServerOptions {
        width: 100,
        height: 30,
        ..ServerOptions::default()
    })
}

/// `taw machine attach <args>` run on `machine` with no terminal on standard input: `(status, stderr)`.
fn run_without_terminal(machine: &Server, args: &[&str]) -> (i32, String) {
    let mut line = vec!["machine", "attach"];
    line.extend_from_slice(args);
    let out = machine
        .taw_command(&line)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// The folder the records are kept in on `machine`.
fn attached_dir(machine: &Server) -> PathBuf {
    machine.state_dir().join("machines").join("attached")
}

/// The words of a refusal of `target`.
fn no_such(target: &str) -> String {
    format!("taw machine attach: {target} is no pane, session or window here\n")
}

const NO_TERMINAL: &str = "taw machine attach: needs a terminal (the app runs it over ssh -t)\n";

/// Without a terminal on standard input the verb says so and ends 2, and records nothing.
#[test]
fn without_a_terminal_it_says_so_and_ends_2() {
    let machine = machine();
    let pane = machine.pane_of("garden");

    let (status, said) = run_without_terminal(&machine, &[&pane, "--holder", "mac-1"]);

    assert_eq!((status, said.as_str()), (2, NO_TERMINAL));
    assert!(!attached_dir(&machine).exists(), "a record was made");
}

/// A target that is no id of tmux's, or that names nothing here, is refused in the words of the plan and
/// ends 2, whatever the terminal: `studio` is a session of the server and is refused all the same (it is
/// not an id), and the ids that are well formed are refused for naming nothing.
#[test]
fn a_target_that_is_no_pane_session_or_window_here_is_refused() {
    let machine = machine();
    machine.tmux_ok(&["new-session", "-d", "-s", "studio"]);
    // Positive half: the server answers, and an id it has is not refused (the terminal is what is missing).
    let pane = machine.pane_of("garden");
    let (status, said) = run_without_terminal(&machine, &[&pane, "--holder", "mac-1"]);
    assert_eq!((status, said.as_str()), (2, NO_TERMINAL));

    for target in [
        "%999", "$999", "@999", "studio", "garden", "1", "$", "%x", "%5;ls",
    ] {
        let (status, said) = run_without_terminal(&machine, &[target, "--holder", "mac-1"]);
        assert_eq!(
            (status, said.as_str()),
            (2, no_such(target).as_str()),
            "{target}"
        );
    }
    assert!(!attached_dir(&machine).exists(), "a record was made");
}

/// With a client attached to the machine's server (a person works there), a target that names nothing is
/// still refused in the same words: tmux's `display-message -t <missing>` does not answer with the pane of
/// that client.
#[test]
fn a_target_that_names_nothing_is_refused_with_a_client_attached_too() {
    let machine = machine();
    let person = terminal();
    // The terminal's pane runs a plain attach, which is a person's client on the machine.
    let tmux = which::which("tmux").expect("tmux is on PATH");
    type_line(
        &person,
        &format!(
            "{} '{}' -L '{}' attach-session -t garden",
            env_prefix(&machine),
            tmux.display(),
            machine.socket()
        ),
    );
    assert!(
        wait_for(
            || machine
                .tmux(&["list-clients", "-F", "#{client_tty}"])
                .is_ok_and(|clients| !clients.trim().is_empty()),
            Duration::from_secs(20)
        ),
        "no client attached to the machine:\n{}",
        screen(&person)
    );

    // Positive half: an id the server has is not refused (the terminal is what is missing).
    let pane = machine.pane_of("garden");
    let (status, said) = run_without_terminal(&machine, &[&pane, "--holder", "mac-1"]);
    assert_eq!((status, said.as_str()), (2, NO_TERMINAL));

    for target in ["%999", "$999", "@999"] {
        let (status, said) = run_without_terminal(&machine, &[target, "--holder", "mac-1"]);
        assert_eq!(
            (status, said.as_str()),
            (2, no_such(target).as_str()),
            "{target}"
        );
    }
    assert!(!attached_dir(&machine).exists(), "a record was made");
}

/// What `type_attach` typed: where the line's standard error and status went.
struct Typed {
    err: PathBuf,
    status: PathBuf,
}

impl Typed {
    /// What taw or tmux said and how the line ended, with the terminal's screen: the words of every panic of
    /// a test that typed the attach.
    fn said(&self, terminal: &Server) -> String {
        let read = |path: &PathBuf| {
            std::fs::read_to_string(path)
                .map(|text| text.trim_end().to_string())
                .unwrap_or_else(|err| format!("(no file: {err})"))
        };
        format!(
            "standard error: {:?}\nstatus: {:?}\nthe terminal's screen:\n{}",
            read(&self.err),
            read(&self.status),
            screen(terminal)
        )
    }
}

/// Types `taw machine attach '<target>' --holder <holder>` into the terminal, as run on `machine`: its server's
/// name and folders in the environment, `$TMUX` out of it (the terminal is outside tmux), what it says on
/// standard error in `err` and its status in `status`.
fn type_attach(terminal: &Server, machine: &Server, target: &str, holder: &str) -> Typed {
    type_attach_with(terminal, machine, target, holder, "", "")
}

/// [`type_attach`] with `before` ahead of the whole line (`env -u TERM `) and `extra` words between `env` and
/// the program (`TERM=x`).
fn type_attach_with(
    terminal: &Server,
    machine: &Server,
    target: &str,
    holder: &str,
    before: &str,
    extra: &str,
) -> Typed {
    let err = machine.work_dir().join("err");
    let status = machine.work_dir().join("status");
    let line = format!(
        "{before}{}{extra} '{}' machine attach '{target}' --holder '{holder}' 2>'{}'; echo $? >'{}'",
        env_prefix(machine),
        taw_bin(),
        err.display(),
        status.display()
    );
    type_line(terminal, &line);
    Typed { err, status }
}

/// `env -u TMUX -u TMUX_PANE NAME=value ...` for `machine`: its server's name and folders in the environment,
/// `$TMUX` out of it (the terminal is outside tmux).
fn env_prefix(machine: &Server) -> String {
    let env: String = machine
        .taw_env()
        .iter()
        .map(|(name, value)| format!("'{name}={value}' "))
        .collect();
    format!("env -u TMUX -u TMUX_PANE {env}")
}

/// Types `line` and Enter into the terminal's shell. Every caller types into a fresh terminal, so it waits
/// for the shell's prompt first: typed sooner, the line is cut at the terminal's line buffer.
fn type_line(terminal: &Server, line: &str) {
    terminal.wait_for_prompt(&terminal.pane());
    terminal.tmux_ok(&["send-keys", "-t", &terminal.pane(), "-l", line]);
    terminal.tmux_ok(&["send-keys", "-t", &terminal.pane(), "Enter"]);
}

fn screen(terminal: &Server) -> String {
    terminal
        .tmux(&["capture-pane", "-p", "-t", &terminal.pane()])
        .unwrap_or_default()
}

/// Waits (20 s) until `ready` holds of the terminal's screen, and fails with what the attach said and the
/// screen when it does not.
fn expect_screen(terminal: &Server, typed: &Typed, what: &str, ready: impl Fn(&str) -> bool) {
    assert!(
        wait_for(|| ready(&screen(terminal)), Duration::from_secs(20)),
        "{what} never showed on the terminal:\n{}",
        typed.said(terminal)
    );
}

/// The verb with a terminal on `target`: the record is written (its file name, the terminal, the holder, the
/// time, and the client's own process), and the terminal shows the machine's session, so its tmux is on it.
fn attaches(target_of: impl Fn(&Server) -> String, holder: &str, current: &str) {
    let machine = machine();
    let terminal = terminal();
    let target = target_of(&machine);
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let typed = type_attach(&terminal, &machine, &target, holder);

    // The machine's own status line: not on the terminal's shell, nor in what was typed.
    expect_screen(&terminal, &typed, "the machine's session", |screen| {
        screen.contains("[garden]")
    });
    // And the window the target is in is the one it shows.
    expect_screen(&terminal, &typed, "the window the target is in", |screen| {
        screen.contains(current)
    });
    let said = || typed.said(&terminal);
    let tty = terminal.tmux_ok(&[
        "display-message",
        "-p",
        "-t",
        &terminal.pane(),
        "#{pane_tty}",
    ]);
    let file = attached_dir(&machine).join(format!("{}.json", tty.replace('/', "_")));
    let text = std::fs::read_to_string(&file)
        .unwrap_or_else(|err| panic!("no record at {}: {err}\n{}", file.display(), said()));
    let record: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("the record is not JSON ({err}): {text}\n{}", said()));
    assert_eq!(record["tty"], tty.as_str(), "{text}\n{}", said());
    assert_eq!(record["holder"], holder, "{text}\n{}", said());
    let at = record["at"]
        .as_u64()
        .unwrap_or_else(|| panic!("no time in the record: {text}\n{}", said()));
    assert!(
        at >= before && at < before + 20_000,
        "{text} (before {before})\n{}",
        said()
    );
    // The process that wrote it is the tmux client: `exec` keeps the pid.
    let client_pid = machine.tmux_ok(&["list-clients", "-F", "#{client_pid}"]);
    assert_eq!(
        record["pid"].to_string(),
        client_pid.trim(),
        "{text}\n{}",
        said()
    );
}

/// A record that cannot be written does not stop the attach: the client still comes up on the terminal (its
/// key then opens the popup, as it did before there was an app). The folder is made impossible by a file
/// where `attached/` must be a folder.
#[test]
fn an_attach_whose_record_cannot_be_written_still_attaches() {
    let machine = machine();
    let terminal = terminal();
    let machines = machine.state_dir().join("machines");
    std::fs::create_dir_all(&machines).unwrap();
    std::fs::write(machines.join("attached"), "not a folder\n").unwrap();
    let pane = machine.pane_of("garden");

    let typed = type_attach(&terminal, &machine, &pane, "mac-4245");

    expect_screen(&terminal, &typed, "the machine's session", |screen| {
        screen.contains("[garden]")
    });
    // Positive half of the check below: the machine has the client, and the folder is still the file.
    assert!(
        !machine
            .tmux_ok(&["list-clients", "-F", "#{client_tty}"])
            .is_empty(),
        "{}",
        typed.said(&terminal)
    );
    assert!(machines.join("attached").is_file());
    // And it said nothing about the record it could not write: standard error is empty.
    assert_eq!(
        std::fs::read_to_string(&typed.err).unwrap_or_default(),
        "",
        "{}",
        typed.said(&terminal)
    );
}

#[test]
fn a_pane_target_puts_the_machines_session_on_the_terminal_and_records_the_client() {
    // The pane of `tools`, which is not the window the session has current.
    attaches(
        |machine| machine.pane_of("garden:tools"),
        "mac-4242",
        "1:tools*",
    );
}

#[test]
fn a_session_target_attaches() {
    attaches(
        |machine| machine.tmux_ok(&["display-message", "-p", "-t", "garden", "#{session_id}"]),
        "mac-4243",
        "0:main*",
    );
}

#[test]
fn a_window_target_attaches() {
    attaches(
        |machine| {
            machine.tmux_ok(&[
                "display-message",
                "-p",
                "-t",
                "garden:tools",
                "#{window_id}",
            ])
        },
        "mac-4244",
        "1:tools*",
    );
}

/// The terminal type the machine's tmux client reports once it is attached, after the attach was typed with
/// `before` ahead of the line and `extra` words in the environment (see [`type_attach_with`]).
fn client_term(before: &str, extra: &str) -> String {
    let machine = machine();
    let terminal = terminal();
    let pane = machine.pane_of("garden");

    let typed = type_attach_with(&terminal, &machine, &pane, "mac-4246", before, extra);

    expect_screen(&terminal, &typed, "the machine's session", |screen| {
        screen.contains("[garden]")
    });
    machine.tmux_ok(&["list-clients", "-F", "#{client_termname}"])
}

/// A terminal type this machine's terminfo does not have (Ghostty's, on a machine that never heard of it)
/// would end the far tmux client with `missing or unsuitable terminal`: it runs with `xterm-256color`.
#[test]
fn a_terminal_type_this_machine_does_not_know_attaches_as_xterm_256color() {
    assert_eq!(
        client_term("", "TERM=taw-no-such-terminal").trim(),
        "xterm-256color"
    );
}

/// A `TERM` that is unset or empty is the same case.
#[test]
fn an_unset_or_empty_terminal_type_attaches_as_xterm_256color() {
    assert_eq!(client_term("env -u TERM ", "").trim(), "xterm-256color");
    assert_eq!(client_term("", "TERM=").trim(), "xterm-256color");
}

/// A terminal type the machine knows keeps its own: `xterm` is in every terminfo database and is not the
/// fallback, so the client reporting it shows `TERM` was left alone.
#[test]
fn a_terminal_type_this_machine_knows_is_kept() {
    assert_eq!(client_term("", "TERM=xterm").trim(), "xterm");
}

/// Types `taw machine attach --new --holder <holder>` into the terminal, as [`type_attach`] types a target.
fn type_attach_new(terminal: &Server, machine: &Server, holder: &str) -> Typed {
    let err = machine.work_dir().join("err");
    let status = machine.work_dir().join("status");
    let line = format!(
        "{} '{}' machine attach --new --holder '{holder}' 2>'{}'; echo $? >'{}'",
        env_prefix(machine),
        taw_bin(),
        err.display(),
        status.display()
    );
    type_line(terminal, &line);
    Typed { err, status }
}

/// `--new` on a machine that runs no tmux starts one: a session with no name (tmux numbers it), and the
/// terminal is its client, with the record written as for any attach.
#[test]
fn new_on_a_machine_with_no_tmux_starts_a_session_and_attaches() {
    let machine = machine();
    let terminal = terminal();
    machine.tmux_ok(&["kill-server"]);
    assert!(
        wait_for(
            || machine.tmux(&["list-sessions"]).is_err(),
            Duration::from_secs(20)
        ),
        "the machine's tmux did not end"
    );

    let typed = type_attach_new(&terminal, &machine, "mac-1");

    let sessions = || {
        machine
            .tmux(&["list-sessions", "-F", "#{session_name}"])
            .unwrap_or_default()
    };
    assert!(
        wait_for(|| sessions().trim() == "0", Duration::from_secs(20)),
        "no session numbered 0 came up, it has: {:?}\n{}",
        sessions(),
        typed.said(&terminal)
    );
    expect_screen(&terminal, &typed, "the new session", |screen| {
        screen.contains("[0]")
    });
    let clients = machine.tmux_ok(&["list-clients", "-F", "#{client_session}"]);
    assert_eq!(clients.trim(), "0", "{}", typed.said(&terminal));
    let records = std::fs::read_dir(attached_dir(&machine))
        .map(|dir| dir.count())
        .unwrap_or(0);
    assert_eq!(records, 1, "one record: {}", typed.said(&terminal));
}

/// `--new` refuses without a terminal as a target does, and starts nothing.
#[test]
fn new_without_a_terminal_says_so_and_ends_2() {
    let machine = machine();

    let (status, said) = run_without_terminal(&machine, &["--new", "--holder", "mac-1"]);

    assert_eq!((status, said.as_str()), (2, NO_TERMINAL));
    assert!(!attached_dir(&machine).exists(), "a record was made");
    // Positive half of "started nothing": the machine still has its one session.
    assert_eq!(
        machine
            .tmux_ok(&["list-sessions", "-F", "#{session_name}"])
            .trim(),
        "garden"
    );
}

/// The machine's own tmux says how to get back to the list on the attached client's status line, with its own
/// prefix and the plugin's key (tmux fills both in), for a target and for `--new` alike.
#[test]
fn an_attach_says_how_to_get_back_to_the_list_on_the_machines_own_status_line() {
    let machine = machine();
    let terminal = terminal();
    machine.tmux_ok(&["set-option", "-g", "@agent-wizard", "A"]);
    let prefix = machine.tmux_ok(&["show-options", "-gv", "prefix"]);
    let words = format!("{} + A: back to the taw list", prefix.trim());
    let pane = machine.pane_of("garden");

    let typed = type_attach(&terminal, &machine, &pane, "mac-1");

    expect_screen(&terminal, &typed, &words, |screen| screen.contains(&words));
}

/// The same for the session `--new` starts, on a machine whose tmux had to be started: the prefix is read when
/// the message is drawn, so nothing had to be asked of a server that was not there.
#[test]
fn a_new_session_says_how_to_get_back_to_the_list_too() {
    let machine = machine();
    let terminal = terminal();
    machine.tmux_ok(&["kill-server"]);
    assert!(
        wait_for(
            || machine.tmux(&["list-sessions"]).is_err(),
            Duration::from_secs(20)
        ),
        "the machine's tmux did not end"
    );

    let typed = type_attach_new(&terminal, &machine, "mac-1");

    // The plugin is not loaded on this server, so it has no popup key: the words name its prefix and `d`, the key
    // that does bring the list back there (they used to name an empty key, `C-b + :`).
    expect_screen(&terminal, &typed, "the way back", |screen| {
        screen.contains("C-b + d: back to the taw list")
    });
}
