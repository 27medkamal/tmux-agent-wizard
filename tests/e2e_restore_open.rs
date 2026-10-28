//! A restore into a session that is open, and into no server at all, end to end through the built
//! `taw` binary on private sockets.
//!
//! An open saved session is no longer skipped whole: each saved window that holds an agent whose
//! conversation runs nowhere in the server is made again at the end of it, and its agent is resumed.
//! Nothing already open changes. With no server running, a restore starts one with the first session
//! it makes. The agents are stand-ins: a fake `claude` that records its arguments and stays up, found
//! first on the panes' `PATH`.

mod common;

use std::path::Path;
use std::time::{Duration, SystemTime};

use common::{Server, ServerOptions, wait_for, write_script};
use serde_json::Value;

const SETTLE: Duration = Duration::from_secs(20);

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// The two conversations of the windows that were lost, and one that the new server runs of its own.
const A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
const OWN: &str = "5e1f0c00-0000-4000-8000-000000000003";
const C: &str = "c0ffee00-0000-4000-8000-000000000004";

/// The fake `claude`: it writes its arguments to `<records>/<id>` (`first` when it is not resumed) and
/// stays up as a child of its shell.
fn put_fake_claude(dir: &Path, records: &Path) {
    std::fs::create_dir_all(records).unwrap();
    write_script(
        &dir.join("bin").join("claude"),
        &format!(
            "id=first; prev=\n\
             for a in \"$@\"; do [ \"$prev\" = --resume ] && id=$a; prev=$a; done\n\
             printf 'ARGS=%s\\n' \"$*\" > '{}'/\"$id\"\n\
             cat\n",
            records.display()
        ),
    );
}

fn read_record(records: &Path, id: &str) -> Option<String> {
    std::fs::read_to_string(records.join(id))
        .ok()
        .filter(|text| text.contains("ARGS="))
}

/// A server whose panes find the fake `claude` first, with `session` as its one session.
fn options(dir: &Path, session: &str) -> ServerOptions {
    ServerOptions {
        session: session.to_string(),
        width: 100,
        height: 30,
        path_prefix: Some(dir.join("bin")),
        ..ServerOptions::default()
    }
}

/// A new window `name` at the end of `session`, its pane marked as a Claude pane holding the
/// conversation `id`, as the watcher marks a pane that runs one. Returns the pane.
fn agent_window(server: &Server, session: &str, name: &str, id: &str) -> String {
    let pane = plain_window(server, session, name);
    server.set_pane_opt(&pane, "@agent_kind", "claude");
    server.set_pane_opt(&pane, "@agent_session_id", id);
    pane
}

/// A new window `name` at the end of `session`, `-d` so that the window that is current stays. Returns
/// its pane.
fn plain_window(server: &Server, session: &str, name: &str) -> String {
    server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        &format!("={session}:"),
        "-n",
        name,
        "-c",
        server.work_dir().to_str().unwrap(),
        "-P",
        "-F",
        "#{pane_id}",
    ])
}

/// `index name active` of each window of `session`, in index order.
fn windows(server: &Server, session: &str) -> Vec<String> {
    server
        .tmux_ok(&[
            "list-windows",
            "-t",
            &format!("={session}"),
            "-F",
            "#{window_index} #{window_name} #{window_active}",
        ])
        .lines()
        .map(str::to_string)
        .collect()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn default_json(server: &Server) -> std::path::PathBuf {
    server.state_dir().join("sessions").join("default.json")
}

fn age_default(server: &Server) {
    std::fs::File::options()
        .write(true)
        .open(default_json(server))
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(120))
        .unwrap();
}

fn autosaved(server: &Server) -> bool {
    let (code, env) = server.taw(&["session", "autosave"]);
    assert_eq!(code, 0, "{env}");
    env["result"]["saved"] == true
}

fn restore(server: &Server, args: &[&str]) -> Value {
    let mut all = vec!["session", "restore"];
    all.extend_from_slice(args);
    server.ok(&all, "cli:session:restore", "session_restored")
}

