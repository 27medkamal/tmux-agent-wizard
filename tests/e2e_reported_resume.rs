// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//
// These tests follow herdr's `reported_resume_follows_the_reporting_agent_until_release`,
// `reported_resume_is_dropped_and_saved_when_another_agent_takes_the_pane`,
// `reported_resume_of_recognized_agent_is_dropped_when_its_process_exits`,
// `shell_return_drops_self_reported_agent_and_its_resume` and
// `shell_return_keeps_agents_herdr_recognizes_by_process` (src/app/actions.rs, v0.9.3);
// `api_report_agent_stores_valid_resume_argv_and_rejects_invalid` and
// `api_resume_argv_is_ignored_when_its_session_report_is_refused`
// (src/server/headless/tests/mod.rs, 2552d101af); and
// `capture_contract_includes_reported_agent_resume` (src/persist/snapshot.rs, v0.9.3). herdr's
// tests drive its state machine and its socket; these run the built binary's report verbs against a
// real tmux pane, with fake agents named for the agent on the pane's `PATH`, and a real `taw watch
// run`, and read the pane options back. A report here carries the command after `--` (herdr's
// `resume_argv`), and a session report names the pane's agent, so another agent's session report
// takes the command of the last one away. herdr refuses a session report that replaces a session
// its rules protect; here a report is refused for its order alone, which is the refusal the
// command's test uses.

//! A command an agent reports for itself that resumes its session, end to end: the built binary
//! against a throwaway tmux server. Who may set it, how long the pane keeps it and every way it
//! ends, what a save keeps of it, and a restore that types it into the restored pane, in the
//! directory the pane was saved in, once for each session.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Server, ServerOptions, named_agent, tmux_named, wait_for, write_script};
use serde_json::Value;

/// Ceiling for what a real tmux, the built binary and a watcher take to do a thing, with `cargo
/// test` running other files beside this one.
const SETTLE: Duration = Duration::from_secs(20);

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// A server whose panes find the fakes in `bin` first on their `PATH`.
fn server_with(bin: &Path) -> Server {
    let server = Server::start_with(ServerOptions {
        width: 100,
        height: 30,
        path_prefix: Some(bin.to_path_buf()),
        env: vec![("TAW_LOG".to_string(), "debug".to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    server
}

/// The three options a pane keeps a reported resume command in: source, agent, and the line a
/// restore types.
fn kept(server: &Server, pane: &str) -> [String; 3] {
    [
        "@agent_resume_source",
        "@agent_resume_agent",
        "@agent_resume_argv",
    ]
    .map(|name| server.pane_opt(pane, name))
}

fn kept_as(source: &str, agent: &str, line: &str) -> [String; 3] {
    [source, agent, line].map(str::to_string)
}

fn none() -> [String; 3] {
    [String::new(), String::new(), String::new()]
}

/// `taw` with `args` and, after `--`, `resume` when it is given: the exit code and the envelope.
fn taw(server: &Server, args: &[&str], resume: Option<&[&str]>) -> (i32, Value) {
    let mut all: Vec<&str> = args.to_vec();
    if let Some(resume) = resume {
        all.push("--");
        all.extend(resume);
    }
    server.taw(&all)
}

/// A `report-agent` of `state` by `source` for `agent` on `pane`, numbered `seq`, with `resume`.
fn report(
    server: &Server,
    pane: &str,
    who: (&str, &str),
    state: &str,
    seq: &str,
    resume: Option<&[&str]>,
) -> (i32, Value) {
    let (source, agent) = who;
    taw(
        server,
        &[
            "pane",
            "report-agent",
            pane,
            "--source",
            source,
            "--agent",
            agent,
            "--state",
            state,
            "--seq",
            seq,
        ],
        resume,
    )
}

/// A `report-agent-session` by `source` for `agent` on `pane`, with `more` and `resume`.
fn report_session(
    server: &Server,
    pane: &str,
    who: (&str, &str),
    more: &[&str],
    resume: Option<&[&str]>,
) -> (i32, Value) {
    let (source, agent) = who;
    let mut args = vec![
        "pane",
        "report-agent-session",
        pane,
        "--source",
        source,
        "--agent",
        agent,
    ];
    args.extend(more);
    taw(server, &args, resume)
}

fn ok(outcome: (i32, Value)) -> Value {
    let (code, envelope) = outcome;
    assert_eq!(code, 0, "{envelope}");
    envelope["result"].clone()
}

fn error_code(outcome: (i32, Value)) -> String {
    let (code, envelope) = outcome;
    assert_ne!(code, 0, "{envelope}");
    envelope["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

const PRIME: (&str, &str) = ("prime-agent", "prime-agent");

/// What herdr's tests report: a custom agent holding the pane, at rest, with its command.
fn prime_holds_the_pane_with(server: &Server, pane: &str, resume: &[&str]) {
    ok(report(server, pane, PRIME, "idle", "1", Some(resume)));
}

#[test]
fn reported_resume_follows_the_reporting_agent_until_release() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with(tmp.path());
    let pane = server.pane();
    prime_holds_the_pane_with(&server, &pane, &["prime-agent", "--resume", "a"]);
    assert_eq!(
        kept(&server, &pane),
        kept_as("prime-agent", "prime-agent", "prime-agent --resume a")
    );

    // Another source, for the same agent, does not hold the pane: its command is refused, and the
    // pane keeps the one its holder reported.
    let refused = report_session(
        &server,
        &pane,
        ("someone-else", "prime-agent"),
        &[],
        Some(&["other"]),
    );
    assert_eq!(error_code(refused), "resume_not_accepted");
    assert_eq!(
        kept(&server, &pane),
        kept_as("prime-agent", "prime-agent", "prime-agent --resume a")
    );

    ok(taw(
        &server,
        &[
            "pane",
            "release-agent",
            &pane,
            "--source",
            "prime-agent",
            "--agent",
            "prime-agent",
            "--seq",
            "2",
        ],
        None,
    ));
    assert_eq!(kept(&server, &pane), none());
}

#[test]
fn reported_resume_is_dropped_and_saved_when_another_agent_takes_the_pane() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with(tmp.path());
    let pane = server.pane();
    prime_holds_the_pane_with(&server, &pane, &["prime-agent", "--resume", "a"]);
    let saved = server.ok(&["session", "save"], "cli:session:save", "session_saved");
    assert_eq!(saved_resume(&server)["argv"][2], "a", "{saved}");

    ok(report(
        &server,
        &pane,
        ("custom:other", "other"),
        "working",
        "1",
        None,
    ));

    assert_eq!(kept(&server, &pane), none());
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    assert_eq!(saved_resume(&server), Value::Null);
}

/// The `agent_resume` the default snapshot saved for the first pane of session `one`.
fn saved_resume(server: &Server) -> Value {
    let text = std::fs::read_to_string(server.state_dir().join("sessions/default.json")).unwrap();
    let snapshot: Value = serde_json::from_str(&text).unwrap();
    let session = snapshot["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["name"] == "one")
        .cloned()
        .unwrap_or_default();
    session["windows"][0]["panes"][0]
        .get("agent_resume")
        .cloned()
        .unwrap_or(Value::Null)
}

// Follows herdr's `capture_contract_includes_reported_agent_resume` (src/persist/snapshot.rs,
// v0.9.3): what `taw session save` writes.
#[test]
fn capture_contract_includes_reported_agent_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with(tmp.path());
    let pane = server.pane();
    prime_holds_the_pane_with(&server, &pane, &["prime-agent", "--resume", "a"]);

    server.ok(&["session", "save"], "cli:session:save", "session_saved");

    assert_eq!(
        saved_resume(&server),
        serde_json::json!({
            "source": "prime-agent",
            "agent": "prime-agent",
            "argv": ["prime-agent", "--resume", "a"],
        })
    );
}

/// A fake agent called `name`, which tmux names `name` while it runs, made in `dir`: the directory to
/// put first on the panes' `PATH`.
fn fake_named(dir: &Path, name: &str) -> PathBuf {
    named_agent(dir, name, None)
}

/// Types `line` at `pane`'s shell, and waits for tmux to name `command` as what the pane runs.
fn run_in(server: &Server, pane: &str, line: &str, command: &str) {
    server.tmux_ok(&["send-keys", "-t", pane, line, "Enter"]);
    assert!(
        wait_for(
            || server.display(pane, "#{pane_current_command}") == command,
            SETTLE
        ),
        "`{line}` never took the pane: {}",
        server.display(pane, "#{pane_current_command}")
    );
}

#[test]
fn reported_resume_of_recognized_agent_is_dropped_when_its_process_exits() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_named(tmp.path(), "pi");
    let server = server_with(&bin);
    let pane = server.pane();
    run_in(&server, &pane, "pi", "pi");
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    // No hook holds the pane, and the agent in front of it is a pi by its process: the pi's own
    // integration may set its command.
    ok(report_session(
        &server,
        &pane,
        ("integration:pi", "pi"),
        &[],
        Some(&["pi", "--continue"]),
    ));
    assert_eq!(
        kept(&server, &pane),
        kept_as("integration:pi", "pi", "pi --continue")
    );

    server.wake("taw-fake-pi-never");
    assert!(
        wait_for(|| kept(&server, &pane) == none(), SETTLE),
        "the command outlived the agent: {:?}\n{}",
        kept(&server, &pane),
        server.watcher_logs()
    );

    // A late report does not bring it back: nothing in front of the pane is a pi.
    let late = report_session(
        &server,
        &pane,
        ("integration:pi", "pi"),
        &[],
        Some(&["pi", "--continue"]),
    );
    assert_eq!(error_code(late), "resume_not_accepted");
    assert_eq!(kept(&server, &pane), none());
}

#[test]
fn shell_return_drops_self_reported_agent_and_its_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with(tmp.path());
    let pane = server.pane();
    run_in(&server, &pane, "sleep 300", "sleep");
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    prime_holds_the_pane_with(&server, &pane, &["prime-agent", "--resume", "a"]);

    server.tmux_ok(&["send-keys", "-t", &pane, "C-c"]);

    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_kind").is_empty() && kept(&server, &pane) == none(),
            SETTLE
        ),
        "the shell came back and the record stayed: kind {:?}, command {:?}\n{}",
        server.pane_opt(&pane, "@agent_kind"),
        kept(&server, &pane),
        server.watcher_logs()
    );
}

