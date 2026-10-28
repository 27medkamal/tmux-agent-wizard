//! Every job the plugin hands tmux exits 0, end to end on a private server: the plugin script loaded
//! with `@agent-wizard-bin` naming a stand-in that answers `--version` like taw and fails every
//! other call, the hooks driven, and no pane left in a mode.
//!
//! A job that exits non-zero makes tmux print `'<command>' returned 1` into the job's pane (a view
//! mode), and with that pane gone with its session, into whichever pane it finds instead
//! (`cmd_find_from_nothing`, which crashes a tmux 3.6a or 3.6b server while a session is being
//! closed). The jobs' output is discarded already; their exit status is what these tests are about.

#[path = "clients/mod.rs"]
mod clients;
mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use clients::Client;
use common::{Server, ServerOptions, wait_for, write_script};
use taw::tmux::Tmux;

const SETTLE: Duration = Duration::from_secs(30);

/// A program that answers `--version` the way taw does and exits 1 for every other call, after
/// writing one line (its arguments) to `log` for each.
fn stand_in(dir: &Path, log: &Path) -> PathBuf {
    let path = dir.join("taw");
    write_script(
        &path,
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'\nif [ \"$1\" = \"--version\" ]; then echo 'taw 0.0.0-stand-in'; exit 0; fi\nexit 1\n",
            log.display()
        ),
    );
    path
}

fn calls(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// How many of the server's jobs (children whose command line names the stand-in) are still running.
fn jobs_running(server_pid: &str, stand_in: &Path) -> usize {
    let out = Command::new("ps")
        .args(["-ww", "-axo", "ppid=,command="])
        .output()
        .unwrap();
    let stand_in = stand_in.display().to_string();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            line.split_whitespace().next() == Some(server_pid) && line.contains(&stand_in)
        })
        .count()
}

/// `pane id and mode` of every pane of the server, one a line: `%3 0`.
fn pane_modes(server: &Server) -> Vec<String> {
    server
        .tmux_ok(&["list-panes", "-a", "-F", "#{pane_id} #{pane_in_mode}"])
        .lines()
        .map(str::to_string)
        .collect()
}

/// A server with the plugin loaded over the stand-in, and the jobs of the load ended.
struct Rig {
    server: Server,
    /// The barrier pane: alone in a session that stays open, made before the plugin loads (so before
    /// any job), and told by `barrier_check` to end a job of its own.
    barrier: String,
    log: PathBuf,
    stand_in: PathBuf,
    _tmp: tempfile::TempDir,
}

