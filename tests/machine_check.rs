//! `taw machine check`: one try at each machine of `[machines]`, on a connection of its own, and what
//! each one said. The stand-in ssh reaches far sides of this test's own (`FarSide`); no real ssh runs.

mod common;

use std::time::{Duration, Instant};

use common::machine::{Failure, FarSide, StandIn};
use common::{Server, ServerOptions, write_script};
use serde_json::Value;

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Rig {
    here: Server,
    studio: FarSide,
    pi: FarSide,
    stand_in: StandIn,
}

impl Rig {
    fn new() -> Rig {
        Rig::with_config("[machines]\nstudio = \"studio\"\npi = \"pi\"\n")
    }

    fn with_config(config: &str) -> Rig {
        let here = Server::start_with(ServerOptions {
            session: "here".to_string(),
            ..ServerOptions::default()
        });
        here.write_config(config);
        let studio = FarSide::new("studio");
        let pi = FarSide::new("pi");
        let stand_in = StandIn::new(&[&studio, &pi]);
        Rig {
            here,
            studio,
            pi,
            stand_in,
        }
    }

    fn taw(&self, args: &[&str]) -> (i32, String, String) {
        let bin = self.stand_in.bin.display().to_string();
        self.here.taw_raw_with_env(args, &[("TAW_SSH_BIN", &bin)])
    }

    /// The lines of the check of one machine, as a `(status, stdout)` pair.
    fn check(&self, args: &[&str]) -> (i32, String) {
        let mut all = vec!["machine", "check"];
        all.extend_from_slice(args);
        let (code, stdout, _) = self.taw(&all);
        (code, stdout)
    }
}

#[test]
fn machines_that_answer_say_the_version_of_their_taw_and_the_status_is_zero() {
    let rig = Rig::new();
    let (code, stdout) = rig.check(&[]);
    assert_eq!(
        stdout,
        format!("studio  connected: taw {VERSION}\npi      connected: taw {VERSION}\n")
    );
    assert_eq!(code, 0);
}

#[test]
fn each_failure_has_its_own_words_and_ssh_s_last_line_under_them() {
    let rig = Rig::new();
    let hint = " (README, \"Remote machines\"); it then connects by itself";
    let cases: Vec<(Failure, String, &str)> = vec![
        (
            Failure::Refused,
            "not answering".to_string(),
            "ssh: connect to host studio port 22: Connection refused",
        ),
        (
            Failure::TimedOut,
            "not answering".to_string(),
            "ssh: connect to host studio port 22: Operation timed out",
        ),
        (
            Failure::PermissionDenied,
            format!(
                "ssh could not log in without asking: add your key to ssh-agent (ssh-add){hint}"
            ),
            "studio: Permission denied (publickey).",
        ),
        (
            Failure::HostKey,
            format!(
                "its host key is not known here: run ssh studio once in a terminal and accept it{hint}"
            ),
            "Host key verification failed.",
        ),
        (
            Failure::NoTaw,
            format!("no taw there: install it as you did here{hint}"),
            "sh: taw: command not found",
        ),
        (
            Failure::OldTaw("0.0.9".to_string()),
            format!("runs taw 0.0.9, which has no machines: update it there{hint}"),
            "error: unrecognized subcommand 'machine'",
        ),
    ];
    for (how, words, said) in cases {
        rig.stand_in.fail_as("studio", how.clone());
        let (code, stdout) = rig.check(&["studio"]);
        assert_eq!(
            stdout,
            format!("studio  {words}\n        {said}\n"),
            "{how:?}"
        );
        assert_eq!(code, 1, "{how:?}");
    }
}

#[test]
fn a_machine_that_never_speaks_is_ended_at_the_deadline_and_holds_up_nothing_else() {
    let rig = Rig::new();
    rig.stand_in.fail_as("studio", Failure::Hang);
    let started = Instant::now();
    let (code, stdout) = rig.check(&["--deadline-ms", "1500"]);
    let took = started.elapsed();
    assert_eq!(
        stdout,
        format!("studio  not answering\npi      connected: taw {VERSION}\n")
    );
    assert_eq!(code, 1);
    assert!(took >= Duration::from_millis(1400), "ended early: {took:?}");
    assert!(took < Duration::from_secs(10), "took {took:?}");
}

