//! A new tmux server after one that crashed, end to end through the built `taw` binary and the real
//! plugin script on a private socket: the crash report macOS wrote is kept in taw's own folder, and
//! the user is told where, once.
//!
//! The reports folder is a fake one of the test's (`$TAW_CRASH_REPORTS_DIR`); the user's real
//! `~/Library/Logs/DiagnosticReports` is never read. The "last server" is a record in the state
//! directory holding the pid of a process that has ended. Toasts go through `system` delivery to the
//! recording `$TAW_NOTIFY_CMD` stub.
//!
//! The reports are kept on macOS only, so these tests are too.
#![cfg(target_os = "macos")]

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use common::{NotifyRecorder, Server, ServerOptions, wait_for};
use serde_json::{Value, json};

const SETTLE: Duration = Duration::from_secs(30);

const CAPTURE: &str = "2026-10-16 00:12:33.1234 +0200";
/// The title that capture time gives with `TZ=UTC` in the server's environment.
const TITLE: &str = "tmux crashed at 22:12:33";

fn config(delivery: &str) -> String {
    format!(
        "[update]\nmanifest_check = false\n\n[ui.toast]\ndelivery = \"{delivery}\"\n\n[ui.sound]\nenabled = false\n"
    )
}

/// A crash report as macOS writes one: its own header on the first line, then the body.
fn ips(pid: u32, proc_name: &str) -> String {
    format!(
        "{{\"app_name\":\"tmux\",\"bug_type\":\"309\"}}\n{{\"pid\":{pid},\"procName\":\"{proc_name}\",\"captureTime\":\"{CAPTURE}\",\"exception\":{{\"type\":\"EXC_BAD_ACCESS\"}}}}\n"
    )
}

/// The pid of a process that has ended and been waited for.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

struct Rig {
    server: Server,
    reports: tempfile::TempDir,
    recorder: NotifyRecorder,
    _tmp: tempfile::TempDir,
}

