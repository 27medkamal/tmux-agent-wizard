//! The app acting on a machine's rows, end to end: the built binary against private tmux servers,
//! the app of this Mac and the REAL far `taw` of the machine `studio` behind the stand-in for ssh, as
//! `tests/app_preview.rs` and `tests/app_notify.rs` do it. A key on a row of studio's sends the verb to
//! studio's own taw, and what the tests read is studio's own tmux server, not the screen alone.
//!
//! "This Mac" is a private server with a session of its own; `studio` is a second one with the sessions
//! `garden` (windows `main` and `tools`) and `photos` (windows `shots` and `edits`); the terminal is a third.
//! No test runs a real ssh. Every wait is positive, with a 20 s deadline (the one test of the 2 s hint waits 4 s),
//! and panics with what was seen; a check that something did NOT happen comes after a wait for the effect
//! of the same event that did.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::machine::{FarSide, StandIn};
use common::owned::{Owned, own, record_names};
use common::{Server, ServerOptions, taw_bin, wait_for, write_script};
use taw::machine::words;

/// The terminal, this Mac, studio and the stand-in.
struct Rig {
    term: Server,
    mac: Server,
    far: FarSide,
    stand_in: StandIn,
    /// This test's own folder: the hooks, the log of this Mac's `pre_close` and the checkouts.
    scratch: tempfile::TempDir,
}

impl Rig {
    /// The file this Mac's `pre_close` recorder appends to.
    fn mac_log(&self) -> PathBuf {
        self.scratch.path().join("mac-pre-close.txt")
    }

