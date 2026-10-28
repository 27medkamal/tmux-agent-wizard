//! A pane whose agent reports for itself is released when its shell comes back, end to end
//! through the built `taw` binary and a real `taw watch run` on a throwaway tmux server.
//!
//! Such a pane (OpenCode, kimi, pi, omp, kilo, mastracode, and any agent whose hook calls
//! `taw pane report-agent`) has its state written by the agent itself, and the watcher backs
//! off it: whatever is in front of the agent is no evidence about it. That left one thing
//! that ended the record, `taw pane release-agent`, so an agent that quit kept the last
//! state it had reported, and its session id, for as long as the pane lived. herdr ends it
//! when the pane's shell is back at its prompt (`ReportedAgentShellReturned`), and so does
//! the watcher.
//!
//! The fake agent is a program run by `python3`, so the pane's command is `python3` and
//! neither an agent's nor a shell's. What it reports the hook's way is what the tests call
//! `taw pane report-agent` with, from outside the pane, as a hook does from inside it.
//!
//! Four panes: an agent that ends under the pane's own shell (released); one run by a shell
//! script, which tmux names a shell for the whole run though it is not the pane's, so the
//! agent behind it must not be released (kept, and released when it does end), whether its
//! hook reports its state or only names it; and one that ends under a shell taw does not know
//! by name (kept, which is the limit of the list of shells).

mod common;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::{Server, ServerOptions, make_executable, read_pid, wait_for, write_script};

/// Ceiling for "the watcher noticed and wrote the new state", well above what an idle machine
/// needs because `cargo test` runs these files in parallel.
const SETTLE: Duration = Duration::from_secs(20);

/// OpenCode's session ids: `ses_`, twelve hexadecimal digits, and fourteen letters and digits.
const OPENCODE_SESSION: &str = "ses_3e1a04b21ffeQ7mZk2VtXc9LwH";

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// The fake agent. Its only argument is where it records its pid. It prints a line on each
/// SIGUSR1, which is the output that makes the watcher read the pane, and says so when it
/// ends.
const FAKE_AGENT: &str = r#"#!/usr/bin/env python3
import os
import signal
import sys
import time

ticks = 0


def draw(*_):
    global ticks
    ticks += 1
    sys.stdout.write("fake-agent: tick %d\n" % ticks)
    sys.stdout.flush()


def leave(*_):
    sys.stdout.write("fake-agent: bye\n")
    sys.stdout.flush()
    sys.exit(0)


signal.signal(signal.SIGUSR1, draw)
signal.signal(signal.SIGTERM, leave)
signal.signal(signal.SIGINT, leave)
draw()
with open(sys.argv[1], "w") as handle:
    handle.write(str(os.getpid()))
while True:
    time.sleep(0.2)
"#;

/// A shell taw's list of shells does not name: it prompts, runs a line, and prompts again. It
/// ignores ^C itself, as a shell does, and what it runs inherits nothing else from it.
const ODD_SHELL: &str = r#"#!/usr/bin/env python3
import signal
import subprocess
import sys

signal.signal(signal.SIGINT, signal.SIG_IGN)
while True:
    sys.stdout.write("odd$ ")
    sys.stdout.flush()
    line = sys.stdin.readline()
    if not line:
        break
    line = line.strip()
    if line:
        subprocess.run(line, shell=True)
"#;