/// herdr ends an agent only it reports at the pane's shell coming back, and an agent it names by its
/// process at that process's exit. Here the pane's own shell is never idle while the pi runs, whatever
/// the pi's own hook reports: the record and its command stay.
#[test]
fn shell_return_keeps_agents_herdr_recognizes_by_process() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_named(tmp.path(), "pi");
    let server = server_with(&bin);
    let pane = server.pane();
    run_in(&server, &pane, "pi", "pi");
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    ok(report(
        &server,
        &pane,
        ("integration:pi", "pi"),
        "working",
        "1",
        Some(&["pi", "--continue"]),
    ));
    let reads = server.watcher_reads(&pane);

    // Three more reads of a pane whose pi is in front, each told by a report of the pi's own hook,
    // with no command, that there is something to read.
    let seq = std::cell::Cell::new(1_u32);
    let read = wait_for(
        || {
            seq.set(seq.get() + 1);
            ok(report(
                &server,
                &pane,
                ("integration:pi", "pi"),
                "working",
                &seq.get().to_string(),
                None,
            ));
            std::thread::sleep(Duration::from_millis(250));
            server.watcher_reads(&pane) >= reads + 3
        },
        SETTLE,
    );
    assert!(
        read,
        "the watcher did not read the pane\n{}",
        server.watcher_logs()
    );
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "pi");
    assert_eq!(
        kept(&server, &pane),
        kept_as("integration:pi", "pi", "pi --continue")
    );
}

#[test]
fn api_report_agent_stores_valid_resume_argv_and_rejects_invalid() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with(tmp.path());
    let pane = server.pane();
    let report_with = |seq: &str, resume: &[&str]| {
        taw(
            &server,
            &[
                "pane",
                "report-agent",
                &pane,
                "--source",
                "prime-agent",
                "--agent",
                "prime-agent",
                "--state",
                "idle",
                "--seq",
                seq,
                "--agent-session-id",
                "01a0",
            ],
            Some(resume),
        )
    };

    ok(report_with(
        "1",
        &["prime-agent", "--resume", "01a0", "--model", "x"],
    ));
    let line = "prime-agent --resume 01a0 --model x";
    assert_eq!(kept(&server, &pane)[2], line);

    // A report with the same number is not newer, and does not replace the command.
    let duplicate = ok(report_with("1", &["prime-agent", "--resume", "dup"]));
    assert_eq!(duplicate["in_order"], false, "{duplicate}");
    assert_eq!(kept(&server, &pane)[2], line);

    // A path for a command name is refused, and nothing of its report is applied: not its state,
    // and not its number either, which the next report may use.
    let invalid = taw(
        &server,
        &[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "prime-agent",
            "--agent",
            "prime-agent",
            "--state",
            "working",
            "--seq",
            "2",
        ],
        Some(&["/opt/prime/prime-agent", "--resume", "01a0"]),
    );
    assert_eq!(error_code(invalid), "invalid_resume_argv");
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "idle");
    assert_eq!(kept(&server, &pane)[2], line);
    assert_eq!(server.pane_opt(&pane, "@agent_report_seq"), "prime-agent=1");

    // Another source does not hold the pane: its command is not kept. A session report names the
    // pane's agent, so another agent's takes the pane, and the command of the agent it replaced goes
    // with the rest of what that agent reported.
    let same_agent = report_session(
        &server,
        &pane,
        ("custom:intruder", "prime-agent"),
        &[],
        Some(&["intruder"]),
    );
    assert_eq!(error_code(same_agent), "resume_not_accepted");
    assert_eq!(kept(&server, &pane)[1], "prime-agent");
    let intruder = report_session(
        &server,
        &pane,
        ("custom:intruder", "intruder"),
        &[],
        Some(&["intruder"]),
    );
    assert_eq!(error_code(intruder), "resume_not_accepted");
    assert_eq!(kept(&server, &pane), none());
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "intruder");
}

#[test]
fn api_resume_argv_is_ignored_when_its_session_report_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_named(tmp.path(), "claude");
    let server = server_with(&bin);
    let pane = server.pane();
    run_in(&server, &pane, "claude", "claude");
    let report_of = |session: &str, seq: &str| {
        report_session(
            &server,
            &pane,
            ("integration:claude", "claude"),
            &["--seq", seq, "--agent-session-id", session],
            Some(&["claude", "--resume", session]),
        )
    };

    ok(report_of("session-a", "2"));
    let refused = ok(report_of("session-b", "1"));

    assert_eq!(refused["in_order"], false, "{refused}");
    assert_eq!(server.pane_opt(&pane, "@agent_session_id"), "session-a");
    assert_eq!(kept(&server, &pane)[2], "claude --resume session-a");
}

/// A command that fails the checks answers `invalid_resume_argv` before anything is written, from
/// either verb, whatever it fails on.
#[test]
fn a_command_that_cannot_be_kept_is_refused_with_nothing_of_its_report_applied() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with(tmp.path());
    let pane = server.pane();
    let words = |n: usize| vec!["w"; n];
    let long = "x".repeat(8192);
    let cases: [Vec<&str>; 6] = [
        vec!["/usr/bin/prime-agent"],
        vec!["-prime"],
        vec!["prime agent"],
        vec!["prime-agent", "can's"],
        vec!["prime-agent", "two\nlines"],
        [vec!["prime-agent"], words(64)].concat(),
    ];
    for resume in cases.iter().chain([&vec!["prime-agent", long.as_str()]]) {
        let state = report(&server, &pane, PRIME, "working", "1", Some(resume));
        assert_eq!(error_code(state), "invalid_resume_argv", "{resume:?}");
        let session = report_session(
            &server,
            &pane,
            PRIME,
            &["--agent-session-id", "s1"],
            Some(resume),
        );
        assert_eq!(error_code(session), "invalid_resume_argv", "{resume:?}");
    }
    for option in [
        "@agent_kind",
        "@agent_state",
        "@agent_source",
        "@agent_session_id",
        "@agent_report_seq",
    ] {
        assert_eq!(server.pane_opt(&pane, option), "", "{option}");
    }
    assert_eq!(kept(&server, &pane), none());
}

