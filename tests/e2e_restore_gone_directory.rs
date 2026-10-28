//! A restored pane whose saved directory is gone, end to end through the built `taw` binary (herdr
//! 0.9.2, #4320). tmux opens such a pane in the home folder, and taw cannot stop it: a restore types
//! nothing into it, says which directory is gone and what to run once it is back, and leaves the pane
//! carrying herdr's error, which `pane get` and `pane list` show as `restore_error`. Every save after
//! that keeps the pane as it was saved, its directory and its agent's session, until it is closed or
//! an agent of its own is recorded in it: then it is saved as it is, with that agent.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{Server, ServerOptions, named_agent, tmux_named, wait_for, write_script};
use serde_json::Value;

/// Ceiling for what a real tmux and the built binary take to do a thing, with `cargo test` running
/// other files beside this one.
const SETTLE: Duration = Duration::from_secs(20);

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// What herdr says of a pane whose saved directory is gone (`src/app/agent_resume.rs` at v0.9.3),
/// word for word.
const UNAVAILABLE: &str =
    "Saved directory is unavailable. Restore the directory and restart this session.";

/// A fake `claude` in `dir/bin`: it notes each run it is given in a file of its own in `runs` (the
/// directory it was started in, then its arguments, one to a line), runs `then` (a line of shell, empty
/// for nothing), and becomes a program tmux names `claude` that waits until the test's server goes.
/// Every test here puts it first on the panes' `PATH`, so that a restore that typed a resume where it
/// should not runs this fake, which says so, and never the Claude Code of whoever runs the suite.
/// Returns `dir/bin`, where other fakes of a test go too.
fn claude_in(dir: &Path, runs: &Path, then: &str) -> PathBuf {
    let bin = dir.join("bin");
    let copy_dir = dir.join("copy");
    std::fs::create_dir_all(&copy_dir).unwrap();
    std::fs::create_dir_all(runs).unwrap();
    let copy = tmux_named(&copy_dir, "claude");
    write_script(
        &bin.join("claude"),
        &format!(
            "run=$(mktemp '{runs}/run.XXXXXX') && printf '%s\\n' \"$(pwd -P)\" \"$@\" > \
             \"$run.part\" && mv \"$run.part\" \"$run.done\"\n{then}\n\
             exec '{copy}' -L \"$TAW_TMUX_SOCKET_NAME\" wait-for taw-fake-claude-never\n",
            runs = runs.display(),
            copy = copy.display()
        ),
    );
    bin
}

/// The runs a [`claude_in`] noted, each as the directory and the arguments.
fn runs_of(runs: &Path) -> Vec<Vec<String>> {
    let mut done: Vec<PathBuf> = std::fs::read_dir(runs)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "done"))
        .collect();
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

/// The panes of `session`, in their order in its window.
fn panes_of(server: &Server, session: &str) -> Vec<String> {
    server
        .tmux_ok(&[
            "list-panes",
            "-t",
            &format!("={session}:"),
            "-F",
            "#{pane_id}",
        ])
        .lines()
        .map(str::to_string)
        .collect()
}

/// The record `taw pane get` gives of `pane`.
fn record_of(server: &Server, pane: &str) -> Value {
    server.ok(&["pane", "get", pane], "cli:pane:get", "pane")["pane"].clone()
}

/// The directory, agent and session the default snapshot holds for each pane of `session`'s first
/// window, in order.
fn saved_panes(server: &Server, session: &str) -> Vec<(String, Value, Value)> {
    let path = server.state_dir().join("sessions").join("default.json");
    let snapshot: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let saved = snapshot["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|saved| saved["name"] == session)
        .unwrap_or_else(|| panic!("no session {session} in {snapshot:#}"))
        .clone();
    saved["windows"][0]["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pane| {
            (
                pane["cwd"].as_str().unwrap().to_string(),
                pane["agent_kind"].clone(),
                pane["agent_session_id"].clone(),
            )
        })
        .collect()
}

