//! The doorway, end to end: the popup of a hand-attached client on this Mac's server, Enter on studio's
//! row, and the terminal's client becomes `taw app` going into studio, as the far `taw` serves it.
//!
//! One terminal holds the app that keeps studio's feed (so the popup has studio's rows, from the files the
//! keeper writes); another has a client attached by hand to this Mac's server, as a person's terminal has. This
//! Mac is a private server, studio a [`FarSide`] reached through the stand-in for ssh, which records every call
//! and runs the far side's own `taw`. No test runs a real ssh. Every wait is positive, with a 20 s deadline.

mod common;

use std::path::Path;
use std::time::Duration;

use common::harness::TmuxBase;
use common::machine::{FarSide, StandIn};
use common::owned::{Owned, own, record_names, status_of};
use common::{Server, ServerOptions, taw_bin, wait_for};

struct Rig {
    keeper_term: Server,
    term: Server,
    mac: Server,
    far: FarSide,
    stand_in: StandIn,
    /// The `$TMUX_TMPDIR` of the hand-attached client's shell: no `taw` or `tmux` it starts without a socket named
    /// can reach the default server of whoever runs the tests, and a server it makes is ended by the guard when the rig drops.
    tmux_base: TmuxBase,
}

fn terminal() -> Server {
    Server::start_with(ServerOptions {
        width: 200,
        height: 30,
        ..ServerOptions::default()
    })
}

/// `'NAME=value' ` for each pair, as the `env` words of a line typed into a terminal.
fn env_words(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(name, value)| format!("'{name}={value}' "))
        .collect()
}

/// The tmux the fixture runs (the first on this process's PATH), as a path: the line typed into the terminal runs
/// under `env` with a narrow PATH, which could not find it.
fn tmux_path() -> String {
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(':')
        .map(|dir| Path::new(dir).join("tmux"))
        .find(|candidate| candidate.is_file())
        .expect("tmux is on the PATH")
        .display()
        .to_string()
}

fn rig() -> Rig {
    let mac = Server::start();
    mac.write_config("[ui]\ndepth = \"all\"\n\n[machines]\nstudio = \"studio\"\n");
    let far = FarSide::with_session("studio", "garden");
    far.server
        .tmux_ok(&["rename-window", "-t", "garden:0", "main"]);
    // The popup key, bound as the plugin binds it, on both servers: on studio's it sends the app's client back.
    for server in [&far.server, &mac] {
        server.tmux_ok(&[
            "bind-key",
            "-n",
            "F5",
            "run-shell",
            &format!(
                "env -u TMUX -u TMUX_PANE {}'{}' ui open >/dev/null 2>&1",
                env_words(&server.taw_env()),
                taw_bin()
            ),
        ]);
    }
    let stand_in = StandIn::new(&[&far]);
    Rig {
        keeper_term: terminal(),
        term: terminal(),
        mac,
        far,
        stand_in,
        tmux_base: TmuxBase::new(),
    }
}

fn screen_of(term: &Server) -> String {
    term.tmux(&["capture-pane", "-p", "-t", &term.pane()])
        .unwrap_or_default()
}

fn seen(rig: &Rig) -> String {
    format!(
        "the terminal:\n{}\nthe keeper's terminal:\n{}\nthe stand-in's calls: {:#?}\nthe records: {:#?}",
        screen_of(&rig.term)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        screen_of(&rig.keeper_term)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        rig.stand_in
            .calls()
            .iter()
            .map(|call| call.last().cloned().unwrap_or_default())
            .collect::<Vec<_>>(),
        records(&rig.mac)
    )
}

fn expect(rig: &Rig, what: &str, mut ready: impl FnMut() -> bool) {
    assert!(
        wait_for(&mut ready, Duration::from_secs(20)),
        "{what} never happened.\n{}",
        seen(rig)
    );
}

