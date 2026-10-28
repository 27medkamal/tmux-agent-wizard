//! A Claude pane comes back with the flags it was started with, end to end through the built `taw`
//! binary on a private socket.
//!
//! The fake `claude` is a script that stays up as a child of the pane's shell, so the job in front of
//! the pane holds a process whose exact arguments are Claude's: it is started with `--effort max
//! --model x 'a prompt'`, and the stand-in hook reports its session as Claude's hook does
//! (`taw pane report-agent-session`). The pane is autosaved, the server is killed, a new one is
//! started, and a plain restore types the profile, the command, the kept flags and the resume. The
//! fake records its arguments and `CLAUDE_CONFIG_DIR` into a file under the test's temp folder.

mod common;

use std::path::Path;
use std::time::Duration;

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

const SESSION: &str = "3f0c2a9e-7d41-4b6e-9c55-2b1e8a6d0f42";

/// The fake `claude`, which writes what it got to `<records>/<id>` (`first` when it is not resumed)
/// and stays up as a child of its shell.
fn put_fake_claude(dir: &Path, records: &Path) {
    std::fs::create_dir_all(records).unwrap();
    write_script(
        &dir.join("bin").join("claude"),
        &format!(
            "id=first; prev=\n\
             for a in \"$@\"; do [ \"$prev\" = --resume ] && id=$a; prev=$a; done\n\
             {{ printf 'ARGS=%s\\n' \"$*\"; printf 'CONFIG=%s\\n' \"${{CLAUDE_CONFIG_DIR-unset}}\"; }} > '{}'/\"$id\"\n\
             cat\n",
            records.display()
        ),
    );
}

/// Panes of this server find the fake first, and shells read no startup file.
fn find_the_fake(server: &Server, dir: &Path) {
    server.tmux_ok(&[
        "set-option",
        "-g",
        "default-command",
        &format!(
            "export PATH='{}:/usr/bin:/bin'; exec /bin/sh",
            dir.join("bin").display()
        ),
    ]);
}

fn new_session(server: &Server, name: &str) {
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        name,
        "-x",
        "100",
        "-y",
        "30",
        "-c",
        server.work_dir().to_str().unwrap(),
    ]);
}

fn default_json(server: &Server) -> std::path::PathBuf {
    server.state_dir().join("sessions").join("default.json")
}