/// A fake agent called `name` in `dir`, which notes each run in a file of its own in `runs`: the
/// directory it was started in and its arguments, one to a line. It then stays in front of its pane as
/// a program tmux names `name`, until the test's server goes. Returns the directory to put first on
/// `PATH`.
fn fake_that_notes_its_runs(dir: &Path, name: &str, runs: &Path) -> PathBuf {
    let bin = dir.join("bin");
    let copy_dir = dir.join("copy");
    std::fs::create_dir_all(&copy_dir).unwrap();
    std::fs::create_dir_all(runs).unwrap();
    let copy = tmux_named(&copy_dir, name);
    write_script(
        &bin.join(name),
        &format!(
            "run=$(mktemp '{runs}/run.XXXXXX') && printf '%s\\n' \"$(pwd -P)\" \"$@\" > \"$run.part\" \
             && mv \"$run.part\" \"$run.done\"\n\
             exec '{copy}' -L \"$TAW_TMUX_SOCKET_NAME\" wait-for taw-fake-{name}-never\n",
            runs = runs.display(),
            copy = copy.display()
        ),
    );
    bin
}

/// The runs noted in `runs`, each as the directory and the arguments.
fn runs_of(runs: &Path) -> Vec<Vec<String>> {
    let mut done: Vec<PathBuf> = std::fs::read_dir(runs)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.extension().is_some_and(|ext| ext == "done"))
                .collect()
        })
        .unwrap_or_default();
    done.sort();
    done.iter()
        .map(|path| {
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect()
        })
        .collect()
}

/// What a real tmux has to show: an agent of its own reports its state and the command that
/// resumes its session; the layout is saved; its sessions go, as a restart takes them; and a restore
/// types the command into the restored pane's shell, in the directory the pane was saved in, quoted so
/// the shell runs it word for word. The same command saved in two panes of one directory is one
/// session and is resumed once; in another directory it is another. The restored pane keeps the
/// command, so the next save does too.
#[test]
fn a_restore_types_the_command_the_agent_reported_in_the_saved_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    let bin = fake_that_notes_its_runs(tmp.path(), "prime-agent", &runs);
    let server = server_with(&bin);
    let (a, b) = (tmp.path().join("project a"), tmp.path().join("project-b"));
    for dir in [&a, &b] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let (a, b) = (
        a.canonicalize().unwrap().display().to_string(),
        b.canonicalize().unwrap().display().to_string(),
    );
    let first = server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-c",
        &a,
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    let twin = server.tmux_ok(&[
        "split-window",
        "-d",
        "-t",
        &first,
        "-c",
        &a,
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    let other = server.tmux_ok(&[
        "split-window",
        "-d",
        "-t",
        &first,
        "-c",
        &b,
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    let resume: &[&str] = &[
        "prime-agent",
        "--resume",
        "01a0",
        "--model",
        "gpt 6",
        "$HOME",
        "~",
        "a;b",
    ];
    for pane in [&first, &twin, &other] {
        assert!(
            wait_for(
                || !server.display(pane, "#{pane_current_path}").is_empty(),
                SETTLE
            ),
            "pane {pane} has no directory"
        );
        prime_holds_the_pane_with(&server, pane, resume);
    }
    server.ok(&["session", "save"], "cli:session:save", "session_saved");

    server.tmux_ok(&["kill-session", "-t", "alpha"]);
    let restored = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );

    assert_eq!(
        restored["resumed"].as_array().map(Vec::len),
        Some(2),
        "{restored}"
    );
    let plan = restored["plan"].to_string();
    assert!(plan.contains("resumes the same session"), "{plan}");
    let want: Vec<String> = resume.iter().map(|word| word.to_string()).collect();
    assert!(
        wait_for(|| runs_of(&runs).len() == 2, SETTLE),
        "the command was not run in two panes: {:?}",
        runs_of(&runs)
    );
    let mut dirs: Vec<String> = runs_of(&runs)
        .into_iter()
        .map(|run| {
            assert_eq!(
                run[1..],
                want[1..],
                "the shell did not run the words reported"
            );
            run[0].clone()
        })
        .collect();
    dirs.sort();
    let mut expected = vec![a.clone(), b.clone()];
    expected.sort();
    assert_eq!(dirs, expected);

    // The restored panes keep the command, and the next save with them.
    let panes = server.tmux_ok(&["list-panes", "-s", "-t", "alpha", "-F", "#{pane_id}"]);
    let kept_lines: Vec<String> = panes
        .lines()
        .map(|pane| kept(&server, pane)[2].clone())
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(
        kept_lines, ["prime-agent --resume 01a0 --model 'gpt 6' '$HOME' '~' 'a;b'"; 2],
        "{panes}"
    );
}

/// The words a restore types come out of the pane's shell as they were reported, whichever shell it is:
/// those a POSIX shell reads for a meaning (`$`, `~`, `;`, a glob, a run of spaces, nothing at all), and
/// those zsh reads for one besides, a word that starts with `=` and, with MAGIC_EQUAL_SUBST, what follows
/// any `=` in a word (`x==y` names the path of a command `y`, which stops the command from running).
fn the_words_of_a_reported_command_come_out_of(shell: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    let bin = fake_that_notes_its_runs(tmp.path(), "prime-agent", &runs);
    let server = server_with_narrow_shells(&bin, shell);
    let pane = server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-c",
        tmp.path().to_str().unwrap(),
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    let words: &[&str] = &[
        "prime-agent",
        "--resume",
        "01a0",
        "x==y",
        "a=b",
        "--k=v",
        "=zsh",
        "x=~",
        "~/x",
        "$HOME",
        "a;b",
        "*",
        "#x",
        "a  b",
        "",
        "%1",
    ];
    prime_holds_the_pane_with(&server, &pane, words);
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "alpha"]);

    server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );

    assert!(
        wait_for(|| runs_of(&runs).len() == 1, SETTLE),
        "the command did not run in {shell}:\n{}",
        server.capture(&server.pane_of("alpha"))
    );
    let want: Vec<String> = words.iter().map(|word| word.to_string()).collect();
    assert_eq!(runs_of(&runs)[0][1..], want[1..], "{shell}");
}

#[test]
fn the_words_of_a_reported_command_come_out_of_sh_as_they_were_reported() {
    the_words_of_a_reported_command_come_out_of("/bin/sh");
}

#[test]
fn the_words_of_a_reported_command_come_out_of_bash_as_they_were_reported() {
    the_words_of_a_reported_command_come_out_of("/bin/bash --norc --noprofile -i");
}

#[test]
fn the_words_of_a_reported_command_come_out_of_zsh_as_they_were_reported() {
    the_words_of_a_reported_command_come_out_of("/bin/zsh -f");
}

#[test]
fn the_words_of_a_reported_command_come_out_of_zsh_with_magic_equal_subst_as_they_were_reported() {
    the_words_of_a_reported_command_come_out_of("/bin/zsh -f -o magicequalsubst");
}

/// `[session] resume_agents_on_restore = false` types nothing, the reported command included.
#[test]
fn a_restore_with_resume_off_types_no_reported_command() {
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    let bin = fake_that_notes_its_runs(tmp.path(), "prime-agent", &runs);
    let server = server_with(&bin);
    server.write_config(&format!(
        "{CONFIG}\n[session]\nresume_agents_on_restore = false\n"
    ));
    let pane = server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-c",
        tmp.path().to_str().unwrap(),
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    prime_holds_the_pane_with(&server, &pane, &["prime-agent", "--continue"]);
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "alpha"]);

    let restored = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );

    assert_eq!(restored["resumed"], serde_json::json!([]), "{restored}");
    std::thread::sleep(Duration::from_millis(500));
    assert!(runs_of(&runs).is_empty(), "{:?}", runs_of(&runs));
}