/// The crash case. The old server had `work` with a shell window and two agent windows; the new one
/// has `work` open with other windows, one of them running an agent of its own, and the first of them
/// current. A plain restore adds exactly the two agent windows at the end of `work`, resumes them,
/// and leaves `work`'s own windows and the current one as they were.
#[test]
fn after_a_crash_an_open_session_gets_its_lost_agent_windows_and_nothing_else_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let records = tmp.path().join("records");
    put_fake_claude(tmp.path(), &records);
    let mut server = Server::start_with(options(tmp.path(), "work"));
    server.write_config(CONFIG);

    // The old server's layout: a shell window and two agent windows, the second with a shell beside
    // its agent in a folder of its own.
    server.tmux_ok(&["rename-window", "-t", "=work:0", "shell"]);
    agent_window(&server, "work", "agent-a", A);
    let second = agent_window(&server, "work", "agent-b", B);
    // The saved current window is one of the two that are added, which must not take the current
    // window of the open session along.
    server.tmux_ok(&["select-window", "-t", &second]);
    let sub = server.work_dir().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    server.tmux_ok(&[
        "split-window",
        "-d",
        "-t",
        &second,
        "-c",
        sub.to_str().unwrap(),
    ]);
    assert!(autosaved(&server));

    // The server dies and a new one starts with `work` in it, which was opened at once: a window of
    // its own and one that runs an agent of its own, the first of them current.
    server.crash_and_start_afresh(&options(tmp.path(), "work"));
    server.tmux_ok(&["rename-window", "-t", "=work:0", "main"]);
    plain_window(&server, "work", "own-1");
    let own_pane = agent_window(&server, "work", "own-2", OWN);
    server.tmux_ok(&["select-window", "-t", "=work:1"]);
    age_default(&server);
    assert!(autosaved(&server));
    let before = windows(&server, "work");
    assert_eq!(
        before,
        ["0 main 0", "1 own-1 1", "2 own-2 0"],
        "precondition: the new server's own windows"
    );

    // The dry run says what it would add, and adds nothing.
    let dry = restore(&server, &["--dry-run"]);
    let plan = strings(&dry["plan"]);
    assert!(
        plan[0].starts_with("would restore the layout saved at "),
        "the old layout was not looked back to: {dry}"
    );
    assert!(
        plan.contains(
            &"would add saved window 1 'agent-a' to open session 'work' as window 3".to_string()
        ) && plan.contains(
            &"would add saved window 2 'agent-b' to open session 'work' as window 4".to_string()
        ),
        "{dry}"
    );
    assert!(
        !plan.iter().any(|line| line.contains("'shell'")),
        "the plain shell window is not added: {dry}"
    );
    assert_eq!(windows(&server, "work"), before, "{dry}");

    let restored = restore(&server, &[]);
    assert_eq!(
        strings(&restored["added"]),
        [
            "saved window 1 'agent-a' as window 3 of 'work'",
            "saved window 2 'agent-b' as window 4 of 'work'"
        ],
        "{restored}"
    );
    assert_eq!(
        strings(&restored["created"]),
        Vec::<String>::new(),
        "{restored}"
    );
    assert_eq!(
        strings(&restored["skipped"]),
        Vec::<String>::new(),
        "{restored}"
    );
    assert_eq!(strings(&restored["resumed"]).len(), 2, "{restored}");
    assert_eq!(
        windows(&server, "work"),
        [
            "0 main 0",
            "1 own-1 1",
            "2 own-2 0",
            "3 agent-a 0",
            "4 agent-b 0"
        ],
        "own windows are as they were, the current one is still current: {restored}"
    );
    // The second added window has both its panes, the second in its saved folder.
    let paths = server.tmux_ok(&[
        "list-panes",
        "-t",
        "=work:agent-b",
        "-F",
        "#{pane_current_path}",
    ]);
    let paths: Vec<&str> = paths.lines().collect();
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert!(paths[1].ends_with("/sub"), "{paths:?}");
    // Both conversations were resumed, each by its own id.
    for id in [A, B] {
        assert!(
            wait_for(|| read_record(&records, id).is_some(), SETTLE),
            "{id} was not resumed"
        );
    }
    assert_eq!(
        server.global_opt("@agent_wizard_restoring"),
        "",
        "autosave is let go"
    );

    // What runs is not added again.
    let again = restore(&server, &[]);
    assert_eq!(strings(&again["added"]), Vec::<String>::new(), "{again}");
    assert_eq!(strings(&again["skipped"]), ["work"], "{again}");
    assert_eq!(windows(&server, "work").len(), 5, "{again}");

    // Nothing was typed into the open session's own agent pane, by either restore: the positive waits
    // above saw the two resumes that did happen, so a line typed into it would have run by now too.
    assert!(
        read_record(&records, OWN).is_none(),
        "the open session's own agent was resumed"
    );
    let screen = server.tmux_ok(&["capture-pane", "-p", "-J", "-t", &own_pane]);
    assert!(
        !screen.contains("claude") && !screen.contains("resume"),
        "something was typed into the open session's own agent pane: {screen:?}"
    );
}