/// What a real tmux has to show: a pane saved in a directory with a Claude conversation (a fake
/// `claude`), the directory removed, and a restore that types nothing into the pane and names the
/// directory, with the command to run once it is back. A second pane of the same window, whose
/// directory is still there, is resumed as before: once its command has run, the other pane's would
/// have, had one been typed. An autosave after the restore still holds the directory and the session,
/// though the pane's shell has gone somewhere else since.
#[test]
fn a_restore_types_nothing_into_a_pane_whose_saved_directory_is_gone_and_says_which_it_is() {
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    let server = Server::start_with(ServerOptions {
        width: 160,
        height: 40,
        path_prefix: Some(claude_in(tmp.path(), &runs, "")),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let (gone, kept) = (tmp.path().join("project"), tmp.path().join("kept"));
    for dir in [&gone, &kept] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let (gone, kept) = (
        gone.canonicalize().unwrap().display().to_string(),
        kept.canonicalize().unwrap().display().to_string(),
    );
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-x",
        "160",
        "-y",
        "40",
        "-c",
        &gone,
    ]);
    server.tmux_ok(&["split-window", "-d", "-t", "=work:", "-c", &kept]);
    let saved = panes_of(&server, "work");
    for (pane, dir, id) in [
        (&saved[0], &gone, "conv-gone"),
        (&saved[1], &kept, "conv-kept"),
    ] {
        assert!(
            wait_for(
                || server.display(pane, "#{pane_current_path}") == *dir,
                SETTLE
            ),
            "pane {pane} never came up in {dir}"
        );
        server.set_pane_opt(pane, "@agent_kind", "claude");
        server.set_pane_opt(pane, "@agent_session_id", id);
    }
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "=work"]);
    std::fs::remove_dir_all(&gone).unwrap();

    let restored = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );

    let panes = panes_of(&server, "work");
    assert_eq!(panes.len(), 2, "{restored}");
    assert_eq!(
        restored["resumed"],
        serde_json::json!([panes[1]]),
        "{restored}"
    );
    assert_eq!(
        restored["not_resumed"],
        serde_json::json!([panes[0]]),
        "{restored}"
    );
    let line = format!(
        "pane {}: saved directory {gone} is gone, so tmux opened it in your home folder and nothing \
         was typed; once the directory is back, resume it with: cd {gone} && claude --resume \
         conv-gone",
        panes[0]
    );
    assert!(
        restored["plan"]
            .as_array()
            .unwrap()
            .contains(&Value::String(line.clone())),
        "no line {line:?} in {restored:#}"
    );
    assert!(
        wait_for(|| runs_of(&runs).len() == 1, SETTLE),
        "the pane in {kept} was not resumed: {:?}",
        runs_of(&runs)
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        runs_of(&runs),
        [vec![kept.clone(), "--resume".into(), "conv-kept".into()]],
        "the agent of the pane whose directory is gone was started somewhere else"
    );
    assert!(
        !server.capture(&panes[0]).contains("claude"),
        "something was typed into the pane whose directory is gone:\n{}",
        server.capture(&panes[0])
    );

    // `pane get` and `pane list` show herdr's error on that pane, and on no other.
    assert_eq!(record_of(&server, &panes[0])["restore_error"], UNAVAILABLE);
    assert_eq!(record_of(&server, &panes[1])["restore_error"], Value::Null);
    let listed = server.ok(&["pane", "list"], "cli:pane:list", "pane_list");
    let errors: Vec<(String, Value)> = listed["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pane| {
            (
                pane["terminal_id"].as_str().unwrap().to_string(),
                pane["restore_error"].clone(),
            )
        })
        .filter(|(_, error)| !error.is_null())
        .collect();
    assert_eq!(errors, [(panes[0].clone(), Value::from(UNAVAILABLE))]);

    // The next autosave keeps the pane as it was saved, its directory and its conversation, and not
    // as tmux opened it, wherever its shell has gone since; the other pane as it is.
    server.tmux_ok(&["send-keys", "-t", &panes[0], "cd /", "Enter"]);
    assert!(
        wait_for(
            || server.display(&panes[0], "#{pane_current_path}") == "/",
            SETTLE
        ),
        "the pane's shell never went to /"
    );
    autosave(&server);
    assert_eq!(
        saved_panes(&server, "work"),
        [
            (
                gone.clone(),
                Value::from("claude"),
                Value::from("conv-gone")
            ),
            (
                kept.clone(),
                Value::from("claude"),
                Value::from("conv-kept")
            ),
        ]
    );
}

/// An autosave, as a hook asks for one, with the saved snapshot older than an autosave waits for.
fn autosave(server: &Server) {
    let snapshot = server.state_dir().join("sessions").join("default.json");
    std::fs::File::options()
        .write(true)
        .open(&snapshot)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(120))
        .unwrap();
    let (code, autosaved) = server.taw(&["session", "autosave"]);
    assert_eq!(code, 0, "{autosaved}");
    assert_eq!(autosaved["result"]["saved"], true, "{autosaved}");
}

