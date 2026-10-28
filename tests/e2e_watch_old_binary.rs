//! Watchers that still run an old taw, end to end.
//!
//! tmux starts a watcher once, with `pipe-pane`, and never restarts it, so after taw is
//! rebuilt (cargo replaces `target/release/taw` with a new file) or reinstalled every pane
//! keeps the taw it started with. `taw watch status` has to say which watchers those are,
//! and `taw watch attach-all --restart` has to start them again on the installed one.
//!
//! Two kinds of watcher are used, and each proves a different thing:
//!
//! * A watcher run **inside the test process** (`watch::run_with_reader`), with a stub file
//!   for its `taw_bin`. The stub is replaced, deleted and so on in a moment, so every way a
//!   watcher can stand against its taw is covered without copying a binary.
//! * A **real `taw watch run`**, started by tmux from a copy of the built binary in a
//!   scratch directory. The copy is replaced by another copy (a new file, as a rebuild
//!   makes), which proves the assembled product records what it runs, tells its own
//!   replacement, and comes back on the new one.
//!
//! What has to be seen in order, or seen to fail, goes through a **stand-in for tmux** (see
//! [`StandIn`]): a script that logs every call and runs the real tmux, and refuses the calls a
//! test asks it to. A restart's waiting is driven in this process with the time it may wait
//! given as an argument, so that nothing here depends on how fast the machine is.
//!
//! Everything runs on a private socket from `tests/common`, and no test touches the
//! binary the suite itself runs.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;

use common::{Server, make_executable, parse_envelope, wait_for, write_script};
use serde_json::Value;
use taw::config::Config;
use taw::paths::Paths;
use taw::tmux::{Socket, Tmux};
use taw::watch::{self, Restart, Timing};

/// Serialises the tests in this file: each holds a tmux server and one to three watchers,
/// and run at once they reach the poll ceilings below for reasons of load, not of the
/// product. (Same reason `tests/e2e_watch.rs` does it.)
static SERIAL: Mutex<()> = Mutex::new(());

/// Ceiling for a watcher to start and write down what it runs, and for an old watcher to
/// leave and its successor to arrive. A ceiling, not a sleep: every poll returns the
/// moment its condition holds.
const SETTLE: Duration = Duration::from_secs(20);

const CONFIG_QUIET: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

const BUILD: &str = "@agent_watch_build";
const PID: &str = "@agent_watch_pid";

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

fn server() -> Server {
    let server = Server::start();
    server.write_config(CONFIG_QUIET);
    server
}

/// A second pane, in a window of its own, of the server's one session.
fn another_pane(server: &Server) -> String {
    server.tmux_ok(&["new-window", "-d", "-P", "-F", "#{pane_id}", "-t", "one:"])
}

/// The library's view of `server`, for a watcher run in this process.
fn tmux_of(server: &Server) -> Tmux {
    Tmux::with_socket_name(server.socket())
}

fn paths_of(server: &Server) -> Paths {
    let state = server.state_dir();
    let agent_detection = state.join("agent-detection");
    let plugins_dir = state.join("plugins");
    Paths {
        config_dir: server.config_dir().to_path_buf(),
        config_file: server.config_dir().join("config.toml"),
        state_dir: state.to_path_buf(),
        override_dir: server.config_dir().join("agent-detection"),
        remote_manifest_dir: agent_detection.join("remote"),
        manifest_marker: agent_detection.join("last-check"),
        sessions_dir: state.join("sessions"),
        plugin_state_dir: plugins_dir.join("state"),
        plugins_dir,
        logs_dir: state.join("logs"),
    }
}

/// A watcher run in this process on `pane`, which says its taw is the file `taw_bin`. Ends
/// when dropped (the pipe's writing end closes, which is what tmux does to a real one).
struct InProcessWatcher {
    writer: Option<std::io::PipeWriter>,
    thread: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
}

impl InProcessWatcher {
    fn start(server: &Server, taw_bin: &Path, pane: &str) -> InProcessWatcher {
        Self::start_with(tmux_of(server), server, taw_bin, pane)
    }

    /// The watcher goes through `t`, which is the server's own tmux or a [`StandIn`] for it. A
    /// real watcher is the reading end of a pipe tmux has open on its pane, and `watch status`
    /// counts only a watcher whose pipe is open: this one has a pipe of its own, which nothing
    /// reads from.
    fn start_with(t: Tmux, server: &Server, taw_bin: &Path, pane: &str) -> InProcessWatcher {
        server.tmux_ok(&["pipe-pane", "-t", pane, "cat > /dev/null"]);
        assert!(wait_for(|| server.piped(pane) == "1", SETTLE));
        let (reader, writer) = std::io::pipe().unwrap();
        let paths = paths_of(server);
        let taw_bin = taw_bin.to_path_buf();
        let pane = pane.to_string();
        let mut cfg = Config::default();
        cfg.update.manifest_check = false;
        let thread = std::thread::spawn(move || {
            watch::run_with_reader(
                &t,
                &cfg,
                &paths,
                &taw_bin,
                &pane,
                Timing::default(),
                Box::new(reader),
            )
        });
        InProcessWatcher {
            writer: Some(writer),
            thread: Some(thread),
        }
    }

    /// Closes the pipe and waits for the watcher to leave.
    fn end(&mut self) {
        drop(self.writer.take());
        if let Some(thread) = self.thread.take() {
            assert!(
                wait_for(|| thread.is_finished(), SETTLE),
                "the watcher did not leave when its pipe closed"
            );
            thread.join().unwrap().unwrap();
        }
    }
}