/// An agent whose conversation already runs in another pane of the server is not added, and one that
/// does not is: `--name` follows the same rule as a plain restore.
#[test]
fn a_window_whose_agent_conversation_runs_in_another_pane_is_not_added() {
    let tmp = tempfile::tempdir().unwrap();
    let records = tmp.path().join("records");
    put_fake_claude(tmp.path(), &records);
    let server = Server::start_with(options(tmp.path(), "work"));
    server.write_config(CONFIG);
    server.tmux_ok(&["rename-window", "-t", "=work:0", "shell"]);
    agent_window(&server, "work", "agent-a", A);
    agent_window(&server, "work", "agent-b", B);
    // A third window holds C, which is lost, and beside it a pane with B again: the window is added
    // for C, and B's pane in it is not resumed, B running in the other session.
    let third = agent_window(&server, "work", "agent-c", C);
    let beside = server.tmux_ok(&["split-window", "-d", "-t", &third, "-P", "-F", "#{pane_id}"]);
    server.set_pane_opt(&beside, "@agent_kind", "claude");
    server.set_pane_opt(&beside, "@agent_session_id", B);
    restore_save(&server, "layout");

    // The two agent windows are closed. B's conversation runs on in a pane of another session.
    server.tmux_ok(&["kill-window", "-t", "=work:agent-a"]);
    server.tmux_ok(&["kill-window", "-t", "=work:agent-b"]);
    server.tmux_ok(&["kill-window", "-t", "=work:agent-c"]);
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "elsewhere",
        "-x",
        "100",
        "-y",
        "30",
        "-c",
        server.work_dir().to_str().unwrap(),
    ]);
    let pane = server.pane_of("elsewhere");
    server.set_pane_opt(&pane, "@agent_kind", "claude");
    server.set_pane_opt(&pane, "@agent_session_id", B);
    assert_eq!(windows(&server, "work"), ["0 shell 1"]);

    let dry = restore(&server, &["--name", "layout", "--dry-run"]);
    let plan = strings(&dry["plan"]);
    assert!(
        plan.contains(
            &"would add saved window 1 'agent-a' to open session 'work' as window 1".to_string()
        ),
        "{dry}"
    );
    assert!(
        plan.contains(
            &"would add saved window 3 'agent-c' to open session 'work' as window 2".to_string()
        ),
        "{dry}"
    );
    assert!(
        !plan.iter().any(|line| line.contains("agent-b")),
        "B runs elsewhere, so its window is not added: {dry}"
    );
    assert!(
        plan.iter().any(|line| line.contains("would not resume")
            && line.contains("already runs the same session")),
        "B's pane beside C is not resumed: {dry}"
    );
    assert_eq!(
        dry["would_type"].as_array().unwrap().len(),
        2,
        "A and C only: {dry}"
    );

    let restored = restore(&server, &["--name", "layout"]);
    assert_eq!(
        strings(&restored["added"]),
        [
            "saved window 1 'agent-a' as window 1 of 'work'",
            "saved window 3 'agent-c' as window 2 of 'work'"
        ],
        "{restored}"
    );
    assert_eq!(
        windows(&server, "work"),
        ["0 shell 1", "1 agent-a 0", "2 agent-c 0"]
    );
    // A positive wait first: A's and C's resumes ran, so a B that was typed would have shown by now too.
    for id in [A, C] {
        assert!(
            wait_for(|| read_record(&records, id).is_some(), SETTLE),
            "{id} was not resumed"
        );
    }
    std::thread::sleep(Duration::from_secs(1));
    assert!(read_record(&records, B).is_none(), "B was resumed twice");
}