/// The saved pane of session `name`, window 0, pane 0, as JSON.
fn saved_pane(server: &Server, name: &str) -> Value {
    let snapshot: Value =
        serde_json::from_str(&std::fs::read_to_string(default_json(server)).unwrap()).unwrap();
    snapshot["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("{snapshot}"))["windows"][0]["panes"][0]
        .clone()
}

fn joined(server: &Server, pane: &str) -> String {
    server.tmux_ok(&["capture-pane", "-p", "-J", "-t", pane])
}

fn read_record(records: &Path, id: &str) -> Option<String> {
    std::fs::read_to_string(records.join(id))
        .ok()
        .filter(|text| text.contains("CONFIG="))
}

/// What Claude's hook does when a session starts.
fn report_session(server: &Server, pane: &str, transcript: &Path) {
    server.ok(
        &[
            "pane",
            "report-agent-session",
            pane,
            "--source",
            "integration:claude",
            "--agent",
            "claude",
            "--seq",
            &std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                .to_string(),
            "--agent-session-id",
            SESSION,
            "--agent-session-path",
            &transcript.display().to_string(),
            "--session-start-source",
            "startup",
        ],
        "cli:pane:report-agent-session",
        "ok",
    );
}

fn autosave(server: &Server) {
    let (code, env) = server.taw(&["session", "autosave"]);
    assert_eq!(code, 0, "{env}");
    assert_eq!(env["result"]["saved"], true, "{env}");
}

/// The server dies, and a new one starts on the same socket two seconds later (the fixture pauses: a
/// snapshot's `taken_unix` and tmux's `#{start_time}` are whole seconds), with one session, `fresh`.
fn crash_and_start_afresh(server: &mut Server, dir: &Path) {
    server.crash_and_start_afresh(&ServerOptions {
        session: "fresh".to_string(),
        width: 100,
        height: 30,
        ..ServerOptions::default()
    });
    find_the_fake(server, dir);
}

#[test]
fn a_claude_pane_comes_back_with_the_flags_it_was_started_with() {
    let tmp = tempfile::tempdir().unwrap();
    let records = tmp.path().join("records");
    put_fake_claude(tmp.path(), &records);
    let mut server = Server::start();
    server.write_config(CONFIG);
    find_the_fake(&server, tmp.path());
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "100",
        "-y",
        "30",
        "/bin/sh",
    ]);
    let team = tmp.path().join(".claude-home");
    let transcript = team.join(format!("projects/-work-repo/{SESSION}.jsonl"));

    // Claude started with flags and a prompt, in a pane that is autosaved once its hook reported.
    new_session(&server, "work");
    let pane = server.pane_of("work");
    server.tmux_ok(&[
        "send-keys",
        "-t",
        &pane,
        "claude --effort max --model x 'a prompt'",
        "Enter",
    ]);
    assert!(
        wait_for(|| read_record(&records, "first").is_some(), SETTLE),
        "the fake did not start:\n{}",
        joined(&server, &pane)
    );
    report_session(&server, &pane, &transcript);
    assert_eq!(
        server.pane_opt(&pane, "@agent_claude_flags"),
        r#"["--effort","max","--model","x"]"#,
        "the flags are kept from the process, and the prompt is not"
    );
    autosave(&server);
    let saved = saved_pane(&server, "work");
    assert_eq!(
        saved["agent_claude_flags"],
        serde_json::json!(["--effort", "max", "--model", "x"]),
        "{saved}"
    );
    let on_disk = std::fs::read_to_string(default_json(&server)).unwrap();
    assert!(!on_disk.contains("a prompt"), "the prompt was saved");

    // The tmux server dies and a new one starts; a plain restore types the line.
    crash_and_start_afresh(&mut server, tmp.path());
    let line = format!(
        "CLAUDE_CONFIG_DIR={} claude --effort max --model x --resume {SESSION}",
        team.display()
    );
    let dry = server.ok(
        &["session", "restore", "--dry-run"],
        "cli:session:restore",
        "session_restored",
    );
    assert!(
        dry["would_type"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["line"] == line.as_str()),
        "{dry}"
    );
    let restored = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    assert_eq!(
        restored["resumed"].as_array().unwrap().len(),
        1,
        "{restored}"
    );
    assert!(
        wait_for(|| read_record(&records, SESSION).is_some(), SETTLE),
        "the fake was not resumed:\n{}",
        joined(&server, &server.pane_of("work"))
    );
    assert_eq!(
        read_record(&records, SESSION).unwrap(),
        format!(
            "ARGS=--effort max --model x --resume {SESSION}\nCONFIG={}\n",
            team.display()
        )
    );
    let pane = server.pane_of("work");
    assert!(
        joined(&server, &pane).contains(&line),
        "{}",
        joined(&server, &pane)
    );

    // A save before its hook has reported again keeps the flags: the restore put them on the pane.
    assert!(
        wait_for(
            || !server.pane_opt(&pane, "@agent_claude_flags").is_empty(),
            SETTLE
        ),
        "the restore did not put the flags back"
    );
    autosave_after_aging(&server);
    let saved = saved_pane(&server, "work");
    assert_eq!(
        saved["agent_claude_flags"],
        serde_json::json!(["--effort", "max", "--model", "x"])
    );
    assert_eq!(
        saved["agent_session_path"],
        serde_json::json!(transcript.display().to_string()),
        "the restore dropped the transcript path, which is all that carries the profile: {saved}"
    );

    // A second crash before the hook has reported again: the fresh server's autosave holds the same
    // pane, and a plain restore types the same line, profile and flags included.
    crash_and_start_afresh(&mut server, tmp.path());
    let dry = server.ok(
        &["session", "restore", "--dry-run"],
        "cli:session:restore",
        "session_restored",
    );
    assert!(
        dry["would_type"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["line"] == line.as_str()),
        "a second crash lost the profile or the flags: {dry}"
    );
    std::fs::remove_file(records.join(SESSION)).unwrap();
    server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    assert!(
        wait_for(|| read_record(&records, SESSION).is_some(), SETTLE),
        "the fake was not resumed the second time:\n{}",
        joined(&server, &server.pane_of("work"))
    );
    assert_eq!(
        read_record(&records, SESSION).unwrap(),
        format!(
            "ARGS=--effort max --model x --resume {SESSION}\nCONFIG={}\n",
            team.display()
        )
    );
    let pane = server.pane_of("work");
    assert!(
        wait_for(
            || !server.pane_opt(&pane, "@agent_claude_flags").is_empty(),
            SETTLE
        ),
        "the second restore did not put the flags back"
    );

    // Its hook reports the resumed session: the flags are read again off the process, whose arguments
    // hold `--resume <id>` and the flags, and the next save keeps the flags and not the resume.
    server.tmux_ok(&["set-option", "-p", "-u", "-t", &pane, "@agent_claude_flags"]);
    report_session(&server, &pane, &transcript);
    assert_eq!(
        server.pane_opt(&pane, "@agent_claude_flags"),
        r#"["--effort","max","--model","x"]"#
    );
    autosave_after_aging(&server);
    let saved = saved_pane(&server, "work");
    assert_eq!(
        saved["agent_claude_flags"],
        serde_json::json!(["--effort", "max", "--model", "x"]),
        "{saved}"
    );
    assert!(
        !saved["agent_claude_flags"].to_string().contains("resume"),
        "{saved}"
    );
}