impl Drop for InProcessWatcher {
    fn drop(&mut self) {
        drop(self.writer.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Waits until the watcher on `pane` has written down both what it runs and its pid.
fn await_record(server: &Server, pane: &str) {
    assert!(
        wait_for(
            || !server.pane_opt(pane, BUILD).is_empty() && !server.pane_opt(pane, PID).is_empty(),
            SETTLE
        ),
        "the watcher on {pane} never wrote {BUILD} and {PID}: build {:?}, pid {:?}",
        server.pane_opt(pane, BUILD),
        server.pane_opt(pane, PID)
    );
}

fn watcher_pid(server: &Server, pane: &str) -> u32 {
    server.pane_opt(pane, PID).parse().unwrap_or_else(|_| {
        panic!(
            "{PID} of {pane} is not a pid: {:?}",
            server.pane_opt(pane, PID)
        )
    })
}

fn alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 sends nothing; it takes two integers.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// What a rebuild or a reinstall does to a taw: a new file at the same path, the old one
/// left to the processes that run it.
fn replace_file(path: &Path, make: impl FnOnce(&Path)) {
    let next = path.with_extension("next");
    make(&next);
    std::fs::rename(&next, path).unwrap();
}

fn replace_stub(path: &Path) {
    replace_file(path, |next| write_script(next, "exit 0\n"));
}

/// A copy of the built taw at `path`: a file of its own, as a build's output is.
fn copy_taw(path: &Path) {
    std::fs::copy(common::taw_bin(), path).unwrap();
    make_executable(path);
}

fn replace_taw(path: &Path) {
    replace_file(path, copy_taw);
}

/// Runs the taw at `bin`, a copy of the built one, against `server`.
fn taw_at(server: &Server, bin: &Path, args: &[&str]) -> (i32, Value) {
    let out = Command::new(bin)
        .args(args)
        .env("TAW_TMUX_SOCKET_NAME", server.socket())
        .env("TAW_CONFIG_DIR", server.config_dir())
        .env("TAW_STATE_DIR", server.state_dir())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    (
        out.status.code().unwrap_or(-1),
        parse_envelope(args, &stdout, &stderr),
    )
}

fn result_at(server: &Server, bin: &Path, args: &[&str], id: &str) -> Value {
    let (code, env) = taw_at(server, bin, args);
    assert_eq!(code, 0, "`taw {}` failed: {env}", args.join(" "));
    assert_eq!(env["id"], id, "{env}");
    env["result"].clone()
}

fn status_at(server: &Server, bin: &Path) -> Value {
    result_at(server, bin, &["watch", "status"], "cli:watch:status")
}

/// The row of `pane` in a `watch status` result.
fn row<'a>(status: &'a Value, pane: &str) -> &'a Value {
    status["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["terminal_id"] == pane)
        .unwrap_or_else(|| panic!("pane {pane} is missing from `watch status`: {status}"))
}

/// What `@agent_watch_build` says without the pid in front: the file and where it is.
fn build_without_pid(server: &Server, pane: &str) -> String {
    let value = server.pane_opt(pane, BUILD);
    value
        .split_once(' ')
        .unwrap_or_else(|| panic!("{BUILD} of {pane} is not a record: {value:?}"))
        .1
        .to_string()
}

/// A stand-in for tmux: a script that a `Tmux` runs as its `bin` (or a `taw` started with
/// `TAW_TMUX_BIN`). It writes every call it is given as one line of a log, then runs the real
/// tmux with the same arguments, unless the test has armed it to refuse or to interfere with
/// that call. A refusal is what a busy or a dying server gives: nothing on stdout, a line on
/// stderr, exit status 1.
///
/// Each time it refuses or interferes it writes the flag's name and the call to a second log,
/// so that a test can say that it happened, and not only that nothing else did: a call whose
/// format has changed would otherwise be passed through, and the test would go on to pass for
/// the wrong reason.
///
/// A flag is armed with a file in the stand-in's directory ([`StandIn::arm`]):
///
/// * `fail-listing`: every listing of who is named in `@agent_watch_pid`, the one a restart
///   waits on, for as long as the file is there.
/// * `fail-attach-once`: the next `pipe-pane` that starts a `watch run`, once.
/// * `fail-record-once`: the next write of `@agent_watch_build`, once.
/// * `off-after-detach`: the next `pipe-pane` that closes a pipe is run, and then
///   `@agent-wizard-watch` is set to `off`, once: watching turned off in the middle of a restart.
struct StandIn {
    dir: tempfile::TempDir,
    bin: PathBuf,
}

impl StandIn {
    fn new(server: &Server) -> StandIn {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("tmux");
        write_script(
            &bin,
            &format!(
                r##"DIR='{dir}'
REAL='{real}'
SOCKET='{socket}'
TAB=$(printf '\t')
printf '%s\n' "$*" >> "$DIR/calls.log"
case "$*" in
  *"list-panes -a -F #{{pane_id}}${{TAB}}#{{@agent_watch_pid}}")
    if [ -e "$DIR/fail-listing" ]; then
      echo "fail-listing $*" >> "$DIR/interventions.log"
      echo "tmux: simulated failure" >&2; exit 1
    fi ;;
  *pipe-pane*"watch run"*)
    if [ -e "$DIR/fail-attach-once" ]; then
      rm -f "$DIR/fail-attach-once"
      echo "fail-attach-once $*" >> "$DIR/interventions.log"
      echo "tmux: simulated failure" >&2; exit 1
    fi ;;
  *pipe-pane*)
    if [ -e "$DIR/off-after-detach" ]; then
      rm -f "$DIR/off-after-detach"
      echo "off-after-detach $*" >> "$DIR/interventions.log"
      "$REAL" "$@" || exit $?
      "$REAL" -L "$SOCKET" set-option -g @agent-wizard-watch off
      exit $?
    fi ;;
  *set-option*"@agent_watch_build"*)
    if [ -e "$DIR/fail-record-once" ]; then
      rm -f "$DIR/fail-record-once"
      echo "fail-record-once $*" >> "$DIR/interventions.log"
      echo "tmux: simulated failure" >&2; exit 1
    fi ;;
esac
exec "$REAL" "$@"
"##,
                dir = dir.path().display(),
                real = tmux_of(server).bin.display(),
                socket = server.socket()
            ),
        );
        StandIn { dir, bin }
    }

    /// The server's tmux, through the stand-in.
    fn tmux(&self, server: &Server) -> Tmux {
        Tmux {
            bin: self.bin.clone(),
            socket: Some(Socket::Name(server.socket().to_string())),
        }
    }

    /// The log of calls so far, a line each, in the order they were made. A call with a newline in
    /// it (a long listing format has some) is more than one line, and the first holds its
    /// command.
    fn calls(&self) -> Vec<String> {
        lines_of(&self.dir.path().join("calls.log"))
    }

    /// From now on, do what `flag` names.
    fn arm(&self, flag: &str) {
        std::fs::write(self.dir.path().join(flag), "").unwrap();
    }

    /// Stop doing what `flag` names.
    fn disarm(&self, flag: &str) {
        let _ = std::fs::remove_file(self.dir.path().join(flag));
    }

    /// How many times it has done what `flag` names.
    fn happened(&self, flag: &str) -> usize {
        lines_of(&self.dir.path().join("interventions.log"))
            .iter()
            .filter(|line| line.starts_with(&format!("{flag} ")))
            .count()
    }

    /// How many times a restart has listed who is named in `@agent_watch_pid`, which it does
    /// again after a pause for as long as an old watcher is there.
    fn departure_listings(&self) -> usize {
        self.calls()
            .iter()
            .filter(|call| call.ends_with("list-panes -a -F #{pane_id}\t#{@agent_watch_pid}"))
            .count()
    }
}