impl Rig {
    fn start() -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls.log");
        let stand_in = stand_in(tmp.path(), &log);
        let server = Server::start_with(ServerOptions {
            // `TAW_BIN` would name another binary than the option does.
            env_remove: vec!["TAW_BIN".to_string()],
            ..ServerOptions::default()
        });
        // The barrier session is made first, so that the plugin's hooks do not run for it.
        server.tmux_ok(&["new-session", "-d", "-s", "barrier", "-x", "80", "-y", "24"]);
        let barrier = server.pane_of("barrier");
        server.write_config("[update]\nmanifest_check = false\n\n[ui.toast]\ndelivery = \"off\"\n");
        server.tmux_ok(&[
            "set-option",
            "-g",
            "@agent-wizard-bin",
            stand_in.to_str().unwrap(),
        ]);
        server.tmux_ok(&[
            "run-shell",
            &format!("bash '{}'", common::plugin_script().display()),
        ]);
        let rig = Rig {
            server,
            barrier,
            log,
            stand_in,
            _tmp: tmp,
        };
        // The startup one-shots are the first calls, and a positive check that the plugin took the
        // stand-in for taw.
        for verb in [
            "watch attach-all",
            "status refresh",
            "plugin startup",
            "manifest update --if-stale",
            "server started",
        ] {
            rig.wait_for_call(verb);
        }
        rig.wait_for_jobs_to_end();
        rig
    }

    fn pid(&self) -> String {
        self.server.tmux_ok(&["display-message", "-p", "#{pid}"])
    }

    /// Waits until the stand-in has been called with `start` as the first words of its arguments.
    fn wait_for_call(&self, start: &str) {
        assert!(
            wait_for(
                || calls(&self.log)
                    .iter()
                    .any(|call| call == start || call.starts_with(&format!("{start} "))),
                SETTLE
            ),
            "the stand-in was never called with `{start}`: {:?}",
            calls(&self.log)
        );
    }

    fn wait_for_jobs_to_end(&self) {
        let pid = self.pid();
        assert!(
            wait_for(|| jobs_running(&pid, &self.stand_in) == 0, SETTLE),
            "jobs still running: {:?}",
            calls(&self.log)
        );
    }

    /// `pane id and mode` of every pane but the barrier's.
    fn other_pane_modes(&self) -> Vec<String> {
        let own = format!("{} ", self.barrier);
        pane_modes(&self.server)
            .into_iter()
            .filter(|line| !line.starts_with(&own))
            .collect()
    }

    /// Proves that tmux has handled every job that ended before this call, then reads what it did
    /// with them.
    ///
    /// `ps` stops listing a job when its shell exits, but tmux prints `returned 1` and sets view mode
    /// only later, when it reaps the job and sees its pipe close. So a job of the barrier's own, `exit 7`,
    /// goes in after the others: once the barrier pane is in a mode, tmux has been through every job
    /// that ended before the barrier's did. Then every other pane must be out of a mode, and (tmux 3.6
    /// and later, which has `capture-pane -M`) the barrier pane's mode screen holds its own `returned 7`
    /// line and no other `returned` line: a message whose pane has gone can land in any pane, the
    /// barrier's included, and that is the one place to see it.
    fn barrier_check(&self) {
        let server = &self.server;
        assert_eq!(
            server.tmux_ok(&[
                "display-message",
                "-p",
                "-t",
                &self.barrier,
                "#{pane_in_mode}"
            ]),
            "0",
            "the barrier pane was in a mode before the barrier job: calls: {:?}",
            calls(&self.log)
        );
        server.tmux_ok(&["run-shell", "-b", "-t", &self.barrier, "exit 7"]);
        assert!(
            wait_for(
                || server.tmux_ok(&[
                    "display-message",
                    "-p",
                    "-t",
                    &self.barrier,
                    "#{pane_in_mode}"
                ]) == "1",
                SETTLE
            ),
            "tmux never put the barrier pane in a mode for its `exit 7`"
        );

        let modes = self.other_pane_modes();
        assert!(!modes.is_empty());
        for mode in &modes {
            assert!(
                mode.ends_with(" 0"),
                "a pane is in a mode: {modes:?}\ncalls: {:?}",
                calls(&self.log)
            );
        }

        // `capture-pane -M` (the mode screen) is tmux 3.6 and later; an older tmux has no such flag,
        // so only this content check is skipped there. `-J` joins a message the mode screen wrapped
        // at the pane's width, so a `returned` split across two rows still counts.
        let inner = Tmux::with_socket_name(server.socket());
        if inner.version().unwrap().at_least(3, 6) {
            let screen = server.tmux_ok(&["capture-pane", "-p", "-M", "-J", "-t", &self.barrier]);
            let returned: Vec<&str> = screen
                .lines()
                .filter(|line| line.contains("returned"))
                .collect();
            assert_eq!(
                returned.len(),
                1,
                "the barrier pane holds other `returned` lines: {screen:?}\ncalls: {:?}",
                calls(&self.log)
            );
            assert!(
                returned[0].contains("returned 7"),
                "the barrier pane's one `returned` line is not its own: {screen:?}"
            );
        }
    }
}

/// Test 1: the plugin's startup one-shots exit 1 (the stand-in fails them all) and no pane is left
/// in a mode.
#[test]
fn a_startup_job_that_fails_leaves_no_message_in_a_pane() {
    let rig = Rig::start();
    rig.barrier_check();
    assert_eq!(
        rig.other_pane_modes(),
        [format!("{} 0", rig.server.pane())],
        "calls: {:?}",
        calls(&rig.log)
    );
}

/// Test 2: new window, split, select-pane, a switch of the client's session and a session closed
/// while a client is attached to another one: every family of hook jobs runs, fails, and no pane
/// anywhere is left in a mode.
#[test]
fn hook_jobs_that_fail_leave_no_message_in_any_pane_and_the_server_stays_up() {
    let rig = Rig::start();
    let server = &rig.server;
    let first_pid = rig.pid();

    server.tmux_ok(&["new-session", "-d", "-s", "two", "-x", "80", "-y", "24"]);
    let inner = Tmux::with_socket_name(server.socket());
    let client = Client::attach(&inner, "one");

    server.tmux_ok(&["new-window", "-d", "-t", "one:"]);
    server.tmux_ok(&["split-window", "-d", "-t", "one:0"]);
    server.tmux_ok(&["select-pane", "-t", "one:0.0"]);
    server.tmux_ok(&["select-window", "-t", "one:1"]);
    server.tmux_ok(&["switch-client", "-c", &client.name, "-t", "two"]);
    server.tmux_ok(&["switch-client", "-c", &client.name, "-t", "one"]);
    // The session that goes while a client is attached to another one.
    server.tmux_ok(&["kill-session", "-t", "two"]);

    // Positive waits, so that the check below is not a check of jobs that have not run: each family
    // has been called, the closing one included, and nothing is still running.
    for call in [
        "watch attach",
        "seen",
        "session autosave",
        "session autosave --on-close",
        "plugin event workspace.closed",
        "plugin event tab.created",
        "plugin event pane.created",
        "plugin event pane.closed",
    ] {
        rig.wait_for_call(call);
    }
    rig.wait_for_jobs_to_end();

    // Every job that ended has been handled by tmux (the barrier), and none left a pane in a mode.
    rig.barrier_check();
    // The server is the same one, still answering.
    assert_eq!(rig.pid(), first_pid);
}