/// Whether a process is there (a zombie that nobody has reaped yet counts as there for a moment).
fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only asks; the pid is one the stand-in recorded for itself.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Ends the pids it holds when it goes, so that a test that fails leaves no helper running.
struct EndOnDrop(Vec<u32>);

impl Drop for EndOnDrop {
    fn drop(&mut self) {
        for pid in &self.0 {
            // SAFETY: the pids are ones the stand-in started for this test.
            unsafe { libc::kill(*pid as i32, libc::SIGKILL) };
        }
    }
}

#[test]
fn at_the_deadline_ssh_and_what_it_started_are_ended() {
    let rig = Rig::new();
    rig.stand_in.fail_as("studio", Failure::HangWithHelper);
    let (code, stdout) = rig.check(&["studio", "--deadline-ms", "1000"]);
    assert_eq!(stdout, "studio  not answering\n");
    assert_eq!(code, 1);
    let ssh = rig.stand_in.pids();
    let helpers = rig.stand_in.helper_pids();
    assert_eq!((ssh.len(), helpers.len()), (1, 1), "{ssh:?} {helpers:?}");
    let _guard = EndOnDrop(helpers.clone());
    // The kill is done before check answers; the wait only covers a process that init has yet to reap.
    for pid in ssh.iter().chain(&helpers) {
        assert!(
            common::wait_for(|| !alive(*pid), Duration::from_secs(1)),
            "process {pid} is still there after the check ended"
        );
    }
}