fn lines_of(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// A process that stands in for an old watcher that leaves the moment its pipe closes, as a real
/// one does: it is the process at the other end of the pipe, and ends at end of file. It is named
/// in the pane's options as the watcher of a taw that is not there any more, and unlike a real
/// one it leaves them behind (which a restart takes as the watcher having gone, as the process
/// has). Returns its pid.
fn old_watcher_that_leaves_at_eof(server: &Server, pane: &str, dir: &Path) -> u32 {
    let pid_file = dir.join("old-watcher.pid");
    let script = dir.join("old-watcher.sh");
    write_script(
        &script,
        &format!("echo $$ > '{}'\nexec cat > /dev/null\n", pid_file.display()),
    );
    server.tmux_ok(&["pipe-pane", "-t", pane, &format!("'{}'", script.display())]);
    assert!(
        wait_for(|| common::read_pid(&pid_file).is_some(), SETTLE),
        "the stand-in for an old watcher never started"
    );
    let pid: u32 = common::read_pid(&pid_file).unwrap().parse().unwrap();
    server.set_pane_opt(pane, BUILD, &format!("{pid} 1:2:3:4:5 /nonexistent/taw"));
    server.set_pane_opt(pane, PID, &pid.to_string());
    pid
}

/// The CLI's `result` for `args`, run with `tmux` as its `TAW_TMUX_BIN`.
fn result_through(server: &Server, tmux: &Path, args: &[&str]) -> Value {
    let (code, stdout, stderr) =
        server.taw_raw_with_env(args, &[("TAW_TMUX_BIN", tmux.to_str().unwrap())]);
    assert_eq!(code, 0, "`taw {}` failed: {stdout}{stderr}", args.join(" "));
    parse_envelope(args, &stdout, &stderr)["result"].clone()
}

// ---------------------------------------------------------------------------------------
// What `watch status` says
// ---------------------------------------------------------------------------------------

/// The watcher says which taw it runs: the pid it is, the file, and where the file is.
#[test]
fn a_watcher_writes_down_its_pid_the_file_it_runs_and_where_the_file_is() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();

    let _watcher = InProcessWatcher::start(&server, &taw, &pane);
    await_record(&server, &pane);

    let record = server.pane_opt(&pane, BUILD);
    let (pid, rest) = record
        .split_once(' ')
        .expect("a record is the pid, then the build");
    assert_eq!(pid, std::process::id().to_string(), "{record}");
    assert!(
        rest.ends_with(&format!(" {}", taw.display())),
        "the record should end with the path it was started from: {record}"
    );
    let identity = rest.rsplit_once(' ').unwrap().0;
    assert_eq!(
        identity.split(':').count(),
        5,
        "device, inode, size and the modified time (seconds and nanoseconds): {record}"
    );
}

/// The record goes in before the pid, so that no reader that finds the pid finds a watcher that
/// has said nothing of itself (and takes it for one of an older taw). Seen in the order tmux was
/// asked, through a stand-in that logs the calls.
#[test]
fn a_watcher_asks_tmux_to_record_its_taw_before_it_publishes_its_pid() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let stand_in = StandIn::new(&server);

    let _watcher = InProcessWatcher::start_with(stand_in.tmux(&server), &server, &taw, &pane);
    await_record(&server, &pane);

    let calls = stand_in.calls();
    let asked = |option: &str| {
        calls
            .iter()
            .position(|call| call.contains("set-option") && call.contains(option))
            .unwrap_or_else(|| panic!("tmux was never asked to set {option}: {calls:#?}"))
    };
    assert!(
        asked(BUILD) < asked(PID),
        "the pid was published before the record: {calls:#?}"
    );
}

/// A record that tmux refuses is written once more: a watcher that has said nothing of itself is
/// taken for one of an older taw for as long as it runs.
#[test]
fn a_watcher_writes_its_record_again_when_tmux_refuses_it_once() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let stand_in = StandIn::new(&server);
    stand_in.arm("fail-record-once");

    let _watcher = InProcessWatcher::start_with(stand_in.tmux(&server), &server, &taw, &pane);
    await_record(&server, &pane);

    assert_eq!(
        stand_in.happened("fail-record-once"),
        1,
        "the stand-in never refused the record, so nothing was retried: {:#?}",
        stand_in.calls()
    );
    let attempts = stand_in
        .calls()
        .iter()
        .filter(|call| call.contains("set-option") && call.contains(BUILD))
        .count();
    assert_eq!(attempts, 2, "one refused, one written");
    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    assert_eq!(row(&status, &pane)["binary"], "current", "{status}");
}

/// Everything in a path that tmux or a shell reads as something else. What the watcher writes
/// has to come back from tmux as it went in, or a watcher on such a taw would be told it was
/// gone, or would never be told it was replaced.
#[test]
fn a_taw_at_a_path_full_of_special_characters_is_judged_by_that_path() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("a 'b' \"c\" $d #{pane_id} %s \\ e:f");
    std::fs::create_dir_all(&dir).unwrap();
    // A name that ends in a `;`, which tmux reads as the end of a command.
    let taw = dir.join("taw;");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let _watcher = InProcessWatcher::start(&server, &taw, &pane);
    await_record(&server, &pane);

    assert!(
        server
            .pane_opt(&pane, BUILD)
            .ends_with(&format!(" {}", taw.display())),
        "the path did not come back as it went in: {:?}",
        server.pane_opt(&pane, BUILD)
    );
    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    assert_eq!(row(&status, &pane)["binary"], "current", "{status}");

    replace_stub(&taw);
    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    let row = row(&status, &pane);
    assert_eq!(row["binary"], "old", "{row}");
    assert_eq!(
        row["binary_note"], "the taw it started from has been replaced since it started",
        "a replaced taw is not a gone one: {row}"
    );
}

#[test]
fn a_watcher_on_the_installed_taw_is_not_marked() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let _watcher = InProcessWatcher::start(&server, &taw, &pane);
    await_record(&server, &pane);

    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    let row = row(&status, &pane);
    assert_eq!(row["watcher_alive"], true, "{row}");
    assert_eq!(row["binary"], "current", "{row}");
    assert!(row["binary_note"].is_null(), "{row}");
    assert_eq!(status["old"], 0, "{status}");
    assert!(status["restart"].is_null(), "{status}");
}

#[test]
fn a_pane_with_no_watcher_has_no_binary_to_judge() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let server = server();
    let pane = server.pane();

    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    let row = row(&status, &pane);
    assert_eq!(row["watcher_alive"], false, "{row}");
    assert!(row["binary"].is_null(), "{row}");
    assert!(row["binary_note"].is_null(), "{row}");
    assert_eq!(status["old"], 0, "{status}");
}