/// A server whose panes find the fakes in `bin` first on their `PATH`.
fn server_with(bin: &Path) -> Server {
    let server = Server::start_with(ServerOptions {
        width: 160,
        height: 40,
        path_prefix: Some(bin.to_path_buf()),
        env: vec![("TAW_LOG".to_string(), "debug".to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    server
}

/// A pane restored with its saved directory gone: saved in `tmp/project` with a Claude conversation
/// (`conv-gone`), its session closed, the directory removed, and the layout restored. Returns the
/// restored pane, which carries herdr's error and keeps what it was saved as, and the directory. The
/// server's panes have a [`claude_in`] that notes its runs in `runs` first on their `PATH`, and nothing
/// was run there by the restore.
fn a_kept_pane(server: &Server, tmp: &Path, runs: &Path) -> (String, String) {
    let gone = tmp.join("project");
    std::fs::create_dir_all(&gone).unwrap();
    let gone = gone.canonicalize().unwrap().display().to_string();
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-x",
        "160",
        "-y",
        "40",
        "-c",
        &gone,
    ]);
    let saved = panes_of(server, "work").remove(0);
    assert!(
        wait_for(
            || server.display(&saved, "#{pane_current_path}") == gone,
            SETTLE
        ),
        "pane {saved} never came up in {gone}"
    );
    server.set_pane_opt(&saved, "@agent_kind", "claude");
    server.set_pane_opt(&saved, "@agent_session_id", "conv-gone");
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "=work"]);
    std::fs::remove_dir_all(&gone).unwrap();
    let restored = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    let pane = panes_of(server, "work").remove(0);
    assert_eq!(
        restored["not_resumed"],
        serde_json::json!([pane]),
        "{restored}"
    );
    assert_eq!(record_of(server, &pane)["restore_error"], UNAVAILABLE);
    // A command the restore typed, which it must not have, has run by now.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        runs_of(runs),
        Vec::<Vec<String>>::new(),
        "the restore ran claude"
    );
    (pane, gone)
}

/// A folder of the test's own for a pane's shell to go to, as it is named once the shell is in it.
fn elsewhere(tmp: &Path) -> String {
    let dir = tmp.join("elsewhere");
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap().display().to_string()
}

/// Has `pane`'s shell go to `dir`, and waits for tmux to say it is there.
fn cd_in(server: &Server, pane: &str, dir: &str) {
    server.tmux_ok(&["send-keys", "-t", pane, &format!("cd '{dir}'"), "Enter"]);
    assert!(
        wait_for(
            || server.display(pane, "#{pane_current_path}") == dir,
            SETTLE
        ),
        "the shell of {pane} never went to {dir}"
    );
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

/// A pane kept for a gone directory lets go of what it was saved as once
/// it has an agent of its own. A fake `claude` started in it, in another folder, reports its session,
/// `conv-new`, as Claude's own hook does: the next save holds that session in the folder the pane is
/// in now, and the pane carries no error. Kept, every autosave wrote the old conversation over the new
/// one, and a later restore lost it.
#[test]
fn a_kept_pane_lets_go_once_an_agent_started_in_it_reports_its_session() {
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    let reports = format!(
        "'{}' pane report-agent-session \"$TMUX_PANE\" --source integration:claude --agent claude \
         --agent-session-id conv-new > /dev/null 2>&1",
        common::taw_bin()
    );
    let server = server_with(&claude_in(tmp.path(), &runs, &reports));
    let (pane, _) = a_kept_pane(&server, tmp.path(), &runs);
    let there = elsewhere(tmp.path());

    cd_in(&server, &pane, &there);
    run_in(&server, &pane, "claude", "claude");
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_session_id") == "conv-new",
            SETTLE
        ),
        "the fake claude's report never landed"
    );
    autosave(&server);

    assert_eq!(
        saved_panes(&server, "work"),
        [(
            there.clone(),
            Value::from("claude"),
            Value::from("conv-new")
        )]
    );
    assert_eq!(record_of(&server, &pane)["restore_error"], Value::Null);
    assert_eq!(
        runs_of(&runs),
        [vec![there]],
        "claude ran other than as typed"
    );
}