/// Whether a process is gone: the system has no process of that pid (a zombie is still there).
fn gone(pid: u32) -> bool {
    // SAFETY: signal 0 only asks; the pid is one the stand-in recorded for itself.
    unsafe {
        libc::kill(pid as i32, 0) == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
}

/// Ends the `taw` a test started when it goes, so that a test that fails leaves no check running.
struct EndTaw(std::process::Child);

impl Drop for EndTaw {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_signal_to_taw_during_a_check_ends_ssh_and_what_it_started() {
    use std::os::unix::process::ExitStatusExt;
    for (sig, name) in [
        (libc::SIGINT, "SIGINT"),
        (libc::SIGTERM, "SIGTERM"),
        (libc::SIGHUP, "SIGHUP"),
    ] {
        let rig = Rig::new();
        rig.stand_in.fail_as("studio", Failure::HangWithHelper);
        let bin = rig.stand_in.bin.display().to_string();
        let mut cmd =
            rig.here
                .taw_command(&["machine", "check", "studio", "--deadline-ms", "60000"]);
        cmd.env("TAW_SSH_BIN", &bin)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut taw = EndTaw(cmd.spawn().unwrap());
        let mut helpers_guard = EndOnDrop(Vec::new());
        // The stand-in has run and started its helper: ssh is in its group and taw waits for it.
        assert!(
            common::wait_for(
                || rig.stand_in.pids().len() == 1 && rig.stand_in.helper_pids().len() == 1,
                Duration::from_secs(10)
            ),
            "{name}: the stand-in did not start: {:?} {:?}",
            rig.stand_in.pids(),
            rig.stand_in.helper_pids()
        );
        let ssh = rig.stand_in.pids();
        let helpers = rig.stand_in.helper_pids();
        helpers_guard.0 = helpers.clone();
        // SAFETY: the signal goes to the pid of the `taw` this test started, and to no one else.
        assert_eq!(unsafe { libc::kill(taw.0.id() as i32, sig) }, 0);
        let mut status = None;
        let ended = common::wait_for(
            || {
                status = taw.0.try_wait().unwrap();
                status.is_some()
            },
            Duration::from_secs(10),
        );
        assert!(ended, "{name}: taw did not end");
        assert_eq!(status.unwrap().signal(), Some(sig), "{name}: how taw ended");
        for pid in ssh.iter().chain(&helpers) {
            assert!(
                common::wait_for(|| gone(*pid), Duration::from_secs(5)),
                "{name}: process {pid} is still there after taw was signalled"
            );
        }
    }
}

#[test]
fn the_machines_are_tried_at_the_same_time_so_two_that_hang_cost_one_deadline() {
    let rig = Rig::new();
    rig.stand_in.fail_as("studio", Failure::Hang);
    rig.stand_in.fail_as("pi", Failure::Hang);
    let started = Instant::now();
    let (code, stdout) = rig.check(&["--deadline-ms", "2000"]);
    let took = started.elapsed();
    assert_eq!(stdout, "studio  not answering\npi      not answering\n");
    assert_eq!(code, 1);
    assert!(took >= Duration::from_millis(1900), "ended early: {took:?}");
    assert!(
        took < Duration::from_millis(3600),
        "one after the other: {took:?}"
    );
}

#[test]
fn something_that_is_not_a_feed_is_said_so_and_not_taken_for_silence() {
    let rig = Rig::new();
    rig.stand_in.fail_as("pi", Failure::Garbage);
    let (code, stdout) = rig.check(&["pi", "--deadline-ms", "1000"]);
    assert_eq!(stdout, "pi  sent something taw does not read\n");
    assert_eq!(code, 1);
}

#[test]
fn a_taw_that_speaks_another_protocol_is_told_to_update_this_one() {
    let rig = Rig::new();
    let script = rig.stand_in_dir_script();
    let config = format!(
        "[machines]\nstudio = \"studio\"\n[machines.pi]\nssh = \"pi\"\ntaw = \"{}\"\n",
        script.display()
    );
    rig.here.write_config(&config);
    let (code, stdout) = rig.check(&["pi"]);
    assert_eq!(
        stdout,
        format!(
            "pi  runs taw 9.9.9, which no longer speaks to this one ({VERSION}): update taw here \
             (README, \"Remote machines\"); it then connects by itself\n"
        )
    );
    assert_eq!(code, 1);
}

impl Rig {
    /// A script that answers as a taw of protocol 2 would, outside every folder the rig keeps.
    fn stand_in_dir_script(&self) -> std::path::PathBuf {
        let path = self.here.home_dir().join("newer-taw");
        write_script(
            &path,
            "echo '{\"type\":\"hello\",\"taw\":\"9.9.9\",\"protocol\":2,\"host\":\"x\",\"server\":\"\"}'\n",
        );
        path
    }
}

impl Rig {
    /// Makes `pi` a machine whose taw is a script that says what `body` says, and says it is.
    fn pi_runs(&self, body: &str) {
        let path = self.here.home_dir().join("odd-taw");
        write_script(&path, body);
        self.here.write_config(&format!(
            "[machines]\nstudio = \"studio\"\n[machines.pi]\nssh = \"pi\"\ntaw = \"{}\"\n",
            path.display()
        ));
    }
}

fn plain_enough(text: &str) {
    assert!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "a control character reached the terminal: {text:?}"
    );
}

#[test]
fn an_escape_sequence_in_the_version_a_machine_says_in_its_hello_does_not_reach_the_terminal() {
    let rig = Rig::new();
    rig.pi_runs(
        "printf '%s\\n' '{\"type\":\"hello\",\"taw\":\"1.2\\u001b]52;c;QUJD\\u0007\\u001b[2J-x\",\"protocol\":1,\"host\":\"x\",\"server\":\"\"}'\n",
    );
    let (code, stdout) = rig.check(&["pi"]);
    plain_enough(&stdout);
    assert_eq!(stdout, "pi  connected: taw 1.2-x\n");
    assert_eq!(code, 0);
    let (_, stdout, _) = rig.taw(&["machine", "check", "--json", "pi"]);
    plain_enough(&stdout);
    let value: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["result"]["machines"][0]["taw_version"], "1.2-x");
    assert_eq!(
        value["result"]["machines"][0]["words"],
        "connected: taw 1.2-x"
    );
}

