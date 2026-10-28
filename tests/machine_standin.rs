//! The stand-in for ssh (`tests/common/machine.rs`): it runs the far side's `taw` where ssh would log
//! in, records what it was run with, and fails as ssh does when it is told to. No real ssh runs here.

mod common;

use std::process::Stdio;

use common::machine::{Failure, FarSide, StandIn, far_path};
use common::{Server, ServerOptions};
use serde_json::Value;

/// Runs the stand-in as `taw` would, with `args` after its own name, and gives (status, stdout, stderr).
fn run(stand_in: &StandIn, args: &[&str]) -> (i32, String, String) {
    let out = stand_in
        .command()
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn session_names(envelope: &Value) -> Vec<String> {
    envelope["result"]["workspaces"]
        .as_array()
        .map(|all| {
            all.iter()
                .filter_map(|w| w["label"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_command_runs_taw_on_the_far_side_and_shows_its_sessions() {
    let here = Server::start_with(ServerOptions {
        session: "here".to_string(),
        ..ServerOptions::default()
    });
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);

    let (code, stdout, stderr) = run(
        &stand_in,
        &[
            "-o",
            "BatchMode=yes",
            "studio",
            "sh -c 'taw workspace list' taw",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let envelope: Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    let names = session_names(&envelope);
    assert!(
        names.iter().any(|n| n == "garden"),
        "far side shown: {stdout}"
    );
    assert!(
        !names.iter().any(|n| n == "here"),
        "this side leaked: {stdout}"
    );

    // This side is the other server: it still shows its own.
    let (_, ours) = here.taw(&["workspace", "list"]);
    assert!(session_names(&ours).iter().any(|n| n == "here"));
}

#[test]
fn the_far_side_runs_in_its_own_environment_and_cannot_reach_a_real_ssh() {
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    let (code, stdout, stderr) = run(
        &stand_in,
        &["studio", "echo \"$TAW_SSH_BIN\"; command -v taw"],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let mut lines = stdout.lines();
    let ssh = lines.next().unwrap_or("");
    assert!(!ssh.is_empty(), "the far side has a TAW_SSH_BIN");
    let log = std::fs::read_to_string(ssh).unwrap_or_default();
    assert!(
        log.contains("refused by the test seam"),
        "the far side's ssh is the refusing recorder: {ssh}"
    );
    assert!(lines.next().unwrap_or("").ends_with("/taw"));
}

#[test]
fn the_far_side_does_not_see_the_near_sides_environment() {
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    // One named variable the near process has, never the whole environment.
    let out = stand_in
        .command()
        .args([
            "studio",
            "echo far-ran; echo \"near=${TAW_NEAR_ONLY-unset}\"",
        ])
        .env("TAW_NEAR_ONLY", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    // The positive half first: the far side did run.
    assert_eq!(stdout, "far-ran\nnear=unset\n");
}

#[test]
fn each_failure_prints_what_ssh_ends_on_and_exits_as_it_does() {
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    let table: Vec<(Failure, i32, &str)> = vec![
        (
            Failure::Refused,
            255,
            "ssh: connect to host studio port 22: Connection refused",
        ),
        (
            Failure::TimedOut,
            255,
            "ssh: connect to host studio port 22: Operation timed out",
        ),
        (
            Failure::PermissionDenied,
            255,
            "studio: Permission denied (publickey).",
        ),
        (Failure::HostKey, 255, "Host key verification failed."),
        (Failure::NoTaw, 127, "sh: taw: command not found"),
    ];
    for (how, status, line) in table {
        stand_in.fail_as("studio", how.clone());
        let (code, stdout, stderr) = run(&stand_in, &["studio", "taw workspace list"]);
        assert_eq!(code, status, "{how:?}");
        assert_eq!(stderr.trim_end(), line, "{how:?}");
        assert_eq!(stdout, "", "{how:?}");
    }
}

#[test]
fn an_old_taw_answers_its_version_and_refuses_the_rest() {
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    stand_in.fail_as("studio", Failure::OldTaw("0.30.0".to_string()));
    let (code, stdout, _) = run(&stand_in, &["studio", "taw --version"]);
    assert_eq!((code, stdout.trim_end()), (0, "taw 0.30.0"));
    let (code, stdout, stderr) = run(&stand_in, &["studio", "taw machine feed"]);
    assert_eq!(code, 2);
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("unrecognized subcommand 'machine'"),
        "{stderr}"
    );
}

#[test]
fn garbage_is_not_json() {
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    stand_in.fail_as("studio", Failure::Garbage);
    // It stays up until ended: read the first line only, then end the child.
    let mut child = stand_in
        .command()
        .args(["studio", "taw machine feed"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
        reader.read_line(&mut line).unwrap();
    }
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(line.trim_end(), "this is not json");
    assert!(serde_json::from_str::<Value>(line.trim_end()).is_err());
}

#[test]
fn hang_connects_and_says_nothing_until_it_is_ended() {
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    stand_in.fail_as("studio", Failure::Hang);
    // A far command that leaves a file when it runs: a stand-in that ran it would not hang.
    let scratch = tempfile::tempdir().unwrap();
    let ran = scratch.path().join("ran");
    let mut child = stand_in
        .command()
        .args(["studio", &format!("touch '{}'", ran.display())])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The positive half: it was started and recorded its call.
    assert!(
        common::wait_for(
            || stand_in.calls().len() == 1,
            std::time::Duration::from_secs(10)
        ),
        "the stand-in never recorded its call"
    );
    // The correct stand-in waits the whole time; one that runs the far command is caught as soon as
    // its marker is there, however late a loaded machine runs it: the look is every 50 ms for 1.5 s.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    while std::time::Instant::now() < deadline {
        assert!(!ran.exists(), "the far command ran");
        assert!(
            matches!(child.try_wait(), Ok(None)),
            "it ended: it should have stayed connected and silent"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!ran.exists(), "the far command ran");
    child.kill().unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}

#[test]
fn what_the_stand_in_was_run_with_is_recorded_as_typed_and_in_order() {
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    assert!(stand_in.calls().is_empty());
    let tricky = "echo 'it'\\''s' '$(touch /nonexistent/pwned)' a  b";
    let first = ["-o", "ControlPath=none", "-p", "2222", "studio", tricky];
    let (code, _, _) = run(&stand_in, &first);
    assert_eq!(code, 0);
    let second = ["-t", "studio", "taw workspace list"];
    run(&stand_in, &second);
    let calls = stand_in.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0], first.map(String::from).to_vec());
    assert_eq!(calls[1], second.map(String::from).to_vec());
}

#[test]
fn an_unknown_destination_is_not_resolved() {
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    let (code, stdout, stderr) = run(&stand_in, &["me@somewhere", "taw workspace list"]);
    assert_eq!(code, 255);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr.trim_end(),
        "ssh: Could not resolve hostname me@somewhere: nodename nor servname provided, or not known"
    );
    // The call was recorded all the same.
    assert_eq!(stand_in.calls().len(), 1);
}

#[test]
fn two_far_sides_are_two_machines() {
    let studio = FarSide::with_session("studio", "garden");
    let pi = FarSide::with_session("pi", "shed");
    let stand_in = StandIn::new(&[&studio, &pi]);
    let (_, out, _) = run(&stand_in, &["studio", "taw workspace list"]);
    let (_, other, _) = run(&stand_in, &["pi", "taw workspace list"]);
    let one = session_names(&serde_json::from_str(out.trim()).unwrap());
    let two = session_names(&serde_json::from_str(other.trim()).unwrap());
    assert!(
        one.contains(&"garden".to_string()) && !one.contains(&"shed".to_string()),
        "{one:?}"
    );
    assert!(
        two.contains(&"shed".to_string()) && !two.contains(&"garden".to_string()),
        "{two:?}"
    );
}

/// Runs the stand-in with `args` and its standard input a pipe the caller holds: what it is given is
/// written, and the pipe is closed only when `close` says (so that a far command that waits for the end of
/// its input is told apart from one that is given none). Returns (status, stdout), or `None` for the
/// status when the child was still running after ten seconds and had to be ended.
fn run_with_input(
    stand_in: &StandIn,
    args: &[&str],
    given: &str,
    close: bool,
) -> (Option<i32>, String) {
    use std::io::{Read, Write};
    let mut child = stand_in
        .command()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take();
    let _ = input.as_mut().unwrap().write_all(given.as_bytes());
    if close {
        input = None;
    }
    let ended = common::wait_for(
        || matches!(child.try_wait(), Ok(Some(_))),
        std::time::Duration::from_secs(10),
    );
    if !ended {
        let _ = child.kill();
    }
    let status = child.wait().unwrap();
    drop(input);
    let mut stdout = String::new();
    let _ = child.stdout.take().unwrap().read_to_string(&mut stdout);
    (ended.then(|| status.code().unwrap_or(-1)), stdout)
}

#[test]
fn with_n_the_far_command_has_no_input_and_without_it_reads_what_was_written() {
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    // `-T -n`, as taw runs a command: the far `cat` reads the end of its input at once, though the near
    // side still holds the pipe open and has written to it.
    let (status, stdout) =
        run_with_input(&stand_in, &["-T", "-n", "studio", "cat"], "typed\n", false);
    assert_eq!(
        status,
        Some(0),
        "the far command did not end: it was given the input"
    );
    assert_eq!(stdout, "");
    // `-T` alone, as a feed runs: what the near side wrote is what the far side reads.
    let (status, stdout) = run_with_input(&stand_in, &["-T", "studio", "cat"], "typed\n", true);
    assert_eq!(status, Some(0));
    assert_eq!(stdout, "typed\n");
}

#[test]
fn the_far_path_has_the_gates_nets_first() {
    assert_eq!(
        far_path(
            "$1",
            Some("/n/agent"),
            Some("/n/desktop"),
            Some("/opt/tmux/bin")
        ),
        "/n/agent/bin:/n/desktop/bin:$1:/opt/tmux/bin:/usr/bin:/bin"
    );
    // Only the nets that are given, and not an empty one.
    assert_eq!(
        far_path("$1", Some("/n/agent"), None, Some("/opt/tmux/bin")),
        "/n/agent/bin:$1:/opt/tmux/bin:/usr/bin:/bin"
    );
    assert_eq!(
        far_path("$1", None, Some("/n/desktop"), None),
        "/n/desktop/bin:$1:/usr/bin:/bin"
    );
    assert_eq!(
        far_path("$1", Some(""), Some(""), Some("")),
        "$1:/usr/bin:/bin"
    );
    // A folder with a character the script's quotes would read is escaped, and `$1` is not.
    assert_eq!(
        far_path("$1", Some("/n/a \"b\"$c"), None, None),
        "/n/a \\\"b\\\"\\$c/bin:$1:/usr/bin:/bin"
    );
}

#[test]
fn under_the_gate_the_far_side_finds_the_nets_stubs_before_the_real_programs() {
    let studio = FarSide::new("studio");
    let stand_in = StandIn::new(&[&studio]);
    let (code, stdout, stderr) = run(&stand_in, &["studio", "echo \"$PATH\""]);
    assert_eq!(code, 0, "{stderr}");
    let path = stdout.trim_end();
    // The positive half: the far side has its own PATH, with a taw on it.
    assert!(path.ends_with(":/usr/bin:/bin"), "{path}");
    for net in ["TAW_AGENT_NET", "TAW_DESKTOP_NET"] {
        // Outside the gate there is no net to find: this runs where the gate has made one.
        if let Some(dir) = std::env::var_os(net).filter(|dir| !dir.is_empty()) {
            let bin = format!("{}/bin", dir.to_string_lossy());
            assert!(path.contains(&bin), "{net}: {bin} is not on {path}");
            assert!(
                path.find(&bin) < path.find("/usr/bin"),
                "{net}: {bin} is not before /usr/bin on {path}"
            );
        }
    }
}