/// Types `line` and Enter into a terminal's shell. Both callers type into a terminal that is fresh and has
/// nothing in front of its shell, so it waits for the prompt first: typed sooner, the line is cut at the
/// terminal's line buffer.
fn type_line(term: &Server, line: &str) {
    term.wait_for_prompt(&term.pane());
    term.tmux_ok(&["send-keys", "-t", &term.pane(), "-l", line]);
    term.tmux_ok(&["send-keys", "-t", &term.pane(), "Enter"]);
}

/// The app records of this Mac, each as JSON, whole files only ([`record_names`]).
fn records(mac: &Server) -> Vec<serde_json::Value> {
    let names: Vec<String> = std::fs::read_dir(mac.state_dir().join("apps"))
        .map(|dir| {
            dir.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    record_names(names)
        .into_iter()
        .filter_map(|name| {
            std::fs::read_to_string(mac.state_dir().join("apps").join(name))
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
        })
        .collect()
}

/// The pids of the clients of this Mac's server.
fn clients(mac: &Server) -> Vec<u32> {
    mac.tmux(&["list-clients", "-F", "#{client_pid}"])
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

fn selected_row(term: &Server) -> String {
    let styled = term
        .tmux(&["capture-pane", "-p", "-e", "-t", &term.pane()])
        .unwrap_or_default();
    styled
        .lines()
        .find(|line| line.contains("48;2;69;71;90m"))
        .map(|line| {
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
            plain
        })
        .unwrap_or_default()
}

/// Starts the app that keeps studio's feed in the keeper's terminal, and waits for studio's rows in the files
/// it writes (a pane of studio's on its list).
fn start_keeper(rig: &Rig) {
    let bin = rig.stand_in.bin.display().to_string();
    let mut env = rig.mac.taw_env();
    env.push(("TAW_SSH_BIN".into(), bin));
    type_line(
        &rig.keeper_term,
        &format!(
            "env -u TMUX -u TMUX_PANE {}'{}' app --child 2>'{}'",
            env_words(&env),
            taw_bin(),
            rig.mac.work_dir().join("keeper-err").display()
        ),
    );
    expect(rig, "the keeper's record", || records(&rig.mac).len() == 1);
    own(records(&rig.mac)[0]["pid"].as_u64().expect("a pid") as u32);
    expect(rig, "studio's rows on the keeper's list", || {
        let screen = screen_of(&rig.keeper_term);
        screen.contains("0:main") && screen.contains(" taw app")
    });
}

/// This Mac's first session's name.
fn mac_session(rig: &Rig) -> String {
    rig.mac
        .tmux(&["list-sessions", "-F", "#{session_name}"])
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string()
}

/// A client attached by hand to this Mac's server in the terminal, as a shell there would run `tmux attach -S
/// <socket>`: with the environment the app needs (the stand-in for ssh, the private `TMUX_TMPDIR`) and none of
/// the outer tmux's, and NOT the variables that name this Mac's server (`TAW_TMUX_SOCKET_NAME`, `_PATH`): the
/// shell of a person's terminal has none, so the doorway's command must carry the server itself. The line ends
/// in the shell's own status, written to a file. Returns the client's pid.
fn attach_by_hand(rig: &Rig) -> u32 {
    let session = mac_session(rig);
    let socket_path = rig
        .mac
        .tmux(&["display-message", "-p", "#{socket_path}"])
        .unwrap();
    let bin = rig.stand_in.bin.display().to_string();
    let mut env: Vec<(String, String)> = rig
        .mac
        .taw_env()
        .into_iter()
        .filter(|(name, _)| !name.starts_with("TAW_TMUX_SOCKET_"))
        .collect();
    env.push(("TAW_SSH_BIN".into(), bin));
    env.push((
        "TMUX_TMPDIR".into(),
        rig.tmux_base.tmux_dir().display().to_string(),
    ));
    type_line(
        &rig.term,
        &format!(
            "env -u TMUX -u TMUX_PANE -u TAW_TMUX_SOCKET_NAME -u TAW_TMUX_SOCKET_PATH {}'{}' -S '{socket_path}' attach -t '{session}'; echo $? >'{}'",
            env_words(&env),
            tmux_path(),
            rig.mac.work_dir().join("status").display()
        ),
    );
    expect(rig, "a client on this Mac's server", || {
        clients(&rig.mac).len() == 1
    });
    clients(&rig.mac)[0]
}

/// The popup key in the client, Enter on studio's session, and that terminal's client becomes the app: the same
/// pid (`#{client_pid}` before, the record's supervisor after), attached to studio. When studio's client ends and
/// Ctrl-C ends the app, the terminal is the tmux client of the session it came from again (`--back`), and when
/// that client ends the shell has status 0.
#[test]
fn enter_on_studios_row_in_the_popup_makes_the_client_the_app_in_studio() {
    let _own = Owned::new();
    let rig = rig();
    start_keeper(&rig);
    let client_pid = attach_by_hand(&rig);
    let first_session = mac_session(&rig);

    // The popup, with studio's rows read from the keeper's files.
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "F5"]);
    expect(&rig, "studio's session in the popup", || {
        screen_of(&rig.term).contains("garden")
    });
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "2"]);
    expect(&rig, "the cursor on studio's session", || {
        selected_row(&rig.term).contains("garden")
    });
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "Enter"]);

    // The client's process is the app's supervisor now, attached to studio.
    let mut app_record = serde_json::Value::Null;
    expect(&rig, "an app whose supervisor is the former client", || {
        app_record = records(&rig.mac)
            .into_iter()
            .find(|record| record["supervisor"] == client_pid)
            .unwrap_or(serde_json::Value::Null);
        app_record["inside"] == "studio"
    });
    own(app_record["pid"].as_u64().expect("a pid") as u32);
    own(client_pid);
    expect(&rig, "studio's status line on the terminal", || {
        screen_of(&rig.term).contains("[garden]")
    });
    expect(
        &rig,
        "no client of this Mac's server left: it is the app now",
        || clients(&rig.mac).is_empty(),
    );
    let attached = rig.stand_in.calls().into_iter().any(|call| {
        call.last()
            .is_some_and(|line| line.contains("'machine' 'attach' '$"))
    });
    assert!(
        attached,
        "studio's session was attached by its own id\n{}",
        seen(&rig)
    );

    // Studio's popup key, pressed in studio's tmux, is the way back to this app's list: this app is not the one
    // that holds the lease (the keeper in the other terminal does), and its client still goes back to it, where
    // studio's own popup used to open. Then Ctrl-C ends it.
    // A session of this Mac's server with a name nothing on the screen could spell by chance (the first session's
    // name is a common word, so a substring of "gone" or "done" would pass for it).
    let session = "doorway-pin";
    rig.mac.tmux_ok(&["new-session", "-d", "-s", session]);
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "F5"]);
    expect(&rig, "studio's client gone, sent back by the key", || {
        rig.far
            .server
            .tmux(&["list-clients", "-F", "#{client_pid}"])
            .is_ok_and(|out| out.trim().is_empty())
    });
    expect(&rig, "the app's list", || {
        screen_of(&rig.term).contains(" taw app")
    });
    // The app knows which server the client was on, though the shell that ran `tmux attach` named none: this
    // Mac's own session is on its list (with the old command the app took tmux's default socket, which has none).
    expect(&rig, "this Mac's own session on the app's list", || {
        screen_of(&rig.term).contains(session)
    });
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "C-c"]);
    // Ending the app puts the terminal back in the tmux it came from: a client of this Mac's server again, on the
    // session the popup was opened from (not the newest one, "doorway-pin"), and it is still the shell's one
    // `tmux attach`, so the shell has no status yet.
    expect(&rig, "a client of this Mac's server again", || {
        clients(&rig.mac).len() == 1
    });
    assert_eq!(
        rig.mac
            .tmux(&["list-clients", "-F", "#{client_session}"])
            .unwrap_or_default()
            .trim(),
        first_session,
        "the client is back on the session the popup was opened from\n{}",
        seen(&rig)
    );
    let status_file = rig.mac.work_dir().join("status");
    assert!(
        status_of(&status_file).is_none(),
        "the shell's tmux attach is still running\n{}",
        seen(&rig)
    );
    rig.mac.tmux_ok(&["detach-client", "-s", &first_session]);
    expect(&rig, "the shell's status of tmux attach", || {
        status_of(&status_file).is_some()
    });
    assert_eq!(
        status_of(&status_file).as_deref(),
        Some("0\n"),
        "the terminal is the shell's again with status 0\n{}",
        seen(&rig)
    );
    assert!(
        records(&rig.mac)
            .iter()
            .all(|record| record["supervisor"] != client_pid),
        "the ended app left no record\n{}",
        seen(&rig)
    );
    let _ = &rig.far;
}