fn have(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn server() -> Server {
    let server = Server::start_with(ServerOptions {
        width: 100,
        height: 30,
        env: vec![("TAW_LOG".to_string(), "debug".to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    server
}

fn write_program(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    make_executable(path);
}

/// Types `line` into `pane` and presses Enter. It does not wait for a prompt: a caller that types into a
/// fresh shell calls [`Server::wait_for_prompt`] first, and one that types into the odd shell has waited for
/// its own prompt.
fn type_line(server: &Server, pane: &str, line: &str) {
    server.tmux_ok(&["send-keys", "-t", pane, line, "Enter"]);
}

/// Waits for the fake agent that `pane` runs to have started, and returns its pid.
fn agent_started(server: &Server, pane: &str, pid_file: &Path) -> String {
    assert!(
        wait_for(
            || read_pid(pid_file).is_some() && server.capture(pane).contains("fake-agent: tick"),
            SETTLE
        ),
        "the fake agent never started:\n{}",
        server.capture(pane)
    );
    read_pid(pid_file).unwrap()
}

/// What its hook does when the agent starts a turn: `taw pane report-agent`, which records the
/// state, the agent, the session, and `hook:kimi` as the authority over the pane.
fn report_working(server: &Server, pane: &str) {
    server.ok(
        &[
            "pane",
            "report-agent",
            pane,
            "--source",
            "kimi",
            "--agent",
            "kimi",
            "--state",
            "working",
            "--agent-session-id",
            "sess-1",
        ],
        "cli:pane:report-agent",
        "ok",
    );
    assert_eq!(server.pane_opt(pane, "@agent_source"), "hook:kimi");
}

/// What a hook does that only names the agent and its session (`report-agent-session`): no
/// state, no source, and the agent's word that it is there.
fn report_session(server: &Server, pane: &str) {
    server.ok(
        &[
            "pane",
            "report-agent-session",
            pane,
            "--source",
            "letta",
            "--agent",
            "letta",
            "--agent-session-id",
            "conv-1",
        ],
        "cli:pane:report-agent-session",
        "ok",
    );
    assert_eq!(server.pane_opt(pane, "@agent_reported_kind"), "letta");
}

/// The record the hook wrote: state, kind, source and session id, in that order.
fn record(server: &Server, pane: &str) -> [String; 4] {
    [
        "@agent_state",
        "@agent_kind",
        "@agent_source",
        "@agent_session_id",
    ]
    .map(|name| server.pane_opt(pane, name))
}

fn reported() -> [String; 4] {
    [
        "working".to_string(),
        "kimi".to_string(),
        "hook:kimi".to_string(),
        "sess-1".to_string(),
    ]
}

fn released() -> [String; 4] {
    [String::new(), String::new(), String::new(), String::new()]
}

/// Has the watcher read `pane` `more` times, making `nudge` print something between reads:
/// what a running agent does, and what lets the watcher decide anything.
fn read_again(server: &Server, pane: &str, more: usize, nudge: impl Fn()) {
    let before = server.watcher_reads(pane);
    let done = wait_for(
        || {
            nudge();
            std::thread::sleep(Duration::from_millis(250));
            server.watcher_reads(pane) >= before + more
        },
        SETTLE,
    );
    assert!(
        done,
        "the watcher did not read the pane {more} more times\n{}",
        server.watcher_logs()
    );
}

fn signal(pid: &str, sig: &str) {
    let status = Command::new("kill").args([sig, pid]).status().unwrap();
    assert!(status.success(), "kill {sig} {pid} failed");
}

fn signal_agent(pid: &str) {
    signal(pid, "-USR1");
}

/// The record went: the shell that came back ended it.
fn wait_until_released(server: &Server, pane: &str, why: &str) {
    assert!(
        wait_for(|| record(server, pane) == released(), SETTLE),
        "{why}: the record is still {:?}\n{}",
        record(server, pane),
        server.watcher_logs()
    );
}

#[test]
fn a_hook_reported_agent_is_released_when_its_shell_comes_back() {
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fake = tmp.path().join("fake-agent");
    write_program(&fake, FAKE_AGENT);
    let pid_file = tmp.path().join("agent.pid");
    let server = server();
    let pane = server.pane();

    // The pane's fresh shell: typed before it reads its terminal, the line is cut at the line buffer.
    server.wait_for_prompt(&pane);
    type_line(
        &server,
        &pane,
        &format!("'{}' '{}'", fake.display(), pid_file.display()),
    );
    let pid = agent_started(&server, &pane, &pid_file);
    let command = server.display(&pane, "#{pane_current_command}");
    assert!(
        !taw::watch::state::SHELLS.contains(&command.trim_start_matches('-')),
        "the pane's command must not be a shell while the agent runs, it is {command:?}"
    );
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    report_working(&server, &pane);

    // The watcher goes on reading a pane whose agent reports for itself, and writes nothing
    // over what it reported.
    read_again(&server, &pane, 3, || signal_agent(&pid));
    assert_eq!(record(&server, &pane), reported());

    // The agent quits: ^C ends it, and the pane's shell has the terminal again.
    server.tmux_ok(&["send-keys", "-t", &pane, "C-c"]);
    wait_until_released(&server, &pane, "the shell is back");
    // The turn it was in the middle of was ended before the record went, as for any exit.
    let logs = server.watcher_logs();
    assert!(
        logs.contains("process_exited"),
        "no end written for the turn the agent was in\n{logs}"
    );
}

#[test]
fn a_hook_reported_agent_run_by_a_shell_script_is_not_released_while_it_runs() {
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fake = tmp.path().join("fake-agent");
    write_program(&fake, FAKE_AGENT);
    let pid_file = tmp.path().join("agent.pid");
    // Not `exec`: the script stays the leader of the process group the agent runs in, so tmux
    // names the script's interpreter, a shell, as the pane's command for as long as it runs.
    let wrapper = tmp.path().join("wrapped-agent");
    write_script(
        &wrapper,
        &format!("python3 '{}' '{}'\n", fake.display(), pid_file.display()),
    );
    let server = server();
    let pane = server.pane();

    // The pane's fresh shell: typed before it reads its terminal, the line is cut at the line buffer.
    server.wait_for_prompt(&pane);
    type_line(&server, &pane, &format!("'{}'", wrapper.display()));
    let pid = agent_started(&server, &pane, &pid_file);
    let command = server.display(&pane, "#{pane_current_command}");
    assert!(
        taw::watch::state::SHELLS.contains(&command.trim_start_matches('-')),
        "the wrapper should read as a shell, it is {command:?}"
    );
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    report_working(&server, &pane);

    // A shell in front, and the agent behind it still running: not the pane's shell back.
    read_again(&server, &pane, 4, || signal_agent(&pid));
    assert_eq!(
        record(&server, &pane),
        reported(),
        "released while the agent was running\n{}",
        server.watcher_logs()
    );

    // Its end is the pane's shell coming back, and that releases it.
    server.tmux_ok(&["send-keys", "-t", &pane, "C-c"]);
    wait_until_released(&server, &pane, "the script and the agent ended");
}

#[test]
fn an_agent_that_only_named_itself_run_by_a_shell_script_keeps_its_session_id() {
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fake = tmp.path().join("fake-agent");
    write_program(&fake, FAKE_AGENT);
    let pid_file = tmp.path().join("agent.pid");
    let wrapper = tmp.path().join("wrapped-agent");
    write_script(
        &wrapper,
        &format!("python3 '{}' '{}'\n", fake.display(), pid_file.display()),
    );
    let server = server();
    let pane = server.pane();

    // The pane's fresh shell: typed before it reads its terminal, the line is cut at the line buffer.
    server.wait_for_prompt(&pane);
    type_line(&server, &pane, &format!("'{}'", wrapper.display()));
    let pid = agent_started(&server, &pane, &pid_file);
    let command = server.display(&pane, "#{pane_current_command}");
    assert!(
        taw::watch::state::SHELLS.contains(&command.trim_start_matches('-')),
        "the wrapper should read as a shell, it is {command:?}"
    );
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    report_session(&server, &pane);

    read_again(&server, &pane, 4, || signal_agent(&pid));
    let kept = ["@agent_kind", "@agent_session_id"].map(|name| server.pane_opt(&pane, name));
    assert_eq!(
        kept,
        ["letta", "conv-1"],
        "cleared while the agent was running\n{}",
        server.watcher_logs()
    );

    server.tmux_ok(&["send-keys", "-t", &pane, "C-c"]);
    assert!(
        wait_for(
            || ["@agent_kind", "@agent_session_id", "@agent_reported_kind"]
                .iter()
                .all(|name| server.pane_opt(&pane, name).is_empty()),
            SETTLE
        ),
        "the shell came back and the record stayed\n{}",
        server.watcher_logs()
    );
}

/// What OpenCode's plugin does when a turn starts: `taw pane report-agent`, with the shape of
/// session id OpenCode gives its sessions.
fn report_opencode_working(server: &Server, pane: &str) {
    server.ok(
        &[
            "pane",
            "report-agent",
            pane,
            "--source",
            "integration:opencode",
            "--agent",
            "opencode",
            "--state",
            "working",
            "--agent-session-id",
            OPENCODE_SESSION,
        ],
        "cli:pane:report-agent",
        "ok",
    );
    assert_eq!(
        server.pane_opt(pane, "@agent_source"),
        "hook:integration:opencode"
    );
}

/// The record `report_opencode_working` leaves: state, kind, source and session id.
fn reported_opencode() -> [String; 4] {
    [
        "working".to_string(),
        "opencode".to_string(),
        "hook:integration:opencode".to_string(),
        OPENCODE_SESSION.to_string(),
    ]
}

/// The first shell on this machine out of the ones a test can start with no configuration of
/// its own to read.
fn quiet_shell() -> &'static str {
    if have("zsh") {
        "zsh -f"
    } else if have("bash") {
        "bash --norc --noprofile"
    } else {
        "sh"
    }
}

#[test]
fn an_agent_run_by_the_panes_own_wrapper_is_kept_while_it_runs_and_released_when_the_shell_takes_over()
 {
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fake = tmp.path().join("fake-agent");
    write_program(&fake, FAKE_AGENT);
    let pid_file = tmp.path().join("agent.pid");
    let server = server();
    // The pane's own process is `sh -c '<the agent>; exec <a shell>'`, which is how a launcher
    // that starts a pane for an agent is written. The wrapper and the agent share one process
    // group, so tmux names the wrapper's shell as the pane's command for as long as the agent
    // runs, and the terminal's foreground group has two processes in it. The wrapper's `exec`
    // then makes the very same process a shell at its prompt.
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "wrapped",
        "-x",
        "100",
        "-y",
        "30",
        "-c",
        server.work_dir().to_str().unwrap(),
        "sh",
        "-c",
        &format!(
            "python3 '{}' '{}'; exec {}",
            fake.display(),
            pid_file.display(),
            quiet_shell()
        ),
    ]);
    let pane = server.pane_of("wrapped");
    let pid = agent_started(&server, &pane, &pid_file);
    let command = server.display(&pane, "#{pane_current_command}");
    assert!(
        taw::watch::state::SHELLS.contains(&command.trim_start_matches('-')),
        "the wrapper should read as a shell, it is {command:?}"
    );
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    // A turn starts three times over, each time the hook reporting it, and the watcher reads
    // the pane in between: the agent is still running, so nothing may end its record, and
    // above all no `done` for a turn that has not finished.
    for turn in 1..=3 {
        report_opencode_working(&server, &pane);
        read_again(&server, &pane, 3, || signal_agent(&pid));
        assert_eq!(
            record(&server, &pane),
            reported_opencode(),
            "released while the agent was running, at turn {turn}\n{}",
            server.watcher_logs()
        );
    }
    let logs = server.watcher_logs();
    assert!(
        !logs.contains("process_exited") && !logs.contains("-> done"),
        "a turn that was still running was ended\n{logs}"
    );

    // The agent quits, the wrapper's `exec` makes the pane's process a shell, and it prompts:
    // now the pane's own shell is idle, and the record goes, the turn's end first.
    signal(&pid, "-TERM");
    wait_until_released(&server, &pane, "the shell took over and prompted");
    let logs = server.watcher_logs();
    assert!(
        logs.contains("process_exited"),
        "no end written for the turn the agent was in\n{logs}"
    );
}

#[test]
fn a_hook_reported_agent_stays_when_the_shell_it_ends_under_is_not_one_taw_names() {
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fake = tmp.path().join("fake-agent");
    write_program(&fake, FAKE_AGENT);
    let odd = tmp.path().join("odd-shell");
    write_program(&odd, ODD_SHELL);
    let pid_file = tmp.path().join("agent.pid");
    let server = server();
    // The pane's own shell is one that no name in `SHELLS` matches.
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "odd",
        "-x",
        "100",
        "-y",
        "30",
        "-c",
        server.work_dir().to_str().unwrap(),
        &format!("exec python3 '{}'", odd.display()),
    ]);
    let pane = server.pane_of("odd");
    assert!(
        wait_for(|| server.capture(&pane).contains("odd$"), SETTLE),
        "the odd shell never prompted:\n{}",
        server.capture(&pane)
    );
    let command = server.display(&pane, "#{pane_current_command}");
    assert!(
        !taw::watch::state::SHELLS.contains(&command.trim_start_matches('-')),
        "the odd shell must not be one taw names, it is {command:?}"
    );

    type_line(
        &server,
        &pane,
        &format!("'{}' '{}'", fake.display(), pid_file.display()),
    );
    agent_started(&server, &pane, &pid_file);
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    report_working(&server, &pane);

    // The agent quits, and the shell it started under is at its prompt again.
    server.tmux_ok(&["send-keys", "-t", &pane, "C-c"]);
    assert!(
        wait_for(
            || {
                let screen = server.capture(&pane);
                screen.contains("fake-agent: bye") && screen.trim_end().ends_with("odd$")
            },
            SETTLE
        ),
        "the agent did not end under the odd shell:\n{}",
        server.capture(&pane)
    );

    // Each Enter is a fresh prompt, which is what makes the watcher read the pane.
    read_again(&server, &pane, 4, || type_line(&server, &pane, ""));
    assert_eq!(
        record(&server, &pane),
        reported(),
        "a shell that is not on the list ended the record\n{}",
        server.watcher_logs()
    );
    // What ends it is what always could: the hook's own release.
    server.ok(
        &[
            "pane",
            "release-agent",
            &pane,
            "--source",
            "kimi",
            "--agent",
            "kimi",
        ],
        "cli:pane:release-agent",
        "ok",
    );
    assert_eq!(server.pane_opt(&pane, "@agent_source"), "");
}