/// `--restart` starts only the watchers whose pipe is open: one whose pipe is closed was let go
/// of and is on its way out. `old` counts what `--restart` would start, so that the sentence
/// that names the command is about the same watchers. The row still says what that process runs.
#[test]
fn a_watcher_whose_pipe_is_closed_is_not_counted_among_those_a_restart_would_start() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let _watcher = InProcessWatcher::start(&server, &taw, &pane);
    await_record(&server, &pane);
    replace_stub(&taw);

    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    assert_eq!(
        status["old"], 1,
        "a watcher on an open pipe counts: {status}"
    );

    // The pipe is closed, and the watcher has not left yet (this one never does).
    server.tmux_ok(&["pipe-pane", "-t", &pane]);
    assert_eq!(server.piped(&pane), "0");
    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    let row = row(&status, &pane);
    assert_eq!(row["piped"], false, "{row}");
    assert_eq!(row["watcher_alive"], true, "{row}");
    assert_eq!(
        row["binary"], "old",
        "the row says what the process runs: {row}"
    );
    assert_eq!(status["old"], 0, "{status}");
    assert!(status["restart"].is_null(), "{status}");
}

/// With watching off a restart starts nothing, so the sentence does not send you to it. It says
/// that watching is off, which is what stands in the way.
#[test]
fn while_watching_is_off_status_says_so_and_names_no_command_to_run() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let _watcher = InProcessWatcher::start(&server, &taw, &pane);
    await_record(&server, &pane);
    replace_stub(&taw);
    server.tmux_ok(&["set-option", "-g", "@agent-wizard-watch", "off"]);

    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    assert_eq!(status["old"], 1, "{status}");
    assert_eq!(
        status["restart"],
        "1 watcher is running an older taw than the one installed. Watching is off (@agent-wizard-watch), so it is not started again until watching is turned on.",
        "{status}"
    );

    // Turned on again, it is the command's turn.
    server.tmux_ok(&["set-option", "-g", "@agent-wizard-watch", "on"]);
    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    assert!(
        status["restart"]
            .as_str()
            .is_some_and(|sentence| sentence.contains("`taw watch attach-all --restart`")),
        "{status}"
    );
}

#[test]
fn a_watcher_whose_taw_was_replaced_is_marked_old_in_plain_words() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let _watcher = InProcessWatcher::start(&server, &taw, &pane);
    await_record(&server, &pane);

    replace_stub(&taw);

    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    let row = row(&status, &pane);
    assert_eq!(row["binary"], "old", "{row}");
    assert_eq!(
        row["binary_note"], "the taw it started from has been replaced since it started",
        "{row}"
    );
    assert_eq!(status["old"], 1, "{status}");
    assert_eq!(
        status["restart"],
        "1 watcher is running an older taw than the one installed. Run `taw watch attach-all --restart` to start it again.",
        "{status}"
    );
}

/// The same bytes copied over the file are a new build as far as a watcher can know, and
/// a file rewritten in place (same inode, new size and time) is one too.
#[test]
fn a_taw_rewritten_in_place_is_a_new_taw() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let _watcher = InProcessWatcher::start(&server, &taw, &pane);
    await_record(&server, &pane);

    // Same file, so the same inode and device. Only its size and its time move.
    write_script(&taw, "exit 0\n# one more line\n");

    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    assert_eq!(row(&status, &pane)["binary"], "old", "{status}");
}

#[test]
fn a_watcher_whose_taw_is_gone_is_marked_old_and_the_note_says_where_it_was() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let _watcher = InProcessWatcher::start(&server, &taw, &pane);
    await_record(&server, &pane);

    std::fs::remove_file(&taw).unwrap();

    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    let row = row(&status, &pane);
    assert_eq!(row["binary"], "old", "{row}");
    assert_eq!(
        row["binary_note"],
        format!("the taw it started from is no longer at {}", taw.display()),
        "{row}"
    );
}

/// Every watcher before this one wrote no record. They are older than the one that does,
/// which is all `old` means, so the first `watch status` after an update names them all.
#[test]
fn a_watcher_that_wrote_no_record_is_an_older_taw() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let _watcher = InProcessWatcher::start(&server, &taw, &pane);
    await_record(&server, &pane);

    server.tmux_ok(&["set-option", "-p", "-u", "-t", &pane, BUILD]);

    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    let row = row(&status, &pane);
    assert_eq!(row["binary"], "old", "{row}");
    assert_eq!(
        row["binary_note"], "started by an older taw, which did not write down which one it was",
        "{row}"
    );
    assert_eq!(status["old"], 1, "{status}");
}

/// A record left on a pane by a watcher that has gone says nothing of the one that runs
/// there now: it names its own pid, and only a record naming the pid in `@agent_watch_pid`
/// counts.
#[test]
fn a_record_of_another_watcher_is_not_this_ones() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let _watcher = InProcessWatcher::start(&server, &taw, &pane);
    await_record(&server, &pane);

    let record = server.pane_opt(&pane, BUILD);
    let (_, rest) = record.split_once(' ').unwrap();
    server.set_pane_opt(&pane, BUILD, &format!("4000000 {rest}"));

    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    let row = row(&status, &pane);
    assert_eq!(row["binary"], "old", "{row}");
    assert_eq!(
        row["binary_note"], "started by an older taw, which did not write down which one it was",
        "{row}"
    );
}

/// A pipe somebody else opened (a log of the pane's output, say) is not a taw watcher,
/// whatever it runs.
#[test]
fn a_pipe_that_is_not_a_taw_watcher_is_not_judged() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let server = server();
    let pane = server.pane();
    server.tmux_ok(&["pipe-pane", "-t", &pane, "cat > /dev/null"]);
    assert!(wait_for(|| server.piped(&pane) == "1", SETTLE));

    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    let row = row(&status, &pane);
    assert_eq!(row["piped"], true, "{row}");
    assert!(row["binary"].is_null(), "{row}");
    assert_eq!(status["old"], 0, "{status}");
}

/// What a watcher that has died left behind (its pid, and its record) says nothing of any
/// watcher: nobody is there to run a taw.
#[test]
fn a_watcher_that_has_died_is_not_judged() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let mut gone = Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    // A record that is the dead watcher's own, of a taw that has since been replaced.
    let identity = taw::watch::binary::Identity::of(&taw).unwrap();
    write_script(&taw, "exit 0\n# replaced\n");
    server.set_pane_opt(&pane, PID, &pid.to_string());
    server.set_pane_opt(&pane, BUILD, &format!("{pid} {identity} {}", taw.display()));

    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    let row = row(&status, &pane);
    assert_eq!(row["watcher_alive"], false, "{row}");
    assert!(row["binary"].is_null(), "{row}");
    assert_eq!(status["old"], 0, "{status}");
}