/// An app started with `--back` in a terminal of its own (no tmux client there), against this Mac's server, as
/// the doorway starts it. The line ends in the shell's own status, written to `status`. Returns once the app's
/// list shows `shown`.
fn start_back_app(rig: &Rig, back: &str, shown: &str) {
    let mut env = rig.mac.taw_env();
    env.push(("TAW_SSH_BIN".into(), rig.stand_in.bin.display().to_string()));
    type_line(
        &rig.term,
        &format!(
            "env -u TMUX -u TMUX_PANE {}'{}' app --back '{back}' 2>'{}'; echo $? >'{}'",
            env_words(&env),
            taw_bin(),
            rig.mac.work_dir().join("back-err").display(),
            rig.mac.work_dir().join("status").display()
        ),
    );
    expect(rig, "the app's record", || records(&rig.mac).len() == 1);
    let record = records(&rig.mac)[0].clone();
    own(record["pid"].as_u64().expect("a pid") as u32);
    own(record["supervisor"].as_u64().expect("a supervisor") as u32);
    expect(rig, "the app's list", || {
        let screen = screen_of(&rig.term);
        screen.contains("Enter go in") && screen.contains(shown)
    });
}

/// The session id of a session of this Mac's server, by name.
fn session_id(mac: &Server, name: &str) -> String {
    mac.tmux(&[
        "display-message",
        "-p",
        "-t",
        &format!("={name}:"),
        "#{session_id}",
    ])
    .expect("a session id")
    .trim()
    .to_string()
}