/// A server whose new panes run `shell` (a command line, run by `exec`) with a narrow `PATH`: the fakes
/// in `bin`, then `/usr/bin` and `/bin`, and nothing of the runner's.
fn server_with_narrow_shells(bin: &Path, shell: &str) -> Server {
    let server = Server::start_with(ServerOptions {
        width: 100,
        height: 30,
        env: vec![("TAW_LOG".to_string(), "debug".to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    server.tmux_ok(&[
        "set-option",
        "-g",
        "default-command",
        &format!(
            "export PATH='{}:/usr/bin:/bin'; exec {shell}",
            bin.display()
        ),
    ]);
    server
}

/// A command of one word that tmux reads as the name of a key (`Up`, `Home`, `BSpace`, `F1`, `C-p`) is
/// typed as the word, which the shell runs, and not pressed as the key: `Up` at a bash prompt brings
/// back the last line of its history, and the Enter after it runs that line.
#[test]
fn a_one_word_command_named_like_a_tmux_key_is_typed_as_a_word() {
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    let bin = fake_that_notes_its_runs(tmp.path(), "Up", &runs);
    let ran_the_history = tmp.path().join("history-ran");
    let rc = tmp.path().join("rc.bash");
    std::fs::write(
        &rc,
        format!(
            "HISTFILE=/dev/null\nPS1='$ '\nhistory -s \"touch '{}'\"\n",
            ran_the_history.display()
        ),
    )
    .unwrap();
    let server =
        server_with_narrow_shells(&bin, &format!("/bin/bash --rcfile '{}' -i", rc.display()));
    let pane = server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-c",
        tmp.path().to_str().unwrap(),
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    ok(report(
        &server,
        &pane,
        ("up-agent", "up-agent"),
        "idle",
        "1",
        Some(&["Up"]),
    ));
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "alpha"]);

    let restored = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );

    assert_eq!(
        restored["resumed"].as_array().map(Vec::len),
        Some(1),
        "{restored}"
    );
    assert!(
        wait_for(
            || !runs_of(&runs).is_empty() || ran_the_history.exists(),
            SETTLE
        ),
        "nothing ran in the restored pane:\n{}",
        server.capture(&server.pane_of("alpha"))
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !ran_the_history.exists(),
        "the restore pressed Up, and the shell ran the last line of its history"
    );
    assert_eq!(runs_of(&runs).len(), 1, "the word was not run");
}

/// A `tmux` for a restore that lets the first call that matches `pattern` (a `case` pattern over its
/// arguments) go, and holds the second until `go` is in `dir`, saying so by making `held` there once its
/// arguments are in `held-args`, one to a line: it stands for the moments between the look that finds a
/// pane's shell at its prompt and the send, which `[session] startup_per_agent_delay_ms` lengthens for
/// every pane after the first, or between the send of the command and its Enter.
fn tmux_that_holds_the_second(dir: &Path, pattern: &str) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-holding-a-resume");
    write_script(
        &wrapper,
        &format!(
            "case \"$*\" in\n  {pattern})\n    \
             if ! mkdir '{dir}/first' 2>/dev/null && mkdir '{dir}/claim' 2>/dev/null; then\n      \
             printf '%s\\n' \"$@\" > '{dir}/held-args'\n      mkdir '{dir}/held'\n      i=0\n      \
             while [ ! -e '{dir}/go' ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done\n    \
             fi ;;\nesac\nexec '{real}' \"$@\"\n",
            dir = dir.display(),
            real = real.display(),
        ),
    );
    wrapper
}

/// The pane a held call of [`tmux_that_holds_the_second`] was for: the word after its first `-t`.
fn held_pane(held: &Path) -> String {
    let args = std::fs::read_to_string(held.join("held-args")).unwrap();
    let mut words = args.lines();
    while let Some(word) = words.next() {
        if word == "-t" {
            return words.next().unwrap_or_default().to_string();
        }
    }
    panic!("no pane in the held call: {args}")
}

/// What takes a restored pane from its shell while its resume is on its way.
#[derive(Clone, Copy, PartialEq)]
enum Taker {
    /// A program in front of the shell (`cat`), before the command is sent.
    Program,
    /// Copy mode, once the command is typed and before its Enter.
    CopyMode,
}

/// Two panes are restored, each with a command to resume its agent: the agent's own (`own` false), or
/// taw's for the session the pane saved (`own` true). The second is taken from its shell while its
/// resume is on its way (`taker`): the restore sends nothing more to it, takes off the marks of an agent
/// it put on the pane, and says in its plan which pane it left alone and the command to run there.
fn a_pane_taken_from_its_shell_before_the_send_is_left_alone(own: bool, taker: Taker) {
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    let name = if own { "claude" } else { "prime-agent" };
    let bin = fake_that_notes_its_runs(tmp.path(), name, &runs);
    let held = tmp.path().join("held");
    std::fs::create_dir_all(&held).unwrap();
    let pattern = match taker {
        Taker::Program => format!("*send-keys*'{name}'*"),
        Taker::CopyMode => "*'send-keys -t '*' Enter '*".to_string(),
    };
    let wrapper = tmux_that_holds_the_second(&held, &pattern);
    let server = server_with_narrow_shells(&bin, "/bin/sh");
    let first = server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-c",
        tmp.path().to_str().unwrap(),
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    let second = server.tmux_ok(&["split-window", "-d", "-t", &first, "-P", "-F", "#{pane_id}"]);
    for (pane, session) in [(&first, "a"), (&second, "b")] {
        assert!(
            wait_for(
                || !server.display(pane, "#{pane_current_path}").is_empty(),
                SETTLE
            ),
            "pane {pane} has no directory"
        );
        if own {
            server.set_pane_opt(pane, "@agent_kind", "claude");
            server.set_pane_opt(pane, "@agent_session_id", &format!("session-{session}"));
        } else {
            prime_holds_the_pane_with(&server, pane, &["prime-agent", "--resume", session]);
        }
    }
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "alpha"]);
    let typed_into_cat = tmp.path().join("typed-into-cat");

    let (outcome, pane) = std::thread::scope(|scope| {
        let restore = scope.spawn(|| {
            server.taw_with_env(
                &["session", "restore"],
                &[("TAW_TMUX_BIN", wrapper.to_str().unwrap())],
            )
        });
        let came = wait_for(|| held.join("held").exists(), SETTLE);
        let pane = if came {
            held_pane(&held)
        } else {
            String::new()
        };
        if came && taker == Taker::Program {
            server.tmux_ok(&[
                "send-keys",
                "-t",
                &pane,
                &format!("cat > '{}'", typed_into_cat.display()),
                "Enter",
            ]);
            assert!(
                wait_for(
                    || server.display(&pane, "#{pane_current_command}") == "cat",
                    SETTLE
                ),
                "cat never came up in {pane}"
            );
        }
        if came && taker == Taker::CopyMode {
            server.tmux_ok(&["copy-mode", "-t", &pane]);
            assert!(
                wait_for(|| server.display(&pane, "#{pane_in_mode}") == "1", SETTLE),
                "{pane} never went into copy mode"
            );
        }
        std::fs::write(held.join("go"), "").unwrap();
        let outcome = restore.join().unwrap();
        assert!(
            came,
            "the restore never sent its second command: {outcome:?}"
        );
        (outcome, pane)
    });

    let restored = ok(outcome);
    let command = if own {
        "claude --resume session-"
    } else {
        "prime-agent --resume "
    };
    assert_eq!(
        restored["not_resumed"],
        serde_json::json!([pane]),
        "{restored}"
    );
    let said = match taker {
        Taker::Program => format!(
            "pane {pane} had another program in front of its shell when its command was to be typed, \
             so nothing was typed; resume it with: {command}"
        ),
        Taker::CopyMode => format!(
            "pane {pane} is in copy mode, and the command typed into it may be left on its line; \
             resume it with: {command}"
        ),
    };
    let plan = restored["plan"].to_string();
    assert!(plan.contains(&said), "{plan}");
    assert_eq!(server.pane_opt(&pane, "@agent_kind_hint"), "", "{restored}");
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "", "{restored}");
    assert_eq!(server.pane_opt(&pane, "@agent_acquiring"), "", "{restored}");
    if taker == Taker::Program {
        // cat writes each line it is given, and the end of its input ends it.
        server.tmux_ok(&["send-keys", "-t", &pane, "C-d"]);
        assert!(
            wait_for(
                || server.display(&pane, "#{pane_current_command}") != "cat",
                SETTLE
            ),
            "cat never ended"
        );
        assert_eq!(
            std::fs::read_to_string(&typed_into_cat).unwrap_or_default(),
            "",
            "the restore typed the command into cat"
        );
    }
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(runs_of(&runs).len(), 1, "{:?}", runs_of(&runs));
}

#[test]
fn a_program_that_takes_the_pane_before_the_agents_own_command_is_sent_is_sent_nothing() {
    a_pane_taken_from_its_shell_before_the_send_is_left_alone(false, Taker::Program);
}