/// An effort set inside the session is on no command line: the pane was started with `--effort max`,
/// and the last turn of its transcript ran at `high` (a record whose per-turn field is `null`, after
/// earlier ones at `max`, so that the level comes from its `effort`), so that is the level the restore
/// types, once, in the line a dry run shows and in the arguments the fake is resumed with.
#[test]
fn a_claude_pane_comes_back_at_the_effort_its_last_turn_ran_at() {
    let tmp = tempfile::tempdir().unwrap();
    let records = tmp.path().join("records");
    put_fake_claude(tmp.path(), &records);
    let mut server = Server::start();
    server.write_config(CONFIG);
    find_the_fake(&server, tmp.path());
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "keepalive",
        "-x",
        "100",
        "-y",
        "30",
        "/bin/sh",
    ]);
    let team = tmp.path().join(".claude-home");
    let transcript = team.join(format!("projects/-work-repo/{SESSION}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    // Earlier turns carry the level twice; the last is the form of a session whose per-turn field is
    // off, `null`, so that the level is read from the `effort` before it.
    let turn = |effort: &str, per_turn: &str| {
        format!(
            "{{\"type\":\"assistant\",\"effort\":\"{effort}\",\"perTurnEffort\":{per_turn},\"session_id\":\"{SESSION}\"}}\n\
             {{\"type\":\"api-request-blob\",\"output_config\":{{\"effort\":\"low\"}}}}\n"
        )
    };
    std::fs::write(
        &transcript,
        format!(
            "{}{}{}",
            turn("max", "\"max\""),
            turn("max", "\"max\""),
            turn("high", "null")
        ),
    )
    .unwrap();

    new_session(&server, "work");
    let pane = server.pane_of("work");
    server.tmux_ok(&[
        "send-keys",
        "-t",
        &pane,
        "claude --effort max --model x",
        "Enter",
    ]);
    assert!(
        wait_for(|| read_record(&records, "first").is_some(), SETTLE),
        "the fake did not start:\n{}",
        joined(&server, &pane)
    );
    report_session(&server, &pane, &transcript);
    autosave(&server);
    // What is saved is what Claude was started with.
    let saved = saved_pane(&server, "work");
    assert_eq!(
        saved["agent_claude_flags"],
        serde_json::json!(["--effort", "max", "--model", "x"]),
        "{saved}"
    );

    crash_and_start_afresh(&mut server, tmp.path());
    let line = format!(
        "CLAUDE_CONFIG_DIR={} claude --effort high --model x --resume {SESSION}",
        team.display()
    );
    let dry = server.ok(
        &["session", "restore", "--dry-run"],
        "cli:session:restore",
        "session_restored",
    );
    assert!(
        dry["would_type"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["line"] == line.as_str()),
        "{dry}"
    );
    server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    assert!(
        wait_for(|| read_record(&records, SESSION).is_some(), SETTLE),
        "the fake was not resumed:\n{}",
        joined(&server, &server.pane_of("work"))
    );
    let record = read_record(&records, SESSION).unwrap();
    assert_eq!(
        record,
        format!(
            "ARGS=--effort high --model x --resume {SESSION}\nCONFIG={}\n",
            team.display()
        )
    );
    assert_eq!(record.matches("--effort").count(), 1, "{record}");
    assert!(!record.contains("--effort max"), "{record}");
}

/// An autosave that is due: the last one is older than the debounce.
fn autosave_after_aging(server: &Server) {
    std::fs::File::options()
        .write(true)
        .open(default_json(server))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(120))
        .unwrap();
    autosave(server);
}