/// The sessions the clients of this Mac's server are on.
fn client_sessions(mac: &Server) -> Vec<String> {
    mac.tmux(&["list-clients", "-F", "#{client_session}"])
        .unwrap_or_default()
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// An app started with `--back S`, quit with `Q`, replaces itself with a tmux client attached to S, on that
/// terminal; not to the newest session or the first.
#[test]
fn quitting_an_app_started_with_back_attaches_the_terminal_to_that_session() {
    let _own = Owned::new();
    let rig = rig();
    rig.mac.tmux_ok(&["new-session", "-d", "-s", "back-here"]);
    rig.mac.tmux_ok(&["new-session", "-d", "-s", "back-newer"]);
    let id = session_id(&rig.mac, "back-here");
    start_back_app(&rig, &id, "back-newer");
    // The hint says Q goes back to tmux, and not that it quits.
    expect(&rig, "the hint that Q goes back to tmux", || {
        screen_of(&rig.term).contains("Q back to tmux")
    });
    assert!(clients(&rig.mac).is_empty(), "{}", seen(&rig));
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "-l", "Q"]);
    expect(&rig, "a client of this Mac's server", || {
        clients(&rig.mac).len() == 1
    });
    assert_eq!(client_sessions(&rig.mac), ["back-here"], "{}", seen(&rig));
    assert!(
        status_of(&rig.mac.work_dir().join("status")).is_none(),
        "the shell's app is now the tmux client\n{}",
        seen(&rig)
    );
}