#[test]
fn a_program_that_takes_the_pane_before_taws_own_resume_command_is_sent_is_sent_nothing() {
    a_pane_taken_from_its_shell_before_the_send_is_left_alone(true, Taker::Program);
}

/// Copy mode between the command and its Enter would have the Enter, and the pane would be said to be
/// resumed with the command on its shell's line, not run.
#[test]
fn a_pane_put_in_copy_mode_before_the_enter_of_its_command_is_left_alone() {
    a_pane_taken_from_its_shell_before_the_send_is_left_alone(false, Taker::CopyMode);
}

/// A `tmux` that, in the guarded call carrying the second piece of a long text, first takes the pane from its
/// shell (`respawn-pane` running `cat`) and waits until `cat` is in front: the first piece has reached the
/// shell's line by then, and the second is held back. The text goes in guarded calls,
/// `if-shell -F -t <pane> <condition> 'send-keys -l -t <pane> -- <text>' <else>`: its pane is the seventh
/// word, after `-L <socket> -u if-shell -F -t`, and its send the ninth.
fn tmux_that_takes_the_pane_after_the_first_piece(dir: &Path) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-that-takes-the-pane");
    write_script(
        &wrapper,
        &format!(
            "real='{real}'\n\
             if [ \"$4\" = if-shell ]; then\n\
             \x20 case \"$9\" in\n\
             \x20   'send-keys -l '*)\n\
             \x20     echo piece >> '{dir}/pieces'\n\
             \x20     if [ \"$(wc -l < '{dir}/pieces')\" -ge 2 ]; then\n\
             \x20       \"$real\" -L \"$2\" respawn-pane -k -t \"$7\" cat\n\
             \x20       i=0\n\
             \x20       while [ $i -lt 200 ] && [ \"$(\"$real\" -L \"$2\" display -p -t \"$7\" '#{{pane_current_command}}')\" != cat ]; do\n\
             \x20         sleep 0.02; i=$((i + 1))\n\
             \x20       done\n\
             \x20     fi ;;\n\
             \x20 esac\n\
             fi\n\
             exec \"$real\" \"$@\"\n",
            real = real.display(),
            dir = dir.display(),
        ),
    );
    wrapper
}

/// A resume command longer than one piece of a guarded send (4 KiB) is typed a piece at a time, and a program
/// that takes the pane between the pieces is given the rest of none of it: the restore leaves the pane alone
/// and says in its plan, as it does of a pane put in copy mode before the Enter, that what was typed may be
/// left on its line, which the first piece is. "So nothing was typed" would be untrue.
#[test]
fn a_program_that_takes_the_pane_after_part_of_a_long_command_is_said_to_leave_it_on_its_line() {
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    let bin = fake_that_notes_its_runs(tmp.path(), "prime-agent", &runs);
    let wrapper = tmux_that_takes_the_pane_after_the_first_piece(tmp.path());
    let server = server_with_narrow_shells(&bin, "/bin/sh");
    let pane = server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-c",
        tmp.path().to_str().unwrap(),
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    assert!(
        wait_for(
            || !server.display(&pane, "#{pane_current_path}").is_empty(),
            SETTLE
        ),
        "pane {pane} has no directory"
    );
    let long = "a".repeat(5000);
    prime_holds_the_pane_with(&server, &pane, &["prime-agent", "--resume", &long]);
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "alpha"]);

    let restored = ok(server.taw_with_env(
        &["session", "restore"],
        &[("TAW_TMUX_BIN", wrapper.to_str().unwrap())],
    ));

    let pieces = std::fs::read_to_string(tmp.path().join("pieces")).unwrap_or_default();
    assert_eq!(
        pieces.lines().count(),
        2,
        "the stand-in did not see exactly two pieces sent: {pieces:?} {restored}"
    );
    let plan = restored["plan"].to_string();
    assert!(
        plan.contains("had another program in front of its shell when its command was to be run, and the command typed into it may be left on its line; resume it with: prime-agent --resume "),
        "{plan}"
    );
    assert!(!plan.contains("so nothing was typed"), "{plan}");
    assert_eq!(runs_of(&runs).len(), 0, "{:?}", runs_of(&runs));
}

/// A restored agent that only its hint names, whose process exits as another program takes its pane:
/// a Claude its own wrapper runs, which reported the wrapper as the command that resumes it. The pane's
/// own shell is not back (the program is in front of it), so the agent's record stays, as that of an
/// agent that reports for itself does; and the command it reported goes with its process, as herdr
/// drops it, so the next save does not keep the session of an agent that has left.
#[test]
fn the_command_of_a_restored_agent_goes_when_its_process_exits() {
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    let bin = fake_that_notes_its_runs(tmp.path(), "claude-wrapper", &runs);
    let reads = tmp.path().join("reads");
    std::fs::create_dir_all(&reads).unwrap();
    // A tmux that holds the watcher's reads while `freeze` is there, and nothing else.
    let wrapper = tmux_that_holds_a_watcher_write(&reads, "*'a call no tmux is given'*");
    let server = server_through(&wrapper, Some(&bin));
    let pane = server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-c",
        tmp.path().to_str().unwrap(),
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    ok(report(
        &server,
        &pane,
        ("my-wrapper", "claude"),
        "idle",
        "1",
        Some(&["claude-wrapper", "--continue"]),
    ));
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "alpha"]);
    server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    let restored = server.pane_of("alpha");
    assert!(
        wait_for(|| runs_of(&runs).len() == 1, SETTLE),
        "the command was never run"
    );
    assert!(
        wait_for(
            || !server.pane_opt(&restored, "@agent_pid").is_empty(),
            SETTLE
        ),
        "the restore recorded no pid for what it started"
    );
    assert_eq!(server.pane_opt(&restored, "@agent_kind_hint"), "claude");
    assert_eq!(server.pane_opt(&restored, "@agent_reported_kind"), "claude");
    assert_eq!(kept(&server, &restored)[2], "claude-wrapper --continue");
    server.ok(&["watch", "attach", &restored], "cli:watch:attach", "ok");
    after_the_next_read(&server, &restored);
    assert_eq!(
        kept(&server, &restored)[2],
        "claude-wrapper --continue",
        "the command went while its agent ran\n{}",
        server.watcher_logs()
    );

    // The watcher's reads wait while the agent exits and a program takes the pane.
    std::fs::write(reads.join("freeze"), "").unwrap();
    std::thread::sleep(Duration::from_millis(800));
    server.wake("taw-fake-claude-wrapper-never");
    assert!(wait_for(
        || server.display(&restored, "#{pane_current_command}") != "claude-wrapper",
        SETTLE
    ));
    run_in(&server, &restored, "sleep 300", "sleep");
    std::fs::remove_file(reads.join("freeze")).unwrap();

    assert!(
        wait_for(|| kept(&server, &restored) == none(), SETTLE),
        "the command outlived the agent: {:?}\n{}",
        kept(&server, &restored),
        server.watcher_logs()
    );
    after_the_next_read(&server, &restored);
    // The record stands, as that of an agent that reports for itself does while its shell is not back.
    assert_eq!(
        server.pane_opt(&restored, "@agent_kind"),
        "claude",
        "{}",
        server.watcher_logs()
    );
}