/// The stand-in for Claude's program as it is on a person's machine, which the first test's script under
/// `/bin/sh` is not: a native program named `claude`, so that tmux and `ps` name it so, which ignores its
/// arguments (they are Claude's: `--effort max --model x`), runs `$STANDIN_HOOK` once as a child with no
/// input and no output (in a process group of its own when `$STANDIN_OWN_GROUP` is set), waits for it,
/// writes `$STANDIN_DONE`, and stays up. A login zsh that runs it makes it the leader of its own group
/// in front of the terminal, as it is for Claude.
const STAND_IN: &str = r#"
#include <fcntl.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>
int main(void) {
    pid_t child = fork();
    if (child == 0) {
        if (getenv("STANDIN_OWN_GROUP")) setpgid(0, 0);
        int null = open("/dev/null", O_RDWR);
        dup2(null, 0); dup2(null, 1); dup2(null, 2);
        execl("/bin/sh", "sh", getenv("STANDIN_HOOK"), (char *)0);
        _exit(127);
    }
    int status;
    waitpid(child, &status, 0);
    close(creat(getenv("STANDIN_DONE"), 0644));
    for (;;) pause();
}
"#;

/// Builds [`STAND_IN`] as `<dir>/native/claude` with the C compiler every Rust toolchain needs as its linker.
fn build_the_stand_in(dir: &Path) -> std::path::PathBuf {
    let source = dir.join("stand-in.c");
    std::fs::write(&source, STAND_IN).unwrap();
    let claude = dir.join("native").join("claude");
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    let built = std::process::Command::new("cc")
        .arg(&source)
        .arg("-o")
        .arg(&claude)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    claude
}

/// Types the stand-in's line at the prompt of `pane`, with a hook that reports the session as Claude's
/// hook does (`taw pane report-agent-session`, from inside the pane).
fn start_a_claude_that_runs_its_hook(
    server: &Server,
    dir: &Path,
    pane: &str,
    transcript: &Path,
    own_group: bool,
) {
    let claude = build_the_stand_in(dir);
    let hook = dir.join("hook.sh");
    std::fs::write(
        &hook,
        format!(
            "'{taw}' pane report-agent-session \"$TMUX_PANE\" --source integration:claude --agent claude \\\n \
             --seq 1 --agent-session-id {SESSION} --agent-session-path '{}' --session-start-source startup\n",
            transcript.display(),
            taw = common::taw_bin()
        ),
    )
    .unwrap();
    let group = if own_group {
        "STANDIN_OWN_GROUP=1 "
    } else {
        ""
    };
    server.tmux_ok(&[
        "send-keys",
        "-t",
        pane,
        &format!(
            "{group}STANDIN_HOOK='{}' STANDIN_DONE='{}' '{}' --effort max --model x",
            hook.display(),
            dir.join("hooked").display(),
            claude.display()
        ),
        "Enter",
    ]);
}

/// The login zsh a person's panes are, in a private home with no startup files.
fn use_a_login_zsh(server: &Server, dir: &Path) {
    let home = dir.join("zhome");
    std::fs::create_dir_all(&home).unwrap();
    server.tmux_ok(&[
        "set-option",
        "-g",
        "default-command",
        &format!(
            "exec env HOME='{h}' ZDOTDIR='{h}' HISTFILE=/dev/null /bin/zsh -l -i",
            h = home.display()
        ),
    ]);
}

fn the_flags_of_a_claude_whose_hook_reports_in_the_pane(own_group: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    use_a_login_zsh(&server, tmp.path());
    new_session(&server, "work");
    let pane = server.pane_of("work");
    let transcript = tmp
        .path()
        .join(format!("projects/-work-repo/{SESSION}.jsonl"));
    assert!(
        wait_for(|| !joined(&server, &pane).trim().is_empty(), SETTLE),
        "the shell drew nothing"
    );
    start_a_claude_that_runs_its_hook(&server, tmp.path(), &pane, &transcript, own_group);
    assert!(
        wait_for(|| tmp.path().join("hooked").exists(), SETTLE),
        "the hook did not finish:\n{}",
        joined(&server, &pane)
    );
    assert_eq!(server.pane_opt(&pane, "@agent_session_id"), SESSION);
    assert_eq!(
        server.pane_opt(&pane, "@agent_claude_flags"),
        r#"["--effort","max","--model","x"]"#,
        "{}",
        joined(&server, &pane)
    );
}

#[test]
fn a_hook_in_a_group_of_its_own_reports_the_flags_of_the_claude_that_ran_it() {
    the_flags_of_a_claude_whose_hook_reports_in_the_pane(true);
}

#[test]
fn a_hook_in_claudes_group_reports_the_flags_of_the_claude_that_ran_it() {
    the_flags_of_a_claude_whose_hook_reports_in_the_pane(false);
}