/// An added window goes after the open session's last window, never into a number a closed window left
/// free between the user's own: with windows 0 and 2 open, the added one is 3.
#[test]
fn an_added_window_goes_after_the_last_window_and_not_into_a_gap() {
    let tmp = tempfile::tempdir().unwrap();
    let records = tmp.path().join("records");
    put_fake_claude(tmp.path(), &records);
    let server = Server::start_with(options(tmp.path(), "work"));
    server.write_config(CONFIG);
    server.tmux_ok(&["rename-window", "-t", "=work:0", "shell"]);
    agent_window(&server, "work", "agent-a", A);
    restore_save(&server, "layout");

    // The agent window is closed, and the user's own windows have a gap: 0 and 2 are open, 1 is not.
    server.tmux_ok(&["kill-window", "-t", "=work:agent-a"]);
    plain_window(&server, "work", "x");
    plain_window(&server, "work", "y");
    server.tmux_ok(&["kill-window", "-t", "=work:x"]);
    assert_eq!(windows(&server, "work"), ["0 shell 1", "2 y 0"]);

    let dry = restore(&server, &["--name", "layout", "--dry-run"]);
    assert!(
        strings(&dry["plan"]).contains(
            &"would add saved window 1 'agent-a' to open session 'work' as window 3".to_string()
        ),
        "{dry}"
    );
    let restored = restore(&server, &["--name", "layout"]);
    assert_eq!(
        strings(&restored["added"]),
        ["saved window 1 'agent-a' as window 3 of 'work'"],
        "{restored}"
    );
    assert_eq!(
        windows(&server, "work"),
        ["0 shell 1", "2 y 0", "3 agent-a 0"],
        "the window is at the end, and the gap is left: {restored}"
    );
    assert!(
        wait_for(|| read_record(&records, A).is_some(), SETTLE),
        "A was not resumed"
    );
}

/// An agent that reports its own resume command and has no session id is running where a pane holds that
/// command: a restore resumed it, and a second restore does not add a second window for it, nor a third.
#[test]
fn a_second_restore_adds_nothing_for_an_agent_resumed_by_its_reported_command() {
    let tmp = tempfile::tempdir().unwrap();
    let records = tmp.path().join("records");
    put_fake_claude(tmp.path(), &records);
    let server = Server::start_with(options(tmp.path(), "work"));
    server.write_config(CONFIG);
    server.tmux_ok(&["rename-window", "-t", "=work:0", "shell"]);
    let pane = plain_window(&server, "work", "agent-r");
    for (option, value) in [
        ("@agent_kind", "claude"),
        ("@agent_resume_source", "herdr"),
        ("@agent_resume_agent", "claude"),
        ("@agent_resume_argv", "claude --continue"),
    ] {
        server.set_pane_opt(&pane, option, value);
    }
    restore_save(&server, "layout");
    server.tmux_ok(&["kill-window", "-t", "=work:agent-r"]);

    let first = restore(&server, &["--name", "layout"]);
    assert_eq!(
        strings(&first["added"]),
        ["saved window 1 'agent-r' as window 1 of 'work'"],
        "{first}"
    );
    assert_eq!(strings(&first["resumed"]).len(), 1, "{first}");
    assert!(
        wait_for(|| read_record(&records, "first").is_some(), SETTLE),
        "the reported command was not typed"
    );
    assert_eq!(
        read_record(&records, "first").unwrap().trim(),
        "ARGS=--continue"
    );

    for round in 1..=2 {
        let again = restore(&server, &["--name", "layout"]);
        assert_eq!(
            strings(&again["added"]),
            Vec::<String>::new(),
            "round {round}: {again}"
        );
        assert_eq!(
            strings(&again["skipped"]),
            ["work"],
            "round {round}: {again}"
        );
        assert_eq!(
            windows(&server, "work"),
            ["0 shell 1", "1 agent-r 0"],
            "round {round}: {again}"
        );
    }
}