/// The session `--back` names is gone when the app ends: the terminal attaches to the one tmux picks.
#[test]
fn a_gone_session_attaches_to_whichever_tmux_picks() {
    let _own = Owned::new();
    let rig = rig();
    rig.mac.tmux_ok(&["new-session", "-d", "-s", "back-gone"]);
    let id = session_id(&rig.mac, "back-gone");
    start_back_app(&rig, &id, "back-gone");
    rig.mac.tmux_ok(&["kill-session", "-t", &id]);
    expect(&rig, "the session gone from the list", || {
        !screen_of(&rig.term).contains("back-gone")
    });
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "-l", "Q"]);
    expect(&rig, "a client of this Mac's server", || {
        clients(&rig.mac).len() == 1
    });
    let on = client_sessions(&rig.mac);
    assert_eq!(on.len(), 1, "{}", seen(&rig));
    assert_ne!(on[0], "back-gone", "{}", seen(&rig));
}

/// The server is gone when the app ends: it ends as an app does, with its own status, and no client.
#[test]
fn a_gone_server_ends_the_app_as_it_does_without_back() {
    let _own = Owned::new();
    let rig = rig();
    rig.mac.tmux_ok(&["new-session", "-d", "-s", "back-last"]);
    let id = session_id(&rig.mac, "back-last");
    start_back_app(&rig, &id, "back-last");
    let _ = rig.mac.tmux(&["kill-server"]);
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "-l", "Q"]);
    let status_file = rig.mac.work_dir().join("status");
    expect(&rig, "the shell's status of the app", || {
        status_of(&status_file).is_some()
    });
    assert_eq!(
        status_of(&status_file).as_deref(),
        Some("0\n"),
        "{}",
        seen(&rig)
    );
}

/// `--back` takes a session id and nothing else: a usage error (2) that says how it is said.
#[test]
fn back_takes_a_session_id() {
    let mac = Server::start();
    for target in ["garden", "3", "$", "$3;x", "@5"] {
        let out = mac
            .taw_command(&["app", "--back", target])
            .output()
            .expect("taw runs");
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert_eq!(out.status.code(), Some(2), "{target}: {stderr}");
        assert!(
            stderr.contains("--back takes the id of a tmux session, as in $3"),
            "{target}: {stderr}"
        );
    }
}

/// `taw app --go` with a bare session or window id, nothing before the colon or a machine with no id is a usage
/// error (2) that says how it is said, before anything is started (`last` and `%7` are forms of their own).
#[test]
fn go_takes_a_machine_and_an_id() {
    let mac = Server::start();
    for target in ["$3", "@5", ":%7", "studio", "%7;x"] {
        let (code, stdout, stderr) = {
            let out = mac
                .taw_command(&["app", "--go", target])
                .output()
                .expect("taw runs");
            (
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout).to_string(),
                String::from_utf8_lossy(&out.stderr).to_string(),
            )
        };
        assert_eq!(code, 2, "{target}: {stdout}{stderr}");
        assert!(
            stderr.contains("--go takes last, a pane of this machine as in %7, a machine and one of its ids as in studio:%16, or a machine and new as in studio:new"),
            "{target}: {stderr}"
        );
    }
}