/// Starts the watcher of `pane` as `taw watch attach` does, with what it writes to stderr kept in a
/// file in `dir`, which it returns: tmux gives a `pipe-pane` command /dev/null for its stderr, where a
/// watcher's panic, error or failed spawn would go unread.
fn watch_keeping_stderr(server: &Server, pane: &str, dir: &Path) -> PathBuf {
    assert_eq!(
        server.display(pane, "#{pane_pipe}"),
        "0",
        "pane {pane} has a watcher already"
    );
    let stderr = dir.join("watch-stderr.log");
    // Each word as taw's own pipe command writes it: quoted for the shell, with `#` and `%` doubled
    // for tmux, which expands them in a `pipe-pane` command.
    let word = |text: &str| {
        shell_words::quote(text)
            .replace('#', "##")
            .replace('%', "%%")
    };
    let command = format!(
        "TAW_TMUX_SOCKET_NAME={} {} watch run {} 2>>{}",
        word(server.socket()),
        word(common::taw_bin()),
        word(pane),
        word(&stderr.display().to_string())
    );
    server.tmux_ok(&["pipe-pane", "-t", pane, &command]);
    stderr
}

/// What a watcher test prints when it fails: the pane's screen, its pipe and what is in front of it,
/// the watcher's pid (the one it recorded on the pane, and the one its log started with), whether that
/// process is still there and in what state, the watcher's whole log, and what it wrote to stderr.
/// Nothing here can fail the test again: a tmux that does not answer is said so.
fn watcher_evidence(server: &Server, pane: &str, stderr: &Path) -> String {
    let ask = |args: &[&str]| {
        server
            .tmux(args)
            .unwrap_or_else(|err| format!("(tmux said: {err})"))
    };
    let logs = server.watcher_logs();
    let mut out = format!(
        "== screen of {pane}\n{}\n== pane_pipe {} pane_current_command {}\n",
        ask(&["capture-pane", "-p", "-t", pane]),
        ask(&["display-message", "-p", "-t", pane, "#{pane_pipe}"]),
        ask(&[
            "display-message",
            "-p",
            "-t",
            pane,
            "#{pane_current_command}"
        ]),
    );
    let recorded = ask(&["show-options", "-pqv", "-t", pane, "@agent_watch_pid"]);
    let started = format!("watching pane {pane} (pid ");
    let logged = logs
        .split(started.as_str())
        .nth(1)
        .map(|rest| {
            rest.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .unwrap_or_default();
    let mut pids = vec![recorded.trim().to_string(), logged];
    pids.retain(|pid| !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()));
    pids.dedup();
    if pids.is_empty() {
        out += "== no watcher pid recorded or logged\n";
    }
    for pid in pids {
        let ps = std::process::Command::new("/bin/ps")
            .args([
                "-o",
                "pid=,ppid=,pgid=,stat=,wchan=,etime=,command=",
                "-p",
                &pid,
            ])
            .output();
        out += &match ps {
            Ok(ps) => format!(
                "== watcher pid {pid}: {}\n{}",
                if ps.status.success() {
                    "still there"
                } else {
                    "gone"
                },
                String::from_utf8_lossy(&ps.stdout)
            ),
            Err(err) => format!("== watcher pid {pid}: ps did not run: {err}\n"),
        };
    }
    out += &format!("== watcher log\n{logs}\n");
    out += &format!(
        "== watcher stderr ({})\n{}\n",
        stderr.display(),
        std::fs::read_to_string(stderr).unwrap_or_else(|err| format!("(not read: {err})"))
    );
    out
}

/// Prints [`watcher_evidence`] when the test it lives in fails, before the server it reads goes (it is
/// made after the server, so it is dropped first).
struct EvidenceOnFailure<'a> {
    server: &'a Server,
    pane: String,
    stderr: PathBuf,
}

impl Drop for EvidenceOnFailure<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "{}",
                watcher_evidence(self.server, &self.pane, &self.stderr)
            );
        }
    }
}

/// The same, for an agent the watcher names by its process: a fake `pi`, which tmux names `pi`,
/// started in the pane while a watcher reads it. Once the watcher records it, the next save holds it in
/// the folder the pane is in now.
#[test]
fn a_kept_pane_lets_go_once_the_watcher_names_an_agent_in_it() {
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    claude_in(tmp.path(), &runs, "");
    let server = server_with(&named_agent(tmp.path(), "pi", None));
    let (pane, _) = a_kept_pane(&server, tmp.path(), &runs);
    let there = elsewhere(tmp.path());
    let _evidence = EvidenceOnFailure {
        server: &server,
        pane: pane.clone(),
        stderr: watch_keeping_stderr(&server, &pane, tmp.path()),
    };

    cd_in(&server, &pane, &there);
    run_in(&server, &pane, "pi", "pi");
    // A pane with no agent is read only after output, and the fake pi draws nothing: the read the
    // echo of `pi` earned can come before the shell has started it. One key, which the terminal
    // echoes, is output that is read with pi in front.
    server.tmux_ok(&["send-keys", "-t", &pane, "x"]);
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_kind") == "pi", SETTLE),
        "the watcher never named the pi"
    );
    autosave(&server);

    assert_eq!(
        saved_panes(&server, "work"),
        [(there, Value::from("pi"), Value::Null)]
    );
    assert_eq!(record_of(&server, &pane)["restore_error"], Value::Null);
    assert_eq!(runs_of(&runs), Vec::<Vec<String>>::new(), "claude ran");
}