/// The plugin's `bin/taw` is a link to the build. The watcher says where it was started
/// from, the link, so a link moved to another build is a replacement too, and a build
/// replaced behind a link that has not moved is one as well.
#[test]
fn a_taw_started_through_a_link_is_judged_by_what_the_link_leads_to() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let build = tmp.path().join("build");
    let other = tmp.path().join("other-build");
    let link = tmp.path().join("taw");
    write_script(&build, "exit 0\n");
    write_script(&other, "exit 0\n");
    std::os::unix::fs::symlink(&build, &link).unwrap();
    let server = server();
    let pane = server.pane();
    let _watcher = InProcessWatcher::start(&server, &link, &pane);
    await_record(&server, &pane);

    assert!(
        server
            .pane_opt(&pane, BUILD)
            .ends_with(&format!(" {}", link.display())),
        "the link is the path, not what it leads to: {:?}",
        server.pane_opt(&pane, BUILD)
    );
    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    assert_eq!(row(&status, &pane)["binary"], "current", "{status}");

    // The link is moved to another build: the watcher started from the link, and the link no
    // longer leads to what it started with.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&other, &link).unwrap();
    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    assert_eq!(row(&status, &pane)["binary"], "old", "{status}");

    // Moved back, it leads to the very file the watcher started with.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&build, &link).unwrap();
    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    assert_eq!(row(&status, &pane)["binary"], "current", "{status}");

    // And the build replaced behind a link that has not moved is a replacement as well.
    replace_stub(&build);
    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    assert_eq!(row(&status, &pane)["binary"], "old", "{status}");
}

/// Several old watchers are counted, and the sentence agrees with the count.
#[test]
fn the_restart_sentence_counts_the_old_watchers() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let (one, two) = (server.pane(), another_pane(&server));
    let _a = InProcessWatcher::start(&server, &taw, &one);
    let _b = InProcessWatcher::start(&server, &taw, &two);
    await_record(&server, &one);
    await_record(&server, &two);

    replace_stub(&taw);

    let status = server.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    assert_eq!(status["old"], 2, "{status}");
    assert_eq!(
        status["restart"],
        "2 watchers are running an older taw than the one installed. Run `taw watch attach-all --restart` to start them again.",
        "{status}"
    );
}

// ---------------------------------------------------------------------------------------
// A watcher leaving does not take its successor's record
// ---------------------------------------------------------------------------------------

/// A restart closes the old watcher's pipe and opens the new one's at once, and the old
/// watcher is still on its way out when the new one has written its pid. What the old one
/// clears on its way out is only what is still its own.
#[test]
fn a_watcher_that_leaves_clears_only_its_own_record() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let mut watcher = InProcessWatcher::start(&server, &taw, &pane);
    await_record(&server, &pane);

    // A successor has taken the pane over: its pid and its record.
    server.set_pane_opt(&pane, PID, "4000000");
    server.set_pane_opt(&pane, BUILD, "4000000 1:2:3:4:5 /somewhere/taw");
    watcher.end();

    assert_eq!(server.pane_opt(&pane, PID), "4000000");
    assert_eq!(
        server.pane_opt(&pane, BUILD),
        "4000000 1:2:3:4:5 /somewhere/taw"
    );
}

/// And when nothing has taken the pane over, it leaves neither a pid nor a record that
/// would say a watcher is there.
#[test]
fn a_watcher_that_leaves_takes_its_pid_and_its_record_with_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let mut watcher = InProcessWatcher::start(&server, &taw, &pane);
    await_record(&server, &pane);

    watcher.end();

    assert_eq!(server.pane_opt(&pane, PID), "", "the pid stays behind");
    assert_eq!(server.pane_opt(&pane, BUILD), "", "the record stays behind");
}

// ---------------------------------------------------------------------------------------
// A real watcher, from copies of the built binary
// ---------------------------------------------------------------------------------------

/// The headline: a watcher started from one build, the binary replaced by a copy (a new
/// file), is listed as old; the restart brings it to the new build; a watcher on the
/// current build is not marked and is left running.
#[test]
fn a_restart_brings_an_old_watcher_to_the_new_taw_and_leaves_a_current_one_alone() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    copy_taw(&taw);
    let server = server();
    let (first, second) = (server.pane(), another_pane(&server));

    // The first pane's watcher starts from the taw as it is.
    let attached = result_at(
        &server,
        &taw,
        &["watch", "attach", &first],
        "cli:watch:attach",
    );
    assert_eq!(attached["attached"], true, "{attached}");
    await_record(&server, &first);
    let old_pid = watcher_pid(&server, &first);
    let status = status_at(&server, &taw);
    assert_eq!(row(&status, &first)["binary"], "current", "{status}");
    assert_eq!(status["old"], 0, "{status}");

    // The taw is rebuilt: a new file where it was.
    replace_taw(&taw);

    // The second pane's watcher starts from the new one.
    result_at(
        &server,
        &taw,
        &["watch", "attach", &second],
        "cli:watch:attach",
    );
    await_record(&server, &second);
    let current_pid = watcher_pid(&server, &second);

    let status = status_at(&server, &taw);
    assert_eq!(row(&status, &first)["binary"], "old", "{status}");
    assert_eq!(row(&status, &second)["binary"], "current", "{status}");
    assert_eq!(status["old"], 1, "{status}");

    // Attaching everywhere, which is what the plugin does at start, leaves it as it is.
    let attached = result_at(
        &server,
        &taw,
        &["watch", "attach-all"],
        "cli:watch:attach-all",
    );
    assert_eq!(attached["attached"], 0, "{attached}");
    assert!(
        attached.get("restarted").is_none(),
        "no restart was asked for: {attached}"
    );
    assert_eq!(
        watcher_pid(&server, &first),
        old_pid,
        "the old watcher was touched"
    );

    // One command starts every old watcher again.
    let restarted = result_at(
        &server,
        &taw,
        &["watch", "attach-all", "--restart"],
        "cli:watch:attach-all",
    );
    assert_eq!(restarted["attached"], 0, "{restarted}");
    assert_eq!(restarted["restarted"], 1, "{restarted}");

    assert!(
        wait_for(
            || server
                .pane_opt(&first, PID)
                .parse::<u32>()
                .is_ok_and(|pid| pid != old_pid && alive(pid)),
            SETTLE
        ),
        "the first pane never got a new watcher (pid {:?}, was {old_pid})",
        server.pane_opt(&first, PID)
    );
    await_record(&server, &first);
    assert!(
        wait_for(|| !alive(old_pid), SETTLE),
        "the old watcher is still running"
    );
    assert_eq!(
        build_without_pid(&server, &first),
        build_without_pid(&server, &second),
        "the new watcher runs the same file the current one does"
    );

    // Nothing is old now, and the watcher on the current taw was never touched.
    let status = status_at(&server, &taw);
    assert_eq!(row(&status, &first)["binary"], "current", "{status}");
    assert_eq!(row(&status, &second)["binary"], "current", "{status}");
    assert_eq!(status["old"], 0, "{status}");
    assert!(status["restart"].is_null(), "{status}");
    assert_eq!(
        watcher_pid(&server, &second),
        current_pid,
        "the current watcher was restarted"
    );
    assert!(alive(current_pid));
}