// Follows herdr's reported agent a restored pane starts with (src/persist/restore.rs, v0.9.3):
// a real tmux, a real watcher and a fake agent that reports nothing.
/// A restore that types the command an agent reported puts back on the pane the agent's word that it is
/// there, so the watcher keeps it as an agent that reports for itself, as herdr's restore does: until
/// the pane's own shell is back at its prompt, and not only until it has been missed a few times. An
/// agent taw has no way to name (no manifest, no name of its process that taw knows) that reports
/// nothing for over 30 seconds, through more of the watcher's rechecks than its misses would have
/// allowed, keeps its record and its command; and its shell back at its prompt ends both.
#[test]
fn a_restored_agent_that_reports_nothing_keeps_its_command_until_its_shell_is_back() {
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    let bin = fake_that_notes_its_runs(tmp.path(), "prime-agent", &runs);
    let server = server_with(&bin);
    let pane = server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-c",
        tmp.path().to_str().unwrap(),
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    prime_holds_the_pane_with(&server, &pane, &["prime-agent", "--resume", "a"]);
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "alpha"]);
    server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    let restored = server.pane_of("alpha");
    assert!(
        wait_for(|| runs_of(&runs).len() == 1, SETTLE),
        "the command was never run"
    );
    assert_eq!(
        server.pane_opt(&restored, "@agent_reported_kind"),
        "prime-agent"
    );
    server.ok(&["watch", "attach", &restored], "cli:watch:attach", "ok");
    let began = std::time::Instant::now();
    let first = server.watcher_reads(&restored);

    // The watcher would take the record off after its sixth miss, one a recheck, five seconds apart: a
    // record taken off sooner ends the wait at 30 seconds.
    assert!(
        wait_for(
            || {
                began.elapsed() >= Duration::from_secs(30)
                    && (server.watcher_reads(&restored) >= first + 8
                        || kept(&server, &restored) == none())
            },
            Duration::from_secs(120)
        ),
        "the watcher read the pane {} times in {:?}\n{}",
        server.watcher_reads(&restored) - first,
        began.elapsed(),
        server.watcher_logs()
    );
    assert_eq!(
        kept(&server, &restored),
        kept_as("prime-agent", "prime-agent", "prime-agent --resume a"),
        "{}",
        server.watcher_logs()
    );
    assert_eq!(server.pane_opt(&restored, "@agent_kind"), "prime-agent");

    server.wake("taw-fake-prime-agent-never");
    assert!(
        wait_for(
            || kept(&server, &restored) == none()
                && server.pane_opt(&restored, "@agent_kind").is_empty(),
            SETTLE
        ),
        "the agent and its command outlived it: {:?} {:?}\n{}",
        server.pane_opt(&restored, "@agent_kind"),
        kept(&server, &restored),
        server.watcher_logs()
    );
}

/// tmux takes about 16 KiB in one command, and a report and its command are one: a command the checks
/// let through (8 KiB at the most) that is too long to write once quoted for tmux, every word of it a
/// quote, is refused in the same words as a command the checks refuse, and nothing of its report is
/// applied.
#[test]
fn a_command_too_long_for_one_tmux_call_once_quoted_is_refused_and_nothing_applied() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with(tmp.path());
    let pane = server.pane();
    let quotes = "\"".repeat(8 * 1024 - "prime-agent".len());
    let resume = ["prime-agent", quotes.as_str()];

    let refused = report(&server, &pane, PRIME, "working", "1", Some(&resume));

    assert_eq!(error_code(refused), "invalid_resume_argv");
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "");
    assert_eq!(kept(&server, &pane), none());
    // One that fits is kept.
    let fits = "\"".repeat(1024);
    ok(report(
        &server,
        &pane,
        PRIME,
        "working",
        "1",
        Some(&["prime-agent", fits.as_str()]),
    ));
    assert_eq!(kept(&server, &pane)[2], format!("prime-agent '{fits}'"));
}

/// A `tmux` for the whole server that, once `arm` is in `dir`, holds the first clear of a pane's
/// agent record (the `set-option -u` of `@agent_kind` the watcher makes when the agent has left) until
/// `go` is in `dir`, and says it is holding by making `held` there. Everything else goes straight to the
/// real tmux.
fn tmux_that_holds_the_watchers_clear(dir: &Path) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-holding-a-clear");
    write_script(
        &wrapper,
        &format!(
            "case \"$*\" in\n  *'-u -p -t '*' @agent_kind '*)\n    \
             if [ -e '{dir}/arm' ] && mkdir '{dir}/held' 2>/dev/null; then\n      i=0\n      \
             while [ ! -e '{dir}/go' ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done\n    \
             fi ;;\nesac\nexec '{real}' \"$@\"\n",
            dir = dir.display(),
            real = real.display(),
        ),
    );
    wrapper
}