/// The same, for an agent `taw agent start` starts in the pane: once it is there, the pane is an
/// agent's, and the next save holds it as it is.
#[test]
fn a_kept_pane_lets_go_once_agent_start_starts_an_agent_in_it() {
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    let server = server_with(&claude_in(tmp.path(), &runs, ""));
    let (pane, _) = a_kept_pane(&server, tmp.path(), &runs);
    let here = server.display(&pane, "#{pane_current_path}");

    server.ok(
        &[
            "agent", "start", "keeper", "--kind", "claude", "--pane", &pane,
        ],
        "cli:agent:start",
        "agent",
    );
    autosave(&server);

    assert_eq!(
        saved_panes(&server, "work"),
        [(here.clone(), Value::from("claude"), Value::Null)]
    );
    assert_eq!(record_of(&server, &pane)["restore_error"], Value::Null);
    assert_eq!(
        runs_of(&runs),
        [vec![here]],
        "claude ran other than as started"
    );
}

/// The same, for an agent's report of its state alone, with no session: the pane is that agent's, and
/// the old conversation is not saved as if it were still there.
#[test]
fn a_kept_pane_lets_go_once_an_agent_reports_its_state_in_it() {
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    let server = server_with(&claude_in(tmp.path(), &runs, ""));
    let (pane, _) = a_kept_pane(&server, tmp.path(), &runs);
    let here = server.display(&pane, "#{pane_current_path}");

    let (code, reported) = server.taw(&[
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
        "1",
    ]);
    assert_eq!(code, 0, "{reported}");
    autosave(&server);

    assert_eq!(
        saved_panes(&server, "work"),
        [(here, Value::from("prime-agent"), Value::Null)]
    );
    assert_eq!(record_of(&server, &pane)["restore_error"], Value::Null);
    assert_eq!(runs_of(&runs), Vec::<Vec<String>>::new(), "claude ran");
}

/// A pane kept for a gone directory keeps what it was saved as while nothing names an agent in it:
/// its shell going somewhere else, and a program that is no agent, are not an agent of its own.
#[test]
fn a_kept_pane_keeps_what_it_was_saved_as_while_no_agent_is_recorded_in_it() {
    let tmp = tempfile::tempdir().unwrap();
    let runs = tmp.path().join("runs");
    claude_in(tmp.path(), &runs, "");
    let server = server_with(&named_agent(tmp.path(), "sleeper", None));
    let (pane, gone) = a_kept_pane(&server, tmp.path(), &runs);
    let there = elsewhere(tmp.path());
    let _evidence = EvidenceOnFailure {
        server: &server,
        pane: pane.clone(),
        stderr: watch_keeping_stderr(&server, &pane, tmp.path()),
    };

    cd_in(&server, &pane, &there);
    run_in(&server, &pane, "sleeper", "sleeper");
    // A pane with no agent is read only after output, and the fake sleeper draws nothing: counted from
    // here, with sleeper in front, the read one echoed key earns is a read of sleeper, and not of the
    // shell before it.
    let reads = server.watcher_reads(&pane);
    server.tmux_ok(&["send-keys", "-t", &pane, "x"]);
    assert!(
        wait_for(|| server.watcher_reads(&pane) > reads, SETTLE),
        "the watcher did not read the pane"
    );
    // Reads come in a burst after output: the last of them has decided by now.
    std::thread::sleep(Duration::from_millis(1500));
    autosave(&server);

    assert_eq!(
        saved_panes(&server, "work"),
        [(gone, Value::from("claude"), Value::from("conv-gone"))]
    );
    assert_eq!(record_of(&server, &pane)["restore_error"], UNAVAILABLE);
    assert_eq!(runs_of(&runs), Vec::<Vec<String>>::new(), "claude ran");
}