/// In a session a restore makes whole, a pane whose conversation already runs in another open pane is
/// made and not resumed, and the plan says so in the words it uses for an added window; its neighbour,
/// whose conversation runs nowhere, is resumed.
#[test]
fn a_session_made_whole_does_not_resume_a_conversation_that_runs_in_another_open_pane() {
    let tmp = tempfile::tempdir().unwrap();
    let records = tmp.path().join("records");
    put_fake_claude(tmp.path(), &records);
    let server = Server::start_with(options(tmp.path(), "work"));
    server.write_config(CONFIG);
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "lost",
        "-x",
        "100",
        "-y",
        "30",
        "-c",
        server.work_dir().to_str().unwrap(),
    ]);
    let first = server.pane_of("lost");
    server.set_pane_opt(&first, "@agent_kind", "claude");
    server.set_pane_opt(&first, "@agent_session_id", B);
    let beside = server.tmux_ok(&["split-window", "-d", "-t", &first, "-P", "-F", "#{pane_id}"]);
    server.set_pane_opt(&beside, "@agent_kind", "claude");
    server.set_pane_opt(&beside, "@agent_session_id", C);
    restore_save(&server, "layout");
    server.tmux_ok(&["kill-session", "-t", "=lost"]);
    // B's conversation runs on in a pane of the session that stayed.
    let running = server.pane_of("work");
    server.set_pane_opt(&running, "@agent_kind", "claude");
    server.set_pane_opt(&running, "@agent_session_id", B);

    let dry = restore(&server, &["--name", "layout", "--dry-run"]);
    let says = format!("pane {running} already runs the same session");
    assert!(
        strings(&dry["plan"])
            .iter()
            .any(|line| line.contains("would not resume") && line.contains(&says)),
        "{dry}"
    );
    assert_eq!(dry["would_type"].as_array().unwrap().len(), 1, "{dry}");

    let restored = restore(&server, &["--name", "layout"]);
    assert_eq!(strings(&restored["created"]), ["lost"], "{restored}");
    assert_eq!(strings(&restored["resumed"]).len(), 1, "{restored}");
    assert!(
        strings(&restored["plan"])
            .iter()
            .any(|line| line.starts_with("skip resume in pane %") && line.ends_with(&says)),
        "{restored}"
    );
    assert_eq!(
        server
            .tmux_ok(&["list-panes", "-t", "=lost", "-F", "#{pane_id}"])
            .lines()
            .count(),
        2,
        "both panes were made"
    );
    // A positive wait first: C's resume ran, so a B that was typed would have shown by now too.
    assert!(
        wait_for(|| read_record(&records, C).is_some(), SETTLE),
        "C was not resumed"
    );
    std::thread::sleep(Duration::from_secs(1));
    assert!(read_record(&records, B).is_none(), "B was resumed twice");
}

fn restore_save(server: &Server, name: &str) {
    server.ok(
        &["session", "save", "--name", name],
        "cli:session:save",
        "session_saved",
    );
}