// Follows herdr's `delayed_shell_return_keeps_an_agent_that_claimed_the_pane_afterwards`
// (src/app/actions.rs, v0.9.3): herdr's
// shell-return signal carries when the shell was seen idle; here the watcher's clear is held between
// the read that found the shell back and its write, and an agent claims the pane in between.
/// The watcher reads a pane whose self-reported agent has quit, finds its shell back, and clears the
/// record; an agent started again in the pane, which claims it with its command between that read and
/// the clear, is not cleared with it: the clear lands only while the pane holds what the read saw.
#[test]
fn delayed_shell_return_keeps_an_agent_that_claimed_the_pane_afterwards() {
    let tmp = tempfile::tempdir().unwrap();
    let held = tmp.path().join("held-clear");
    std::fs::create_dir_all(&held).unwrap();
    let wrapper = tmux_that_holds_the_watchers_clear(&held);
    let server = Server::start_with(ServerOptions {
        width: 100,
        height: 30,
        env: vec![
            ("TAW_LOG".to_string(), "debug".to_string()),
            ("TAW_TMUX_BIN".to_string(), wrapper.display().to_string()),
        ],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();
    run_in(&server, &pane, "sleep 300", "sleep");
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    prime_holds_the_pane_with(&server, &pane, &["prime-agent", "--resume", "a"]);

    std::fs::write(held.join("arm"), "").unwrap();
    server.tmux_ok(&["send-keys", "-t", &pane, "C-c"]);
    assert!(
        wait_for(|| held.join("held").exists(), SETTLE),
        "the watcher never cleared the record of the agent that quit\n{}",
        server.watcher_logs()
    );
    run_in(&server, &pane, "sleep 300", "sleep");
    ok(report(
        &server,
        &pane,
        PRIME,
        "idle",
        "2",
        Some(&["prime-agent", "--resume", "b"]),
    ));
    std::fs::write(held.join("go"), "").unwrap();

    // The clear has gone through, or been held back, once the watcher has read the pane again.
    let reads = server.watcher_reads(&pane);
    assert!(
        wait_for(|| server.watcher_reads(&pane) > reads, SETTLE),
        "the watcher never read the pane again\n{}",
        server.watcher_logs()
    );
    assert_eq!(
        server.pane_opt(&pane, "@agent_kind"),
        "prime-agent",
        "{}",
        server.watcher_logs()
    );
    assert_eq!(
        kept(&server, &pane),
        kept_as("prime-agent", "prime-agent", "prime-agent --resume b"),
        "{}",
        server.watcher_logs()
    );
}

/// A `tmux` for the whole server that keeps the watcher's reads of a pane waiting while `freeze` is in
/// `dir`, and, once `arm` is there, holds the first call that matches `pattern` (a `case` pattern over its
/// arguments, put after an arm that lets every call that writes `@agent_kind` go) until `go` is there,
/// saying so by making `held` there. Everything else goes straight to the real tmux.
fn tmux_that_holds_a_watcher_write(dir: &Path, pattern: &str) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-holding-a-write");
    write_script(
        &wrapper,
        &format!(
            "case \"$*\" in\n  *' display-message -p -t '*'@agent_report_stamp'*)\n    \
             while [ -e '{dir}/freeze' ]; do sleep 0.05; done ;;\n  *' @agent_kind '*) ;;\n  \
             {pattern})\n    \
             if [ -e '{dir}/arm' ] && mkdir '{dir}/held' 2>/dev/null; then\n      i=0\n      \
             while [ ! -e '{dir}/go' ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done\n    \
             fi ;;\nesac\nexec '{real}' \"$@\"\n",
            dir = dir.display(),
            real = real.display(),
        ),
    );
    wrapper
}

/// A server whose tmux, its watchers' and every `taw` the test runs, is `wrapper`, with the fakes in `bin`
/// first on its panes' `PATH`.
fn server_through(wrapper: &Path, bin: Option<&Path>) -> Server {
    let server = Server::start_with(ServerOptions {
        width: 100,
        height: 30,
        path_prefix: bin.map(Path::to_path_buf),
        env: vec![
            ("TAW_LOG".to_string(), "debug".to_string()),
            ("TAW_TMUX_BIN".to_string(), wrapper.display().to_string()),
        ],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    server
}

/// Waits for the watcher of `pane` to read it again, and for what that read decides to land.
fn after_the_next_read(server: &Server, pane: &str) {
    let reads = server.watcher_reads(pane);
    assert!(
        wait_for(|| server.watcher_reads(pane) > reads, SETTLE),
        "the watcher never read the pane again\n{}",
        server.watcher_logs()
    );
    std::thread::sleep(Duration::from_millis(1000));
}

/// A pi takes a pane from a codex the watcher had recorded, and reports the command that resumes its
/// session between the watcher's read that finds the pi and the watcher's drop of what the codex
/// reported: the drop lands only while the pane holds the report stamp that read saw, so the pi's
/// command is not taken with the codex's.
#[test]
fn a_report_between_the_watchers_read_and_its_release_of_the_last_agent_keeps_its_command() {
    let tmp = tempfile::tempdir().unwrap();
    let held = tmp.path().join("held");
    std::fs::create_dir_all(&held).unwrap();
    let wrapper = tmux_that_holds_a_watcher_write(&held, "*'#{==:#{@agent_session_id},}'*");
    let bin = named_agent(tmp.path(), "codex", None);
    named_agent(tmp.path(), "pi", None);
    let server = server_through(&wrapper, Some(&bin));
    let pane = server.pane();
    run_in(&server, &pane, "codex", "codex");
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_kind") == "codex", SETTLE),
        "codex never recorded\n{}",
        server.watcher_logs()
    );
    // The watcher's reads wait while the codex leaves and a pi takes its place.
    std::fs::write(held.join("freeze"), "").unwrap();
    std::thread::sleep(Duration::from_millis(800));
    server.wake("taw-fake-codex-never");
    assert!(wait_for(
        || server.display(&pane, "#{pane_current_command}") != "codex",
        SETTLE
    ));
    run_in(&server, &pane, "pi", "pi");
    std::fs::write(held.join("arm"), "").unwrap();
    std::fs::remove_file(held.join("freeze")).unwrap();
    assert!(
        wait_for(|| held.join("held").exists(), SETTLE),
        "the watcher never dropped what codex reported\n{}",
        server.watcher_logs()
    );

    ok(report(
        &server,
        &pane,
        ("integration:pi", "pi"),
        "working",
        "1",
        Some(&["pi", "--continue"]),
    ));
    std::fs::write(held.join("go"), "").unwrap();
    after_the_next_read(&server, &pane);

    assert_eq!(
        kept(&server, &pane),
        kept_as("integration:pi", "pi", "pi --continue"),
        "{}",
        server.watcher_logs()
    );
    assert_eq!(
        server.pane_opt(&pane, "@agent_source"),
        "hook:integration:pi"
    );
}

/// A restored agent that only its hint names exits, and the watcher's read finds its pid dead; before
/// the watcher takes the command it reported off the pane, the agent is started again and reports a new
/// one. The drop lands only while the pane holds the report stamp the read saw: the new command stays.
#[test]
fn a_report_between_the_watchers_read_and_its_drop_at_a_hinted_agents_exit_keeps_its_command() {
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    let bin = fake_that_notes_its_runs(tmp.path(), "claude-wrapper", &runs);
    let held = tmp.path().join("held");
    std::fs::create_dir_all(&held).unwrap();
    let wrapper =
        tmux_that_holds_a_watcher_write(&held, "*'@agent_resume_source ; '*'@agent_resume_argv'*");
    let server = server_through(&wrapper, Some(&bin));
    let pane = server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-c",
        tmp.path().to_str().unwrap(),
        "-P",
        "-F",
        "#{pane_id}",
    ]);
    ok(report(
        &server,
        &pane,
        ("my-wrapper", "claude"),
        "idle",
        "1",
        Some(&["claude-wrapper", "--continue"]),
    ));
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "alpha"]);
    server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    let restored = server.pane_of("alpha");
    assert!(
        wait_for(|| runs_of(&runs).len() == 1, SETTLE),
        "the command was never run"
    );
    assert!(
        wait_for(
            || !server.pane_opt(&restored, "@agent_pid").is_empty(),
            SETTLE
        ),
        "the restore recorded no pid for what it started"
    );
    server.ok(&["watch", "attach", &restored], "cli:watch:attach", "ok");
    after_the_next_read(&server, &restored);
    std::fs::write(held.join("arm"), "").unwrap();

    server.wake("taw-fake-claude-wrapper-never");
    assert!(
        wait_for(|| held.join("held").exists(), SETTLE),
        "the watcher never took the command off at the agent's exit\n{}",
        server.watcher_logs()
    );
    // Reported from the pane, by what runs in front of it, as an agent's own hook reports: such a report
    // takes off the pid of the agent that exited, which said the agent had gone.
    run_in(
        &server,
        &restored,
        &format!(
            "sh -c '\"{}\" pane report-agent \"$TMUX_PANE\" --source my-wrapper --agent claude --state \
             working --seq 2 -- claude-wrapper --resume b > /dev/null; exec sleep 300'",
            common::taw_bin()
        ),
        "sleep",
    );
    assert_eq!(
        kept(&server, &restored),
        kept_as("my-wrapper", "claude", "claude-wrapper --resume b")
    );
    std::fs::write(held.join("go"), "").unwrap();
    after_the_next_read(&server, &restored);
    after_the_next_read(&server, &restored);

    assert_eq!(
        kept(&server, &restored),
        kept_as("my-wrapper", "claude", "claude-wrapper --resume b"),
        "{}",
        server.watcher_logs()
    );
}

/// The two-step exit: an agent that reports for itself quits mid-turn, and the watcher's read finds its
/// shell back. It writes the turn's end (`idle`, rule `process_exited`, as `manifest`'s) and clears the
/// record at the next read. An agent started again in the pane claims it with its command between that
/// read and the write: the write lands only while the pane holds the report stamp the read saw, so the
/// pane stays its hook's, with its command, and the next read does not clear it.
#[test]
fn a_report_between_the_watchers_read_and_its_write_of_an_exit_is_not_handed_back_to_the_manifest()
{
    let tmp = tempfile::tempdir().unwrap();
    let held = tmp.path().join("held");
    std::fs::create_dir_all(&held).unwrap();
    let wrapper = tmux_that_holds_a_watcher_write(&held, "*'@agent_rule'*'process_exited'*");
    let server = server_through(&wrapper, None);
    let pane = server.pane();
    run_in(&server, &pane, "sleep 300", "sleep");
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    ok(report(
        &server,
        &pane,
        PRIME,
        "working",
        "1",
        Some(&["prime-agent", "--resume", "a"]),
    ));
    after_the_next_read(&server, &pane);
    std::fs::write(held.join("arm"), "").unwrap();

    server.tmux_ok(&["send-keys", "-t", &pane, "C-c"]);
    assert!(
        wait_for(|| held.join("held").exists(), SETTLE),
        "the watcher never wrote the end of the turn of the agent that quit\n{}",
        server.watcher_logs()
    );
    run_in(&server, &pane, "sleep 300", "sleep");
    ok(report(
        &server,
        &pane,
        PRIME,
        "working",
        "2",
        Some(&["prime-agent", "--resume", "b"]),
    ));
    std::fs::write(held.join("go"), "").unwrap();
    after_the_next_read(&server, &pane);
    after_the_next_read(&server, &pane);

    assert_eq!(
        server.pane_opt(&pane, "@agent_source"),
        "hook:prime-agent",
        "{}",
        server.watcher_logs()
    );
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "working");
    assert_eq!(
        kept(&server, &pane),
        kept_as("prime-agent", "prime-agent", "prime-agent --resume b"),
        "{}",
        server.watcher_logs()
    );
}

/// herdr keeps a command while the source and the agent that reported it hold the pane: another source
/// that reports the same agent's state takes the pane, and the command with it, as another agent does.
#[test]
fn another_source_that_takes_the_pane_for_the_same_agent_takes_the_command_away() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with(tmp.path());
    let pane = server.pane();
    prime_holds_the_pane_with(&server, &pane, &["prime-agent", "--resume", "a"]);

    // The same source again, with no command: the command stays.
    ok(report(&server, &pane, PRIME, "working", "2", None));
    assert_eq!(kept(&server, &pane)[2], "prime-agent --resume a");

    ok(report(
        &server,
        &pane,
        ("custom:prime", "prime-agent"),
        "idle",
        "1",
        None,
    ));
    assert_eq!(kept(&server, &pane), none());
    assert_eq!(server.pane_opt(&pane, "@agent_source"), "hook:custom:prime");
}