#[test]
fn an_escape_sequence_in_what_version_prints_does_not_reach_the_terminal() {
    let rig = Rig::new();
    // No `machine` verb there (status 2), and a version line with a title change, a bell and a clear.
    rig.pi_runs(
        "case \"$1\" in\n--version) printf 'taw 0.0.9\\033]0;pwned\\007\\033[2J-y\\n';;\n*) exit 2;;\nesac\n",
    );
    let (code, stdout) = rig.check(&["pi"]);
    plain_enough(&stdout);
    assert!(
        stdout.starts_with("pi  runs taw 0.0.9-y, which has no machines"),
        "{stdout:?}"
    );
    assert_eq!(code, 1);
}

#[test]
fn the_version_call_gets_what_is_left_of_the_deadline_and_not_a_fresh_one() {
    let rig = Rig::new();
    // No `machine` verb there, after a second; and a `--version` that never answers.
    rig.pi_runs("case \"$1\" in\n--version) exec sleep 600;;\n*) sleep 1; exit 2;;\nesac\n");
    let started = Instant::now();
    let (code, stdout) = rig.check(&["pi", "--deadline-ms", "2500"]);
    let took = started.elapsed();
    assert_eq!(code, 1);
    assert!(
        stdout.starts_with("pi  runs a taw that has no machines"),
        "{stdout:?}"
    );
    assert!(took >= Duration::from_millis(2400), "ended early: {took:?}");
    // One deadline in all (2.5 s and the start up), not 1 s and a whole second deadline of 2.5 s.
    assert!(took < Duration::from_millis(3300), "took {took:?}");
}

#[test]
fn a_machine_that_never_stops_talking_is_read_to_a_cap_and_still_answers() {
    let rig = Rig::new();
    let long = "x".repeat(1000);
    rig.pi_runs(&format!(
        "echo '{{\"type\":\"hello\",\"taw\":\"3.1.4\",\"protocol\":1}}'\nwhile :; do echo {long}; done\n"
    ));
    let started = Instant::now();
    let (code, stdout) = rig.check(&["pi", "--deadline-ms", "8000"]);
    assert_eq!(stdout, "pi  connected: taw 3.1.4\n");
    assert_eq!(code, 0);
    // The read stopped at the cap and let go of the pipe, which ended the talker; it was not read until
    // the deadline.
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "read until the deadline: {:?}",
        started.elapsed()
    );
}

#[test]
fn what_ssh_said_before_a_helper_held_its_output_open_is_kept() {
    let rig = Rig::new();
    // Exits at once with its complaint, leaving a helper that holds the output open for a while.
    rig.pi_runs("echo 'boom said' >&2\nsleep 4 &\nexit 255\n");
    let (code, stdout) = rig.check(&["pi", "--deadline-ms", "5000"]);
    assert_eq!(stdout, "pi  not answering\n    boom said\n");
    assert_eq!(code, 1);
}

#[test]
fn the_version_a_machine_says_is_cut_to_64_characters() {
    let rig = Rig::new();
    let long = "9".repeat(100);
    rig.pi_runs(&format!(
        "printf '%s\\n' '{{\"type\":\"hello\",\"taw\":\"{long}\",\"protocol\":1}}'\n"
    ));
    let (_, stdout) = rig.check(&["pi"]);
    assert_eq!(stdout, format!("pi  connected: taw {}\n", "9".repeat(64)));
    rig.pi_runs(&format!(
        "case \"$1\" in\n--version) echo 'taw {long}';;\n*) exit 2;;\nesac\n"
    ));
    let (_, stdout) = rig.check(&["pi"]);
    assert!(
        stdout.starts_with(&format!("pi  runs taw {}, which", "9".repeat(64))),
        "{stdout:?}"
    );
}