    /// The lines this Mac's `pre_close` recorder has written.
    fn mac_closes(&self) -> Vec<String> {
        std::fs::read_to_string(self.mac_log())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
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

/// `text` as a TOML basic string.
fn toml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// This Mac, with `studio` in `[machines]` and a `[hooks] pre_close` that only writes a line to a file and
/// allows the close, and studio: `garden` (`main`, `tools`) and `photos` (`shots`, `edits`). `far_hooks` is the
/// `[hooks]` body of studio's config (empty for none); `git` makes studio's directory a repository with one
/// commit and a folder of checkouts, so that its worktree verbs work.
fn rig(far_hooks: &str, git: bool) -> Rig {
    let scratch = tempfile::tempdir().unwrap();
    let mac = Server::start();
    let recorder = scratch.path().join("mac-pre-close.sh");
    let log = scratch.path().join("mac-pre-close.txt");
    write_script(
        &recorder,
        &format!("printf '%s\\n' \"$TAW_PANE\" >> '{}'\n", log.display()),
    );
    mac.write_config(&format!(
        "[ui]\ndepth = \"all\"\n\n[machines]\nstudio = \"studio\"\n\n[hooks]\npre_close = {}\n",
        toml_string(&recorder.display().to_string())
    ));
    // A second window, so that a close of a pane here does not end the server.
    mac.tmux_ok(&["new-window", "-d", "-n", "extra"]);
    let far = FarSide::with_session("studio", "garden");
    far.server
        .tmux_ok(&["rename-window", "-t", "garden:0", "main"]);
    far.server
        .tmux_ok(&["new-window", "-d", "-t", "garden:", "-n", "tools"]);
    far.server
        .tmux_ok(&["new-session", "-d", "-s", "photos", "-n", "shots"]);
    far.server
        .tmux_ok(&["new-window", "-d", "-t", "photos:", "-n", "edits"]);
    let mut config = String::new();
    if !far_hooks.is_empty() {
        config.push_str(&format!("[hooks]\n{far_hooks}\n"));
    }
    if git {
        let checkouts = scratch.path().join("checkouts");
        std::fs::create_dir_all(&checkouts).unwrap();
        config.push_str(&format!(
            "[worktrees]\ndirectory = {}\n",
            toml_string(&checkouts.display().to_string())
        ));
        let repo = |args: &[&str]| {
            let status = taw::git::command()
                .arg("-C")
                .arg(far.server.work_dir())
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        repo(&["init", "-q", "-b", "main"]);
        std::fs::write(far.server.work_dir().join("README.md"), "hello\n").unwrap();
        repo(&["add", "."]);
        repo(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=Test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "first",
        ]);
    }
    if !config.is_empty() {
        far.server.write_config(&config);
    }
    let stand_in = StandIn::new(&[&far]);
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
            .into_iter()
            .rev()
            .filter(|call| !call.last().is_some_and(|line| line.contains("'read'")))
            .take(8)
            .map(|call| {
                let line = call.last().cloned().unwrap_or_default();
                line.split(" taw ").last().unwrap_or_default().to_string()
            })
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

/// Puts the cursor on the row of studio's session `name`, whatever number the list gave it: the key is read off
/// the screen, where a session row begins with it.
fn move_to_session(rig: &Rig, name: &str) {
    let key = |screen: &str| {
        screen.lines().find_map(|line| {
            let row = line.trim_start_matches('\u{2502}');
            let digit = row.chars().next().filter(char::is_ascii_digit)?;
            (row[1..].trim_start().starts_with(name) && row.contains("  ")).then_some(digit)
        })
    };
    expect_screen(rig, "the session on the list", |screen| {
        key(screen).is_some()
    });
    let digit = key(&terminal_screen(&rig.term)).unwrap();
    press(&rig.term, &[&digit.to_string()]);
    expect(rig, "the cursor on the session", || {
        let row = selected_row(&rig.term);
        row.contains(name) && !row.contains(':')
    });
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

// ---------------------------------------------------------------- the app

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

// ---------------------------------------------------------------- what the tests read and do

/// Studio's panes as its own server lists them: `session:window pane-id`, one to a line.
fn far_panes(rig: &Rig) -> String {
    rig.far.server.tmux_ok(&[
        "list-panes",
        "-a",
        "-F",
        "#{session_name}:#{window_name} #{pane_id}",
    ])
}

/// Studio's session names, sorted.
fn far_sessions(rig: &Rig) -> Vec<String> {
    let mut names: Vec<String> = rig
        .far
        .server
        .tmux_ok(&["list-sessions", "-F", "#{session_name}"])
        .lines()
        .map(str::to_string)
        .collect();
    names.sort();
    names
}

/// The window names of studio's session `session`.
fn far_windows(rig: &Rig, session: &str) -> Vec<String> {
    rig.far
        .server
        .tmux_ok(&["list-windows", "-t", session, "-F", "#{window_name}"])
        .lines()
        .map(str::to_string)
        .collect()
}

/// Waits (20 s) until `ready` holds, and fails with `what`, studio's own panes and everything [`seen`] when it
/// does not: a check of the far side, not of the screen.
fn expect_far(rig: &Rig, what: &str, mut ready: impl FnMut() -> bool) {
    assert!(
        wait_for(&mut ready, Duration::from_secs(20)),
        "{what} never happened on studio.\nstudio's panes: {}\nstudio's sessions: {:?}\n{}",
        far_panes(rig),
        far_sessions(rig),
        seen(rig)
    );
}

/// Types `text` (literally) into the terminal.
fn type_text(term: &Server, text: &str) {
    press(term, &["-l", text]);
}

/// Opens a prompt with `open`, answers it with `text`, and waits (10 s) for the fact that the verb `verb` was sent
/// to studio. The app holds one action in flight per machine until its worker is drained on a tick, and the
/// previous verb has changed studio's state before the app has been told: an answer in that gap is the toast
/// `still answering`, nothing sent, so the prompt is opened and answered again until the verb has gone.
fn answer_when_free(rig: &Rig, open: &dyn Fn(), prompt: &str, text: &str, verb: &str) {
    let before = far_calls(rig, verb).len();
    let busy = words::still_answering("studio");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        open();
        expect_screen(rig, &format!("the prompt {prompt:?}"), |screen| {
            screen.contains(prompt)
        });
        press(&rig.term, &["BSpace"; 24]);
        type_text(&rig.term, text);
        press(&rig.term, &["Enter"]);
        let outcome = wait_for(
            || far_calls(rig, verb).len() > before || terminal_screen(&rig.term).contains(&busy),
            Duration::from_secs(10),
        );
        if far_calls(rig, verb).len() > before {
            return;
        }
        assert!(
            outcome && std::time::Instant::now() < deadline,
            "the verb {verb} was never sent, and studio never said it was still answering\n{}",
            seen(rig)
        );
    }
}

/// The calls the stand-in has run, as the far command lines, those that name `verb words`.
fn far_calls(rig: &Rig, words: &str) -> Vec<String> {
    rig.stand_in
        .calls()
        .into_iter()
        .filter_map(|call| call.last().cloned())
        .filter(|line| line.contains(&format!(" taw {words}")))
        .collect()
}

/// A worktree made on studio from `garden` by its own taw: its record.
fn far_worktree(rig: &Rig, branch: &str) -> serde_json::Value {
    rig.far.server.ok(
        &[
            "worktree",
            "create",
            "--workspace",
            "garden",
            "--branch",
            branch,
            "--no-focus",
        ],
        "cli:worktree:create",
        "worktree_created",
    )["worktree"]
        .clone()
}

// ---------------------------------------------------------------- the tests

/// `x` then `y` on studio's pane in `photos`: the question names studio, and the pane is gone from studio's own
/// server. Mutation: `remote_argv` dropping the pane id from `pane close`
/// (src/machine/actions.rs): the far verb gets no id and the pane stays.
#[test]
fn x_then_y_on_a_machines_pane_closes_it_on_that_machine() {
    let _own = Owned::new();
    let rig = rig("", false);
    let doomed = rig
        .far
        .server
        .tmux_ok(&["list-panes", "-t", "photos:edits", "-F", "#{pane_id}"])
        .trim()
        .to_string();
    start_app(&rig);

    move_to_in(&rig, "3", "photos", "1:edits");
    press(&rig.term, &["x"]);
    expect_screen(&rig, "the question naming studio", |screen| {
        screen.contains("kill-pane studio")
    });
    press(&rig.term, &["y"]);
    expect_far(&rig, "the pane going away", || {
        !far_panes(&rig).contains(&doomed)
    });
    assert!(
        far_panes(&rig).contains("photos:shots"),
        "only the one pane went: {}",
        far_panes(&rig)
    );
    assert_eq!(
        far_calls(&rig, &format!("'pane' 'close' '{doomed}'")).len(),
        1,
        "the raw id, once: {:?}",
        far_calls(&rig, "'pane' 'close'")
    );
}

/// `x` in `garden`, where studio's own `pre_close` refuses: the toast has the far words, the pane stays, and this
/// Mac's own `pre_close` recorder is empty; the positive half is a close of a pane of this Mac's in the same run,
/// which makes the recorder log one line. Mutation: `classify` reading a `hook_refused` answer as done
/// (src/machine/actions.rs): no toast shows and the pane goes.
#[test]
fn a_far_hook_refusal_is_a_toast_and_this_macs_hook_is_not_asked() {
    let _own = Owned::new();
    // The folder of the far hook lives as long as the test and goes with it.
    let hooks = tempfile::tempdir().unwrap();
    let hook = hooks.path().join("refuse.sh");
    write_script(
        &hook,
        "if [ \"$TAW_SESSION\" = garden ]; then echo 'uncommitted changes in ~/code/garden' >&2; exit 1; fi\n",
    );
    let rig = rig(
        &format!("pre_close = {}", toml_string(&hook.display().to_string())),
        false,
    );
    let tools = rig.far.server.pane_of("garden:tools");
    start_app(&rig);

    move_to(&rig, "1:tools");
    press(&rig.term, &["x"]);
    expect_screen(&rig, "the question naming studio", |screen| {
        screen.contains("kill-pane studio")
    });
    press(&rig.term, &["y"]);
    let said = words::hook_refused(
        "studio",
        "pre-close hook",
        "uncommitted changes in ~/code/garden",
    );
    expect_screen(&rig, "the far hook's refusal", |screen| {
        screen.contains(&said)
    });
    assert!(far_panes(&rig).contains(&tools), "the pane is still there");

    // The positive half: a close of this Mac's own pane in the same run is asked about by the recorder.
    let session = rig
        .mac
        .tmux_ok(&["list-sessions", "-F", "#{session_name}"])
        .lines()
        .next()
        .unwrap()
        .to_string();
    move_to_in(&rig, "1", &session, "1:extra");
    press(&rig.term, &["x"]);
    expect_screen(&rig, "the question about this Mac's pane", |screen| {
        screen.contains("kill-pane") && !screen.contains("kill-pane studio")
    });
    press(&rig.term, &["y"]);
    expect(
        &rig,
        "this Mac's pre_close recorder logging the local close",
        || !rig.mac_closes().is_empty(),
    );
    assert_eq!(
        rig.mac_closes().len(),
        1,
        "only the local close was asked about: {:?}\n{}",
        rig.mac_closes(),
        seen(&rig)
    );
    assert!(far_panes(&rig).contains(&tools), "still there");
}

/// The group question and the dirty question round-trip: a worktree of studio's with an uncommitted file is
/// removed after `y` to "remove anyway?", and a session with a worktree session open asks the group question
/// naming studio and `y` closes both. Mutations (src/machine/actions.rs, `remote_argv`): `--group` dropped from the
/// group verb, and `--force` dropped from the removal; each leaves its half of the test waiting for studio.
///
/// macOS temp folders sit behind the `/var` link (`/private/var`), which is why the rig makes `garden` again in the
/// resolved folder (see `rig`): without it the app never pairs the worktree's record with its session on studio.
#[test]
fn the_dirty_question_and_the_group_question_round_trip_with_the_machine() {
    let _own = Owned::new();
    let rig = rig("", true);
    let first = far_worktree(&rig, "zed");
    let path = PathBuf::from(first["path"].as_str().unwrap());
    // Closed, so that the list has a row for the record; one file in it that git does not know.
    rig.far.server.tmux_ok(&[
        "kill-session",
        "-t",
        first["session_name"].as_str().unwrap(),
    ]);
    std::fs::write(path.join("scratch.txt"), "not committed\n").unwrap();
    start_app(&rig);

    move_to_in(&rig, "2", "garden", "zed");
    press(&rig.term, &["x"]);
    expect_screen(&rig, "the question naming studio", |screen| {
        screen
            .lines()
            .any(|line| line.contains("studio") && line.contains("zed") && line.contains('?'))
    });
    press(&rig.term, &["y"]);
    expect_screen(&rig, "the dirty question", |screen| {
        screen.contains("uncommitted changes are lost")
    });
    assert!(path.exists(), "nothing is removed before the answer");
    press(&rig.term, &["y"]);
    expect_far(&rig, "the checkout going away", || !path.exists());

    // The group: a worktree session open under garden. The feed of a machine carries the records of the
    // worktrees its sessions run beside the closed ones, so the app knows the open one is linked and the question
    // names it and their count, as on this machine; it is strict: `Enter` answers no and `y` alone runs `--group`.
    let second = far_worktree(&rig, "zork");
    let linked = second["session_name"].as_str().unwrap().to_string();
    expect_screen(&rig, "the new worktree session on the list", |screen| {
        screen.contains(&linked)
    });
    move_to_session(&rig, "garden");
    press(&rig.term, &["x"]);
    let named = words::group_close_named("kill-session studio \u{b7} garden?", &[&linked]);
    expect_screen(&rig, "the group question naming studio", |screen| {
        screen.contains(&named)
    });
    assert!(
        far_sessions(&rig).contains(&"garden".to_string()),
        "nothing is closed before the answer"
    );
    // Strict: Enter is no.
    press(&rig.term, &["Enter"]);
    expect_screen(&rig, "the group question gone after Enter", |screen| {
        !screen.contains(&named)
    });
    assert!(
        far_sessions(&rig).contains(&"garden".to_string()) && far_sessions(&rig).contains(&linked),
        "Enter closed nothing: {:?}",
        far_sessions(&rig)
    );
    press(&rig.term, &["x"]);
    expect_screen(&rig, "the group question again", |screen| {
        screen.contains(&named)
    });
    press(&rig.term, &["y"]);
    expect_screen(
        &rig,
        "the toast with the count the machine closed",
        |screen| {
            screen.contains(&format!(
                "closed {} and its 1 worktree session",
                words::on_machine("studio", "garden")
            ))
        },
    );
    expect_far(&rig, "both sessions going away", || {
        let sessions = far_sessions(&rig);
        !sessions.contains(&"garden".to_string()) && !sessions.contains(&linked)
    });
    assert_eq!(far_sessions(&rig), vec!["photos".to_string()]);
    assert_eq!(
        far_calls(&rig, "'workspace' 'close'")
            .iter()
            .filter(|line| line.contains("'--group'"))
            .count(),
        1,
        "the group verb once: {:?}",
        far_calls(&rig, "'workspace' 'close'")
    );
}

/// `r` renames studio's window, its agent and its workspace, each once and on studio's own server; `w` makes a
/// worktree session on studio and the app goes in by the record's workspace id (`w1`: `worktree create` prints
/// no session id, so `worktree open --workspace w1` is run and the app attaches by the session id it answers).
/// Mutation: the agent rename sent as `tab rename` in `remote_argv` (src/machine/actions.rs): the agent keeps
/// its name.
#[test]
fn r_renames_the_window_the_agent_and_the_workspace_and_w_makes_a_worktree_session_to_go_in() {
    let _own = Owned::new();
    let rig = rig("", true);
    let main = rig.far.server.pane_of("garden:main");
    rig.far.server.ok(
        &[
            "pane",
            "report-agent",
            &main,
            "--source",
            "integration:test",
            "--agent=claude",
            "--state",
            "working",
        ],
        "cli:pane:report-agent",
        "ok",
    );
    expect_far(&rig, "the agent being read as working", || {
        rig.far.server.pane_opt(&main, "@agent_state") == "working"
    });
    start_app(&rig);

    // The window of a plain pane.
    move_to(&rig, "1:tools");
    answer_when_free(
        &rig,
        &|| press(&rig.term, &["r"]),
        "rename-window",
        "logs",
        "'tab' 'rename'",
    );
    expect_far(&rig, "the window's new name", || {
        let names = far_windows(&rig, "garden");
        names.contains(&"logs".to_string()) && !names.contains(&"tools".to_string())
    });
    assert_eq!(far_calls(&rig, "'tab' 'rename'").len(), 1);

    // The agent of the pane that has one: `r` asks which, `a` is the agent.
    move_to(&rig, "0:main");
    answer_when_free(
        &rig,
        &|| {
            press(&rig.term, &["r"]);
            expect_screen(&rig, "the chooser", |screen| {
                screen.contains("rename what?")
            });
            press(&rig.term, &["a"]);
        },
        "name agent",
        "scribe",
        "'agent' 'rename'",
    );
    expect_far(&rig, "the agent's new name", || {
        rig.far.server.pane_opt(&main, "@agent_name") == "scribe"
    });
    assert_eq!(far_calls(&rig, "'agent' 'rename'").len(), 1);

    // The workspace.
    press(&rig.term, &["2"]);
    expect_screen(&rig, "the cursor on studio's session", |screen| {
        selected_row(&rig.term).contains("garden") && !screen.is_empty()
    });
    answer_when_free(
        &rig,
        &|| press(&rig.term, &["r"]),
        "rename-session",
        "orchard",
        "'workspace' 'rename'",
    );
    expect_far(&rig, "the session's new name", || {
        far_sessions(&rig).contains(&"orchard".to_string())
    });
    assert_eq!(far_calls(&rig, "'workspace' 'rename'").len(), 1);

    // A worktree session, and the app in it.
    move_to_in(&rig, "2", "orchard", "orchard");
    answer_when_free(
        &rig,
        &|| press(&rig.term, &["w"]),
        "new worktree in",
        "zed",
        "'worktree' 'create'",
    );
    expect_far(&rig, "the worktree's session", || {
        far_sessions(&rig).len() == 3
    });
    let new: String = far_sessions(&rig)
        .into_iter()
        .find(|name| name != "orchard" && name != "photos")
        .unwrap();
    expect_far(
        &rig,
        "a client of the app attached to the new session",
        || {
            rig.far
                .server
                .tmux_ok(&["list-clients", "-F", "#{session_name}"])
                .lines()
                .any(|name| name == new)
        },
    );
    let created = far_calls(&rig, "'worktree' 'create'");
    assert_eq!(created.len(), 1, "{created:?}");
    assert!(created[0].contains("'--no-focus'"), "{created:?}");
    let opened = far_calls(&rig, "'worktree' 'open'");
    assert!(
        opened.iter().any(|line| line.contains("'--workspace' 'w")),
        "went in by the record's workspace id: {opened:?}"
    );
}

/// The stand-in hangs on studio's `pane` verbs: `waiting for studio` shows within the 2 s of the hint plus a
/// margin (4 s), a key still moves the cursor while the close waits, and the test ends by the pids of the hung
/// calls and never by a 20 s wait. Mutation: `HINT_AFTER` made 60 s (src/tui/app_screen/remote_actions.rs):
/// the words never show within 4 s.
#[test]
fn a_close_that_hangs_says_waiting_for_the_machine_and_the_keys_still_move() {
    let _own = Owned::new();
    let rig = rig("", false);
    start_app(&rig);
    move_to(&rig, "1:tools");
    press(&rig.term, &["x"]);
    expect_screen(&rig, "the question naming studio", |screen| {
        screen.contains("kill-pane studio")
    });
    rig.stand_in.hang_verb("pane");
    press(&rig.term, &["y"]);

    let said = words::waiting("studio");
    assert!(
        wait_for(
            || terminal_screen(&rig.term).contains(&said),
            Duration::from_secs(4)
        ),
        "no {said:?} within 4 s\n{}",
        seen(&rig)
    );
    let row = selected_row(&rig.term);
    press(&rig.term, &["k"]);
    expect(
        &rig,
        "the cursor moving a row while the close waits",
        || selected_row(&rig.term) != row,
    );
    assert!(
        far_panes(&rig).contains(&rig.far.server.pane_of("garden:tools")),
        "nothing was closed: the verb never ran"
    );
    // End it: the hung calls by their own pids (the guard kills what is still there), then the stand-in answers.
    let hung: Vec<u32> = rig
        .stand_in
        .calls()
        .iter()
        .zip(rig.stand_in.pids())
        .filter(|(call, _)| {
            call.last()
                .is_some_and(|line| line.contains(" taw 'pane' 'close'"))
        })
        .map(|(_, pid)| pid)
        .collect();
    assert_eq!(hung.len(), 1, "one close was sent: {hung:?}");
    for pid in hung {
        own(pid);
    }
    rig.stand_in.answer_verb("pane");
}

/// This Mac's popup (`taw ui`): a live machine's row is still read-only there. The refusal sentence shows, and
/// nothing is sent: the stand-in saw no verb that closes. Mutation: `refusal_for` (src/tui/app/actions_for_row.rs) filtering
/// on `!header.live` alone, so a live machine's row is not refused in the popup: the question opens and the
/// sentence never shows.
#[test]
fn the_popup_of_this_mac_still_refuses_a_live_machines_row() {
    let _own = Owned::new();
    let rig = rig("", false);
    // The app is the keeper of the machines' files, which the popup reads: it runs in the rig's terminal.
    start_app(&rig);
    let popup = terminal();
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
        "env -u TMUX -u TMUX_PANE {env}'{}' ui 2>'{}'",
        taw_bin(),
        rig.mac.work_dir().join("popup-err").display(),
    );
    popup.tmux_ok(&["send-keys", "-t", &popup.pane(), "-l", &line]);
    popup.tmux_ok(&["send-keys", "-t", &popup.pane(), "Enter"]);
    let shown = |term: &Server, ready: &dyn Fn(&str) -> bool| {
        wait_for(|| ready(&terminal_screen(term)), Duration::from_secs(20))
    };
    assert!(
        shown(&popup, &|screen| screen.contains("0:main")
            && screen.contains("1:tools")),
        "studio's rows never showed in the popup\n{}",
        seen(&rig)
    );
    press(&popup, &["2"]);
    assert!(
        wait_for(
            || selected_row(&popup).contains("garden"),
            Duration::from_secs(20)
        ),
        "the cursor never reached studio's session in the popup\n{}",
        terminal_screen(&popup)
    );
    press(&popup, &["x"]);
    let said = words::read_only("studio");
    assert!(
        shown(&popup, &|screen| screen.contains(&said)),
        "the popup's refusal sentence never showed\n{}",
        terminal_screen(&popup)
    );
    assert!(
        far_calls(&rig, "'pane' 'close'").is_empty()
            && far_calls(&rig, "'workspace' 'close'").is_empty(),
        "nothing was sent: {:?}",
        rig.stand_in.calls()
    );
    assert!(far_panes(&rig).contains(&rig.far.server.pane_of("garden:tools")));
}

/// No git that the two tests which make a repository run (`rig("", true)`: the worktree questions and the
/// worktree made by `w`) reaches the repository that git's variables of whoever runs it point at (git runs
/// the command of a `rebase -x` with `GIT_DIR` set): they run again with each of the variables pointing into
/// a decoy, pass and leave it byte for byte as it was (tests/common/git_decoy.rs). The far taw the stand-in
/// starts runs under `env -i` and never sees them; the git the rig runs itself (`taw::git::command()`) takes
/// them out.
#[test]
fn the_tests_that_make_a_repository_leave_the_one_gits_variables_point_at_as_it_was() {
    common::git_decoy::rerun(
        &[
            "the_dirty_question_and_the_group_question_round_trip_with_the_machine",
            "r_renames_the_window_the_agent_and_the_workspace_and_w_makes_a_worktree_session_to_go_in",
        ],
        "leave_the_one_gits_variables_point_at",
    );
}