/// Nothing to restart: the command says so and touches nothing.
#[test]
fn a_restart_with_no_old_watcher_starts_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    copy_taw(&taw);
    let server = server();
    let pane = server.pane();
    result_at(
        &server,
        &taw,
        &["watch", "attach", &pane],
        "cli:watch:attach",
    );
    await_record(&server, &pane);
    let pid = watcher_pid(&server, &pane);

    let restarted = result_at(
        &server,
        &taw,
        &["watch", "attach-all", "--restart"],
        "cli:watch:attach-all",
    );
    assert_eq!(restarted["attached"], 0, "{restarted}");
    assert_eq!(restarted["restarted"], 0, "{restarted}");
    assert_eq!(watcher_pid(&server, &pane), pid);
    assert!(alive(pid));
}

/// A watcher of the taw before this one wrote no record, and the restart takes it too.
#[test]
fn a_restart_takes_a_watcher_that_wrote_no_record() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    copy_taw(&taw);
    let server = server();
    let pane = server.pane();
    result_at(
        &server,
        &taw,
        &["watch", "attach", &pane],
        "cli:watch:attach",
    );
    await_record(&server, &pane);
    let old_pid = watcher_pid(&server, &pane);
    server.tmux_ok(&["set-option", "-p", "-u", "-t", &pane, BUILD]);
    assert_eq!(row(&status_at(&server, &taw), &pane)["binary"], "old");

    let restarted = result_at(
        &server,
        &taw,
        &["watch", "attach-all", "--restart"],
        "cli:watch:attach-all",
    );
    assert_eq!(restarted["restarted"], 1, "{restarted}");

    assert!(
        wait_for(
            || server
                .pane_opt(&pane, PID)
                .parse::<u32>()
                .is_ok_and(|pid| pid != old_pid && alive(pid))
                && !server.pane_opt(&pane, BUILD).is_empty(),
            SETTLE
        ),
        "no new watcher with a record (pid {:?}, record {:?})",
        server.pane_opt(&pane, PID),
        server.pane_opt(&pane, BUILD)
    );
    assert_eq!(row(&status_at(&server, &taw), &pane)["binary"], "current");
}

/// The watcher starts again from where it started, so whichever taw types the command, the
/// watcher comes back on the taw that is installed there now. A taw of another build, run
/// from another directory, must not take the user's watchers with it.
#[test]
fn a_restart_starts_each_watcher_from_where_it_started_whichever_taw_ran_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let installed = tmp.path().join("taw");
    let other = tmp.path().join("taw-elsewhere");
    copy_taw(&installed);
    copy_taw(&other);
    let server = server();
    let pane = server.pane();
    result_at(
        &server,
        &installed,
        &["watch", "attach", &pane],
        "cli:watch:attach",
    );
    await_record(&server, &pane);
    let old_pid = watcher_pid(&server, &pane);
    let started_from = build_without_pid(&server, &pane);
    assert!(
        started_from.ends_with(&installed.display().to_string()),
        "{started_from}"
    );
    replace_taw(&installed);

    let restarted = result_at(
        &server,
        &other,
        &["watch", "attach-all", "--restart"],
        "cli:watch:attach-all",
    );
    assert_eq!(restarted["restarted"], 1, "{restarted}");

    assert!(
        wait_for(
            || server
                .pane_opt(&pane, PID)
                .parse::<u32>()
                .is_ok_and(|pid| pid != old_pid && alive(pid))
                && server
                    .pane_opt(&pane, BUILD)
                    .ends_with(&installed.display().to_string()),
            SETTLE
        ),
        "the watcher did not come back from {} (record {:?})",
        installed.display(),
        server.pane_opt(&pane, BUILD)
    );
    assert!(
        !server.pane_opt(&pane, BUILD).contains("taw-elsewhere"),
        "the watcher moved to the taw that ran the command: {:?}",
        server.pane_opt(&pane, BUILD)
    );
}

/// When the file the watcher started from is gone, there is nothing there to start it
/// from, and the taw that ran the command is the one that is installed.
#[test]
fn a_restart_uses_the_taw_that_ran_it_when_the_old_one_is_gone() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let installed = tmp.path().join("taw");
    let moved_to = tmp.path().join("taw-moved");
    copy_taw(&installed);
    let server = server();
    let pane = server.pane();
    result_at(
        &server,
        &installed,
        &["watch", "attach", &pane],
        "cli:watch:attach",
    );
    await_record(&server, &pane);
    let old_pid = watcher_pid(&server, &pane);
    // Reinstalled somewhere else: the path the watcher knows is empty.
    std::fs::rename(&installed, &moved_to).unwrap();
    let status = status_at(&server, &moved_to);
    assert_eq!(row(&status, &pane)["binary"], "old", "{status}");

    let restarted = result_at(
        &server,
        &moved_to,
        &["watch", "attach-all", "--restart"],
        "cli:watch:attach-all",
    );
    assert_eq!(restarted["restarted"], 1, "{restarted}");

    assert!(
        wait_for(
            || server
                .pane_opt(&pane, PID)
                .parse::<u32>()
                .is_ok_and(|pid| pid != old_pid && alive(pid))
                && server
                    .pane_opt(&pane, BUILD)
                    .ends_with(&moved_to.display().to_string()),
            SETTLE
        ),
        "the watcher did not come back from {} (record {:?})",
        moved_to.display(),
        server.pane_opt(&pane, BUILD)
    );
    assert_eq!(
        row(&status_at(&server, &moved_to), &pane)["binary"],
        "current"
    );
}

/// `@agent-wizard-watch off` means no watchers, and a restart that closed the old ones'
/// pipes could not open new ones. It leaves them as they are.
#[test]
fn a_restart_leaves_the_watchers_alone_while_watching_is_off() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    copy_taw(&taw);
    let server = server();
    let pane = server.pane();
    result_at(
        &server,
        &taw,
        &["watch", "attach", &pane],
        "cli:watch:attach",
    );
    await_record(&server, &pane);
    let pid = watcher_pid(&server, &pane);
    replace_taw(&taw);
    server.tmux_ok(&["set-option", "-g", "@agent-wizard-watch", "off"]);

    let restarted = result_at(
        &server,
        &taw,
        &["watch", "attach-all", "--restart"],
        "cli:watch:attach-all",
    );
    assert_eq!(restarted["attached"], 0, "{restarted}");
    assert_eq!(restarted["restarted"], 0, "{restarted}");

    assert_eq!(server.piped(&pane), "1", "the pipe was closed");
    assert_eq!(watcher_pid(&server, &pane), pid);
    assert!(alive(pid), "the watcher was ended");
}