/// A `tmux` that holds the first session report to ask who holds a pane, right after it answers, until
/// `go` is in `dir`, and says so by making `held` there.
fn tmux_that_holds_a_report_after_it_asks_who_holds_the_pane(dir: &Path) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-holding-after-the-holder");
    write_script(
        &wrapper,
        &format!(
            "out=$('{real}' \"$@\"); status=$?\nprintf '%s\\n' \"$out\"\n\
             case \"$*\" in\n  *' display-message -p -t '*'#{{m:hook:*,#{{@agent_source}}}}')\n    \
             if mkdir '{dir}/held' 2>/dev/null; then\n      i=0\n      \
             while [ ! -e '{dir}/go' ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done\n    \
             fi ;;\nesac\nexit $status\n",
            dir = dir.display(),
            real = real.display(),
        ),
    );
    wrapper
}

/// Who holds the pane is read, and the command is written, in two tmux calls; another source that takes
/// the pane in between, with a report of its own, is what the write finds: the command was decided on a
/// pane its reporter held, and it holds it no longer, so the report is decided again, and refused.
#[test]
fn a_command_whose_reporter_lost_the_pane_since_it_asked_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with(tmp.path());
    let pane = server.pane();
    ok(report(&server, &pane, PRIME, "idle", "1", None));
    let held = tmp.path().join("held-report");
    std::fs::create_dir_all(&held).unwrap();
    let wrapper = tmux_that_holds_a_report_after_it_asks_who_holds_the_pane(&held);

    let outcome = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            server.taw_with_env(
                &[
                    "pane",
                    "report-agent-session",
                    &pane,
                    "--source",
                    "prime-agent",
                    "--agent",
                    "prime-agent",
                    "--seq",
                    "2",
                    "--",
                    "prime-agent",
                    "--resume",
                    "a",
                ],
                &[("TAW_TMUX_BIN", wrapper.to_str().unwrap())],
            )
        });
        assert!(
            wait_for(|| held.join("held").exists(), SETTLE),
            "the session report never asked who holds the pane"
        );
        // With no number: the order of reports the session report read stays as it was, and only who
        // holds the pane tells the write that the pane is not as it was decided on.
        ok(taw(
            &server,
            &[
                "pane",
                "report-agent",
                &pane,
                "--source",
                "custom:other",
                "--agent",
                "prime-agent",
                "--state",
                "working",
            ],
            None,
        ));
        assert_eq!(server.pane_opt(&pane, "@agent_report_seq"), "prime-agent=1");
        std::fs::write(held.join("go"), "").unwrap();
        first.join().unwrap()
    });

    assert_eq!(error_code(outcome), "resume_not_accepted");
    assert_eq!(kept(&server, &pane), none());
    assert_eq!(server.pane_opt(&pane, "@agent_source"), "hook:custom:other");
}

/// A `report-agent-session` by `source` for `agent` on `pane`, with no number, carrying `resume`, made
/// through the `tmux` at `wrapper`, while `between` runs once the wrapper says it holds the report (the
/// `held` it makes in `held`), and the report's outcome.
fn a_session_report_held_while(
    server: &Server,
    pane: &str,
    who: (&str, &str),
    resume: &[&str],
    held: &Path,
    wrapper: &Path,
    between: impl FnOnce(),
) -> (i32, Value) {
    let (source, agent) = who;
    let mut args = vec![
        "pane",
        "report-agent-session",
        pane,
        "--source",
        source,
        "--agent",
        agent,
        "--agent-session-id",
        "s1",
        "--",
    ];
    args.extend(resume);
    std::thread::scope(|scope| {
        let first = scope
            .spawn(|| server.taw_with_env(&args, &[("TAW_TMUX_BIN", wrapper.to_str().unwrap())]));
        let came = wait_for(|| held.join("held").exists(), SETTLE);
        if came {
            between();
        }
        std::fs::write(held.join("go"), "").unwrap();
        let outcome = first.join().unwrap();
        assert!(
            came,
            "the session report never asked who holds the pane: {outcome:?}"
        );
        outcome
    })
}

/// A `report-agent` of `state` by `source` for `agent` on `pane`, with no number.
fn unnumbered_report(server: &Server, pane: &str, who: (&str, &str), state: &str) {
    let (source, agent) = who;
    ok(taw(
        server,
        &[
            "pane",
            "report-agent",
            pane,
            "--source",
            source,
            "--agent",
            agent,
            "--state",
            state,
        ],
        None,
    ));
}

/// A session report is decided on a pane that no hook held, and whose agent in front of it is the one
/// it reports, so it may set its command. Another hook takes the pane before the write, with a report
/// of no number, so the order of reports the session report read stays as it was: the write lands only
/// while no hook holds the pane, so the report is decided again, on the pane another hook holds, and its
/// command is refused.
#[test]
fn a_command_decided_with_no_hook_on_the_pane_is_refused_once_a_hook_has_taken_it() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = named_agent(tmp.path(), "codex", None);
    let server = server_with(&bin);
    let pane = server.pane();
    run_in(&server, &pane, "codex", "codex");
    let held = tmp.path().join("held-report");
    std::fs::create_dir_all(&held).unwrap();
    let wrapper = tmux_that_holds_a_report_after_it_asks_who_holds_the_pane(&held);

    let outcome = a_session_report_held_while(
        &server,
        &pane,
        ("integration:codex", "codex"),
        &["codex", "resume", "s1"],
        &held,
        &wrapper,
        || {
            unnumbered_report(&server, &pane, ("other-hook", "codex"), "working");
            assert_eq!(server.pane_opt(&pane, "@agent_source"), "hook:other-hook");
        },
    );

    assert_eq!(error_code(outcome), "resume_not_accepted");
    assert_eq!(kept(&server, &pane), none());
    assert_eq!(server.pane_opt(&pane, "@agent_source"), "hook:other-hook");
}

/// A session report is decided on a pane another hook held, and its command is offered for nothing. Its
/// reporter takes the pane itself before the write, with a report of no number: the write lands only
/// while another hook holds the pane, so the report is decided again, on the pane its reporter holds, and
/// its command is kept.
#[test]
fn a_command_decided_while_another_hook_held_the_pane_is_kept_once_its_reporter_has_taken_it() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with(tmp.path());
    let pane = server.pane();
    unnumbered_report(&server, &pane, ("other-hook", "codex"), "working");
    let held = tmp.path().join("held-report");
    std::fs::create_dir_all(&held).unwrap();
    let wrapper = tmux_that_holds_a_report_after_it_asks_who_holds_the_pane(&held);

    let outcome = a_session_report_held_while(
        &server,
        &pane,
        ("integration:codex", "codex"),
        &["codex", "resume", "s1"],
        &held,
        &wrapper,
        || {
            unnumbered_report(&server, &pane, ("integration:codex", "codex"), "idle");
            assert_eq!(
                server.pane_opt(&pane, "@agent_source"),
                "hook:integration:codex"
            );
        },
    );

    ok(outcome);
    assert_eq!(
        kept(&server, &pane),
        kept_as("integration:codex", "codex", "codex resume s1")
    );
}

/// An agent that the process in front of its pane names only by its arguments, a `node` running the
/// agent's script as an npm install runs it, sets its own command while no hook holds the pane, as one
/// that the process's name names does: taw reads the processes of the foreground job, and their
/// arguments, as the verbs that type do.
#[test]
fn an_agent_named_by_the_arguments_of_its_process_sets_its_own_command_with_no_hook() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = tmp.path().join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    // A program tmux calls `node`, with the path of a script called `codex` among its arguments, as
    // `node <dir>/bin/codex` has: a copy of tmux, whose `-f` takes the path (a config file that only a
    // server it started would read), waiting until the test's server goes.
    let node = tmux_named(&runtime, "node");
    let script = tmp.path().join("lib").join("codex");
    let server = server_with(tmp.path());
    let pane = server.pane();
    run_in(
        &server,
        &pane,
        &format!(
            "'{}' -f '{}' -L \"$TAW_TMUX_SOCKET_NAME\" wait-for taw-fake-node-codex-never",
            node.display(),
            script.display()
        ),
        "node",
    );

    ok(report_session(
        &server,
        &pane,
        ("integration:codex", "codex"),
        &["--agent-session-id", "s1"],
        Some(&["codex", "resume", "s1"]),
    ));

    assert_eq!(
        kept(&server, &pane),
        kept_as("integration:codex", "codex", "codex resume s1")
    );
}