/// The doorway from a session the app itself went into: the client there is the app's (its standard output is a
/// pipe the app reads, not the terminal), and `detach-client -E` keeps that standard output for the command it
/// runs. The popup is the one `taw ui` opens (the key that would send an app's client back to the list opens none
/// for it; a `taw ui` of a person's own binding does), and Enter on studio's row makes the client the app in studio,
/// on the terminal, as it does for a client attached by hand.
#[test]
fn enter_on_studios_row_in_a_popup_on_the_apps_own_client_makes_it_the_app_in_studio() {
    let _own = Owned::new();
    let rig = rig();
    let first_session = mac_session(&rig);
    // tmux does not expand `#{client_name}` in a popup's command or in `detach-client -t`, so the key runs a script
    // that is given the name and opens the popup on that client, as `ui open` does.
    let socket_path = rig
        .mac
        .tmux(&["display-message", "-p", "#{socket_path}"])
        .unwrap();
    let script = rig.mac.work_dir().join("popup.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nexec '{}' -S '{}' display-popup -c \"$1\" -E -w 90% -h 90% \"env -u TMUX -u TMUX_PANE {}'{}' ui --client '$1'\"\n",
            tmux_path(),
            socket_path.trim(),
            env_words(&rig.mac.taw_env()),
            taw_bin()
        ),
    )
    .unwrap();
    rig.mac.tmux_ok(&[
        "bind-key",
        "-n",
        "F6",
        "run-shell",
        &format!("sh '{}' '#{{client_name}}'", script.display()),
    ]);
    let mut env = rig.mac.taw_env();
    env.push(("TAW_SSH_BIN".into(), rig.stand_in.bin.display().to_string()));
    type_line(
        &rig.term,
        &format!(
            "env -u TMUX -u TMUX_PANE {}'{}' app 2>'{}'; echo $? >'{}'",
            env_words(&env),
            taw_bin(),
            rig.mac.work_dir().join("app-err").display(),
            rig.mac.work_dir().join("status").display()
        ),
    );
    expect(&rig, "the app's record", || records(&rig.mac).len() == 1);
    let record = records(&rig.mac)[0].clone();
    own(record["pid"].as_u64().expect("a pid") as u32);
    let supervisor = record["supervisor"].as_u64().expect("a supervisor") as u32;
    own(supervisor);
    expect(
        &rig,
        "this Mac's session and studio's on the app's list",
        || {
            let screen = screen_of(&rig.term);
            screen.contains("Enter go in")
                && screen.contains(&first_session)
                && screen.contains("garden")
        },
    );
    // Into this Mac's own session: the app starts a tmux client with a pipe for its standard output.
    expect(&rig, "the cursor on this Mac's session", || {
        if selected_row(&rig.term).contains(&first_session) {
            return true;
        }
        rig.term
            .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "-l", "j"]);
        std::thread::sleep(Duration::from_millis(300));
        false
    });
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "Enter"]);
    expect(&rig, "the app's client on this Mac's server", || {
        clients(&rig.mac).len() == 1
    });
    let client_pid = clients(&rig.mac)[0];
    assert_ne!(
        client_pid, supervisor,
        "the client is the app's child, not its supervisor"
    );

    // The popup on that client, studio's row, Enter.
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "F6"]);
    expect(&rig, "studio's session in the popup", || {
        screen_of(&rig.term).contains("garden")
    });
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "2"]);
    expect(&rig, "the cursor on studio's session", || {
        selected_row(&rig.term).contains("garden")
    });
    rig.term
        .tmux_ok(&["send-keys", "-t", &rig.term.pane(), "Enter"]);

    // The client's process is a supervisor now, attached to studio, on the terminal.
    expect(&rig, "an app whose supervisor is the former client", || {
        records(&rig.mac)
            .iter()
            .any(|record| record["supervisor"] == client_pid && record["inside"] == "studio")
    });
    own(client_pid);
    expect(&rig, "studio's status line on the terminal", || {
        screen_of(&rig.term).contains("[garden]")
    });
    let _ = &rig.far;
}

/// `taw app` with its standard input on a terminal and its standard output on a pipe (as a client's command is
/// run by `detach-client -E`) draws on the terminal it was started from, instead of refusing.
#[test]
fn an_app_whose_standard_output_is_a_pipe_draws_on_its_terminal() {
    let _own = Owned::new();
    let rig = rig();
    let mut env = rig.mac.taw_env();
    env.push(("TAW_SSH_BIN".into(), rig.stand_in.bin.display().to_string()));
    type_line(
        &rig.term,
        &format!(
            "env -u TMUX -u TMUX_PANE {}'{}' app 2>'{}' | cat >/dev/null; echo $? >'{}'",
            env_words(&env),
            taw_bin(),
            rig.mac.work_dir().join("pipe-err").display(),
            rig.mac.work_dir().join("status").display()
        ),
    );
    expect(&rig, "the app's record", || records(&rig.mac).len() == 1);
    let record = records(&rig.mac)[0].clone();
    own(record["pid"].as_u64().expect("a pid") as u32);
    own(record["supervisor"].as_u64().expect("a supervisor") as u32);
    expect(&rig, "the app's list on the terminal", || {
        screen_of(&rig.term).contains("Enter go in")
    });
}