/// A watcher whose pipe is closed was let go of (by `taw watch detach`, say) and is on its way
/// out. Nobody asked for a watcher on that pane, so the restart does not open one.
#[test]
fn a_restart_does_not_open_a_watcher_on_a_pane_whose_pipe_was_closed() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let _watcher = InProcessWatcher::start(&server, &taw, &pane);
    await_record(&server, &pane);
    replace_stub(&taw);
    // Alive, with a record of a taw that has been replaced, and with its pipe closed.
    server.tmux_ok(&["pipe-pane", "-t", &pane]);
    assert_eq!(server.piped(&pane), "0");

    let result = watch::restart_old(&tmux_of(&server), &taw).unwrap();

    assert_eq!(result, Restart::default());
    assert_eq!(server.piped(&pane), "0", "a pipe was opened on the pane");
}

/// A process that stands in for an old watcher that is slow to leave: alive, and named in the
/// pane's options as the watcher of a taw that is not there any more, with a pipe of its own
/// open on the pane. A real watcher leaves the moment its pipe closes, which leaves no time to
/// look at what is done in between.
struct SlowOldWatcher {
    child: Child,
}

impl SlowOldWatcher {
    fn on(server: &Server, pane: &str) -> SlowOldWatcher {
        let child = Command::new("sleep").arg("120").spawn().unwrap();
        let pid = child.id();
        server.set_pane_opt(pane, BUILD, &format!("{pid} 1:2:3:4:5 /nonexistent/taw"));
        server.set_pane_opt(pane, PID, &pid.to_string());
        server.tmux_ok(&["pipe-pane", "-t", pane, "cat > /dev/null"]);
        assert!(wait_for(|| server.piped(pane) == "1", SETTLE));
        SlowOldWatcher { child }
    }

    /// What a watcher does as it leaves: takes its pid and its record off the pane.
    fn leaves(&self, server: &Server, pane: &str) {
        server.tmux_ok(&["set-option", "-p", "-u", "-t", pane, PID]);
        server.tmux_ok(&["set-option", "-p", "-u", "-t", pane, BUILD]);
    }
}

impl Drop for SlowOldWatcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What a restart does in a thread, with the time it may wait for the old watchers given to it.
/// A test that fails before the thread has finished leaves it behind, so the time is a minute and
/// not more: it stops soon, as it does as soon as the old watcher it waits for is killed.
fn restart_in_a_thread(
    t: Tmux,
    taw_bin: PathBuf,
    wait: Duration,
) -> JoinHandle<anyhow::Result<Restart>> {
    std::thread::spawn(move || watch::restart_old_waiting(&t, &taw_bin, wait))
}

/// The time a test gives a restart that is to be let go on by the old watcher leaving, and by
/// nothing else.
const LONG_WAIT: Duration = Duration::from_secs(60);

/// A new watcher of `pane` that is not the one `old_pid` was: its pipe is open and its pid is
/// another process, alive.
fn a_new_watcher_is_on(server: &Server, pane: &str, old_pid: u32) -> bool {
    wait_for(
        || {
            server.piped(pane) == "1"
                && server
                    .pane_opt(pane, PID)
                    .parse::<u32>()
                    .is_ok_and(|pid| pid != old_pid && alive(pid))
        },
        SETTLE,
    )
}

/// The old watcher's pipe is closed, and the next watcher is not opened until the old one has
/// gone from the pane: two watchers reading one pane at once could announce one change twice.
/// The restart is given far longer than the test takes, so that only the old watcher leaving can
/// let it go on, and the test never waits on a clock for that.
#[test]
fn a_restart_waits_for_the_old_watcher_to_leave_before_it_starts_the_next() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let server = server();
    let pane = server.pane();
    let old = SlowOldWatcher::on(&server, &pane);
    let old_pid = old.child.id();

    let restart = restart_in_a_thread(
        tmux_of(&server),
        PathBuf::from(common::taw_bin()),
        LONG_WAIT,
    );
    assert!(
        wait_for(|| server.piped(&pane) == "0", SETTLE),
        "the restart never closed the old watcher's pipe"
    );
    // The old watcher is on the pane: give the restart every chance to go on without it.
    assert!(
        !wait_for(
            || restart.is_finished() || server.piped(&pane) == "1",
            Duration::from_millis(500)
        ),
        "the restart went on while the old watcher was still on the pane"
    );

    old.leaves(&server, &pane);

    assert!(
        wait_for(|| restart.is_finished(), SETTLE),
        "the restart did not go on once the old watcher had left"
    );
    let result = restart.join().unwrap();
    assert_eq!(
        result.unwrap(),
        Restart {
            restarted: 1,
            failed: 0
        }
    );
    assert!(
        a_new_watcher_is_on(&server, &pane, old_pid),
        "no new watcher on the pane (pid {:?})",
        server.pane_opt(&pane, PID)
    );
}

/// A watcher that will not leave holds nothing up for ever: the restart goes on once the time it
/// was given is up. What is asserted is that it looked for the old watcher again after a pause,
/// which the work it does besides cannot do however slow the machine is, and that it then went on.
/// The time is a second: the first look has to come before it is up, and the test is that long.
#[test]
fn a_restart_goes_on_when_the_old_watcher_does_not_leave() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let server = server();
    let pane = server.pane();
    let stand_in = StandIn::new(&server);
    let old = SlowOldWatcher::on(&server, &pane);
    let old_pid = old.child.id();

    let restart = restart_in_a_thread(
        stand_in.tmux(&server),
        PathBuf::from(common::taw_bin()),
        Duration::from_secs(1),
    );

    assert!(
        wait_for(|| restart.is_finished(), SETTLE),
        "the restart waited for ever for a watcher that was not leaving"
    );
    let result = restart.join().unwrap();
    assert_eq!(
        result.unwrap(),
        Restart {
            restarted: 1,
            failed: 0
        }
    );
    let listings = stand_in.departure_listings();
    assert!(
        listings >= 2,
        "it did not look for the old watcher again before it went on: {listings} listing(s) in {:#?}",
        stand_in.calls()
    );
    assert!(
        a_new_watcher_is_on(&server, &pane, old_pid),
        "no new watcher on the pane (pid {:?})",
        server.pane_opt(&pane, PID)
    );
}