#[test]
fn every_try_is_on_a_connection_of_its_own_and_none_makes_a_control_folder() {
    let rig = Rig::new();
    let (code, _) = rig.check(&[]);
    assert_eq!(code, 0);
    let calls = rig.stand_in.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    for call in &calls {
        assert!(
            call.iter().any(|a| a == "ControlPath=none"),
            "no ControlPath=none: {call:?}"
        );
        assert!(
            !call.iter().any(|a| a.contains("ControlMaster")),
            "a control master: {call:?}"
        );
        assert!(call.iter().any(|a| a == "-n"), "reads input: {call:?}");
    }
    assert!(
        !rig.here.state_dir().join("m").exists(),
        "check made the control folder"
    );
}

#[test]
fn an_old_taw_is_asked_for_its_version_with_one_more_call() {
    let rig = Rig::new();
    rig.stand_in
        .fail_as("studio", Failure::OldTaw("0.0.9".to_string()));
    let (_, stdout) = rig.check(&["studio"]);
    assert!(stdout.contains("runs taw 0.0.9,"), "{stdout}");
    let calls = rig.stand_in.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls[1].last().unwrap().ends_with("'--version'"),
        "{:?}",
        calls[1]
    );
    // The follow-up is a connection of its own too, as the first try is.
    for call in &calls {
        assert!(
            call.iter().any(|a| a == "ControlPath=none"),
            "no ControlPath=none: {call:?}"
        );
        assert!(
            !call.iter().any(|a| a.contains("ControlMaster")),
            "a control master: {call:?}"
        );
    }
}

#[test]
fn json_gives_one_envelope_with_a_row_for_each_machine_and_the_status_of_the_check() {
    let rig = Rig::new();
    rig.stand_in.fail_as("pi", Failure::NoTaw);
    let (code, stdout, _) = rig.taw(&["machine", "check", "--json"]);
    assert_eq!(code, 1);
    let value: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["id"], "cli:machine:check", "{value}");
    assert_eq!(value["result"]["type"], "machine_check", "{value}");
    let rows = value["result"]["machines"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], "studio");
    assert_eq!(rows[0]["destination"], "studio");
    assert_eq!(rows[0]["connected"], true);
    assert_eq!(rows[0]["taw_version"], VERSION);
    assert_eq!(rows[0]["protocol"], 1);
    assert_eq!(rows[0]["ssh_said"], "");
    assert_eq!(rows[1]["name"], "pi");
    assert_eq!(rows[1]["connected"], false);
    assert_eq!(rows[1]["words"], "no taw there: install it as you did here");
    assert_eq!(rows[1]["taw_version"], Value::Null);
    assert_eq!(rows[1]["ssh_said"], "sh: taw: command not found");

    let (code, stdout, _) = rig.taw(&["machine", "check", "--json", "studio"]);
    assert_eq!(code, 0);
    let value: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["result"]["machines"].as_array().unwrap().len(), 1);
}

#[test]
fn a_name_that_is_not_in_the_table_is_a_usage_error_and_nothing_is_tried() {
    let rig = Rig::new();
    let (code, stdout, stderr) = rig.taw(&["machine", "check", "attic"]);
    assert_eq!(code, 2);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "taw: no machine called attic in [machines] (yours: studio, pi)\n"
    );
    assert!(rig.stand_in.calls().is_empty());
}

#[test]
fn without_a_table_it_says_so_in_one_line_and_tries_nothing() {
    let rig = Rig::with_config("model = \"x\"\n");
    let (code, stdout, _) = rig.taw(&["machine", "check"]);
    assert_eq!(code, 0);
    // The line names the config file, with `~` for the home folder the taw there runs in.
    let file = taw::session::naming::tilde(
        &rig.here.config_dir().join("config.toml"),
        rig.here.home_dir(),
    );
    assert_eq!(stdout, format!("no machines in [machines] ({file})\n"));
    let (code, stdout, _) = rig.taw(&["machine", "check", "--json"]);
    assert_eq!(code, 0);
    let value: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["result"]["machines"], serde_json::json!([]));
    assert!(rig.stand_in.calls().is_empty());
    assert!(!rig.here.state_dir().join("m").exists());
    let _ = (&rig.studio, &rig.pi);
}