impl Rig {
    /// A server that finds reports in a folder of the test's, looks for one for `watch_secs` seconds
    /// and waits `client_secs` for a client, with toasts recorded.
    fn start(delivery: &str, watch_secs: &str, client_secs: &str) -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let reports = tempfile::tempdir().unwrap();
        let recorder = NotifyRecorder::new(tmp.path());
        let server = Server::start_with(ServerOptions {
            env: vec![
                (
                    "TAW_NOTIFY_CMD".to_string(),
                    recorder.script.display().to_string(),
                ),
                (
                    "TAW_CRASH_REPORTS_DIR".to_string(),
                    reports.path().display().to_string(),
                ),
                ("TAW_CRASH_WATCH_SECS".to_string(), watch_secs.to_string()),
                ("TAW_CRASH_POLL_MS".to_string(), "100".to_string()),
                (
                    "TAW_CRASH_CLIENT_WAIT_SECS".to_string(),
                    client_secs.to_string(),
                ),
                ("TZ".to_string(), "UTC".to_string()),
            ],
            ..ServerOptions::default()
        });
        server.write_config(&config(delivery));
        Rig {
            server,
            reports,
            recorder,
            _tmp: tmp,
        }
    }

    fn socket_path(&self) -> String {
        self.server
            .tmux_ok(&["display-message", "-p", "#{socket_path}"])
    }

    fn live_pid(&self) -> u32 {
        self.server
            .tmux_ok(&["display-message", "-p", "#{pid}"])
            .parse()
            .unwrap()
    }

    fn record_path(&self) -> PathBuf {
        self.server
            .state_dir()
            .join("servers")
            .join(taw::crash::record_file_name(&self.socket_path()))
    }

    /// The record the last server on this socket left: `pid`, started at unix second 1.
    fn leave_record(&self, pid: u32) {
        let path = self.record_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, json!({"pid": pid, "start_time": 1}).to_string()).unwrap();
    }

    fn record(&self) -> Option<Value> {
        serde_json::from_str(&std::fs::read_to_string(self.record_path()).ok()?).ok()
    }

    fn write_report(&self, name: &str, pid: u32, proc_name: &str) -> PathBuf {
        let path = self.reports.path().join(name);
        std::fs::write(&path, ips(pid, proc_name)).unwrap();
        path
    }

    fn crashes_dir(&self) -> PathBuf {
        self.server.state_dir().join("tmux-crashes")
    }

    fn copies(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.crashes_dir())
            .map(|entries| {
                entries
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// `taw server started`, run at once and waited for, with its envelope's result.
    fn started(&self) -> Value {
        let (code, env) = self.server.taw(&["server", "started"]);
        assert_eq!(code, 0, "{env}");
        assert_eq!(env["id"], "cli:server:started", "{env}");
        assert_eq!(env["result"]["type"], "server_started", "{env}");
        env["result"].clone()
    }
}

/// The ids of the jobs the server runs for `server started`: its children whose command line says so.
fn started_jobs(server_pid: u32) -> usize {
    let out = Command::new("ps")
        .args(["-ww", "-axo", "ppid=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            line.split_whitespace().next() == Some(&server_pid.to_string())
                && line.contains("server started")
        })
        .count()
}

/// Test 1: the plugin loads on a server whose record names a pid that is gone, and a matching report
/// is in the folder: it is copied, the user is told where once, and the record is the live server's.
/// A reload does nothing.
#[test]
fn the_plugin_keeps_the_report_of_the_server_before_and_says_where_once() {
    let rig = Rig::start("system", "10", "10");
    let gone = dead_pid();
    rig.leave_record(gone);
    let report = rig.write_report("tmux-2026-10-16-001233.ips", gone, "tmux");

    rig.server.source_plugin();
    assert!(
        wait_for(|| rig.recorder.lines().len() == 1, SETTLE),
        "no toast: copies {:?}, record {:?}, notifications {:?}",
        rig.copies(),
        rig.record(),
        rig.recorder.lines()
    );

    assert_eq!(rig.copies(), ["tmux-2026-10-16-001233.ips"]);
    let kept = rig.crashes_dir().join("tmux-2026-10-16-001233.ips");
    assert_eq!(
        std::fs::read(&kept).unwrap(),
        std::fs::read(&report).unwrap()
    );
    assert_eq!(
        rig.recorder.lines(),
        [format!(
            "{TITLE}|Its crash report is kept at {}. taw session restore brings back what it had.",
            kept.display()
        )]
    );
    // The record is the live server's now.
    let record = rig.record().unwrap();
    assert_eq!(record["pid"], rig.live_pid());
    assert!(record["start_time"].as_u64().unwrap() > 1, "{record}");

    // A reload, as a source-file of tmux.conf makes: the same server, so nothing more is done.
    let conf = rig.server.work_dir().join("reload.conf");
    std::fs::write(
        &conf,
        format!(
            "run-shell \"TAW_BIN='{}' bash '{}'\"\n",
            common::taw_bin(),
            common::plugin_script().display()
        ),
    )
    .unwrap();
    // Another report for the gone server, which a reload that took itself for a new server would keep.
    rig.write_report("tmux-2026-10-16-001234.ips", gone, "tmux");
    let before = std::fs::read_to_string(rig.record_path()).unwrap();
    let server_pid = rig.live_pid();
    rig.server.tmux_ok(&["source-file", conf.to_str().unwrap()]);
    assert!(
        wait_for(|| started_jobs(server_pid) == 0, SETTLE),
        "the one-shot of the reload never ended"
    );
    assert_eq!(rig.copies(), ["tmux-2026-10-16-001233.ips"]);
    assert_eq!(rig.recorder.lines().len(), 1);
    assert_eq!(std::fs::read_to_string(rig.record_path()).unwrap(), before);
}

/// Test 2: a report for another pid, and one of another program, are not the last server's.
#[test]
fn a_report_of_another_pid_or_program_is_not_kept_and_nothing_is_said() {
    let rig = Rig::start("system", "1", "1");
    let gone = dead_pid();
    rig.leave_record(gone);
    rig.write_report("tmux-2026-10-16-000001.ips", gone.wrapping_add(7), "tmux");
    rig.write_report("tmux-2026-10-16-000002.ips", gone, "node");
    // The first test is the control: a report that does match is kept. Here the record is replaced,
    // which only a run that took the server for a new one does, so the search was made and found
    // nothing.
    let result = rig.started();
    assert_eq!(result["record"], "written", "{result}");
    assert_eq!(result["crash_report"], Value::Null, "{result}");
    assert_eq!(result["announced"], false, "{result}");
    assert_eq!(rig.record().unwrap()["pid"], rig.live_pid());
    assert!(rig.copies().is_empty(), "{:?}", rig.copies());
    assert!(
        rig.recorder.lines().is_empty(),
        "{:?}",
        rig.recorder.lines()
    );
}

/// Test 3: with no record at all, there is no last server: nothing is looked for or kept, and the
/// server is written down.
#[test]
fn with_no_record_nothing_is_kept_and_the_server_is_written_down() {
    let rig = Rig::start("system", "1", "1");
    // A report for a pid that is gone, which only a run that took it for the last server's would keep.
    rig.write_report("tmux-2026-10-16-000001.ips", dead_pid(), "tmux");
    assert!(rig.record().is_none());
    let result = rig.started();
    assert_eq!(result["record"], "written", "{result}");
    assert_eq!(result["crash_report"], Value::Null, "{result}");
    assert_eq!(rig.record().unwrap()["pid"], rig.live_pid());
    assert!(rig.copies().is_empty());
    assert!(rig.recorder.lines().is_empty());
    // The same server again is a reload, which says so.
    assert_eq!(rig.started()["record"], "unchanged");
}

/// Test 4: a last server that is still running did not crash: its pid is there, so no report is
/// looked for, though one for that pid is in the folder.
#[test]
fn a_last_server_that_is_still_running_is_not_taken_for_a_crash() {
    let rig = Rig::start("system", "1", "1");
    let alive = std::process::id();
    rig.leave_record(alive);
    rig.write_report("tmux-2026-10-16-000001.ips", alive, "tmux");
    let result = rig.started();
    assert_eq!(result["record"], "written", "{result}");
    assert_eq!(result["crash_report"], Value::Null, "{result}");
    assert!(rig.copies().is_empty(), "{:?}", rig.copies());
    assert!(rig.recorder.lines().is_empty());
}

/// Test 5: a toast that is drawn on a client waits for one, and with none attached the copy stays
/// and nothing is shown.
#[test]
fn a_toast_that_needs_a_client_is_not_shown_when_none_comes_but_the_copy_stays() {
    for delivery in ["herdr", "terminal"] {
        let rig = Rig::start(delivery, "5", "1");
        let gone = dead_pid();
        rig.leave_record(gone);
        rig.write_report("tmux-2026-10-16-001233.ips", gone, "tmux");
        let result = rig.started();
        assert_eq!(result["announced"], false, "{delivery}: {result}");
        assert_eq!(
            rig.copies(),
            ["tmux-2026-10-16-001233.ips"],
            "{delivery}: {result}"
        );
        assert_eq!(
            result["crash_report"],
            rig.crashes_dir()
                .join("tmux-2026-10-16-001233.ips")
                .display()
                .to_string(),
            "{delivery}"
        );
        assert!(rig.recorder.lines().is_empty(), "{delivery}");
    }
}

/// A fake reports folder is the only one this file reads: the default is under the home of the
/// server's shells, which is a folder of the fixture's own.
#[test]
fn the_default_reports_folder_is_under_the_home_of_the_server() {
    let rig = Rig::start("system", "1", "1");
    let home = rig.server.home_dir();
    let folder = home.join("Library/Logs/DiagnosticReports");
    std::fs::create_dir_all(&folder).unwrap();
    let gone = dead_pid();
    rig.leave_record(gone);
    std::fs::write(folder.join("tmux-2026-10-16-001233.ips"), ips(gone, "tmux")).unwrap();
    // The seam is taken away for this one run, as it is on a real machine.
    let (code, env) = rig
        .server
        .taw_with_env(&["server", "started"], &[("TAW_CRASH_REPORTS_DIR", "")]);
    assert_eq!(code, 0, "{env}");
    assert_eq!(
        env["result"]["crash_report"],
        rig.crashes_dir()
            .join("tmux-2026-10-16-001233.ips")
            .display()
            .to_string(),
        "{env}"
    );
}