/// A listing of who is on the panes that fails says nothing of who has left, and the restart
/// takes every old watcher to be there until a listing says otherwise.
#[test]
fn a_restart_does_not_take_a_failed_listing_for_every_old_watcher_gone() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let server = server();
    let pane = server.pane();
    let stand_in = StandIn::new(&server);
    let old = SlowOldWatcher::on(&server, &pane);
    let old_pid = old.child.id();
    stand_in.arm("fail-listing");

    let restart = restart_in_a_thread(
        stand_in.tmux(&server),
        PathBuf::from(common::taw_bin()),
        LONG_WAIT,
    );
    assert!(wait_for(|| server.piped(&pane) == "0", SETTLE));
    // A listing has failed: were the stand-in never to refuse one (the call it matches has
    // changed), this test would see an old watcher still there and pass for that reason.
    assert!(
        wait_for(|| stand_in.happened("fail-listing") >= 1, SETTLE),
        "the stand-in never refused a listing: {:#?}",
        stand_in.calls()
    );
    assert!(
        !wait_for(
            || restart.is_finished() || server.piped(&pane) == "1",
            Duration::from_millis(500)
        ),
        "the restart took a listing that failed for every old watcher having left"
    );

    // The old watcher leaves, and a listing works again.
    old.leaves(&server, &pane);
    stand_in.disarm("fail-listing");

    assert!(wait_for(|| restart.is_finished(), SETTLE));
    let result = restart.join().unwrap();
    assert_eq!(result.unwrap().restarted, 1);
    assert!(a_new_watcher_is_on(&server, &pane, old_pid));
}

/// A watcher the restart let go of whose successor cannot be started leaves its pane with none.
/// The result counts it and says so in words, and the command it names starts one.
#[test]
fn a_restart_says_which_watchers_could_not_be_started_again() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let server = server();
    let pane = server.pane();
    let stand_in = StandIn::new(&server);
    let old_pid = old_watcher_that_leaves_at_eof(&server, &pane, tmp.path());
    stand_in.arm("fail-attach-once");

    let result = result_through(
        &server,
        &stand_in.bin,
        &["watch", "attach-all", "--restart"],
    );

    assert_eq!(result["attached"], 0, "{result}");
    assert_eq!(result["restarted"], 0, "{result}");
    assert_eq!(result["failed"], 1, "{result}");
    assert_eq!(
        result["note"],
        "1 watcher could not be started again, and its pane has none now. Run `taw watch attach-all` to try again.",
        "{result}"
    );
    assert_eq!(
        stand_in.happened("fail-attach-once"),
        1,
        "the stand-in never refused the attach: {:#?}",
        stand_in.calls()
    );
    assert_eq!(server.piped(&pane), "0", "the pane has no watcher now");

    // The command the note names does start one.
    let again = result_through(&server, &stand_in.bin, &["watch", "attach-all"]);
    assert_eq!(again["attached"], 1, "{again}");
    assert!(a_new_watcher_is_on(&server, &pane, old_pid));
}

/// Watching turned off in the middle of a restart: the old watcher was let go of, and the next
/// cannot be opened under it. The pane has none, which is a failure as much as tmux refusing,
/// and the result says so.
#[test]
fn a_restart_counts_a_watcher_it_could_not_start_because_watching_was_turned_off_meanwhile() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let server = server();
    let pane = server.pane();
    let stand_in = StandIn::new(&server);
    let _old_pid = old_watcher_that_leaves_at_eof(&server, &pane, tmp.path());
    stand_in.arm("off-after-detach");

    let result = result_through(
        &server,
        &stand_in.bin,
        &["watch", "attach-all", "--restart"],
    );

    assert_eq!(
        stand_in.happened("off-after-detach"),
        1,
        "the stand-in never turned watching off: {:#?}",
        stand_in.calls()
    );
    assert_eq!(server.global_opt("@agent-wizard-watch"), "off");
    assert_eq!(result["restarted"], 0, "{result}");
    assert_eq!(result["failed"], 1, "{result}");
    assert_eq!(
        result["note"],
        "1 watcher could not be started again, and its pane has none now. Run `taw watch attach-all` to try again.",
        "{result}"
    );
    assert_eq!(server.piped(&pane), "0", "the pane has no watcher now");
}

/// `status` asks whether watching is on only when it has an old watcher to say something about,
/// so that a listing with none costs no tmux call for it.
#[test]
fn status_reads_the_watching_option_only_when_there_is_an_old_watcher_to_tell_about() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let taw = tmp.path().join("taw");
    write_script(&taw, "exit 0\n");
    let server = server();
    let pane = server.pane();
    let stand_in = StandIn::new(&server);
    let reads = |stand_in: &StandIn| {
        stand_in
            .calls()
            .iter()
            .filter(|call| call.contains("show-option") && call.contains("@agent-wizard-watch"))
            .count()
    };

    // A watcher on the taw that is installed: nothing to tell about.
    let _watcher = InProcessWatcher::start(&server, &taw, &pane);
    await_record(&server, &pane);
    let status = result_through(&server, &stand_in.bin, &["watch", "status"]);
    assert_eq!(status["old"], 0, "{status}");
    assert!(status["restart"].is_null(), "{status}");
    assert_eq!(
        reads(&stand_in),
        0,
        "the option was read with nothing old to tell about: {:#?}",
        stand_in.calls()
    );

    // Now the taw is replaced, and the sentence needs to know.
    replace_stub(&taw);
    let status = result_through(&server, &stand_in.bin, &["watch", "status"]);
    assert_eq!(status["old"], 1, "{status}");
    assert!(status["restart"].is_string(), "{status}");
    assert_eq!(reads(&stand_in), 1, "{:#?}", stand_in.calls());
}

/// Nothing failed: no count of failures to worry about, and no note.
#[test]
fn a_restart_that_started_everything_has_no_failures_and_no_note() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let server = server();
    let pane = server.pane();
    let stand_in = StandIn::new(&server);
    let old_pid = old_watcher_that_leaves_at_eof(&server, &pane, tmp.path());

    let result = result_through(
        &server,
        &stand_in.bin,
        &["watch", "attach-all", "--restart"],
    );

    assert_eq!(result["restarted"], 1, "{result}");
    assert_eq!(result["failed"], 0, "{result}");
    assert!(result["note"].is_null(), "{result}");
    assert!(a_new_watcher_is_on(&server, &pane, old_pid));
}

#[test]
fn restart_is_a_flag_of_attach_all_and_says_what_it_does() {
    let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
    let server = server();
    let (code, stdout, _) = server.taw_raw(&["watch", "attach-all", "--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("--restart"), "{stdout}");
    assert!(
        stdout.contains("older taw") || stdout.contains("replaced"),
        "the flag should say which watchers it takes: {stdout}"
    );
}