/// The `PATH` of a restore that starts the tmux server itself: the fake `claude` first, then the folder of
/// the `tmux` the tests run and the system's, and nothing else, so that no real agent can be found. The
/// server a restore starts takes the panes' `PATH` from it, which `ServerOptions::path_prefix` (set from
/// inside the fixture's own panes) does not reach.
fn restore_path(bin: &Path) -> String {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmux = std::env::split_paths(&path)
        .find(|dir| dir.join("tmux").is_file())
        .expect("tmux is on the PATH the tests run with");
    format!(
        "{}:{}:/usr/bin:/bin:/usr/sbin:/sbin",
        bin.display(),
        tmux.display()
    )
}

/// With no tmux server: a dry run starts nothing and says it would start one; a restore starts one
/// with the first session it makes, makes every saved session, says how to attach, lets go of the
/// autosave hold it set as it started, and resumes the saved agent in the pane of the server it started,
/// by the line it types.
#[test]
fn a_restore_with_no_server_starts_one_and_makes_every_session() {
    let tmp = tempfile::tempdir().unwrap();
    let records = tmp.path().join("records");
    put_fake_claude(tmp.path(), &records);
    let server = Server::start_with(ServerOptions {
        env: vec![("PATH".to_string(), restore_path(&tmp.path().join("bin")))],
        ..options(tmp.path(), "work")
    });
    server.write_config(CONFIG);
    plain_window(&server, "work", "second");
    // A saved agent window: the restore starts the server and resumes it.
    agent_window(&server, "work", "agent-a", A);
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "play",
        "-x",
        "100",
        "-y",
        "30",
        "-c",
        server.work_dir().to_str().unwrap(),
    ]);
    assert!(autosaved(&server));

    // The server goes, as a crash ends it, and nothing is running.
    server.tmux_ok(&["kill-server"]);
    assert!(
        wait_for(
            || server.tmux(&["list-sessions"]).is_err(),
            Duration::from_secs(20)
        ),
        "the server did not go"
    );

    let dry = restore(&server, &["--dry-run"]);
    assert_eq!(
        dry["plan"][0], "would start a tmux server (none is running), with your own tmux config",
        "{dry}"
    );
    assert_eq!(dry["started_server"], false, "{dry}");
    assert!(
        server.tmux(&["list-sessions"]).is_err(),
        "a dry run started a server: {dry}"
    );

    let restored = restore(&server, &[]);
    assert_eq!(restored["started_server"], true, "{restored}");
    assert_eq!(
        restored["plan"][0], "started a tmux server (none was running), with your own tmux config",
        "{restored}"
    );
    assert_eq!(
        strings(&restored["created"]),
        ["play", "work"],
        "{restored}"
    );
    assert_eq!(
        restored["attach"], "tmux attach -t '=play'",
        "the first session made: {restored}"
    );
    let mut sessions: Vec<String> = server
        .tmux_ok(&["list-sessions", "-F", "#{session_name}"])
        .lines()
        .map(str::to_string)
        .collect();
    sessions.sort();
    assert_eq!(sessions, ["play", "work"], "{restored}");
    assert_eq!(windows(&server, "work").len(), 3, "{restored}");
    // The agent came back in the server the restore started: one pane resumed, the line typed there.
    assert_eq!(strings(&restored["resumed"]).len(), 1, "{restored}");
    assert!(
        wait_for(|| read_record(&records, A).is_some(), SETTLE),
        "A was not resumed in the server the restore started: {restored}"
    );
    assert_eq!(
        read_record(&records, A).unwrap().trim(),
        format!("ARGS=--resume {A}")
    );
    let agent_pane = strings(&restored["resumed"])[0].clone();
    assert_eq!(
        server.display(&agent_pane, "#{window_name}"),
        "agent-a",
        "{restored}"
    );
    assert_eq!(
        server.global_opt("@agent_wizard_restoring"),
        "",
        "autosave is let go"
    );
}
